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

#[path = "display.rs"]
mod display;

use crate::Options;
use crate::game::GameConn;
use crate::programs::{GameExit, Programs, Supervised};
use horizon_wm_ipc::{
    DmabufCaps, Input, LayerInfo, PixelFormat, Region, SurfaceId, ToGame, ToSession, VERSION,
    WindowId, WindowInfo,
};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::{
    X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    xwm::{Reorder, ResizeEdge, XwmId},
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
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform},
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
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                request_data_device_client_selection, set_data_device_selection,
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
    /// The game's own window, when the session has a screen: shown full screen, never sent to
    /// the game as a window.
    game: bool,
}

impl Toplevel {
    /// Its root Wayland surface; an X11 window has one once XWayland associates it.
    fn surface(&self) -> Option<WlSurface> {
        self.window.wl_surface().map(|s| s.into_owned())
    }

    /// Ask it to close, as its close button does.
    fn close(&self) {
        if let Some(xdg) = self.window.toplevel() {
            xdg.send_close();
        } else if let Some(x11) = self.window.x11_surface() {
            let _ = x11.close();
        }
    }

    /// Send what changed in its pending state (an X11 window has none to send).
    fn configure(&self) {
        if let Some(xdg) = self.window.toplevel() {
            xdg.send_pending_configure();
        }
    }
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
    /// Bumped whenever its content changes: the screen's textures follow it.
    version: u64,
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
    seat: Seat<Self>,
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
    /// A menu that asked for a grab, and its window's surface: it has the keyboard, and a click
    /// anywhere else closes it (and every menu of that window).
    popup_grab: Option<(WlSurface, WlSurface)>,
    /// The X11 window manager, once XWayland is up; its display number for programs.
    xwm: Option<X11Wm>,
    x_display: Option<u32>,
    xwayland_shell: XWaylandShellState,
    xwayland_started: bool,
    /// The dmabuf formats offered were narrowed to the game's.
    dmabuf_narrowed: bool,
    /// Globals no longer offered, kept for the programs bound to them.
    old_dmabuf_globals: Vec<DmabufGlobal>,
    /// X11 menus and tooltips (override-redirect windows) on screen.
    x11_menus: Vec<X11Surface>,
    /// Screen lockers (swaylock, hyprlock...): while locked the screen shows only the locker.
    lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    locked: bool,
    lock_surface: Option<smithay::wayland::session_lock::LockSurface>,
    /// The session's own screen, with `--display`.
    screen: Option<display::Screen>,
    /// The game's process, from its control connection: its window is the one the screen shows.
    game_pid: Option<u32>,
    _relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState,
    _pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState,
    /// The pointer's image as the program under it set it, and what the game was last told.
    cursor: CursorImageStatus,
    cursor_sent: Option<(Option<SurfaceId>, [i32; 2])>,
    /// Text programs copied, from the threads that read it.
    clipboard_in: calloop::channel::Sender<String>,
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
    let (clipboard_in, copied) = calloop::channel::channel::<String>();
    let xwayland_shell = XWaylandShellState::new::<State>(&dh);

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

