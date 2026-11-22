//! The Wayland server, on its own thread: smithay's protocol state, a calloop event loop, and the
//! bridge to the game (commands in, window snapshots out).
//!
//! No renderer: each surface's committed buffer is copied (wl_shm) into that surface's
//! [`LiveTexture`], only its damaged regions, and the game's renderer draws the texture.
#![allow(unsafe_code)]

use crate::{Input, Layer, LivePixelFormat, LiveRegion, LiveTexture, Window, WindowId};
use smithay::{
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager, WindowSurfaceType},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle,
        },
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent, Sender, channel},
            generic::Generic,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage,
            SurfaceAttributes, TraversalAction, get_parent, is_sync_subsurface, with_states,
            with_surface_tree_downward,
        },
        output::{OutputHandler, OutputManagerState},
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        socket::ListeningSocketSource,
    },
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// What the game sends the server thread.
pub(crate) enum Command {
    Input(Input),
    /// The game showed a frame: windows may draw their next.
    FrameDone,
}

/// The game's end of the command channel.
pub(crate) struct Commands {
    sender: Mutex<Sender<Command>>,
}

impl Commands {
    pub(crate) fn send(&self, command: Command) {
        // The thread only stops with the process; a closed channel means it died, already logged.
        let _ = self
            .sender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(command);
    }
}

