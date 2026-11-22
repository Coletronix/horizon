//! The session's own screen: `--display window` (a window on another desktop, for trying it) or
//! `--display drm` (the computer's screens, from a text console: the session is the desktop).
//!
//! The screen shows the game, which is then a Wayland client of this session like any program,
//! full screen. When the game isn't there (starting, crashed, gone) or the escape chord
//! (Ctrl+Alt+Backspace) asks for it, the screen shows the **fallback desktop** instead: every
//! window side by side on a plain background, used with the mouse and keyboard directly. The
//! chord again goes back to the game. Nothing about the game can keep the person from their
//! programs.
//!
//! Input: the game has a seat of its own (`horizon-game`), so its keyboard focus never fights
//! the programs' (`horizon`), which the game drives through the control socket.
use super::*;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, GbmBufferedSurface};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, InputBackend, InputEvent, KeyState,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::Bind;
use smithay::backend::renderer::{
    Color32F, Frame, ImportDma, ImportMem, Renderer,
    gles::{GlesRenderer, GlesTexture},
};
use smithay::backend::session::{Event as SessionEvent, Session, libseat::LibSeatSession};
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::input::pointer::RelativeMotionEvent;
use smithay::reexports::drm::control::{Device as ControlDevice, ModeTypeFlags, connector};
use smithay::utils::{Physical, Size};
use std::collections::BTreeSet;

/// Background of the fallback desktop.
const BACKGROUND: Color32F = Color32F::new(0.08, 0.09, 0.11, 1.0);
const TILE_BORDER: Color32F = Color32F::new(0.2, 0.22, 0.27, 1.0);
const TILE_FOCUS: Color32F = Color32F::new(0.18, 0.35, 0.67, 1.0);
const CURSOR: Color32F = Color32F::new(0.95, 0.95, 0.95, 1.0);
/// Space around each tile, in pixels.
const MARGIN: i32 = 24;
/// Linux key codes the chord and console switching use.
const KEY_BACKSPACE: u32 = 14;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;

pub enum Backend {
    Window(Box<WinitGraphicsBackend<GlesRenderer>>),
    Drm(Box<DrmScreen>),
}

/// The computer's screen, driven directly: the first connected display, at its preferred mode.
pub struct DrmScreen {
    session: LibSeatSession,
    drm: DrmDevice,
    renderer: GlesRenderer,
    surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    libinput: smithay::reexports::input::Libinput,
    /// A frame is queued, waiting for its vblank.
    pending: bool,
    /// The session has the console: another console has it while switched away.
    active: bool,
}

pub struct Screen {
    backend: Backend,
    size: Size<i32, Physical>,
    /// The person asked for the fallback desktop (the chord).
    fallback: bool,
    /// Keys held, for the chord.
    held: BTreeSet<u32>,
    /// Each surface's texture, and the content version it holds.
    textures: HashMap<SurfaceId, (u64, GlesTexture)>,
    /// The pointer on the screen.
    pointer: Point<f64, Logical>,
    /// Where each window's tile is on the fallback desktop, and its scale.
    tiles: Vec<(WindowId, Rectangle<i32, Logical>, f64)>,
    game_keyboard: KeyboardHandle<State>,
    game_pointer: PointerHandle<State>,
    last_render: Instant,
}

