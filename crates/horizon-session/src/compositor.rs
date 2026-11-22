//! The Wayland server: smithay's protocol state on a calloop event loop, and the game's
//! connection.
//!
//! No renderer of its own: each surface's committed buffer goes to the game. A wl_shm buffer is
//! copied, only its damaged regions, into a mirror the session keeps (so a game that reconnects
//! gets every window's picture at once), sent on, and given back to its program at once. A dmabuf
//! (a GPU program's buffer, advertised with the formats the game's GPU imports) is passed to the
//! game as a file descriptor: the commit waits for the program's GPU work on it (implicit sync),
//! and the buffer goes back to its program once the game's GPU has copied it.
#![allow(unsafe_code)]

use crate::Options;
use crate::game::GameConn;
use crate::programs::{GameExit, Programs, Supervised};
use horizon_wm_ipc::{
    DmabufCaps, Input, LayerInfo, PixelFormat, Region, SurfaceId, ToGame, ToSession, VERSION,
    WindowId, WindowInfo,
};
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
        calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle, Resource, Weak,
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
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Frames for programs while no game shows them: slow, so they stay responsive without drawing
/// for nobody.
const IDLE_FRAME: Duration = Duration::from_secs(1);
/// The longest `--spawn` programs wait for a game.
const SPAWN_WAIT: Duration = Duration::from_secs(10);
/// How long programs get to close when the session ends.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

static NEXT_SURFACE: AtomicU64 = AtomicU64::new(1);

/// One toplevel the session keeps.
struct Toplevel {
    id: WindowId,
    window: smithay::desktop::Window,
    /// Its mapped surfaces as last published, topmost last, in the window's space: what the
    /// pointer can be over.
    hits: Vec<(WlSurface, Rectangle<i32, Logical>)>,
    /// Whether the game shows it: a hidden one is suspended and gets no frames.
    visible: bool,
}

/// What a surface last showed, so a game that (re)connects gets it at once.
enum Content {
    Empty,
    Shm(ShmMirror),
    Dmabuf { dmabuf: Dmabuf, id: u64 },
}

/// A shared-memory surface's pixels, rows of `width * 4` bytes.
struct ShmMirror {
    width: u32,
    height: u32,
    format: PixelFormat,
    pixels: Vec<u8>,
}

/// Per-surface data.
struct SurfaceData {
    id: SurfaceId,
    /// Its size in logical pixels, from its last buffer.
    size: [i32; 2],
    /// It has a buffer now.
    mapped: bool,
    content: Content,
    /// A dmabuf the game reads, and its `seq`: given back once the game has read it, or when a
    /// newer buffer replaces it.
    held: Option<(WlBuffer, u64)>,
    /// This surface's dmabufs sent so far.
    seq: u64,
}

/// A message for the game, built where the state can't be borrowed.
type Outgoing = (ToGame, Vec<u8>, Vec<OwnedFd>);

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub struct State {
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
    next_window: u64,
    data_device: DataDeviceState,
    dmabuf: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    /// Each live client dmabuf's id for the game's import cache: stable while it lives.
    dmabuf_ids: Vec<(WeakDmabuf, u64)>,
    next_dmabuf: u64,
    /// Every surface with a buffer so far, by id.
    surfaces: HashMap<SurfaceId, Weak<WlSurface>>,
    handle: LoopHandle<'static, State>,
    focused: Option<WindowId>,
    /// Something the window list shows changed since it was last sent.
    dirty: bool,
    /// Each window's shape when last logged, so the log has one line per change.
    logged: Vec<(WindowId, [i32; 2], usize)>,
    start: Instant,
    socket_name: String,
    game: Option<GameConn>,
    next_game: u64,
    /// Since when no game has been connected.
    game_gone: Option<Instant>,
    last_idle_frame: Instant,
    programs: Programs,
    /// A signal asked the session to end.
    stopping: bool,
    /// Names this run of the session for the game ([`ToGame::Hello`]).
    instance: u64,
    /// `--spawn` programs, held until a game says which GPU buffers it imports (else GPU programs
    /// would settle for shared memory), or until [`SPAWN_WAIT`] is up.
    held_spawns: Vec<String>,
}

