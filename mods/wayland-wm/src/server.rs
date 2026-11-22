//! The Wayland server, on its own thread: smithay's protocol state, a calloop event loop, and the
//! bridge to the game (commands in, window snapshots out).
//!
//! No renderer of its own: each surface's committed buffer goes into that surface's
//! [`LiveTexture`], and the game's renderer draws the texture. A wl_shm buffer is copied, only its
//! damaged regions, and given back at once. A dmabuf (a GPU client's buffer, advertised with the
//! formats the game's GPU imports) is handed to the renderer as it is: the commit waits for the
//! client's GPU work on it (implicit sync), and the buffer is given back once the renderer's GPU
//! has copied it.
#![allow(unsafe_code)]

use crate::{Input, Layer, LivePixelFormat, LiveRegion, LiveTexture, Window, WindowId};
use smithay::{
    backend::allocator::{
        Buffer, Format, Fourcc, Modifier,
        dmabuf::{Dmabuf, WeakDmabuf},
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle, XkbConfig},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent, Sender, channel},
            generic::Generic,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage,
            SurfaceAttributes, TraversalAction, add_blocker, add_pre_commit_hook, get_parent,
            is_sync_subsurface, with_states, with_surface_tree_downward,
        },
        dmabuf::{
            DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
            get_dmabuf,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
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
    /// Its mapped surfaces as last published, topmost last, in the window's space: what the
    /// pointer can be over.
    hits: Vec<(WlSurface, Rectangle<i32, Logical>)>,
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
    data_device: DataDeviceState,
    dmabuf: DmabufState,
    _dmabuf_global: Option<DmabufGlobal>,
    /// Each live client dmabuf's id for the renderer's import cache: stable while it lives.
    dmabuf_ids: Vec<(WeakDmabuf, u64)>,
    next_dmabuf: u64,
    handle: LoopHandle<'static, State>,
    focused: Option<WindowId>,
    /// The window the pointer is over, and where in it (its root surface's space).
    pointer_window: Option<(WindowId, Point<f64, Logical>)>,
    /// Something a snapshot shows changed since the last one.
    dirty: bool,
    /// Each window's shape when last logged, so the log has one line per change.
    logged: Mutex<Vec<(WindowId, [i32; 2], usize)>>,
    start: Instant,
    _signal: LoopSignal,
}

/// Per-surface data: the texture its buffers go into.
struct SurfaceTexture {
    texture: LiveTexture,
    /// A dmabuf the renderer reads, with the generation it was given at: given back once the
    /// renderer has consumed that generation, or when a newer buffer replaces it.
    held: Option<(WlBuffer, u64)>,
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

