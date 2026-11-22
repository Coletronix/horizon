//! Desktop windows in the game.
//!
//! The windows belong to `horizon-session`, the Wayland compositor that owns the desktop's
//! programs: they outlive the game. This mod connects the game to it (starting one if none runs),
//! turns each window's surfaces into [`LiveTexture`]s the game draws, and passes input back.
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
mod session;

pub use horizon_wm_ipc::{Input, WindowId, button};

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
    /// The window this one belongs to: a dialog's main window.
    pub parent: Option<WindowId>,
    /// The window's surfaces, bottom first.
    pub layers: Vec<Layer>,
}

impl Window {
    /// Whether the window has drawn anything yet.
    pub fn mapped(&self) -> bool {
        !self.layers.is_empty() && self.size[0] > 0 && self.size[1] > 0
    }
}

/// What a presenter draws each frame. Every method has an empty default.
pub trait Presenter: Send + Sync + 'static {
    /// Draw on the GUI, over everything else, once a frame. `host` is the game: its camera, its
    /// screen, the player.
    fn draw_gui(
        &self,
        host: &dyn ClientHost,
        wm: &Wm,
        windows: &[Window],
        draw: &mut dyn DrawContext,
    ) {
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

/// The clipboard bridge between the session's programs and the desktop the game runs on.
#[derive(Default)]
struct Clipboard {
    /// The text both sides have.
    known: Option<String>,
    /// A program copied this: it goes on the desktop's clipboard on the next frame.
    copied: Option<String>,
    /// A window got the keyboard: offer it the desktop's clipboard on the next frame.
    offer: bool,
}

/// The game's link to the session ([`wm`]).
pub struct Wm {
    socket: Mutex<Option<String>>,
    /// The clipboard as the session and the desktop last agreed on it, and text a program
    /// copied that the desktop doesn't have yet.
    clipboard: Mutex<Clipboard>,
    instance: AtomicU64,
    windows: Mutex<Arc<Vec<Window>>>,
    generation: AtomicU64,
    #[cfg(target_os = "linux")]
    link: session::Link,
}

static WM: OnceLock<Option<&'static Wm>> = OnceLock::new();

/// The link to the session: made the first time anything asks; `None` where it can't be (not
/// Linux, no `XDG_RUNTIME_DIR`). Why is logged once. It may not be connected yet
/// ([`Wm::connected`]); it connects, and reconnects, by itself.
pub fn wm() -> Option<&'static Wm> {
    *WM.get_or_init(start)
}

fn start() -> Option<&'static Wm> {
    #[cfg(target_os = "linux")]
    {
        let wm: &'static Wm = Box::leak(Box::new(Wm {
            socket: Mutex::new(None),
            clipboard: Mutex::new(Clipboard::default()),
            instance: AtomicU64::new(0),
            windows: Mutex::new(Arc::new(Vec::new())),
            generation: AtomicU64::new(0),
            link: session::Link::default(),
        }));
        match session::start(wm) {
            Ok(()) => Some(wm),
            Err(error) => {
                log::warn!("wayland_wm: no session: {error}");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        log::warn!("wayland_wm: Wayland windows need Linux");
        None
    }
}

impl Wm {
    /// The session's `WAYLAND_DISPLAY` while connected.
    pub fn socket_name(&self) -> Option<String> {
        self.socket
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Whether the session is connected now.
    pub fn connected(&self) -> bool {
        self.socket_name().is_some()
    }

    /// The windows now, in the order they were opened. Cheap: a shared snapshot. Empty while the
    /// session isn't connected.
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

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn publish(&self, windows: Vec<Window>) {
        *self.windows.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(windows);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn connected_to(&self, socket: String, instance: u64) {
        self.instance.store(instance, Ordering::Release);
        *self.socket.lock().unwrap_or_else(|p| p.into_inner()) = Some(socket);
    }

    /// A number naming the connected run of the session (0 before the first connection): while it
    /// stays the same, a [`WindowId`] names the same window, across game restarts too.
    pub fn session_instance(&self) -> u64 {
        self.instance.load(Ordering::Acquire)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn disconnected(&self) {
        *self.socket.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.publish(Vec::new());
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn copied(&self, text: String) {
        self.clipboard
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .copied = Some(text);
    }

    /// Call from a hook that handles the player's key or click (the only moments the game lets
    /// a mod reach the clipboard): what a program copied goes on the desktop's clipboard, and
    /// when a window has just been given the keyboard, the desktop's text is offered to the
    /// session's programs.
    pub fn player_input(&self, host: &mut dyn ClientHost) {
        let mut clipboard = self.clipboard.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(text) = clipboard.copied.take() {
            host.set_clipboard_text(&text);
            clipboard.known = Some(text);
        }
        if std::mem::take(&mut clipboard.offer)
            && let Some(text) = host.clipboard_text()
            && clipboard.known.as_ref() != Some(&text)
        {
            clipboard.known = Some(text.clone());
            drop(clipboard);
            #[cfg(target_os = "linux")]
            self.link
                .send(horizon_wm_ipc::ToSession::Clipboard { text });
        }
    }

    /// Send the session the player's input or a window management decision. Dropped while not
    /// connected.
    pub fn send(&self, input: Input) {
        if matches!(input, Input::Focus(Some(_))) {
            self.clipboard
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .offer = true;
        }
        #[cfg(target_os = "linux")]
        self.link.send(horizon_wm_ipc::ToSession::Input(input));
        #[cfg(not(target_os = "linux"))]
        let _ = input;
    }

    /// Start `command` (a shell command line) as a program of the session: its windows open
    /// here, and it outlives the game.
    pub fn spawn(&self, command: &str) {
        #[cfg(target_os = "linux")]
        self.link.send(horizon_wm_ipc::ToSession::Spawn {
            command: command.to_string(),
        });
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }

    /// Tell windows a frame was shown, so they draw their next.
    fn frame_done(&self) {
        #[cfg(target_os = "linux")]
        self.link.frame_done();
    }
}

/// The mod's hooks: drive the compositor from the game's frames.
#[derive(Default)]
pub struct WaylandWm {
    started: OnceLock<()>,
    /// `HORIZON_WM_SPAWN`, started once the session is connected.
    spawned: std::sync::atomic::AtomicBool,
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
            let _ = wm;
        });
    }

    /// `HORIZON_WM_SPAWN`: a program to start once, when the session first answers.
    fn spawn_once(&self, wm: &Wm) {
        if !wm.connected() || self.spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(command) = std::env::var("HORIZON_WM_SPAWN")
            && !command.trim().is_empty()
        {
            log::info!("wayland_wm: starting `{command}`");
            wm.spawn(&command);
        }
    }
}

#[mod_api::horizon_mod]
impl Hooks for WaylandWm {
    fn render_frame_post(&self, _host: &mut dyn ClientHost, _event: &mut RenderFramePostEvent) {
        self.start_once();
        if let Some(wm) = wm() {
            self.spawn_once(wm);
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