/// Open a window on the desktop the session runs on, and show the game (or the fallback
/// desktop) in it.
pub fn start_window(state: &mut State, event_loop: &mut EventLoop<State>) -> Result<(), String> {
    let (backend, events) =
        winit::init::<GlesRenderer>().map_err(|e| format!("the session's window: {e}"))?;
    let size = backend.window_size();
    backend.window().set_title("Horizon session");
    // Programs (the game first) get GPU buffers in the layouts this renderer imports, from the
    // start: the game's Vulkan window needs them before it has said anything.
    let mut backend = backend;
    if let Some(caps) = renderer_caps(backend.renderer()) {
        state.offer_dmabufs(&caps);
    }
    event_loop
        .handle()
        .insert_source(events, |event, _, state: &mut State| match event {
            WinitEvent::Resized { size, .. } => state.screen_resized(size),
            WinitEvent::Input(event) => state.screen_input(event),
            WinitEvent::Redraw => {
                if let Some(screen) = &mut state.screen {
                    screen.last_render = Instant::now() - Duration::from_secs(1);
                }
            }
            WinitEvent::CloseRequested => state.stopping = true,
            WinitEvent::Focus(_) => {}
        })
        .map_err(|e| e.to_string())?;
    let mut game_seat = state.seat_state.new_wl_seat(&state.display, "horizon-game");
    let game_keyboard = game_seat
        .add_keyboard(XkbConfig::default(), 400, 30)
        .map_err(|e| format!("the game's keyboard: {e}"))?;
    let game_pointer = game_seat.add_pointer();
    state.screen = Some(Screen {
        backend: Backend::Window(Box::new(backend)),
        size,
        fallback: false,
        held: BTreeSet::new(),
        textures: HashMap::new(),
        pointer: (0.0, 0.0).into(),
        tiles: Vec::new(),
        game_keyboard,
        game_pointer,
        last_render: Instant::now(),
    });
    state.screen_resized(size);
    log::info!("showing the game in a window: {}x{}", size.w, size.h);
    Ok(())
}

/// What one frame shows: surfaces' textures where they go, solid rectangles under them, and the
/// fallback desktop's pointer.
struct Picture<'a> {
    size: Size<i32, Physical>,
    layers: &'a [(SurfaceId, Rectangle<f64, Logical>)],
    solids: &'a [(Rectangle<i32, Physical>, Color32F)],
    pointer: Option<Point<f64, Logical>>,
}