pub fn run(options: Options) -> Result<ExitCode, String> {
    let control = options.control_path()?;
    let listener = bind_control(&control)?;

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
    let data_device = DataDeviceState::new::<State>(&dh);

    let socket = ListeningSocketSource::new_auto().map_err(|e| e.to_string())?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    let handle = event_loop.handle();
    handle
        .insert_source(socket, |stream, _, state: &mut State| {
            if let Err(error) = state
                .display
                .insert_client(stream, std::sync::Arc::new(ClientState::default()))
            {
                log::warn!("a program couldn't connect: {error}");
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
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state: &mut State| {
                while let Ok((stream, _)) = listener.accept() {
                    state.connect_game(stream);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| e.to_string())?;

    // Ending the session closes its windows politely, as quitting the game does.
    let signals = calloop::signals::Signals::new(&[
        calloop::signals::Signal::SIGTERM,
        calloop::signals::Signal::SIGINT,
        calloop::signals::Signal::SIGHUP,
    ])
    .map_err(|e| format!("signals: {e}"))?;
    handle
        .insert_source(signals, |event, _, state: &mut State| {
            log::info!("{:?}: ending the session", event.signal());
            state.stopping = true;
        })
        .map_err(|e| e.to_string())?;

    log::info!(
        "listening: WAYLAND_DISPLAY={socket_name}, game control {}",
        control.display()
    );
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
        next_window: 1,
        data_device,
        dmabuf: DmabufState::new(),
        dmabuf_global: None,
        dmabuf_ids: Vec::new(),
        next_dmabuf: 0,
        surfaces: HashMap::new(),
        handle: event_loop.handle(),
        focused: None,
        dirty: false,
        logged: Vec::new(),
        start: Instant::now(),
        socket_name,
        game: None,
        next_game: 1,
        game_gone: Some(Instant::now()),
        last_idle_frame: Instant::now(),
        programs: Programs::default(),
        stopping: false,
        instance: instance_number(),
        held_spawns: options.spawn.clone(),
    };
    let mut supervised = (!options.game.is_empty())
        .then(|| Supervised::new(options.game.clone(), control.display().to_string()));

    let result = loop {
        let busy = state.game.as_ref().is_some_and(GameConn::pending);
        let timeout = Duration::from_millis(if busy { 2 } else { 50 });
        if let Err(error) = event_loop.dispatch(Some(timeout), &mut state) {
            break Err(error.to_string());
        }
        state.popups.cleanup();
        if state.dirty {
            state.dirty = false;
            state.publish();
        }
        state.flush_game();
        let _ = state.display.flush_clients();
        state.programs.reap();
        state.idle_frames();
        if !state.held_spawns.is_empty()
            && (state.game_ready() || state.start.elapsed() >= SPAWN_WAIT)
        {
            for command in std::mem::take(&mut state.held_spawns) {
                state.programs.spawn(&command, &state.socket_name);
            }
        }
        if state.stopping {
            if let Some(game) = &mut supervised {
                game.stop();
            }
            state.close_all(&mut event_loop);
            break Ok(ExitCode::SUCCESS);
        }
        if let Some(game) = &mut supervised
            && let GameExit::Quit = game.poll()
        {
            state.close_all(&mut event_loop);
            break Ok(ExitCode::SUCCESS);
        }
        if let (Some(linger), Some(since)) = (options.linger, state.game_gone)
            && since.elapsed() >= linger
        {
            log::info!("no game for {}s: ending the session", linger.as_secs());
            state.close_all(&mut event_loop);
            break Ok(ExitCode::SUCCESS);
        }
    };
    let _ = std::fs::remove_file(&control);
    result
}

/// A number unlikely to name another run: the start time's nanoseconds and the process id.
fn instance_number() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 40)
}

/// Listen on `path`, unless another session already does.
fn bind_control(path: &PathBuf) -> Result<UnixListener, String> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(format!(
                "another session already listens on {}",
                path.display()
            ));
        }
        // A session that died left its socket behind.
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(listener)
}

impl State {
    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn toplevel(&self, id: WindowId) -> Option<&Toplevel> {
        self.toplevels.iter().find(|t| t.id == id)
    }

    fn game_ready(&self) -> bool {
        self.game.as_ref().is_some_and(|g| g.ready && !g.broken)
    }

    fn send(&mut self, message: ToGame, payload: &[u8], fds: Vec<OwnedFd>) {
        if let Some(game) = &mut self.game
            && game.ready
        {
            game.send(&message, payload, fds);
        }
    }

    // --- The game ---

