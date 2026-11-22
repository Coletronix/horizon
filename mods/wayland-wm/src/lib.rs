//! A Wayland compositor inside the game.
//!
//! This mod runs a Wayland server on its own thread. Linux programs started with its
//! `WAYLAND_DISPLAY` ([`Wm::socket_name`], [`Wm::spawn`]) open their windows into it, and each
//! window's surfaces become [`LiveTexture`]s the game draws.
//!
//! This mod does not decide how windows look or how the player uses them. **Presenter mods**
//! do: they depend on this one, read [`Wm::windows`] every frame, draw the windows' layers where
//! they like (the HUD, a screen, a block face in the world) and send the player's input back with
//! [`Wm::send`]. Several presenters may run at once ([`register_presenter`]).
//!
//! Coordinates: every window has its own logical space, the window's visible area (its
//! *geometry*, without client-side shadows) from `(0, 0)` to [`Window::size`]. Layers and input use
//! that space; a layer may lie partly outside it (shadows, popups).
#![deny(unsafe_code)]

use mod_api::{
    ClientHost, DrawContext, GuiRenderPostEvent, Hooks, RenderFramePostEvent, RenderLevelStageEvent,
};
pub use mod_api::{LivePixelFormat, LiveRegion, LiveTexture};
use std::sync::{
    Arc, Mutex, OnceLock, RwLock,
    atomic::{AtomicU64, Ordering},
};

mod overlay;
mod panels;
#[cfg(target_os = "linux")]
mod server;

/// A window's identity, unique while this game runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// One surface of a window, in the window's space.
#[derive(Clone, Debug)]
pub struct Layer {
    /// The surface's pixels. A layer at a higher index draws over the ones before it.
    pub texture: LiveTexture,
    /// Where the surface's top left corner is, in logical pixels of the window's space.
    pub at: [i32; 2],
    /// The surface's size in logical pixels (its texture may hold more pixels: a HiDPI buffer).
    pub size: [i32; 2],
    /// Whether this layer is a popup (a menu, a tooltip) rather than part of the window itself.
    pub popup: bool,
}

/// A window as it was at the last commit.
#[derive(Clone, Debug)]
pub struct Window {
    pub id: WindowId,
    /// `xdg_toplevel.set_title`.
    pub title: String,
    /// `xdg_toplevel.set_app_id`: the program, as its desktop file names it.
    pub app_id: String,
    /// The window's visible size in logical pixels.
    pub size: [i32; 2],
    /// Whether the window has the keyboard ([`Input::Focus`]).
    pub focused: bool,
    /// The window's surfaces, bottom first.
    pub layers: Vec<Layer>,
}

impl Window {
    /// Whether the window has drawn anything yet.
    pub fn mapped(&self) -> bool {
        !self.layers.is_empty() && self.size[0] > 0 && self.size[1] > 0
    }
}

/// Mouse buttons, by their Linux input codes (`BTN_LEFT`...).
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
    pub const SIDE: u32 = 0x113;
    pub const EXTRA: u32 = 0x114;
}

/// What a presenter tells the compositor: the player's input and the window management it decides.
#[derive(Clone, Debug)]
pub enum Input {
    /// The pointer is over `window` at `at` (logical pixels of its space).
    PointerMotion { window: WindowId, at: [f64; 2] },
    /// The pointer left every window.
    PointerLeave,
    /// A button (a [`button`] code) went down or up where the pointer is.
    PointerButton { button: u32, pressed: bool },
    /// Scrolling where the pointer is, in logical pixels (`discrete` in wheel steps, if a wheel).
    PointerAxis {
        horizontal: f64,
        vertical: f64,
        discrete: Option<[i32; 2]>,
    },
    /// A key went down or up, by its Linux input code (`KEY_A` is 30; on Linux the game's raw
    /// key code is this code).
    Key { code: u32, pressed: bool },
    /// Give `window` the keyboard, or no window.
    Focus(Option<WindowId>),
    /// Ask `window` to take this size in logical pixels (0 lets it choose).
    Resize { window: WindowId, size: [i32; 2] },
    /// Ask `window` to close.
    Close(WindowId),
}

/// What a presenter draws each frame. Every method has an empty default.
pub trait Presenter: Send + Sync + 'static {
    /// Draw on the GUI, over everything else, once a frame. `host` is the game: its camera, its
    /// screen, the player.
    fn draw_gui(&self, host: &dyn ClientHost, wm: &Wm, windows: &[Window], draw: &mut dyn DrawContext) {
        let _ = (host, wm, windows, draw);
    }
    /// Draw in the world, at each of a world view's stages (`event.stage`): windows on blocks,
    /// floating panels. [`mod_api::WorldDrawContext::live_quads`] draws a layer's texture.
    fn draw_world(
        &self,
        host: &dyn ClientHost,
        wm: &Wm,
        windows: &[Window],
        event: &mut RenderLevelStageEvent<'_>,
    ) {
        let _ = (host, wm, windows, event);
    }
}