    // GPU clients' buffers, in the layouts the game's GPU imports. Without them (no Vulkan
    // import), clients fall back to wl_shm.
    let data_device = DataDeviceState::new::<State>(&dh);
    let mut dmabuf = DmabufState::new();
    let dmabuf_global = horizon_live_texture_caps().and_then(|caps| {
        let formats: Vec<Format> = caps
            .formats
            .iter()
            .filter_map(|f| {
                Some(Format {
                    code: Fourcc::try_from(f.fourcc).ok()?,
                    modifier: Modifier::from(f.modifier),
                })
            })
            .collect();
        let feedback = DmabufFeedbackBuilder::new(caps.device as _, formats)
            .build()
            .map_err(|e| log::warn!("wayland_wm: no dmabuf feedback: {e}"))
            .ok()?;
        log::info!(
            "wayland_wm: offering dmabufs in {} formats",
            caps.formats.len()
        );
        Some(dmabuf.create_global_with_default_feedback::<State>(&dh, &feedback))
    });
    if dmabuf_global.is_none() {
        log::info!("wayland_wm: the game's GPU imports no dmabufs; clients draw to shared memory");
    }

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
        data_device,
        dmabuf,
        _dmabuf_global: dmabuf_global,
        dmabuf_ids: Vec::new(),
        next_dmabuf: 0,
        handle: event_loop.handle(),
        focused: None,
        pointer_window: None,
        dirty: false,
        logged: Mutex::new(Vec::new()),
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
                    let surface = toplevel
                        .window
                        .toplevel()
                        .expect("wayland window")
                        .wl_surface();
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
                // The topmost surface under the pointer; both in the window's space.
                let location = Point::<f64, Logical>::from((at[0], at[1]));
                let focus = toplevel
                    .hits
                    .iter()
                    .rev()
                    .find(|(_, rect)| rect.to_f64().contains(location))
                    .map(|(surface, rect)| (surface.clone(), rect.loc.to_f64()));
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
                        state.size =
                            (size[0] > 0 && size[1] > 0).then(|| (size[0], size[1]).into());
                    });
                    surface.send_pending_configure();
                }
            }
            Input::Close(window) => {
                if let Some(toplevel) = self.toplevel(window) {
                    toplevel
                        .window
                        .toplevel()
                        .expect("wayland window")
                        .send_close();
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
        let surface = self.focused.and_then(|id| self.toplevel(id)).map(|t| {
            t.window
                .toplevel()
                .expect("wayland window")
                .wl_surface()
                .clone()
        });
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface, serial);
        self.dirty = true;
    }

    /// The renderer's id for `dmabuf`, the same each time it comes back.
    fn dmabuf_id(&mut self, dmabuf: &Dmabuf) -> u64 {
        self.dmabuf_ids.retain(|(weak, _)| !weak.is_gone());
        if let Some((_, id)) = self
            .dmabuf_ids
            .iter()
            .find(|(weak, _)| weak.upgrade().as_ref() == Some(dmabuf))
        {
            return *id;
        }
        self.next_dmabuf += 1;
        self.dmabuf_ids.push((dmabuf.weak(), self.next_dmabuf));
        self.next_dmabuf
    }

    /// Copy the buffers committed anywhere in `root`'s tree into their textures.
    fn import_tree(&mut self, root: &WlSurface) {
        let mut changed = false;
        // Ids first: the traversal can't borrow the state.
        let mut ids = Vec::new();
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                if let Some(BufferAssignment::NewBuffer(buffer)) = &cached.current().buffer
                    && let Ok(dmabuf) = get_dmabuf(buffer)
                {
                    ids.push(dmabuf.clone());
                }
            },
            |_, _, _| true,
        );
        let ids: Vec<(Dmabuf, u64)> = ids
            .into_iter()
            .map(|dmabuf| {
                let id = self.dmabuf_id(&dmabuf);
                (dmabuf, id)
            })
            .collect();
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
                let texture = states.data_map.get_or_insert_threadsafe(|| {
                    Mutex::new(SurfaceTexture {
                        texture: LiveTexture::new(0, 0),
                        held: None,
                        size: [0, 0],
                        mapped: false,
                    })
                });
                let mut texture = texture.lock().unwrap_or_else(|p| p.into_inner());
                match assignment {
                    BufferAssignment::Removed => {
                        changed |= texture.mapped;
                        texture.mapped = false;
                        if let Some((held, _)) = texture.held.take() {
                            held.release();
                        }
                    }
                    BufferAssignment::NewBuffer(buffer) if get_dmabuf(&buffer).is_ok() => {
                        let dmabuf = get_dmabuf(&buffer).expect("checked").clone();
                        let id = ids
                            .iter()
                            .find(|(d, _)| *d == dmabuf)
                            .map_or(0, |(_, id)| *id);
                        match give_dmabuf(&texture.texture, &dmabuf, id) {
                            Some(generation) => {
                                let size = [
                                    dmabuf.width() as i32 / scale,
                                    dmabuf.height() as i32 / scale,
                                ];
                                changed |= !texture.mapped || texture.size != size;
                                texture.size = size;
                                texture.mapped = true;
                                // The one it replaces won't be read again.
                                if let Some((old, _)) = texture.held.replace((buffer, generation))
                                    && texture.held.as_ref().is_none_or(|(b, _)| *b != old)
                                {
                                    old.release();
                                }
                            }
                            None => buffer.release(),
                        }
                    }
                    BufferAssignment::NewBuffer(buffer) => {
                        let imported = import(&buffer, &texture.texture, &damage, scale);
                        if imported.is_none() {
                            log::warn!(
                                "wayland_wm: a buffer couldn't be read (not wl_shm, or a format it doesn't copy)"
                            );
                        }
                        if let Some([width, height]) = imported {
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
    fn publish(&mut self) {
        let Some(wm) = crate::wm() else { return };
        let windows: Vec<Window> = self
            .toplevels
            .iter_mut()
            .map(|toplevel| {
                let surface = toplevel.window.toplevel().expect("wayland window").clone();
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
                // The window's own surfaces in the root surface's space first: its visible area
                // is what xdg geometry says, else all of them.
                let mut layers = Vec::new();
                let mut hits = Vec::new();
                collect_layers(
                    surface.wl_surface(),
                    (0, 0).into(),
                    false,
                    &mut layers,
                    &mut hits,
                );
                let geometry = with_states(surface.wl_surface(), |states| {
                    states
                        .cached_state
                        .get::<smithay::wayland::shell::xdg::SurfaceCachedState>()
                        .current()
                        .geometry
                })
                .unwrap_or_else(|| {
                    hits.iter()
                        .map(|(_, rect)| *rect)
                        .reduce(|a: Rectangle<i32, Logical>, b| a.merge(b))
                        .unwrap_or_default()
                });
                // Then the window's space: its geometry's corner is (0, 0).
                for layer in &mut layers {
                    layer.at = [layer.at[0] - geometry.loc.x, layer.at[1] - geometry.loc.y];
                }
                for (_, rect) in &mut hits {
                    rect.loc -= geometry.loc;
                }
                for (popup, at) in PopupManager::popups_for_surface(surface.wl_surface()) {
                    // Popup offsets are from the window's geometry already.
                    let at = at - popup.geometry().loc;
                    collect_layers(popup.wl_surface(), at, true, &mut layers, &mut hits);
                }
                toplevel.hits = hits;
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
        let shapes: Vec<_> = windows
            .iter()
            .map(|w: &Window| (w.id, w.size, w.layers.len()))
            .collect();
        let mut logged = self.logged.lock().unwrap_or_else(|p| p.into_inner());
        for (window, shape) in windows.iter().zip(&shapes) {
            if !logged.contains(shape) {
                log::info!(
                    "wayland_wm: window {} {:?} is {}x{} with {} layers",
                    window.id.0,
                    window.title,
                    window.size[0],
                    window.size[1],
                    window.layers.len()
                );
            }
        }
        *logged = shapes;
        drop(logged);
        wm.publish(windows);
    }
}

/// The mapped surfaces of `surface`'s tree (subsurfaces at their places), with `surface` at `at`,
/// and where each lies for hit testing.
fn collect_layers(
    surface: &WlSurface,
    at: Point<i32, Logical>,
    popup: bool,
    layers: &mut Vec<Layer>,
    hits: &mut Vec<(WlSurface, Rectangle<i32, Logical>)>,
) {
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
        |surface, states, at| {
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
                    hits.push((
                        surface.clone(),
                        Rectangle::new(at, (texture.size[0], texture.size[1]).into()),
                    ));
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
fn import(
    buffer: &WlBuffer,
    texture: &LiveTexture,
    damage: &[Damage],
    scale: i32,
) -> Option<[u32; 2]> {
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

/// Hand `dmabuf` to `texture`'s renderer as buffer `id`; the generation it was given at, or `None`
/// for a layout the renderer can't read.
fn give_dmabuf(texture: &LiveTexture, dmabuf: &Dmabuf, id: u64) -> Option<u64> {
    if dmabuf.num_planes() != 1 {
        return None;
    }
    let format = dmabuf.format();
    let planes = dmabuf
        .handles()
        .zip(dmabuf.offsets())
        .zip(dmabuf.strides())
        .map(|((fd, offset), stride)| {
            Some(mod_api::LiveDmabufPlane {
                fd: fd.try_clone_to_owned().ok()?,
                offset,
                stride,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(texture.set_dmabuf(mod_api::LiveDmabuf {
        id,
        width: dmabuf.width(),
        height: dmabuf.height(),
        fourcc: format.code as u32,
        modifier: u64::from(format.modifier),
        planes,
        y_inverted: dmabuf.y_inverted(),
    }))
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
            // A dmabuf the renderer's GPU has copied goes back to the client.
            if let Some(texture) = states.data_map.get::<Mutex<SurfaceTexture>>() {
                let mut texture = texture.lock().unwrap_or_else(|p| p.into_inner());
                if texture
                    .held
                    .as_ref()
                    .is_some_and(|(_, generation)| texture.texture.consumed() >= *generation)
                    && let Some((buffer, _)) = texture.held.take()
                {
                    buffer.release();
                }
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

    fn new_surface(&mut self, surface: &WlSurface) {
        // A dmabuf's commit waits until the client's GPU has finished drawing it.
        add_pre_commit_hook::<Self, _>(surface, |state, _display, surface| {
            let dmabuf = with_states(surface, |states| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                match cached.pending().buffer.as_ref() {
                    Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
                    _ => None,
                }
            });
            let Some(dmabuf) = dmabuf else { return };
            let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            let inserted = state.handle.insert_source(source, move |_, _, state| {
                let display = state.display.clone();
                state
                    .client_compositor_state(&client)
                    .blocker_cleared(state, &display);
                Ok(())
            });
            if inserted.is_ok() {
                add_blocker(surface, blocker);
            }
        });
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
        log::info!("wayland_wm: window {} opened", id.0);
        self.toplevels.push(Toplevel {
            id,
            window: smithay::desktop::Window::new_wayland_window(surface),
            hits: Vec::new(),
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
        if self
            .pointer_window
            .is_some_and(|(id, _)| gone.contains(&id))
        {
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

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
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

// Clipboard and drag and drop between the compositor's own programs (GTK requires the global).
// The player's desktop clipboard isn't bridged yet.
impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl ClientDndGrabHandler for State {}

impl ServerDndGrabHandler for State {}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Only layouts the renderer imports are offered; a buffer it still can't read keeps the
        // window's last picture (the renderer logs why).
        let _ = notifier.successful::<State>();
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_dmabuf!(State);
delegate_data_device!(State);

fn horizon_live_texture_caps() -> Option<mod_api::LiveDmabufCaps> {
    mod_api::live_dmabuf_caps()
}