    fn connect_game(&mut self, stream: UnixStream) {
        if self.game.is_some() {
            log::info!("a new game connected: it replaces the old one");
            self.disconnect_game();
        }
        let id = self.next_game;
        self.next_game += 1;
        let conn = match GameConn::new(id, stream) {
            Ok(conn) => conn,
            Err(error) => {
                log::warn!("the game's connection failed: {error}");
                return;
            }
        };
        let watch = match conn.stream().try_clone() {
            Ok(watch) => watch,
            Err(error) => {
                log::warn!("the game's connection failed: {error}");
                return;
            }
        };
        let inserted = self.handle.insert_source(
            Generic::new(watch, Interest::READ, Mode::Level),
            move |_, _, state: &mut State| Ok(state.game_readable(id)),
        );
        if let Err(error) = inserted {
            log::warn!("the game's connection can't be watched: {error}");
            return;
        }
        log::info!("a game connected");
        self.game = Some(conn);
    }

    fn game_readable(&mut self, id: u64) -> PostAction {
        let Some(game) = self.game.as_mut().filter(|g| g.id == id) else {
            return PostAction::Remove;
        };
        let messages = match game.read() {
            Ok(messages) => messages,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    log::info!("the game disconnected");
                } else {
                    log::warn!("the game's connection failed: {error}");
                }
                self.disconnect_game();
                return PostAction::Remove;
            }
        };
        for message in messages {
            self.game_message(message.message);
        }
        if self.game.as_ref().is_some_and(|g| g.id == id) {
            PostAction::Continue
        } else {
            PostAction::Remove
        }
    }

    fn game_message(&mut self, message: ToSession) {
        match message {
            ToSession::Hello { version, caps } => {
                if version != VERSION {
                    log::error!(
                        "the game speaks protocol {version}, this session {VERSION}: rebuild them together"
                    );
                    self.disconnect_game();
                    return;
                }
                if let Some(caps) = caps {
                    self.offer_dmabufs(&caps);
                }
                self.game_gone = None;
                let socket = self.socket_name.clone();
                if let Some(game) = &mut self.game {
                    game.ready = true;
                    game.send(
                        &ToGame::Hello {
                            version: VERSION,
                            socket,
                            instance: self.instance,
                        },
                        &[],
                        vec![],
                    );
                }
                self.resend_all();
            }
            ToSession::Input(input) => self.input(input),
            ToSession::FrameDone => self.frame_done(),
            ToSession::Consumed { surface, seq } => self.consumed(surface, seq),
            ToSession::Spawn { command } => {
                let socket = self.socket_name.clone();
                self.programs.spawn(&command, &socket);
            }
        }
    }

    /// The game is gone: give every buffer back, and let go of every key and button it held, so
    /// no program is left with a key stuck down.
    fn disconnect_game(&mut self) {
        if let Some(game) = self.game.take()
            && let Some(watch) = game.stream().try_clone().ok()
        {
            drop(watch);
        }
        self.game_gone = Some(Instant::now());
        for surface in self.surfaces.values().filter_map(|s| s.upgrade().ok()) {
            with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() {
                    let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some((buffer, _)) = data.held.take() {
                        buffer.release();
                    }
                }
            });
        }
        let serial = SERIAL_COUNTER.next_serial();
        self.input(Input::PointerLeave);
        self.focus(None, serial);
    }

    fn flush_game(&mut self) {
        if let Some(game) = &mut self.game {
            game.flush();
            if game.broken {
                self.disconnect_game();
            }
        }
    }

    /// Offer dmabufs in the layouts the game's GPU imports. Programs started before that draw to
    /// shared memory.
    fn offer_dmabufs(&mut self, caps: &DmabufCaps) {
        if self.dmabuf_global.is_some() {
            return;
        }
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
        match DmabufFeedbackBuilder::new(caps.device as _, formats).build() {
            Ok(feedback) => {
                log::info!("offering dmabufs in {} formats", caps.formats.len());
                self.dmabuf_global = Some(
                    self.dmabuf
                        .create_global_with_default_feedback::<State>(&self.display, &feedback),
                );
            }
            Err(error) => log::warn!("no dmabuf feedback: {error}"),
        }
    }

    /// Everything a new game needs: every surface's picture, then the windows.
    fn resend_all(&mut self) {
        let mut out: Vec<Outgoing> = Vec::new();
        for surface in self.surfaces.values().filter_map(|s| s.upgrade().ok()) {
            with_states(&surface, |states| {
                let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() else {
                    return;
                };
                let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                if !data.mapped {
                    return;
                }
                let id = data.id;
                data.seq += 1;
                let seq = data.seq;
                match &data.content {
                    Content::Empty => {}
                    Content::Shm(mirror) => out.push((
                        ToGame::Pixels {
                            surface: id,
                            width: mirror.width,
                            height: mirror.height,
                            format: mirror.format,
                            regions: Vec::new(),
                        },
                        mirror.pixels.clone(),
                        Vec::new(),
                    )),
                    Content::Dmabuf { dmabuf, id: buffer } => {
                        out.extend(dmabuf_message(id, *buffer, seq, dmabuf));
                    }
                }
            });
        }
        for (message, payload, fds) in out {
            self.send(message, &payload, fds);
        }
        self.dirty = true;
    }

    fn consumed(&mut self, surface: SurfaceId, seq: u64) {
        let Some(surface) = self.surfaces.get(&surface).and_then(|s| s.upgrade().ok()) else {
            return;
        };
        with_states(&surface, |states| {
            if let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() {
                let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                if data.held.as_ref().is_some_and(|(_, held)| *held <= seq)
                    && let Some((buffer, _)) = data.held.take()
                {
                    buffer.release();
                }
            }
        });
    }

    fn frame_done(&mut self) {
        let time = self.time();
        for toplevel in self.toplevels.iter().filter(|t| t.visible) {
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

    /// With no game, programs still get a frame now and then.
    fn idle_frames(&mut self) {
        if !self.game_ready() && self.last_idle_frame.elapsed() >= IDLE_FRAME {
            self.last_idle_frame = Instant::now();
            self.frame_done();
        }
    }

    /// Ask every window to close and give programs a moment to.
    fn close_all(&mut self, event_loop: &mut EventLoop<State>) {
        for toplevel in &self.toplevels {
            toplevel
                .window
                .toplevel()
                .expect("wayland window")
                .send_close();
        }
        let deadline = Instant::now() + CLOSE_GRACE;
        while !self.toplevels.is_empty() && Instant::now() < deadline {
            let _ = self.display.flush_clients();
            if event_loop
                .dispatch(Some(Duration::from_millis(50)), self)
                .is_err()
            {
                break;
            }
        }
    }

    // --- Input ---

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
                log::debug!("asking window {} to be {}x{}", window.0, size[0], size[1]);
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
            Input::Visible { window, visible } => {
                if let Some(toplevel) = self.toplevels.iter_mut().find(|t| t.id == window)
                    && toplevel.visible != visible
                {
                    toplevel.visible = visible;
                    let surface = toplevel.window.toplevel().expect("wayland window");
                    surface.with_pending_state(|state| {
                        if visible {
                            state.states.unset(xdg_toplevel::State::Suspended);
                        } else {
                            state.states.set(xdg_toplevel::State::Suspended);
                        }
                    });
                    surface.send_pending_configure();
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

    // --- Buffers ---

    /// The game's id for `dmabuf`, the same each time it comes back.
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

    /// Take the buffers committed anywhere in `root`'s tree, and send them to the game.
    fn import_tree(&mut self, root: &WlSurface) {
        // Ids first: the traversal can't borrow the state.
        let mut dmabufs = Vec::new();
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                if let Some(BufferAssignment::NewBuffer(buffer)) = &cached.current().buffer
                    && let Ok(dmabuf) = get_dmabuf(buffer)
                {
                    dmabufs.push(dmabuf.clone());
                }
            },
            |_, _, _| true,
        );
        let ids: Vec<(Dmabuf, u64)> = dmabufs
            .into_iter()
            .map(|dmabuf| {
                let id = self.dmabuf_id(&dmabuf);
                (dmabuf, id)
            })
            .collect();
        let ready = self.game_ready();
        let mut out: Vec<Outgoing> = Vec::new();
        let mut new_surfaces = Vec::new();
        let mut changed = false;
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let attributes = cached.current();
                let Some(assignment) = attributes.buffer.take() else {
                    return;
                };
                let damage = std::mem::take(&mut attributes.damage);
                let scale = attributes.buffer_scale.max(1);
                let data = states.data_map.get_or_insert_threadsafe(|| {
                    let id = SurfaceId(NEXT_SURFACE.fetch_add(1, Ordering::Relaxed));
                    new_surfaces.push((id, surface.downgrade()));
                    Mutex::new(SurfaceData {
                        id,
                        size: [0, 0],
                        mapped: false,
                        content: Content::Empty,
                        held: None,
                        seq: 0,
                    })
                });
                let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                match assignment {
                    BufferAssignment::Removed => {
                        changed |= data.mapped;
                        data.mapped = false;
                        data.content = Content::Empty;
                        if let Some((held, _)) = data.held.take() {
                            held.release();
                        }
                    }
                    BufferAssignment::NewBuffer(buffer) if get_dmabuf(&buffer).is_ok() => {
                        let dmabuf = get_dmabuf(&buffer).expect("checked").clone();
                        let buffer_id = ids
                            .iter()
                            .find(|(d, _)| *d == dmabuf)
                            .map_or(0, |(_, id)| *id);
                        if dmabuf.num_planes() != 1 {
                            log::warn!("a multi-plane dmabuf can't be shown yet");
                            buffer.release();
                            return;
                        }
                        let size = [
                            dmabuf.width() as i32 / scale,
                            dmabuf.height() as i32 / scale,
                        ];
                        changed |= !data.mapped || data.size != size;
                        data.size = size;
                        data.mapped = true;
                        let message = ready
                            .then(|| dmabuf_message(data.id, buffer_id, data.seq + 1, &dmabuf))
                            .flatten();
                        match message {
                            Some(message) => {
                                data.seq += 1;
                                out.push(message);
                                let seq = data.seq;
                                // The one it replaces won't be read again.
                                if let Some((old, _)) = data.held.replace((buffer.clone(), seq))
                                    && old != buffer
                                {
                                    old.release();
                                }
                            }
                            None => {
                                if let Some((old, _)) = data.held.take() {
                                    old.release();
                                }
                                buffer.release();
                            }
                        }
                        data.content = Content::Dmabuf {
                            dmabuf,
                            id: buffer_id,
                        };
                    }
                    BufferAssignment::NewBuffer(buffer) => {
                        let mirror = match std::mem::replace(&mut data.content, Content::Empty) {
                            Content::Shm(mirror) => Some(mirror),
                            _ => None,
                        };
                        match copy_shm(&buffer, mirror, &damage, scale) {
                            Some((mirror, regions)) => {
                                let size =
                                    [mirror.width as i32 / scale, mirror.height as i32 / scale];
                                changed |= !data.mapped || data.size != size;
                                data.size = size;
                                data.mapped = true;
                                if ready {
                                    out.push(pixels_message(data.id, &mirror, regions));
                                }
                                data.content = Content::Shm(mirror);
                            }
                            None => log::warn!(
                                "a buffer couldn't be read (not wl_shm, or a format it doesn't copy)"
                            ),
                        }
                        if let Some((old, _)) = data.held.take() {
                            old.release();
                        }
                        buffer.release();
                    }
                }
            },
            |_, _, _| true,
        );
        self.surfaces.extend(new_surfaces);
        for (message, payload, fds) in out {
            self.send(message, &payload, fds);
        }
        self.dirty |= changed;
    }

    /// The window list, to the game.
    fn publish(&mut self) {
        let parents: Vec<(WlSurface, WindowId)> = self
            .toplevels
            .iter()
            .map(|t| {
                (
                    t.window
                        .toplevel()
                        .expect("wayland window")
                        .wl_surface()
                        .clone(),
                    t.id,
                )
            })
            .collect();
        let focused = self.focused;
        let windows: Vec<WindowInfo> = self
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
                let parent = surface
                    .parent()
                    .and_then(|p| parents.iter().find(|(s, _)| *s == p).map(|(_, id)| *id));
                WindowInfo {
                    id: toplevel.id,
                    title,
                    app_id,
                    size: [geometry.size.w, geometry.size.h],
                    focused: focused == Some(toplevel.id),
                    parent,
                    layers,
                }
            })
            .collect();
        let shapes: Vec<_> = windows
            .iter()
            .map(|w| (w.id, w.size, w.layers.len()))
            .collect();
        for (window, shape) in windows.iter().zip(&shapes) {
            if !self.logged.contains(shape) {
                log::info!(
                    "window {} {:?} is {}x{} with {} layers",
                    window.id.0,
                    window.title,
                    window.size[0],
                    window.size[1],
                    window.layers.len()
                );
            }
        }
        self.logged = shapes;
        self.send(ToGame::Windows(windows), &[], Vec::new());
    }
}