/// Draw `picture` into `framebuffer`.
fn paint(
    renderer: &mut GlesRenderer,
    framebuffer: &mut smithay::backend::renderer::gles::GlesTarget<'_>,
    transform: Transform,
    picture: &Picture<'_>,
    textures: &HashMap<SurfaceId, (u64, GlesTexture)>,
) -> Result<(), String> {
    let size = picture.size;
    let full = Rectangle::from_size(size);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(|e| e.to_string())?;
    frame
        .clear(BACKGROUND, &[full])
        .map_err(|e| e.to_string())?;
    for (rect, colour) in picture.solids {
        frame
            .draw_solid(*rect, &[full], *colour)
            .map_err(|e| e.to_string())?;
    }
    for (id, dst) in picture.layers {
        let Some((_, texture)) = textures.get(id) else {
            continue;
        };
        let tex_size = smithay::backend::renderer::Texture::size(texture);
        let src = Rectangle::from_size((f64::from(tex_size.w), f64::from(tex_size.h)).into());
        let dst = Rectangle::new(
            (dst.loc.x as i32, dst.loc.y as i32).into(),
            (dst.size.w.round() as i32, dst.size.h.round() as i32).into(),
        );
        Frame::render_texture_from_to(
            &mut frame,
            texture,
            src,
            dst,
            &[full],
            &[],
            Transform::Normal,
            1.0,
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some(p) = picture.pointer {
        // A plain pointer: the programs' own cursors come later.
        frame
            .draw_solid(
                Rectangle::new((p.x as i32, p.y as i32).into(), (6, 6).into()),
                &[full],
                CURSOR,
            )
            .map_err(|e| e.to_string())?;
    }
    let _ = frame.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// For tests and bug reports: with `HORIZON_SESSION_SHOTS=DIR`, a file `DIR/shot.request` asks
/// for the next frame as `DIR/shot.png` (the request is taken away once it is written).
fn shoot(
    renderer: &mut GlesRenderer,
    framebuffer: &smithay::backend::renderer::gles::GlesTarget<'_>,
    size: Size<i32, Physical>,
) {
    use smithay::backend::renderer::ExportMem;
    let Some(dir) = std::env::var_os("HORIZON_SESSION_SHOTS").map(std::path::PathBuf::from) else {
        return;
    };
    let request = dir.join("shot.request");
    if !request.exists() {
        return;
    }
    let _ = std::fs::remove_file(&request);
    let region = Rectangle::from_size((size.w, size.h).into());
    let Ok(mapping) = renderer.copy_framebuffer(framebuffer, region, Fourcc::Abgr8888) else {
        return;
    };
    let Ok(pixels) = renderer.map_texture(&mapping) else {
        return;
    };
    let Ok(file) = std::fs::File::create(dir.join("shot.png")) else {
        return;
    };
    let mut encoder = png::Encoder::new(file, size.w as u32, size.h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let Ok(mut writer) = encoder.write_header() else {
        return;
    };
    // Rows come bottom first.
    let row = size.w as usize * 4;
    let flipped: Vec<u8> = pixels.chunks(row).rev().flatten().copied().collect();
    let _ = writer.write_image_data(&flipped);
}

/// The GPU buffers `renderer` imports, and its device's render node.
fn renderer_caps(renderer: &GlesRenderer) -> Option<DmabufCaps> {
    use smithay::backend::egl::EGLDevice;
    use smithay::backend::renderer::ImportDma;
    let device = EGLDevice::device_for_display(renderer.egl_context().display()).ok()?;
    let node = device.try_get_render_node().ok().flatten()?;
    let formats = renderer
        .dmabuf_formats()
        .iter()
        .map(|f| horizon_wm_ipc::DmabufFormat {
            fourcc: f.code as u32,
            modifier: u64::from(f.modifier),
        })
        .collect();
    Some(DmabufCaps {
        device: node.dev_id(),
        formats,
    })
}

/// Take the computer's screen from a text console: the seat (libseat), the first GPU and its
/// first connected display, and the keyboards and mice (libinput).
pub fn start_drm(state: &mut State, event_loop: &mut EventLoop<State>) -> Result<(), String> {
    use smithay::reexports::rustix::fs::OFlags;
    let (mut session, notifier) = LibSeatSession::new()
        .map_err(|e| format!("no seat to run on (from a text console?): {e}"))?;
    let seat = session.seat();
    let gpu = smithay::backend::udev::primary_gpu(&seat)
        .ok()
        .flatten()
        .or_else(|| {
            smithay::backend::udev::all_gpus(&seat)
                .ok()?
                .into_iter()
                .next()
        })
        .ok_or("no GPU")?;
    let fd = session
        .open(
            &gpu,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| format!("{}: {e}", gpu.display()))?;
    let fd = DrmDeviceFd::new(smithay::utils::DeviceFd::from(fd));
    let (mut drm, drm_events) = DrmDevice::new(fd.clone(), true).map_err(|e| e.to_string())?;
    let gbm = GbmDevice::new(fd.clone()).map_err(|e| e.to_string())?;
    // Safety: the EGL display and context live as long as the renderer that owns them.
    let egl = unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|e| e.to_string())?;
    let context = EGLContext::new(&egl).map_err(|e| e.to_string())?;
    let renderer = unsafe { GlesRenderer::new(context) }.map_err(|e| e.to_string())?;

    // The first connected display, its preferred mode, and a CRTC that can drive it.
    let resources = drm.resource_handles().map_err(|e| e.to_string())?;
    let connector = resources
        .connectors()
        .iter()
        .filter_map(|c| drm.get_connector(*c, false).ok())
        .find(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
        .ok_or("no connected display")?;
    let mode = connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .unwrap_or(connector.modes()[0]);
    let crtc = connector
        .encoders()
        .iter()
        .filter_map(|e| drm.get_encoder(*e).ok())
        .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
        .next()
        .ok_or("no CRTC for the display")?;
    let surface = drm
        .create_surface(crtc, mode, &[connector.handle()])
        .map_err(|e| e.to_string())?;
    let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let formats = renderer.egl_context().dmabuf_render_formats().clone();
    let surface = GbmBufferedSurface::new(
        surface,
        allocator,
        &[Fourcc::Argb8888, Fourcc::Xrgb8888],
        formats,
    )
    .map_err(|e| e.to_string())?;
    let (w, h) = mode.size();
    let size: Size<i32, Physical> = (i32::from(w), i32::from(h)).into();

    // Keyboards and mice of this seat.
    let mut libinput = smithay::reexports::input::Libinput::new_with_udev(
        LibinputSessionInterface::from(session.clone()),
    );
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| "libinput couldn't take the seat's devices".to_string())?;
    let handle = event_loop.handle();
    handle
        .insert_source(
            LibinputInputBackend::new(libinput.clone()),
            |event, _, state: &mut State| state.screen_input(event),
        )
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(drm_events, |event, _, state: &mut State| match event {
            DrmEvent::VBlank(_) => state.drm_vblank(),
            DrmEvent::Error(error) => log::warn!("the display: {error}"),
        })
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(notifier, |event, _, state: &mut State| {
            state.drm_session(event)
        })
        .map_err(|e| e.to_string())?;

    if let Some(caps) = renderer_caps(&renderer) {
        state.offer_dmabufs(&caps);
    }
    let mut game_seat = state.seat_state.new_wl_seat(&state.display, "horizon-game");
    let game_keyboard = game_seat
        .add_keyboard(XkbConfig::default(), 400, 30)
        .map_err(|e| format!("the game's keyboard: {e}"))?;
    let game_pointer = game_seat.add_pointer();
    state.screen = Some(Screen {
        backend: Backend::Drm(Box::new(DrmScreen {
            session,
            drm,
            renderer,
            surface,
            libinput,
            pending: false,
            active: true,
        })),
        size,
        fallback: false,
        held: BTreeSet::new(),
        textures: HashMap::new(),
        pointer: (f64::from(size.w) / 2.0, f64::from(size.h) / 2.0).into(),
        tiles: Vec::new(),
        game_keyboard,
        game_pointer,
        last_render: Instant::now(),
    });
    state.screen_resized(size);
    log::info!(
        "showing the game on {}: {}x{}",
        gpu.display(),
        size.w,
        size.h
    );
    Ok(())
}

impl State {
    /// The game's own window, once it has one.
    pub(super) fn game_toplevel(&self) -> Option<&Toplevel> {
        self.toplevels.iter().find(|t| t.game)
    }

    /// Whether the screen shows the fallback desktop: asked for, or no game to show.
    fn fallback_shown(&self) -> bool {
        self.screen.as_ref().is_some_and(|s| s.fallback)
            || self.game_toplevel().is_none_or(|t| t.surface().is_none())
    }

    /// The screen changed size: the output (what programs size themselves by) follows, and
    /// the game's window fills it.
    pub(super) fn screen_resized(&mut self, size: Size<i32, Physical>) {
        if let Some(screen) = &mut self.screen {
            screen.size = size;
        }
        let mode = OutputMode {
            size: (size.w, size.h).into(),
            refresh: 60_000,
        };
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        self.configure_game();
    }

    /// Mark the game's window (its program is the game whose control connection we hold) and
    /// make it fill the screen.
    pub(super) fn configure_game(&mut self) {
        let Some(screen) = &self.screen else { return };
        let size = screen.size;
        let Some(pid) = self.game_pid else { return };
        let display = self.display.clone();
        for toplevel in &mut self.toplevels {
            let Some(xdg) = toplevel.window.toplevel() else {
                continue;
            };
            let ours = xdg
                .wl_surface()
                .client()
                .and_then(|c| c.get_credentials(&display).ok())
                .is_some_and(|c| c.pid as u32 == pid);
            if !ours {
                continue;
            }
            if !toplevel.game {
                log::info!(
                    "window {} is the game's: it fills the screen",
                    toplevel.id.0
                );
            }
            toplevel.game = true;
            xdg.with_pending_state(|s| {
                s.states.set(xdg_toplevel::State::Fullscreen);
                s.states.set(xdg_toplevel::State::Activated);
                s.size = Some((size.w, size.h).into());
            });
            if xdg.is_initial_configure_sent() {
                xdg.send_pending_configure();
            }
        }
        if let (Some(screen), Some(surface)) = (
            &self.screen,
            self.game_toplevel().and_then(Toplevel::surface),
        ) {
            let keyboard = screen.game_keyboard.clone();
            if keyboard.current_focus().as_ref() != Some(&surface) {
                keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
            }
        }
    }

    // --- Drawing ---

    /// Draw the screen if a frame is due (about 60 a second), then let what it shows draw its
    /// next frame.
    pub(super) fn render_screen(&mut self) {
        let Some(mut screen) = self.screen.take() else {
            return;
        };
        if screen.last_render.elapsed() < Duration::from_millis(16) {
            self.screen = Some(screen);
            return;
        }
        screen.last_render = Instant::now();
        let fallback = self.fallback_shown();
        if let Err(error) = self.draw(&mut screen, fallback) {
            log::warn!("the screen couldn't be drawn: {error}");
        }
        self.screen = Some(screen);
        // What was shown draws its next frame.
        let time = self.time();
        if let Some(surface) = self.game_toplevel().and_then(Toplevel::surface) {
            send_frames(&surface, time);
        }
        if fallback {
            for toplevel in self.toplevels.iter().filter(|t| !t.game) {
                if let Some(surface) = toplevel.surface() {
                    send_frames(&surface, time);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        send_frames(popup.wl_surface(), time);
                    }
                }
            }
        }
    }

    fn draw(&mut self, screen: &mut Screen, fallback: bool) -> Result<(), String> {
        let size = screen.size;
        // What to draw: (surface, where, scale), bottom first, then solid rectangles.
        let mut layers: Vec<(SurfaceId, Rectangle<f64, Logical>)> = Vec::new();
        let mut solids: Vec<(Rectangle<i32, Physical>, Color32F)> = Vec::new();
        screen.tiles.clear();
        if fallback {
            let shown: Vec<&Toplevel> = self.toplevels.iter().filter(|t| !t.game).collect();
            let n = shown.len().max(1) as i32;
            let cols = (f64::from(n).sqrt().ceil() as i32).max(1);
            let rows = (n + cols - 1) / cols;
            let (cw, ch) = (size.w / cols, size.h / rows);
            for (i, toplevel) in shown.iter().enumerate() {
                let Some(surface) = toplevel.surface() else {
                    continue;
                };
                let mut own = Vec::new();
                let mut hits = Vec::new();
                collect_layers(&surface, (0, 0).into(), false, &mut own, &mut hits);
                let bounds = hits
                    .iter()
                    .map(|(_, r)| *r)
                    .reduce(|a, b| a.merge(b))
                    .unwrap_or_default();
                if bounds.size.w <= 0 || bounds.size.h <= 0 {
                    continue;
                }
                let (col, row) = (i as i32 % cols, i as i32 / cols);
                let room = (
                    f64::from(cw - 2 * MARGIN).max(1.0),
                    f64::from(ch - 2 * MARGIN).max(1.0),
                );
                let scale = (room.0 / f64::from(bounds.size.w))
                    .min(room.1 / f64::from(bounds.size.h))
                    .min(1.0);
                let (w, h) = (
                    (f64::from(bounds.size.w) * scale) as i32,
                    (f64::from(bounds.size.h) * scale) as i32,
                );
                let x = col * cw + (cw - w) / 2;
                let y = row * ch + (ch - h) / 2;
                let colour = if self.focused == Some(toplevel.id) {
                    TILE_FOCUS
                } else {
                    TILE_BORDER
                };
                solids.push((
                    Rectangle::new((x - 3, y - 3).into(), (w + 6, h + 6).into()),
                    colour,
                ));
                screen.tiles.push((
                    toplevel.id,
                    Rectangle::new((x, y).into(), (w, h).into()),
                    scale,
                ));
                for layer in own {
                    let at = (
                        f64::from(x) + f64::from(layer.at[0] - bounds.loc.x) * scale,
                        f64::from(y) + f64::from(layer.at[1] - bounds.loc.y) * scale,
                    );
                    let extent = (
                        f64::from(layer.size[0]) * scale,
                        f64::from(layer.size[1]) * scale,
                    );
                    layers.push((layer.surface, Rectangle::new(at.into(), extent.into())));
                }
            }
        } else if let Some(surface) = self.game_toplevel().and_then(Toplevel::surface) {
            let mut own = Vec::new();
            let mut hits = Vec::new();
            collect_layers(&surface, (0, 0).into(), false, &mut own, &mut hits);
            for layer in own {
                let at = (f64::from(layer.at[0]), f64::from(layer.at[1]));
                let extent = (f64::from(layer.size[0]), f64::from(layer.size[1]));
                layers.push((layer.surface, Rectangle::new(at.into(), extent.into())));
            }
        }
        // Textures for what is drawn, then the frame, on whichever screen this is.
        let picture = Picture {
            size,
            layers: &layers,
            solids: &solids,
            pointer: fallback.then_some(screen.pointer),
        };
        match &mut screen.backend {
            Backend::Window(backend) => {
                for (id, _) in &layers {
                    self.update_texture(backend.renderer(), &mut screen.textures, *id);
                }
                screen
                    .textures
                    .retain(|id, _| layers.iter().any(|(l, _)| l == id));
                let (renderer, mut framebuffer) = backend.bind().map_err(|e| e.to_string())?;
                paint(
                    renderer,
                    &mut framebuffer,
                    Transform::Flipped180,
                    &picture,
                    &screen.textures,
                )?;
                shoot(renderer, &framebuffer, size);
                drop(framebuffer);
                backend.submit(None).map_err(|e| e.to_string())?;
            }
            Backend::Drm(drm) => {
                // One frame on its way to the screen at a time: the next after its vblank.
                if drm.pending || !drm.active {
                    return Ok(());
                }
                for (id, _) in &layers {
                    self.update_texture(&mut drm.renderer, &mut screen.textures, *id);
                }
                screen
                    .textures
                    .retain(|id, _| layers.iter().any(|(l, _)| l == id));
                let (mut dmabuf, _age) = drm.surface.next_buffer().map_err(|e| e.to_string())?;
                let mut framebuffer = drm.renderer.bind(&mut dmabuf).map_err(|e| e.to_string())?;
                paint(
                    &mut drm.renderer,
                    &mut framebuffer,
                    Transform::Normal,
                    &picture,
                    &screen.textures,
                )?;
                shoot(&mut drm.renderer, &framebuffer, size);
                drop(framebuffer);
                drm.surface
                    .queue_buffer(None, None, ())
                    .map_err(|e| e.to_string())?;
                drm.pending = true;
            }
        }
        Ok(())
    }

    /// `surface`'s texture, imported again when its content changed.
    fn update_texture(
        &self,
        renderer: &mut GlesRenderer,
        textures: &mut HashMap<SurfaceId, (u64, GlesTexture)>,
        id: SurfaceId,
    ) {
        let Some(surface) = self.surfaces.get(&id).and_then(|s| s.upgrade().ok()) else {
            return;
        };
        with_states(&surface, |states| {
            let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() else {
                return;
            };
            let data = data.lock().unwrap_or_else(|p| p.into_inner());
            if textures.get(&id).is_some_and(|(v, _)| *v == data.version) {
                return;
            }
            let texture = match &data.content {
                Content::Empty => None,
                Content::Shm(mirror) => {
                    // Uploaded as R, G, B, A bytes, which every GLES takes; BGRA uploads aren't
                    // everywhere what they say.
                    let (fourcc, swap) = match mirror.format {
                        PixelFormat::Bgra8 => (Fourcc::Abgr8888, true),
                        PixelFormat::Bgrx8 => (Fourcc::Xbgr8888, true),
                        PixelFormat::Rgba8 => (Fourcc::Abgr8888, false),
                        PixelFormat::Rgbx8 => (Fourcc::Xbgr8888, false),
                    };
                    let rgba: std::borrow::Cow<[u8]> = if swap {
                        let mut bytes = mirror.pixels.clone();
                        for px in bytes.as_chunks_mut::<4>().0 {
                            px.swap(0, 2);
                        }
                        bytes.into()
                    } else {
                        (&mirror.pixels[..]).into()
                    };
                    renderer
                        .import_memory(
                            &rgba,
                            fourcc,
                            (mirror.width as i32, mirror.height as i32).into(),
                            false,
                        )
                        .ok()
                }
                Content::Dmabuf { dmabuf, .. } => renderer.import_dmabuf(dmabuf, None).ok(),
            };
            log::trace!(
                "surface {} texture from {}",
                id.0,
                match &data.content {
                    Content::Empty => "nothing".to_string(),
                    Content::Shm(m) => format!("shared memory {:?}", m.format),
                    Content::Dmabuf { dmabuf, .. } => format!(
                        "a dmabuf {:?} with {} planes",
                        dmabuf.format(),
                        dmabuf.num_planes()
                    ),
                }
            );
            if let Some(texture) = texture {
                textures.insert(id, (data.version, texture));
            }
        });
    }

    // --- Input ---

    /// Input from the screen's devices: to the game, or on the fallback desktop to the
    /// window under the pointer.
    pub(super) fn screen_input<B: InputBackend>(&mut self, event: InputEvent<B>) {
        let fallback = self.fallback_shown();
        match event {
            InputEvent::Keyboard { event } => {
                let code = event.key_code().raw().saturating_sub(8);
                let pressed = event.state() == KeyState::Pressed;
                if self.chord(code, pressed) {
                    return;
                }
                if fallback {
                    self.input(Input::Key { code, pressed });
                } else if let Some(screen) = &self.screen {
                    let keyboard = screen.game_keyboard.clone();
                    keyboard.input::<(), _>(
                        self,
                        event.key_code(),
                        event.state(),
                        SERIAL_COUNTER.next_serial(),
                        self.time(),
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(screen) = &self.screen else { return };
                let size = screen.size;
                let at = event.position_transformed((size.w, size.h).into());
                let delta = at - screen.pointer;
                self.move_pointer(at, delta, fallback);
            }
            InputEvent::PointerMotion { event } => {
                let Some(screen) = &self.screen else { return };
                let size = screen.size;
                let delta = event.delta();
                let mut at = screen.pointer + delta;
                at.x = at.x.clamp(0.0, f64::from(size.w - 1));
                at.y = at.y.clamp(0.0, f64::from(size.h - 1));
                self.move_pointer(at, delta, fallback);
            }
            InputEvent::PointerButton { event } => {
                let button = event.button_code();
                let pressed = event.state() == ButtonState::Pressed;
                if fallback {
                    if pressed
                        && let Some(window) = self.tile_under_pointer().map(|(w, _)| w)
                        && self.focused != Some(window)
                    {
                        self.focus(Some(window), SERIAL_COUNTER.next_serial());
                    }
                    self.input(Input::PointerButton { button, pressed });
                } else if let Some(screen) = &self.screen {
                    let pointer = screen.game_pointer.clone();
                    pointer.button(
                        self,
                        &ButtonEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: self.time(),
                            button,
                            state: event.state(),
                        },
                    );
                    pointer.frame(self);
                }
            }
            InputEvent::PointerAxis { event } => {
                let horizontal = event.amount(Axis::Horizontal).unwrap_or(0.0);
                let vertical = event.amount(Axis::Vertical).unwrap_or(0.0);
                let steps = (
                    event
                        .amount_v120(Axis::Horizontal)
                        .map(|v| (v / 120.0) as i32),
                    event
                        .amount_v120(Axis::Vertical)
                        .map(|v| (v / 120.0) as i32),
                );
                if fallback {
                    self.input(Input::PointerAxis {
                        horizontal,
                        vertical,
                        discrete: (event.source() == AxisSource::Wheel)
                            .then(|| [steps.0.unwrap_or(0), steps.1.unwrap_or(0)]),
                    });
                } else if let Some(screen) = &self.screen {
                    let pointer = screen.game_pointer.clone();
                    let mut frame = AxisFrame::new(self.time()).source(event.source());
                    if horizontal != 0.0 {
                        frame = frame.value(Axis::Horizontal, horizontal);
                    }
                    if vertical != 0.0 {
                        frame = frame.value(Axis::Vertical, vertical);
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            _ => {}
        }
    }

    /// The escape chord: Ctrl+Alt+Backspace shows the fallback desktop, and again the game.
    /// Whatever the game or a program does, the person can reach their programs.
    fn chord(&mut self, code: u32, pressed: bool) -> bool {
        let Some(screen) = &mut self.screen else {
            return false;
        };
        if pressed {
            screen.held.insert(code);
        } else {
            screen.held.remove(&code);
        }
        let ctrl = screen.held.contains(&KEY_LEFTCTRL) || screen.held.contains(&KEY_RIGHTCTRL);
        let alt = screen.held.contains(&KEY_LEFTALT) || screen.held.contains(&KEY_RIGHTALT);
        if pressed && code == KEY_BACKSPACE && ctrl && alt {
            self.toggle_fallback();
            return true;
        }
        if pressed && ctrl && alt && self.switch_console(code) {
            return true;
        }
        false
    }

    /// The frame queued last reached the screen: the next can be drawn.
    pub(super) fn drm_vblank(&mut self) {
        if let Some(Screen {
            backend: Backend::Drm(drm),
            ..
        }) = &mut self.screen
        {
            let _ = drm.surface.frame_submitted();
            drm.pending = false;
        }
    }

    /// Another console took the screen, or gave it back.
    pub(super) fn drm_session(&mut self, event: SessionEvent) {
        let Some(Screen {
            backend: Backend::Drm(drm),
            textures,
            ..
        }) = &mut self.screen
        else {
            return;
        };
        match event {
            SessionEvent::PauseSession => {
                log::info!("another console has the screen");
                drm.libinput.suspend();
                drm.drm.pause();
                drm.active = false;
            }
            SessionEvent::ActivateSession => {
                log::info!("the screen is ours again");
                if drm.libinput.resume().is_err() {
                    log::warn!("libinput didn't take the devices back");
                }
                if let Err(error) = drm.drm.activate(true) {
                    log::warn!("the display didn't come back: {error}");
                }
                drm.surface.reset_buffers();
                drm.pending = false;
                drm.active = true;
                textures.clear();
            }
        }
    }

    /// Ctrl+Alt+F1 to F12 switch text consoles (on the computer's screen).
    fn switch_console(&mut self, code: u32) -> bool {
        // KEY_F1..KEY_F10 are 59..68, KEY_F11 87, KEY_F12 88.
        let vt = match code {
            59..=68 => code - 58,
            87 => 11,
            88 => 12,
            _ => return false,
        };
        if let Some(Screen {
            backend: Backend::Drm(drm),
            ..
        }) = &mut self.screen
        {
            if let Err(error) = drm.session.change_vt(vt as i32) {
                log::warn!("console {vt}: {error}");
            }
            return true;
        }
        false
    }

    /// Show the fallback desktop, or the game again.
    pub(super) fn toggle_fallback(&mut self) {
        if let Some(screen) = &mut self.screen {
            screen.fallback = !screen.fallback;
            log::info!(
                "{}",
                if screen.fallback {
                    "showing the fallback desktop"
                } else {
                    "showing the game"
                }
            );
        }
    }

    /// The window tile under the pointer on the fallback desktop, and where in its window.
    fn tile_under_pointer(&self) -> Option<(WindowId, [f64; 2])> {
        let screen = self.screen.as_ref()?;
        let p = screen.pointer;
        screen.tiles.iter().find_map(|(window, rect, scale)| {
            rect.to_f64().contains(p).then(|| {
                let local = (p - rect.loc.to_f64()).to_f64();
                (*window, [local.x / scale, local.y / scale])
            })
        })
    }

    fn move_pointer(
        &mut self,
        at: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        fallback: bool,
    ) {
        let Some(screen) = &mut self.screen else {
            return;
        };
        screen.pointer = at;
        if fallback {
            match self.tile_under_pointer() {
                Some((window, at)) => self.input(Input::PointerMotion { window, at }),
                None => self.input(Input::PointerLeave),
            }
            return;
        }
        let pointer = screen.game_pointer.clone();
        let focus = self
            .game_toplevel()
            .and_then(Toplevel::surface)
            .map(|s| (s, Point::<f64, Logical>::from((0.0, 0.0))));
        let time = self.time();
        // The game locks the pointer to look around: it reads the motion, not the position.
        pointer.relative_motion(
            self,
            focus.clone(),
            &RelativeMotionEvent {
                delta,
                delta_unaccel: delta,
                utime: u64::from(time) * 1000,
            },
        );
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: at,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }
}