static PRESENTERS: RwLock<Vec<Arc<dyn Presenter>>> = RwLock::new(Vec::new());

/// Add a presenter: it draws from the next frame on, after the ones registered before it.
pub fn register_presenter(presenter: Arc<dyn Presenter>) {
    PRESENTERS
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .push(presenter);
}

/// The compositor, once started ([`wm`]).
pub struct Wm {
    socket: String,
    windows: Mutex<Arc<Vec<Window>>>,
    generation: AtomicU64,
    #[cfg(target_os = "linux")]
    commands: server::Commands,
}

static WM: OnceLock<Option<Wm>> = OnceLock::new();

/// The compositor: started the first time anything asks, `None` where it can't run (not Linux,
/// no `XDG_RUNTIME_DIR`, the socket can't be made). Why it couldn't is logged once.
pub fn wm() -> Option<&'static Wm> {
    WM.get_or_init(start).as_ref()
}

fn start() -> Option<Wm> {
    #[cfg(target_os = "linux")]
    match server::start() {
        Ok((socket, commands)) => {
            log::info!("wayland_wm: compositor listening on WAYLAND_DISPLAY={socket}");
            Some(Wm {
                socket,
                windows: Mutex::new(Arc::new(Vec::new())),
                generation: AtomicU64::new(0),
                commands,
            })
        }
        Err(error) => {
            log::warn!("wayland_wm: the compositor didn't start: {error}");
            None
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        log::warn!("wayland_wm: Wayland windows need Linux");
        None
    }
}

impl Wm {
    /// The socket's name: what `WAYLAND_DISPLAY` is for a program to open its windows here.
    pub fn socket_name(&self) -> &str {
        &self.socket
    }

    /// The windows now, in the order they were opened. Cheap: a shared snapshot.
    pub fn windows(&self) -> Arc<Vec<Window>> {
        self.windows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Bumped whenever [`windows`](Self::windows) changes (a window opened, closed, retitled,
    /// resized, or a surface added). A new frame in an existing layer doesn't bump it: the layer's
    /// texture has it.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn publish(&self, windows: Vec<Window>) {
        *self.windows.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(windows);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Send the compositor the player's input or a window management decision.
    pub fn send(&self, input: Input) {
        #[cfg(target_os = "linux")]
        self.commands.send(server::Command::Input(input));
        #[cfg(not(target_os = "linux"))]
        let _ = input;
    }

    /// Start `command` (a shell command line) as a program whose windows open here.
    pub fn spawn(&self, command: &str) -> std::io::Result<std::process::Child> {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("WAYLAND_DISPLAY", &self.socket)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("SDL_VIDEODRIVER", "wayland")
            .env("MOZ_ENABLE_WAYLAND", "1")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
            // No X server of ours yet: an X program must not open on the player's desktop.
            .env_remove("DISPLAY")
            .spawn()
    }

    /// Tell windows a frame was shown, so they draw their next one.
    fn frame_done(&self) {
        #[cfg(target_os = "linux")]
        self.commands.send(server::Command::FrameDone);
    }
}

/// The mod's hooks: drive the compositor from the game's frames.
#[derive(Default)]
pub struct WaylandWm {
    started: OnceLock<()>,
}

impl WaylandWm {
    fn start_once(&self) {
        self.started.get_or_init(|| {
            let Some(wm) = wm() else { return };
            if overlay::enabled() {
                register_presenter(Arc::new(overlay::Overlay));
            }
            if panels::enabled() {
                register_presenter(Arc::new(panels::Panels::default()));
            }
            if let Ok(command) = std::env::var("HORIZON_WM_SPAWN")
                && !command.trim().is_empty()
            {
                match wm.spawn(&command) {
                    Ok(_) => log::info!("wayland_wm: started `{command}`"),
                    Err(error) => log::warn!("wayland_wm: `{command}` didn't start: {error}"),
                }
            }
        });
    }
}

#[mod_api::horizon_mod]
impl Hooks for WaylandWm {
    fn render_frame_post(&self, _host: &mut dyn ClientHost, _event: &mut RenderFramePostEvent) {
        self.start_once();
        if let Some(wm) = wm() {
            wm.frame_done();
        }
    }

    fn render_level_stage(&self, host: &dyn ClientHost, event: &mut RenderLevelStageEvent<'_>) {
        let Some(wm) = wm() else { return };
        let presenters = PRESENTERS.read().unwrap_or_else(|p| p.into_inner()).clone();
        if presenters.is_empty() {
            return;
        }
        let windows = wm.windows();
        for presenter in &presenters {
            presenter.draw_world(host, wm, &windows, event);
        }
    }

    fn gui_render_post(&self, host: &dyn ClientHost, event: &mut GuiRenderPostEvent<'_>) {
        let Some(wm) = wm() else { return };
        let presenters = PRESENTERS.read().unwrap_or_else(|p| p.into_inner()).clone();
        if presenters.is_empty() {
            return;
        }
        let windows = wm.windows();
        for presenter in &presenters {
            presenter.draw_gui(host, wm, &windows, &mut *event.batch);
        }
    }
}