/// A dmabuf for the game, its plane's file descriptor duplicated.
fn dmabuf_message(surface: SurfaceId, buffer: u64, seq: u64, dmabuf: &Dmabuf) -> Option<Outgoing> {
    let fd = dmabuf.handles().next()?.try_clone_to_owned().ok()?;
    let format = dmabuf.format();
    Some((
        ToGame::Dmabuf {
            surface,
            buffer,
            seq,
            width: dmabuf.width(),
            height: dmabuf.height(),
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            offset: dmabuf.offsets().next()?,
            stride: dmabuf.strides().next()?,
            y_inverted: dmabuf.y_inverted(),
        },
        Vec::new(),
        vec![fd],
    ))
}

/// The damaged regions of `mirror` for the game: no regions, the whole buffer.
fn pixels_message(surface: SurfaceId, mirror: &ShmMirror, regions: Vec<Region>) -> Outgoing {
    let row = mirror.width as usize * 4;
    let payload = if regions.is_empty() {
        mirror.pixels.clone()
    } else {
        let mut payload = Vec::new();
        for region in &regions {
            for y in region.y..region.y + region.height {
                let start = y as usize * row + region.x as usize * 4;
                payload.extend_from_slice(&mirror.pixels[start..start + region.width as usize * 4]);
            }
        }
        payload
    };
    (
        ToGame::Pixels {
            surface,
            width: mirror.width,
            height: mirror.height,
            format: mirror.format,
            regions,
        },
        payload,
        Vec::new(),
    )
}