/// Start the server thread; returns the socket name once it listens.
pub(crate) fn start() -> Result<(String, Commands), String> {
    let (sender, receiver) = channel::<Command>();
    let (ready, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::Builder::new()
        .name("Wayland compositor".into())
        .spawn(move || {
            if let Err(error) = run(receiver, &ready) {
                let _ = ready.send(Err(error.clone()));
                log::error!("wayland_wm: the compositor stopped: {error}");
            }
        })
        .map_err(|e| e.to_string())?;
    let socket = ready_rx
        .recv()
        .map_err(|_| "the compositor thread ended before listening".to_string())??;
    Ok((
        socket,
        Commands {
            sender: Mutex::new(sender),
        },
    ))
}

/// One toplevel the server keeps.
struct Toplevel {
    id: WindowId,
    window: smithay::desktop::Window,
}

pub(crate) struct State {
    display: DisplayHandle,
    compositor: CompositorState,
    xdg_shell: XdgShellState,
    shm: ShmState,
    seat_state: SeatState<Self>,
    _output_manager: OutputManagerState,
    output: Output,
    _seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,
    popups: PopupManager,
    toplevels: Vec<Toplevel>,
    next_id: u64,
    focused: Option<WindowId>,
    /// The window the pointer is over, and where in it (its root surface's space).
    pointer_window: Option<(WindowId, Point<f64, Logical>)>,
    /// Something a snapshot shows changed since the last one.
    dirty: bool,
    start: Instant,
    _signal: LoopSignal,
}

/// Per-surface data: the texture its buffers go into.
struct SurfaceTexture {
    texture: LiveTexture,
    /// The surface's size in logical pixels, from its last buffer.
    size: [i32; 2],
    /// Whether the surface has a buffer now.
    mapped: bool,
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

fn run(
    commands: Channel<Command>,
    ready: &std::sync::mpsc::Sender<Result<String, String>>,
) -> Result<(), String> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let display: Display<State> = Display::new().map_err(|e| e.to_string())?;
    let dh = display.handle();

    let compositor = CompositorState::new::<State>(&dh);
    let xdg_shell = XdgShellState::new::<State>(&dh);
    let shm = ShmState::new::<State>(&dh, vec![]);
    let output_manager = OutputManagerState::new_with_xdg_output::<State>(&dh);
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "horizon");
    let keyboard = seat
        .add_keyboard(XkbConfig::default(), 400, 30)
        .map_err(|e| format!("keyboard: {e}"))?;
    let pointer = seat.add_pointer();

    // One output the size of a common screen: programs size their windows by it.
    let output = Output::new(
        "HORIZON-1".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Horizon".into(),
            model: "Minecraft".into(),
        },
    );
    let _global = output.create_global::<State>(&dh);
    let mode = OutputMode {
        size: (1920, 1080).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let socket = ListeningSocketSource::new_auto().map_err(|e| e.to_string())?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    let handle = event_loop.handle();
    handle
        .insert_source(socket, |stream, _, state: &mut State| {
            if let Err(error) = state
                .display
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                log::warn!("wayland_wm: a client couldn't connect: {error}");
            }
        })
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut State| {
                // Safety: the display is never dropped while the loop owns this source.
                unsafe {
                    display.get_mut().dispatch_clients(state).ok();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(commands, |event, _, state: &mut State| {
            if let ChannelEvent::Msg(command) = event {
                state.command(command);
            }
        })
        .map_err(|e| e.to_string())?;

    let mut state = State {
        display: dh,
        compositor,
        xdg_shell,
        shm,
        seat_state,
        _output_manager: output_manager,
        output,
        _seat: seat,
        keyboard,
        pointer,
        popups: PopupManager::default(),
        toplevels: Vec::new(),
        next_id: 1,
        focused: None,
        pointer_window: None,
        dirty: false,
        start: Instant::now(),
        _signal: event_loop.get_signal(),
    };
    let _ = ready.send(Ok(socket_name));

    loop {
        event_loop
            .dispatch(Some(Duration::from_millis(100)), &mut state)
            .map_err(|e| e.to_string())?;
        state.popups.cleanup();
        if state.dirty {
            state.dirty = false;
            state.publish();
        }
        let _ = state.display.flush_clients();
    }
}

impl State {
    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn toplevel(&self, id: WindowId) -> Option<&Toplevel> {
        self.toplevels.iter().find(|t| t.id == id)
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::FrameDone => {
                let time = self.time();
                for toplevel in &self.toplevels {
                    let surface = toplevel.window.toplevel().expect("wayland window").wl_surface();
                    send_frames(surface, time);
                    for (popup, _) in PopupManager::popups_for_surface(surface) {
                        send_frames(popup.wl_surface(), time);
                    }
                }
            }
            Command::Input(input) => self.input(input),
        }
    }

    fn input(&mut self, input: Input) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        match input {
            Input::PointerMotion { window, at } => {
                let Some(toplevel) = self.toplevel(window) else {
                    return;
                };
                // The window's space starts at its geometry; the surfaces' at the root surface.
                let geometry = toplevel.window.geometry();
                let location = Point::<f64, Logical>::from((at[0], at[1])) + geometry.loc.to_f64();
                let focus = toplevel
                    .window
                    .surface_under(location, WindowSurfaceType::ALL)
                    .map(|(surface, origin)| (surface, origin.to_f64()));
                self.pointer_window = Some((window, location));
                let pointer = self.pointer.clone();
                pointer.motion(
                    self,
                    focus,
                    &MotionEvent {
                        location,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
            }
            Input::PointerLeave => {
                self.pointer_window = None;
                let pointer = self.pointer.clone();
                pointer.motion(
                    self,
                    None,
                    &MotionEvent {
                        location: (0.0, 0.0).into(),
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
            }
            Input::PointerButton { button, pressed } => {
                let pointer = self.pointer.clone();
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state: if pressed {
                            smithay::backend::input::ButtonState::Pressed
                        } else {
                            smithay::backend::input::ButtonState::Released
                        },
                    },
                );
                pointer.frame(self);
            }
            Input::PointerAxis {
                horizontal,
                vertical,
                discrete,
            } => {
                use smithay::backend::input::{Axis, AxisSource};
                let mut frame = AxisFrame::new(time).source(if discrete.is_some() {
                    AxisSource::Wheel
                } else {
                    AxisSource::Finger
                });
                if horizontal != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal);
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                }
                if let Some([h, v]) = discrete {
                    if h != 0 {
                        frame = frame.v120(Axis::Horizontal, h * 120);
                    }
                    if v != 0 {
                        frame = frame.v120(Axis::Vertical, v * 120);
                    }
                }
                let pointer = self.pointer.clone();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            Input::Key { code, pressed } => {
                let keyboard = self.keyboard.clone();
                keyboard.input::<(), _>(
                    self,
                    // XKB key codes are the Linux input codes plus 8.
                    (code + 8).into(),
                    if pressed {
                        smithay::backend::input::KeyState::Pressed
                    } else {
                        smithay::backend::input::KeyState::Released
                    },
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            Input::Focus(window) => self.focus(window, serial),
            Input::Resize { window, size } => {
                if let Some(toplevel) = self.toplevel(window) {
                    let surface = toplevel.window.toplevel().expect("wayland window");
                    surface.with_pending_state(|state| {
                        state.size = (size[0] > 0 && size[1] > 0)
                            .then(|| (size[0], size[1]).into());
                    });
                    surface.send_pending_configure();
                }
            }
            Input::Close(window) => {
                if let Some(toplevel) = self.toplevel(window) {
                    toplevel.window.toplevel().expect("wayland window").send_close();
                }
            }
        }
    }

    fn focus(&mut self, window: Option<WindowId>, serial: Serial) {
        let previous = self.focused;
        self.focused = window.filter(|id| self.toplevel(*id).is_some());
        for toplevel in &self.toplevels {
            let active = Some(toplevel.id) == self.focused;
            if active != (Some(toplevel.id) == previous) || active {
                toplevel.window.set_activated(active);
                toplevel
                    .window
                    .toplevel()
                    .expect("wayland window")
                    .send_pending_configure();
            }
        }
        let surface = self
            .focused
            .and_then(|id| self.toplevel(id))
            .map(|t| t.window.toplevel().expect("wayland window").wl_surface().clone());
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface, serial);
        self.dirty = true;
    }

    /// Copy the buffers committed anywhere in `root`'s tree into their textures.
    fn import_tree(&mut self, root: &WlSurface) {
        let mut changed = false;
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let attributes = cached.current();
                let Some(assignment) = attributes.buffer.take() else {
                    return;
                };
                let damage = std::mem::take(&mut attributes.damage);
                let scale = attributes.buffer_scale.max(1);
                let texture = states
                    .data_map
                    .get_or_insert_threadsafe(|| Mutex::new(SurfaceTexture {
                        texture: LiveTexture::new(0, 0),
                        size: [0, 0],
                        mapped: false,
                    }));
                let mut texture = texture.lock().unwrap_or_else(|p| p.into_inner());
                match assignment {
                    BufferAssignment::Removed => {
                        changed |= texture.mapped;
                        texture.mapped = false;
                    }
                    BufferAssignment::NewBuffer(buffer) => {
                        if let Some([width, height]) = import(&buffer, &texture.texture, &damage, scale)
                        {
                            let size = [width as i32 / scale, height as i32 / scale];
                            changed |= !texture.mapped || texture.size != size;
                            texture.size = size;
                            texture.mapped = true;
                        }
                        buffer.release();
                    }
                }
            },
            |_, _, _| true,
        );
        self.dirty |= changed;
    }

    /// The windows as presenters see them.
    fn publish(&self) {
        let Some(wm) = crate::wm() else { return };
        let windows = self
            .toplevels
            .iter()
            .map(|toplevel| {
                let surface = toplevel.window.toplevel().expect("wayland window");
                let geometry = toplevel.window.geometry();
                let (title, app_id) = with_states(surface.wl_surface(), |states| {
                    let data = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .map(|d| d.lock().unwrap_or_else(|p| p.into_inner()));
                    data.map(|d| {
                        (
                            d.title.clone().unwrap_or_default(),
                            d.app_id.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default()
                });
                let mut layers = Vec::new();
                let origin = Point::<i32, Logical>::from((-geometry.loc.x, -geometry.loc.y));
                collect_layers(surface.wl_surface(), origin, false, &mut layers);
                for (popup, at) in PopupManager::popups_for_surface(surface.wl_surface()) {
                    // Popup offsets are from the window's geometry already.
                    let at = at - popup.geometry().loc;
                    collect_layers(popup.wl_surface(), at, true, &mut layers);
                }
                Window {
                    id: toplevel.id,
                    title,
                    app_id,
                    size: [geometry.size.w, geometry.size.h],
                    focused: self.focused == Some(toplevel.id),
                    layers,
                }
            })
            .collect();
        wm.publish(windows);
    }
}

/// The mapped surfaces of `surface`'s tree (subsurfaces at their places), with `surface` at `at`.
fn collect_layers(surface: &WlSurface, at: Point<i32, Logical>, popup: bool, layers: &mut Vec<Layer>) {
    with_surface_tree_downward(
        surface,
        at,
        |_, states, at| {
            let mut at = *at;
            if states.role == Some("subsurface") {
                let current = states
                    .cached_state
                    .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                    .current()
                    .location;
                at += current;
            }
            TraversalAction::DoChildren(at)
        },
        |_, states, at| {
            let mut at = *at;
            if states.role == Some("subsurface") {
                at += states
                    .cached_state
                    .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                    .current()
                    .location;
            }
            if let Some(texture) = states.data_map.get::<Mutex<SurfaceTexture>>() {
                let texture = texture.lock().unwrap_or_else(|p| p.into_inner());
                if texture.mapped {
                    layers.push(Layer {
                        texture: texture.texture.clone(),
                        at: [at.x, at.y],
                        size: texture.size,
                        popup,
                    });
                }
            }
        },
        |_, _, _| true,
    );
}

/// Copy a committed buffer's damage into `texture`; its size in buffer pixels, or `None` for a
/// buffer this compositor can't read (not wl_shm, an unknown format).
fn import(buffer: &WlBuffer, texture: &LiveTexture, damage: &[Damage], scale: i32) -> Option<[u32; 2]> {
    with_buffer_contents(buffer, |ptr, len, data| {
        let format = match data.format {
            wl_shm::Format::Argb8888 => LivePixelFormat::Bgra8,
            wl_shm::Format::Xrgb8888 => LivePixelFormat::Bgrx8,
            wl_shm::Format::Abgr8888 => LivePixelFormat::Rgba8,
            wl_shm::Format::Xbgr8888 => LivePixelFormat::Rgbx8,
            _ => return None,
        };
        let (width, height) = (data.width.max(0) as u32, data.height.max(0) as u32);
        let offset = data.offset.max(0) as usize;
        let stride = data.stride.max(0) as usize;
        if offset > len {
            return None;
        }
        // Safety: smithay maps the pool for the closure's duration and checked `len`.
        let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
        let regions: Vec<LiveRegion> = damage
            .iter()
            .map(|damage| {
                let rect: Rectangle<i32, smithay::utils::Buffer> = match damage {
                    Damage::Buffer(rect) => *rect,
                    Damage::Surface(rect) => Rectangle::new(
                        (rect.loc.x * scale, rect.loc.y * scale).into(),
                        (rect.size.w * scale, rect.size.h * scale).into(),
                    ),
                };
                let x = rect.loc.x.max(0) as u32;
                let y = rect.loc.y.max(0) as u32;
                let w = (rect.size.w + rect.loc.x.min(0)).max(0) as u32;
                let h = (rect.size.h + rect.loc.y.min(0)).max(0) as u32;
                LiveRegion::new(x, y, w, h)
            })
            .collect();
        // No damage on a new buffer still means its contents are new: copy all of it.
        let regions = if regions.iter().all(LiveRegion::is_empty) {
            Vec::new()
        } else {
            regions
        };
        texture
            .write(width, height, stride, format, &pool[offset..], &regions)
            .then_some([width, height])
    })
    .ok()
    .flatten()
}

fn send_frames(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, _| true,
    );
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        if is_sync_subsurface(surface) {
            // Applied with its parent's next commit.
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.popups.commit(surface);
        if let Some(toplevel) = self
            .toplevels
            .iter()
            .find(|t| t.window.toplevel().is_some_and(|s| s.wl_surface() == &root))
        {
            toplevel.window.on_commit();
            let surface = toplevel.window.toplevel().expect("wayland window");
            if !surface.is_initial_configure_sent() {
                surface.send_configure();
            }
        }
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(&root)
            && !popup.is_initial_configure_sent()
        {
            let _ = popup.send_configure();
        }
        self.import_tree(&root);
        // A popup's tree belongs to its toplevel's window: republish that one.
        self.dirty = true;
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.output.enter(surface.wl_surface());
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        let id = WindowId(self.next_id);
        self.next_id += 1;
        self.toplevels.push(Toplevel {
            id,
            window: smithay::desktop::Window::new_wayland_window(surface),
        });
        self.dirty = true;
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let gone: Vec<WindowId> = self
            .toplevels
            .iter()
            .filter(|t| t.window.toplevel().is_some_and(|s| s == &surface))
            .map(|t| t.id)
            .collect();
        self.toplevels
            .retain(|t| !t.window.toplevel().is_some_and(|s| s == &surface));
        if self.focused.is_some_and(|id| gone.contains(&id)) {
            self.focused = None;
        }
        if self.pointer_window.is_some_and(|(id, _)| gone.contains(&id)) {
            self.pointer_window = None;
        }
        self.dirty = true;
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.output.enter(surface.wl_surface());
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            log::warn!("wayland_wm: a popup couldn't be tracked: {error}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        self.dirty = true;
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // Popup grabs (a menu closing on a click outside it) come with the presenters' input
        // model; until then a menu closes when the program closes it.
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.dirty = true;
    }

    fn app_id_changed(&mut self, _surface: ToplevelSurface) {
        self.dirty = true;
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl OutputHandler for State {}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);