    handle
        .insert_source(copied, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(text) = event {
                state.send(ToGame::Clipboard { text }, &[], Vec::new());
            }
        })
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
    // SIGUSR1 toggles the fallback desktop, as the escape chord does: from another console or
    // over ssh, `pkill -USR1 horizon-session` reaches the programs whatever the screen shows.
    let toggle = calloop::signals::Signals::new(&[calloop::signals::Signal::SIGUSR1])
        .map_err(|e| format!("signals: {e}"))?;
    handle
        .insert_source(toggle, |_, _, state: &mut State| state.toggle_fallback())
        .map_err(|e| e.to_string())?;

    log::info!(
        "listening: WAYLAND_DISPLAY={socket_name}, game control {}",
        control.display()
    );
    let dh2 = dh.clone();
    let mut state = State {
        display: dh,
        compositor,
        xdg_shell,
        shm,
        seat_state,
        _output_manager: output_manager,
        output,
        seat: seat.clone(),
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
        popup_grab: None,
        xwm: None,
        x_display: None,
        xwayland_shell,
        xwayland_started: false,
        dmabuf_narrowed: false,
        old_dmabuf_globals: Vec::new(),
        x11_menus: Vec::new(),
        lock_state: smithay::wayland::session_lock::SessionLockManagerState::new::<State, _>(
            &dh2,
            |_| true,
        ),
        locked: false,
        lock_surface: None,
        screen: None,
        game_pid: None,
        _relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState::new::<
            State,
        >(&dh2),
        _pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState::new::<
            State,
        >(&dh2),
        cursor: CursorImageStatus::default_named(),
        cursor_sent: None,
        clipboard_in,
        held_spawns: options.spawn.clone(),
    };
    match options.display {
        Some(crate::Screen::Window) => display::start_window(&mut state, &mut event_loop)?,
        Some(crate::Screen::Drm) => display::start_drm(&mut state, &mut event_loop)?,
        None => {}
    }
    // With a screen, the game is a program of the session too.
    let game_env: Vec<(String, String)> = match options.display {
        Some(_) => vec![("WAYLAND_DISPLAY".into(), state.socket_name.clone())],
        None => Vec::new(),
    };
    let mut supervised = (!options.game.is_empty()).then(|| {
        Supervised::new(
            options.game.clone(),
            control.display().to_string(),
            game_env,
        )
    });

    let result = loop {
        let busy = state.game.as_ref().is_some_and(GameConn::pending);
        let timeout = Duration::from_millis(if busy {
            2
        } else if state.screen.is_some() {
            4
        } else {
            50
        });
        if let Err(error) = event_loop.dispatch(Some(timeout), &mut state) {
            break Err(error.to_string());
        }
        state.popups.cleanup();
        if state.dirty {
            state.dirty = false;
            state.publish();
        }
        state.flush_game();
        state.render_screen();
        let _ = state.display.flush_clients();
        state.programs.reap();
        state.idle_frames();
        if state.game_ready() || state.start.elapsed() >= SPAWN_WAIT {
            state.start_xwayland();
        }
        if !state.held_spawns.is_empty()
            && (state.game_ready() || state.start.elapsed() >= SPAWN_WAIT)
            && (state.x_display.is_some() || state.start.elapsed() >= SPAWN_WAIT)
        {
            for command in std::mem::take(&mut state.held_spawns) {
                state
                    .programs
                    .spawn(&command, &state.socket_name, state.x_display);
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
        self.game_pid = rustix::net::sockopt::socket_peercred(conn.stream())
            .ok()
            .map(|cred| cred.pid.as_raw_nonzero().get() as u32);
        self.game = Some(conn);
        self.configure_game();
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
                self.cursor_sent = None;
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
            ToSession::Scale { scale } => {
                let scale = scale.clamp(1, 3) as i32;
                log::info!("programs draw at {scale}x");
                self.output
                    .change_current_state(None, None, Some(Scale::Integer(scale)), None);
                // Programs that read the scale off each surface (wl_surface 6) hear it there.
                for surface in self.surfaces.values().filter_map(|s| s.upgrade().ok()) {
                    with_states(&surface, |states| {
                        smithay::wayland::compositor::send_surface_state(
                            &surface,
                            states,
                            scale,
                            Transform::Normal,
                        );
                    });
                }
            }
            ToSession::Clipboard { text } => {
                let mimes = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                let seat = self.seat.clone();
                set_data_device_selection(&self.display, &seat, mimes, std::sync::Arc::new(text));
            }
            ToSession::Spawn { command } => {
                let socket = self.socket_name.clone();
                self.programs.spawn(&command, &socket, self.x_display);
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
        if let Some(global) = self.dmabuf_global.take() {
            if !self.dmabuf_narrowed && self.screen.is_some() {
                // The screen offered what it imports, for the game's own window; programs that
                // start now get what the game imports too, so they can stand in its world.
                // Hidden from programs that connect from now on; the ones using it (the
                // game's own window) keep it.
                self.dmabuf_narrowed = true;
                self.dmabuf.disable_global::<State>(&self.display, &global);
                self.old_dmabuf_globals.push(global);
            } else {
                self.dmabuf_global = Some(global);
                return;
            }
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
        for toplevel in self.toplevels.iter().filter(|t| t.visible && !t.game) {
            let Some(surface) = toplevel.surface() else {
                continue;
            };
            send_frames(&surface, time);
            for (popup, _) in PopupManager::popups_for_surface(&surface) {
                send_frames(popup.wl_surface(), time);
            }
        }
        for menu in &self.x11_menus {
            if let Some(surface) = menu.wl_surface() {
                send_frames(&surface, time);
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

    /// Tell the game the pointer's image when it changed: a cursor surface with pixels, else
    /// none (the game's own crosshair stands for the pointer).
    fn send_cursor(&mut self) {
        let cursor = match &self.cursor {
            CursorImageStatus::Surface(surface) => with_states(surface, |states| {
                let id = states
                    .data_map
                    .get::<Mutex<SurfaceData>>()
                    .map(|d| d.lock().unwrap_or_else(|p| p.into_inner()))
                    .filter(|d| d.mapped)
                    .map(|d| d.id);
                let hotspot = states
                    .data_map
                    .get::<smithay::input::pointer::CursorImageSurfaceData>()
                    .map(|a| a.lock().unwrap_or_else(|p| p.into_inner()).hotspot)
                    .unwrap_or_default();
                (id, [hotspot.x, hotspot.y])
            }),
            _ => (None, [0, 0]),
        };
        if self.cursor_sent != Some(cursor) && self.game_ready() {
            self.cursor_sent = Some(cursor);
            self.send(
                ToGame::Cursor {
                    surface: cursor.0,
                    hotspot: cursor.1,
                },
                &[],
                Vec::new(),
            );
        }
    }

    /// Start XWayland for X11 programs, once: after a game said which GPU buffers it imports
    /// (XWayland then accelerates X11 programs with them), or when no game came in time.
    /// Without it the session still runs Wayland programs.
    fn start_xwayland(&mut self) {
        if self.xwayland_started {
            return;
        }
        self.xwayland_started = true;
        let dh = self.display.clone();
        let handle = self.handle.clone();
        match XWayland::spawn(
            &dh,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| {},
        ) {
            Ok((xwayland, client)) => {
                let inserted = handle.insert_source(
                    xwayland,
                    move |event, _, state: &mut State| match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => {
                            match X11Wm::start_wm(state.handle.clone(), x11_socket, client.clone())
                            {
                                Ok(wm) => {
                                    log::info!("X11 programs: DISPLAY=:{display_number}");
                                    state.xwm = Some(wm);
                                    state.x_display = Some(display_number);
                                }
                                Err(error) => {
                                    log::warn!("the X11 window manager didn't start: {error}")
                                }
                            }
                        }
                        XWaylandEvent::Error => log::warn!("XWayland stopped while starting"),
                    },
                );
                if let Err(error) = inserted {
                    log::warn!("XWayland can't be watched: {error}");
                }
            }
            Err(error) => log::info!("no X11 programs: XWayland didn't start ({error})"),
        }
    }

    /// Ask every window to close and give programs a moment to.
    fn close_all(&mut self, event_loop: &mut EventLoop<State>) {
        for toplevel in &self.toplevels {
            toplevel.close();
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
                log::trace!(
                    "pointer on window {} at {:.1} {:.1}",
                    window.0,
                    at[0],
                    at[1]
                );
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
                // A click outside a grabbing menu closes the menus, and does nothing else.
                if pressed && self.popup_grab_outside() {
                    self.end_popup_grab(serial);
                    return;
                }
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
            Input::Focus(window) => {
                if self.popup_grab.is_some() {
                    self.end_popup_grab(serial);
                }
                self.focus(window, serial);
            }
            Input::Resize { window, size } => {
                log::debug!("asking window {} to be {}x{}", window.0, size[0], size[1]);
                if let Some(toplevel) = self.toplevel(window) {
                    if let Some(surface) = toplevel.window.toplevel() {
                        surface.with_pending_state(|state| {
                            state.size =
                                (size[0] > 0 && size[1] > 0).then(|| (size[0], size[1]).into());
                        });
                        surface.send_pending_configure();
                    } else if let Some(x11) = toplevel.window.x11_surface()
                        && size[0] > 0
                        && size[1] > 0
                    {
                        let at = x11.geometry().loc;
                        let _ = x11.configure(Rectangle::new(at, (size[0], size[1]).into()));
                    }
                }
            }
            Input::Close(window) => {
                if let Some(toplevel) = self.toplevel(window) {
                    toplevel.close();
                }
            }
            Input::Visible { window, visible } => {
                if let Some(toplevel) = self.toplevels.iter_mut().find(|t| t.id == window)
                    && toplevel.visible != visible
                {
                    toplevel.visible = visible;
                    if let Some(surface) = toplevel.window.toplevel() {
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
    }

    /// Whether a menu grabs the pointer and the pointer is on none of its window's menus.
    fn popup_grab_outside(&mut self) -> bool {
        let Some((root, popup)) = &self.popup_grab else {
            return false;
        };
        if !popup.alive() {
            self.popup_grab = None;
            return false;
        }
        let on = self.pointer.current_focus();
        !PopupManager::popups_for_surface(root)
            .any(|(menu, _)| on.as_ref() == Some(menu.wl_surface()))
    }

    /// Close every menu of the grabbing window, and give the keyboard back to the window.
    fn end_popup_grab(&mut self, serial: Serial) {
        let Some((root, _)) = self.popup_grab.take() else {
            return;
        };
        log::debug!("closing the grabbing window's menus");
        for (menu, _) in PopupManager::popups_for_surface(&root) {
            let _ = PopupManager::dismiss_popup(&root, &menu);
        }
        self.focus(self.focused, serial);
        self.dirty = true;
    }

    fn focus(&mut self, window: Option<WindowId>, serial: Serial) {
        let previous = self.focused;
        self.focused = window.filter(|id| self.toplevel(*id).is_some());
        for toplevel in &self.toplevels {
            let active = Some(toplevel.id) == self.focused;
            if active != (Some(toplevel.id) == previous) || active {
                toplevel.window.set_activated(active);
                toplevel.configure();
            }
        }
        // An X11 window with the keyboard goes on top of the X11 stack too.
        if let Some(x11) = self
            .focused
            .and_then(|id| self.toplevel(id))
            .and_then(|t| t.window.x11_surface().cloned())
            && let Some(xwm) = &mut self.xwm
        {
            let _ = xwm.raise_window(&x11);
        }
        let surface = self
            .focused
            .and_then(|id| self.toplevel(id))
            .and_then(Toplevel::surface);
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
        // The game's own window isn't sent to it; its buffers stay held for the screen until
        // replaced.
        let own = self.game_pid.is_some_and(|pid| {
            root.client()
                .and_then(|c| c.get_credentials(&self.display).ok())
                .is_some_and(|c| c.pid as u32 == pid)
        });
        let ready = self.game_ready() && !own;
        let screen = self.screen.is_some();
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
                        version: 0,
                    })
                });
                let mut data = data.lock().unwrap_or_else(|p| p.into_inner());
                data.version += 1;
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
                        // The game imports one-plane buffers; the session's own screen takes any.
                        let one_plane = dmabuf.num_planes() == 1;
                        if !one_plane && !own && !screen {
                            log::warn!("a multi-plane dmabuf can't be shown in the game");
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
                        let message = (ready && one_plane)
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
                            None if own || screen => {
                                // Held for the screen, given back once the next replaces it.
                                if let Some((old, _)) = data.held.replace((buffer.clone(), 0))
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
        let parents: Vec<(Option<WlSurface>, Option<u32>, WindowId)> = self
            .toplevels
            .iter()
            .map(|t| {
                (
                    t.surface(),
                    t.window.x11_surface().map(|x| x.window_id()),
                    t.id,
                )
            })
            .collect();
        let focused = self.focused;
        let menus = self.x11_menus.clone();
        let focused_x11 = focused
            .and_then(|id| self.toplevel(id))
            .and_then(|t| t.window.x11_surface().cloned());
        let windows: Vec<WindowInfo> = self
            .toplevels
            .iter_mut()
            .filter(|t| !t.game)
            .filter_map(|toplevel| {
                let info = if let Some(xdg) = toplevel.window.toplevel().cloned() {
                    xdg_window(toplevel, &xdg, &parents)
                } else {
                    let x11 = toplevel.window.x11_surface()?.clone();
                    x11_window(toplevel, &x11, &parents, &menus, focused_x11.as_ref())
                };
                Some(WindowInfo {
                    focused: focused == Some(toplevel.id),
                    ..info
                })
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
        self.send_cursor();
    }
}

/// An xdg toplevel's info: its surfaces in its geometry's space, popups included; it keeps
/// what the pointer can be over.
fn xdg_window(
    toplevel: &mut Toplevel,
    surface: &ToplevelSurface,
    parents: &[(Option<WlSurface>, Option<u32>, WindowId)],
) -> WindowInfo {
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
    // The window's own surfaces in the root surface's space first: its visible area is what xdg
    // geometry says, else all of them.
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
    let parent = surface.parent().and_then(|p| {
        parents
            .iter()
            .find(|(s, _, _)| s.as_ref() == Some(&p))
            .map(|(_, _, id)| *id)
    });
    WindowInfo {
        id: toplevel.id,
        title,
        app_id,
        size: [geometry.size.w, geometry.size.h],
        focused: false,
        parent,
        layers,
    }
}

/// An X11 window's info: its surface (and its subsurfaces) at the corner, and its menus and
/// tooltips (override-redirect windows, placed in X11's root space) where they lie relative to
/// it. A menu belongs to the window it is transient for, else to the X11 window with the
/// keyboard.
fn x11_window(
    toplevel: &mut Toplevel,
    x11: &X11Surface,
    parents: &[(Option<WlSurface>, Option<u32>, WindowId)],
    menus: &[X11Surface],
    focused: Option<&X11Surface>,
) -> WindowInfo {
    let geometry = x11.geometry();
    let mut layers = Vec::new();
    let mut hits = Vec::new();
    if let Some(surface) = toplevel.surface() {
        collect_layers(&surface, (0, 0).into(), false, &mut layers, &mut hits);
    }
    for menu in menus {
        let ours = match menu.is_transient_for() {
            Some(owner) => owner == x11.window_id(),
            None => focused == Some(x11),
        };
        if ours && let Some(surface) = menu.wl_surface() {
            let at = menu.geometry().loc - geometry.loc;
            collect_layers(&surface, at, true, &mut layers, &mut hits);
        }
    }
    toplevel.hits = hits;
    let parent = x11.is_transient_for().and_then(|owner| {
        parents
            .iter()
            .find(|(_, x, _)| *x == Some(owner))
            .map(|(_, _, id)| *id)
    });
    WindowInfo {
        id: toplevel.id,
        title: x11.title(),
        app_id: x11.class(),
        size: [geometry.size.w, geometry.size.h],
        focused: false,
        parent,
        layers,
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
        // XWayland is the one client smithay inserts itself, with its own data.
        if let Some(xwayland) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("every client but XWayland is inserted with ClientState")
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
            .find(|t| t.surface().as_ref() == Some(&root))
        {
            toplevel.window.on_commit();
            if let Some(surface) = toplevel.window.toplevel()
                && !surface.is_initial_configure_sent()
            {
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
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|(_, popup)| popup == surface)
        {
            self.popup_grab = None;
            self.focus(self.focused, SERIAL_COUNTER.next_serial());
        }
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
            game: false,
        });
        self.configure_game();
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
        log::debug!("a menu opened");
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

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, serial: Serial) {
        log::debug!("a menu grabs the keyboard and pointer");
        // The menu takes the keyboard (arrows, Enter, Escape move through it) until it closes
        // or a click lands outside it.
        let kind = PopupKind::Xdg(surface.clone());
        let Ok(root) = smithay::desktop::find_popup_root_surface(&kind) else {
            let _ = PopupManager::dismiss_popup(surface.wl_surface(), &kind);
            return;
        };
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
        self.popup_grab = Some((root, surface.wl_surface().clone()));
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor = image;
        self.dirty = true;
    }
}

impl OutputHandler for State {}

// Clipboard and drag and drop between the session's own programs (GTK requires the global).
// The clipboard of a desktop around a nested session isn't bridged yet.
impl SelectionHandler for State {
    /// The text the session offers as the clipboard (the game's desktop clipboard).
    type SelectionUserData = std::sync::Arc<String>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        // A program copied: read its text and hand it to the game for the desktop's clipboard.
        let (SelectionTarget::Clipboard, Some(source)) = (ty, source) else {
            return;
        };
        let mimes = source.mime_types();
        let Some(mime) = TEXT_MIMES
            .iter()
            .find(|m| mimes.iter().any(|have| have == *m))
        else {
            return;
        };
        let Ok((read, write)) = std::io::pipe() else {
            return;
        };
        if request_data_device_client_selection(&seat, mime.to_string(), OwnedFd::from(write))
            .is_err()
        {
            return;
        }
        let sender = self.clipboard_in.clone();
        // The program writes when it gets to it: read off the loop, at most a megabyte.
        let _ = std::thread::Builder::new()
            .name("clipboard read".into())
            .spawn(move || {
                use std::io::Read;
                let mut text = String::new();
                if read.take(1 << 20).read_to_string(&mut text).is_ok() && !text.is_empty() {
                    let _ = sender.send(text);
                }
            });
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let text = user_data.clone();
        let _ = std::thread::Builder::new()
            .name("clipboard write".into())
            .spawn(move || {
                use std::io::Write;
                let _ = std::fs::File::from(fd).write_all(text.as_bytes());
            });
    }
}

/// The text types the clipboard is offered and read in, preferred first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

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

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }
}

/// The X11 window manager: X11 windows are windows like Wayland ones; their menus and
/// tooltips are popups of the window they belong to.
impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("X11 events come from a running window manager")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if window.set_mapped(true).is_err() {
            return;
        }
        let id = WindowId(self.next_window);
        self.next_window += 1;
        log::info!("window {} opened (X11)", id.0);
        self.toplevels.push(Toplevel {
            id,
            window: smithay::desktop::Window::new_x11_window(window),
            hits: Vec::new(),
            visible: true,
            game: false,
        });
        self.dirty = true;
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_menus.push(window);
        self.dirty = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Where it wants to be in X11's space, and the size it asks for: granted.
        let mut rect = window.geometry();
        if let Some(x) = x {
            rect.loc.x = x;
        }
        if let Some(y) = y {
            rect.loc.y = y;
        }
        if let Some(w) = w {
            rect.size.w = w as i32;
        }
        if let Some(h) = h {
            rect.size.h = h as i32;
        }
        let _ = window.configure(rect);
        self.dirty = true;
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        self.dirty = true;
    }

    fn property_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _property: smithay::xwayland::xwm::WmWindowProperty,
    ) {
        self.dirty = true;
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edge: ResizeEdge,
    ) {
        // Windows are resized from their bezel in the world.
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // Windows are carried in the world.
    }
}

impl State {
    /// An X11 window or menu went away.
    fn forget_x11(&mut self, window: &X11Surface) {
        self.x11_menus.retain(|m| m != window);
        let gone: Vec<WindowId> = self
            .toplevels
            .iter()
            .filter(|t| t.window.x11_surface() == Some(window))
            .map(|t| t.id)
            .collect();
        for id in &gone {
            log::info!("window {} closed (X11)", id.0);
        }
        self.toplevels
            .retain(|t| t.window.x11_surface() != Some(window));
        if self.focused.is_some_and(|id| gone.contains(&id)) {
            self.focused = None;
        }
        self.dirty = true;
    }
}

impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // The game locks the pointer to look around: granted at once.
        smithay::wayland::pointer_constraints::with_pointer_constraint(surface, pointer, |c| {
            if let Some(c) = c {
                c.activate();
            }
        });
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

/// Screen lockers. Only a session with a screen can lock: without one, the desktop around it
/// does its own locking, and a lock here is refused.
impl smithay::wayland::session_lock::SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut smithay::wayland::session_lock::SessionLockManagerState {
        &mut self.lock_state
    }

    fn lock(&mut self, confirmation: smithay::wayland::session_lock::SessionLocker) {
        if self.screen.is_none() {
            return;
        }
        log::info!("the screen is locked");
        self.locked = true;
        // Nothing but the locker hears the keyboard from now on.
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, None, serial);
        self.unfocus_game();
        confirmation.lock();
    }

    fn unlock(&mut self) {
        log::info!("the screen is unlocked");
        self.locked = false;
        self.lock_surface = None;
        let serial = SERIAL_COUNTER.next_serial();
        self.focus(self.focused, serial);
        self.configure_game();
    }

    fn new_surface(
        &mut self,
        surface: smithay::wayland::session_lock::LockSurface,
        _output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        if let Some(size) = self.screen_size() {
            surface.with_pending_state(|state| {
                state.size = Some((size.w as u32, size.h as u32).into());
            });
        }
        surface.send_configure();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(
            self,
            Some(surface.wl_surface().clone()),
            SERIAL_COUNTER.next_serial(),
        );
        self.lock_surface = Some(surface);
    }
}

smithay::delegate_session_lock!(State);
smithay::delegate_pointer_constraints!(State);
smithay::delegate_relative_pointer!(State);
smithay::delegate_xwayland_shell!(State);
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