/// Copy a committed shared-memory buffer's damage into the surface's mirror. The mirror and the
/// regions copied (none: all of it), or `None` for a buffer this can't read.
fn copy_shm(
    buffer: &WlBuffer,
    mirror: Option<ShmMirror>,
    damage: &[Damage],
    scale: i32,
) -> Option<(ShmMirror, Vec<Region>)> {
    with_buffer_contents(buffer, |ptr, len, data| {
        let format = match data.format {
            wl_shm::Format::Argb8888 => PixelFormat::Bgra8,
            wl_shm::Format::Xrgb8888 => PixelFormat::Bgrx8,
            wl_shm::Format::Abgr8888 => PixelFormat::Rgba8,
            wl_shm::Format::Xbgr8888 => PixelFormat::Rgbx8,
            _ => return None,
        };
        let (width, height) = (data.width.max(0) as u32, data.height.max(0) as u32);
        let offset = data.offset.max(0) as usize;
        let stride = data.stride.max(0) as usize;
        let row = width as usize * 4;
        if stride < row || offset + stride * height.saturating_sub(1) as usize + row > len {
            return None;
        }
        // Safety: smithay maps the pool for the closure's duration and checked `len`.
        let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
        let pool = &pool[offset..];
        let (mut mirror, fresh) = match mirror {
            Some(m) if m.width == width && m.height == height && m.format == format => (m, false),
            _ => (
                ShmMirror {
                    width,
                    height,
                    format,
                    pixels: vec![0; row * height as usize],
                },
                true,
            ),
        };
        let mut regions: Vec<Region> = damage
            .iter()
            .filter_map(|damage| {
                let rect: Rectangle<i32, smithay::utils::Buffer> = match damage {
                    Damage::Buffer(rect) => *rect,
                    Damage::Surface(rect) => Rectangle::new(
                        (rect.loc.x * scale, rect.loc.y * scale).into(),
                        (rect.size.w * scale, rect.size.h * scale).into(),
                    ),
                };
                clip(
                    rect.loc.x,
                    rect.loc.y,
                    rect.size.w,
                    rect.size.h,
                    width,
                    height,
                )
            })
            .collect();
        // A new buffer with no damage still has new contents; a resize too: all of it.
        if fresh || regions.is_empty() {
            regions.clear();
            for y in 0..height as usize {
                mirror.pixels[y * row..y * row + row]
                    .copy_from_slice(&pool[y * stride..y * stride + row]);
            }
        } else {
            for region in &regions {
                let (x, w) = (region.x as usize * 4, region.width as usize * 4);
                for y in region.y as usize..(region.y + region.height) as usize {
                    mirror.pixels[y * row + x..y * row + x + w]
                        .copy_from_slice(&pool[y * stride + x..y * stride + x + w]);
                }
            }
        }
        Some((mirror, regions))
    })
    .ok()
    .flatten()
}

/// A damage rectangle clipped to a `width` × `height` buffer; `None` if nothing is left.
fn clip(x: i32, y: i32, w: i32, h: i32, width: u32, height: u32) -> Option<Region> {
    let x0 = x.max(0) as i64;
    let y0 = y.max(0) as i64;
    let x1 = (x as i64 + w as i64).min(width as i64);
    let y1 = (y as i64 + h as i64).min(height as i64);
    (x1 > x0 && y1 > y0).then(|| Region {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// The mapped surfaces of `surface`'s tree (subsurfaces at their places), with `surface` at `at`,
/// and where each lies for hit testing.
fn collect_layers(
    surface: &WlSurface,
    at: Point<i32, Logical>,
    popup: bool,
    layers: &mut Vec<LayerInfo>,
    hits: &mut Vec<(WlSurface, Rectangle<i32, Logical>)>,
) {
    let place = |states: &smithay::wayland::compositor::SurfaceData, at: Point<i32, Logical>| {
        if states.role == Some("subsurface") {
            at + states
                .cached_state
                .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                .current()
                .location
        } else {
            at
        }
    };
    with_surface_tree_downward(
        surface,
        at,
        |_, states, at| TraversalAction::DoChildren(place(states, *at)),
        |surface, states, at| {
            let at = place(states, *at);
            if let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() {
                let data = data.lock().unwrap_or_else(|p| p.into_inner());
                if data.mapped {
                    hits.push((
                        surface.clone(),
                        Rectangle::new(at, (data.size[0], data.size[1]).into()),
                    ));
                    layers.push(LayerInfo {
                        surface: data.id,
                        at: [at.x, at.y],
                        size: data.size,
                        popup,
                    });
                }
            }
        },
        |_, _, _| true,
    );
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

    fn new_surface(&mut self, surface: &WlSurface) {
        // A dmabuf's commit waits until the program's GPU has finished drawing it.
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

    fn destroyed(&mut self, surface: &WlSurface) {
        let id = with_states(surface, |states| {
            states.data_map.get::<Mutex<SurfaceData>>().map(|data| {
                let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((buffer, _)) = data.held.take() {
                    buffer.release();
                }
                data.id
            })
        });
        if let Some(id) = id {
            self.surfaces.remove(&id);
            self.send(ToGame::Forget { surface: id }, &[], Vec::new());
        }
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
        let id = WindowId(self.next_window);
        self.next_window += 1;
        log::info!("window {} opened", id.0);
        self.toplevels.push(Toplevel {
            id,
            window: smithay::desktop::Window::new_wayland_window(surface),
            hits: Vec::new(),
            visible: true,
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
        for id in &gone {
            log::info!("window {} closed", id.0);
        }
        self.toplevels
            .retain(|t| !t.window.toplevel().is_some_and(|s| s == &surface));
        if self.focused.is_some_and(|id| gone.contains(&id)) {
            self.focused = None;
        }
        self.dirty = true;
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.output.enter(surface.wl_surface());
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            log::warn!("a popup couldn't be tracked: {error}");
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
        // Popup grabs (a menu closing on a click outside it) come with the interaction model;
        // until then a menu closes when the program closes it.
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.dirty = true;
    }

    fn app_id_changed(&mut self, _surface: ToplevelSurface) {
        self.dirty = true;
    }

    fn parent_changed(&mut self, _surface: ToplevelSurface) {
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

// Clipboard and drag and drop between the session's own programs (GTK requires the global).
// The clipboard of a desktop around a nested session isn't bridged yet.
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
        // Only layouts the game's GPU imports are offered; a buffer it still can't read keeps
        // the window's last picture (the game logs why).
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

#[cfg(test)]
mod tests {
    use super::clip;
    use horizon_wm_ipc::Region;

    #[test]
    fn damage_is_clipped_to_the_buffer() {
        let region = |x, y, width, height| Region {
            x,
            y,
            width,
            height,
        };
        assert_eq!(clip(-5, 2, 10, 4, 8, 8), Some(region(0, 2, 5, 4)));
        assert_eq!(
            clip(6, 6, i32::MAX, i32::MAX, 8, 8),
            Some(region(6, 6, 2, 2))
        );
        assert_eq!(clip(9, 0, 3, 3, 8, 8), None);
    }
}
