//! Desktop windows as things in the world.
//!
//! A presenter for `wayland_wm`: every window is a slab standing in the world, its screen on the
//! front. The player uses it by looking at it:
//!
//! - **Pointing**: where the crosshair meets a screen is where the pointer is.
//! - **Clicking a screen** gives that window the keyboard, and the click reaches it.
//! - **Typing**: while the crosshair stays on the window with the keyboard, every key goes to it,
//!   Escape included. Look away and the keys are the game's again; look back and they are the
//!   window's again. The release key (Super, rebindable) or holding Escape for a second gives the
//!   keyboard back for good.
//! - **The bezel** is the window as a thing: right-click it to carry the window (the wheel brings it
//!   nearer or pushes it away, right-click again sets it down), sneak and right-click an edge or
//!   corner and drag to resize, hold left-click to pick it up. A picked-up window is put away (its program
//!   keeps running, suspended); the recall key brings the last one back in front of you.
//! - **New windows** open in front of the player, facing them; a dialog opens in front of its
//!   window.
//!
//! Where windows stand is kept while the session lasts, across the game's restarts.
#![deny(unsafe_code)]

mod apps;
mod draw;
mod items;
mod space;
mod store;

use mod_api::{
    ClientHost, Flow, Hooks, KeyAction, KeyRegistration, Registrar, RegistrationContext,
    RenderFramePostEvent, RenderLevelStageEvent,
};
use space::{Edges, Part, Pose, Shape, V3};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wayland_wm::{Input, Presenter, Window, WindowId, Wm, button};

const MOD_ID: &str = "wm_world";
/// The setting that curves snapped rows towards the player (on unless turned off).
const CURVE_SETTING: &str = "curved_rows";
/// How far each window snapped beside another turns towards the row's viewer, in degrees.
const ROW_CURVE: f32 = 20.0;
/// The setting that shows the hints on the HUD (on unless turned off).
const HINTS_SETTING: &str = "hints";
/// How far the crosshair reaches windows to point at them, in blocks.
const POINT_REACH: f64 = 64.0;
/// How far it reaches to grab, resize or mine one: a player's block reach.
const HAND_REACH: f64 = 6.0;
/// How long Escape is held to give the keyboard back to the game.
const ESCAPE_HOLD: f64 = 1.0;
/// Seconds of holding attack to pick a window up.
const MINE_TIME: f64 = 0.8;
/// The smallest size a resize asks for, in window pixels.
const MIN_SIZE: [f64; 2] = [160.0, 100.0];
/// Seconds of play before new windows are placed: the player has landed.
const SETTLE: f64 = 1.0;
/// The Linux code of Escape.
const KEY_ESC: u32 = 1;

/// A window standing in the world.
#[derive(Clone, Debug)]
struct Placed {
    window: WindowId,
    pose: Pose,
    /// Blocks per window pixel.
    pixel: f64,
    /// The window's size when last seen, in window pixels.
    size: [i32; 2],
    /// Put away: not in the world, its program suspended.
    pocketed: bool,
    /// The world (save or server, and dimension) it stands in: in another, it waits put away.
    world: String,
    /// Which edges stay put when its size changes (the ones opposite those last dragged).
    anchor: Edges,
}

impl Placed {
    fn shape<'a>(&self, layers: &'a [[f64; 4]]) -> Shape<'a> {
        Shape {
            size_px: [f64::from(self.size[0]), f64::from(self.size[1])],
            pixel: self.pixel,
            layers,
        }
    }
}

/// What the crosshair is on this frame.
#[derive(Clone, Copy, Debug)]
struct Gaze {
    window: WindowId,
    part: Part,
    distance: f64,
}

/// A window being carried: it hangs this far along the view, its grabbed point under the
/// crosshair.
#[derive(Clone, Copy, Debug)]
struct Carry {
    window: WindowId,
    distance: f64,
    /// The grabbed point, right and up of the centre.
    offset: [f64; 2],
}

/// An edge or corner being dragged.
#[derive(Clone, Copy, Debug)]
struct Drag {
    window: WindowId,
    edges: Edges,
    /// Where on the front plane the drag started, and the size then.
    from: [f64; 2],
    size: [f64; 2],
    /// The size asked for last.
    asked: [i32; 2],
}

#[derive(Default)]
struct State {
    placed: Vec<Placed>,
    /// The last window list placed.
    generation: Option<u64>,
    instance: u64,
    gaze: Option<Gaze>,
    /// The window with the keyboard: it keeps it until another is clicked or Escape is held.
    focus: Option<WindowId>,
    /// The keyboard goes to `focus` now: the crosshair is on its screen (the bezel is the
    /// window as a thing: sneaking to resize it needs Shift).
    captured: bool,
    /// Keys and buttons a window has down: it gets their release whatever happens.
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    escape_since: Option<Instant>,
    /// The window under the pointer, and where.
    pointer: Option<(WindowId, [f64; 2])>,
    carry: Option<Carry>,
    drag: Option<Drag>,
    /// The window attack is held on, since when.
    mining: Option<(WindowId, Instant)>,
    /// Put-away windows, the most recent last.
    pocket: Vec<WindowId>,
    /// Where the next window to open goes (a dispenser started its program), and since when:
    /// it lapses after a while.
    place_next: Option<(Pose, Instant)>,
    /// Windows just picked up, whose item is to be asked of the server.
    pick_up: Vec<WindowId>,
    /// Put-away windows whose item the server gave: the item brings them back, not the recall
    /// key.
    itemised: BTreeSet<WindowId>,
    /// The view this frame: eye and direction.
    eye: Option<(V3, V3, f32, f32)>,
    dirty_since: Option<Instant>,
    /// The release key went down and took the keyboard back: its repeats and release are ours.
    release_held: bool,
    /// The world the player is in: its save folder or server, and its dimension.
    world: String,
    /// The dimension, from the last level load.
    dimension: String,
    /// Since when the player has been playing (no screen over the world).
    playing_since: Option<Instant>,
    /// The player has been playing a moment: new windows can be placed in front of them.
    settled: bool,
}

impl State {
    /// Whether `placed` stands in the world the player is in now.
    fn here(&self, placed: &Placed) -> bool {
        !placed.pocketed && placed.world == self.world
    }

    fn placed(&self, id: WindowId) -> Option<&Placed> {
        self.placed.iter().find(|p| p.window == id)
    }
    fn placed_mut(&mut self, id: WindowId) -> Option<&mut Placed> {
        self.placed.iter_mut().find(|p| p.window == id)
    }
    fn changed(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }
}

/// The mod.
#[derive(Default)]
pub struct WmWorld {
    state: Mutex<State>,
    registered: OnceLock<()>,
}

/// Each popup's rectangle in window pixels: popups take the pointer past the window's edge. The
/// window's own surfaces don't: what lies outside its geometry is a client-side shadow, over the
/// bezel.
fn layer_rects(window: &Window) -> Vec<[f64; 4]> {
    window
        .layers
        .iter()
        .filter(|l| l.popup)
        .map(|l| {
            [
                f64::from(l.at[0]),
                f64::from(l.at[1]),
                f64::from(l.size[0]),
                f64::from(l.size[1]),
            ]
        })
        .collect()
}

impl WmWorld {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Once the session answers: draw the windows, and pick up where they stood.
    fn register_presenter(&'static self) {
        self.registered.get_or_init(|| {
            wayland_wm::register_presenter(Arc::new(Drawer(self)));
        });
    }

    /// Match the placed windows to the session's list: place new ones, forget closed ones, follow
    /// size changes from their anchored edges.
    fn sync(&self, state: &mut State, wm: &Wm, windows: &[Window]) {
        if !wm.connected() {
            return;
        }
        if state.instance != wm.session_instance() {
            let (instance, eye, since, settled) = (
                wm.session_instance(),
                state.eye,
                state.playing_since,
                state.settled,
            );
            *state = State {
                instance,
                eye,
                playing_since: since,
                settled,
                ..store::load(instance).unwrap_or_default()
            };
        }
        let generation = wm.generation();
        if state.generation == Some(generation) {
            return;
        }
        state.generation = Some(generation);
        let before = state.placed.len();
        state
            .placed
            .retain(|p| windows.iter().any(|w| w.id == p.window));
        let gone = before != state.placed.len();
        state
            .pocket
            .retain(|id| windows.iter().any(|w| w.id == *id));
        if state
            .focus
            .is_some_and(|f| windows.iter().all(|w| w.id != f))
        {
            state.focus = None;
            state.captured = false;
        }
        for window in windows.iter().filter(|w| w.mapped()) {
            let size = window.size;
            match state.placed_mut(window.id) {
                Some(placed) if placed.size != size => {
                    let (old, anchor) = (placed.size, placed.anchor);
                    let b = placed.pose.basis();
                    let dw = f64::from(size[0] - old[0]) * placed.pixel / 2.0;
                    let dh = f64::from(size[1] - old[1]) * placed.pixel / 2.0;
                    // Grow away from the anchored edges: centred where neither side is.
                    let sx = match (anchor.left, anchor.right) {
                        (true, false) => 1.0,
                        (false, true) => -1.0,
                        _ => 0.0,
                    };
                    let sy = match (anchor.top, anchor.bottom) {
                        (true, false) => -1.0,
                        (false, true) => 1.0,
                        _ => 0.0,
                    };
                    placed.pose.centre = space::add(
                        placed.pose.centre,
                        space::add(space::mul(b.right, dw * sx), space::mul(b.up, dh * sy)),
                    );
                    placed.size = size;
                    state.changed();
                }
                Some(_) => {}
                None => {
                    if !state.settled {
                        // Not yet known where the player looks: placed on a later frame.
                        state.generation = None;
                        continue;
                    }
                    let pose = self.place_new(state, window);
                    log::info!(
                        "wm_world: window {} {:?} placed at {:?}, facing yaw {} pitch {}",
                        window.id.0,
                        window.title,
                        pose.centre,
                        pose.yaw,
                        pose.pitch
                    );
                    if window.parent.is_none() {
                        state.place_next = None;
                    }
                    state.placed.push(Placed {
                        window: window.id,
                        pose,
                        pixel: 1.0 / space::PIXELS_PER_BLOCK,
                        size,
                        pocketed: false,
                        world: state.world.clone(),
                        anchor: Edges {
                            top: true,
                            ..Edges::default()
                        },
                    });
                    state.changed();
                }
            }
        }
        if gone {
            state.changed();
        }
    }

    /// Where a new window goes: in front of its parent, else in front of the player, facing them,
    /// beside any window already there.
    fn place_new(&self, state: &State, window: &Window) -> Pose {
        let pixel = 1.0 / space::PIXELS_PER_BLOCK;
        if window.parent.is_none()
            && let Some((pose, since)) = state.place_next
            && since.elapsed().as_secs() < 20
        {
            return pose;
        }
        if let Some(parent) = window.parent.and_then(|p| state.placed(p)) {
            let b = parent.pose.basis();
            return Pose {
                centre: space::add(parent.pose.centre, space::mul(b.out, 0.25)),
                ..parent.pose
            };
        }
        let Some((eye, _, yaw, pitch)) = state.eye else {
            return Pose {
                centre: [0.0, 80.0, 0.0],
                yaw: 0.0,
                pitch: 0.0,
            };
        };
        let (w, h) = (
            f64::from(window.size[0]) * pixel,
            f64::from(window.size[1]) * pixel,
        );
        let yaw = space::snap(yaw, 22.5);
        let pitch = space::snap(pitch, 22.5).clamp(-45.0, 45.0);
        let distance = (w.max(h) * 0.9).clamp(1.5, 8.0);
        let mut pose = Pose {
            centre: space::add(eye, space::mul(space::forward(yaw, pitch), distance)),
            yaw,
            pitch,
        };
        // Step aside, right then left, until no other window is in the way.
        let right = pose.basis().right;
        let start = pose.centre;
        for step in 0..12 {
            let overlaps = state.placed.iter().filter(|p| state.here(p)).any(|p| {
                let d = space::sub(p.pose.centre, pose.centre);
                let reach = (w + f64::from(p.size[0]) * p.pixel) / 2.0 + space::BEZEL * 2.0;
                space::dot(d, d).sqrt() < reach.max(0.5)
            });
            if !overlaps {
                break;
            }
            let k = f64::from(step / 2 + 1) * if step % 2 == 0 { 1.0 } else { -1.0 };
            pose.centre = space::add(start, space::mul(right, k * (w + 0.4)));
        }
        pose
    }

    /// This frame's view: what the crosshair is on, and what follows from it.
    fn frame(&self, host: &mut dyn ClientHost) {
        let Some(wm) = wayland_wm::wm() else { return };
        let windows = wm.windows();
        let mut state = self.lock();
        let Some(camera) = host.camera() else {
            return;
        };
        // The camera's position is the eye's.
        let eye = camera.position;
        let dir = space::forward(camera.yaw, camera.pitch);
        self.enter_world(&mut state, wm, host);
        let playing = host.playing();
        if !playing {
            state.playing_since = None;
        } else if state.playing_since.is_none() {
            state.playing_since = Some(Instant::now());
        }
        // Only a player settled in the world places windows: as a world opens, the camera moves
        // to where the player lands.
        let settled = state
            .playing_since
            .is_some_and(|t| t.elapsed().as_secs_f64() >= SETTLE);
        state.eye = Some((eye, dir, camera.yaw, camera.pitch));
        state.settled = settled;
        self.sync(&mut state, wm, &windows);

        let block = host.view_hit().map(|(d, _)| f64::from(d));
        state.gaze = if playing {
            gaze(&state, &windows, eye, dir, block)
        } else {
            None
        };
        self.carry(&mut state, eye, dir);
        self.separate(&mut state);
        self.drag(&mut state, wm, eye, dir);
        self.mine(&mut state, wm);
        self.point(&mut state, wm, playing);
        self.capture(&mut state, wm);
        self.keys(&mut state, wm, host);
        self.item_traffic(&mut state, wm, &windows, host);
        if state
            .dirty_since
            .is_some_and(|t| t.elapsed().as_secs_f64() > 1.0)
        {
            state.dirty_since = None;
            store::save(&state);
        }
    }

    /// The world the player is in, and on a change, which windows are seen: the ones standing
    /// in another world are suspended until the player is there again (or recalls them).
    fn enter_world(&self, state: &mut State, wm: &Wm, host: &mut dyn ClientHost) {
        let server = host.client_session().client_server();
        let place = server
            .and_then(|s| {
                s.save_folder
                    .map(|f| format!("save:{f}"))
                    .or(s.address.map(|a| format!("server:{a}")))
            })
            .unwrap_or_default();
        let world = format!("{place}|{}", state.dimension);
        if world == state.world {
            return;
        }
        state.world = world;
        // Windows that never had a world take this one.
        let current = state.world.clone();
        for placed in state.placed.iter_mut().filter(|p| p.world.is_empty()) {
            placed.world = current.clone();
        }
        for placed in &state.placed {
            wm.send(Input::Visible {
                window: placed.window,
                visible: state.here(placed),
            });
        }
        state.changed();
    }

    /// The pointer follows the crosshair over screens.
    fn point(&self, state: &mut State, wm: &Wm, playing: bool) {
        let on = match state.gaze {
            Some(Gaze {
                window,
                part: Part::Screen(at),
                ..
            }) if playing && state.carry.is_none() && state.drag.is_none() => Some((window, at)),
            _ => None,
        };
        match (on, state.pointer) {
            (Some((window, at)), last) => {
                if last.is_none_or(|(w, a)| w != window || a != at) {
                    wm.send(Input::PointerMotion { window, at });
                    state.pointer = Some((window, at));
                }
            }
            (None, Some(_)) => {
                // Off the screen: a held button lets go there, as the edge of a desktop would.
                for button in std::mem::take(&mut state.buttons) {
                    wm.send(Input::PointerButton {
                        button,
                        pressed: false,
                    });
                }
                wm.send(Input::PointerLeave);
                state.pointer = None;
            }
            (None, None) => {}
        }
    }

    /// The keyboard goes back to the game until a window is clicked again: the window lets go
    /// of every key it has down.
    fn release_focus(&self, state: &mut State, wm: &Wm) {
        for code in std::mem::take(&mut state.keys) {
            wm.send(Input::Key {
                code,
                pressed: false,
            });
        }
        state.escape_since = None;
        state.focus = None;
        state.captured = false;
        wm.send(Input::Focus(None));
    }

    /// The keyboard is the focused window's while the crosshair is on it.
    fn capture(&self, state: &mut State, wm: &Wm) {
        if let Some(since) = state.escape_since
            && since.elapsed().as_secs_f64() >= ESCAPE_HOLD
        {
            // Held Escape: the keyboard goes back to the game until a window is clicked.
            state.escape_since = None;
            self.release_focus(state, wm);
        }
        let captured = state.focus.is_some()
            && state.carry.is_none()
            && state.gaze.is_some_and(|g| {
                Some(g.window) == state.focus && matches!(g.part, Part::Screen(_))
            });
        if state.captured && !captured {
            // Looking away: the window lets go of every key it has down.
            for code in std::mem::take(&mut state.keys) {
                wm.send(Input::Key {
                    code,
                    pressed: false,
                });
            }
            state.escape_since = None;
        }
        state.captured = captured;
    }

    fn carry(&self, state: &mut State, eye: V3, dir: V3) {
        let Some(carry) = state.carry else { return };
        let Some((_, _, yaw, pitch)) = state.eye else {
            return;
        };
        let Some(placed) = state.placed_mut(carry.window) else {
            state.carry = None;
            return;
        };
        // It faces the player as they carry it.
        let pose = Pose {
            centre: [0.0; 3],
            yaw,
            pitch: pitch.clamp(-80.0, 80.0),
        };
        let b = pose.basis();
        let point = space::add(eye, space::mul(dir, carry.distance));
        placed.pose = Pose {
            centre: space::sub(
                point,
                space::add(
                    space::mul(b.right, carry.offset[0]),
                    space::mul(b.up, carry.offset[1]),
                ),
            ),
            ..pose
        };
        state.changed();
    }

    /// Windows that overlap push each other apart, a little each frame, as mobs do: the one being
    /// carried or used stands firm, and pushes the others along.
    fn separate(&self, state: &mut State) {
        const SHARE: f64 = 0.35;
        let firm = |s: &State, id: WindowId| {
            s.carry.is_some_and(|c| c.window == id)
                || s.drag.is_some_and(|d| d.window == id)
                || (s.captured && s.focus == Some(id))
        };
        let half = |p: &Placed| {
            [
                f64::from(p.size[0]) * p.pixel / 2.0 + space::BEZEL,
                f64::from(p.size[1]) * p.pixel / 2.0 + space::BEZEL,
                space::DEPTH / 2.0,
            ]
        };
        let shown: Vec<usize> = (0..state.placed.len())
            .filter(|&i| state.here(&state.placed[i]))
            .collect();
        let mut moved = false;
        for (n, &i) in shown.iter().enumerate() {
            for &j in &shown[n + 1..] {
                let (a, b) = (&state.placed[i], &state.placed[j]);
                let Some(push) = space::separation(&a.pose, half(a), &b.pose, half(b)) else {
                    continue;
                };
                let (fa, fb) = (firm(state, a.window), firm(state, b.window));
                let (ka, kb) = match (fa, fb) {
                    (true, true) => continue,
                    (true, false) => (0.0, 1.0),
                    (false, true) => (1.0, 0.0),
                    (false, false) => (0.5, 0.5),
                };
                let a_centre = &mut state.placed[i].pose.centre;
                *a_centre = space::sub(*a_centre, space::mul(push, ka * SHARE));
                let b_centre = &mut state.placed[j].pose.centre;
                *b_centre = space::add(*b_centre, space::mul(push, kb * SHARE));
                moved = true;
            }
        }
        if moved {
            state.changed();
        }
    }

    fn drag(&self, state: &mut State, wm: &Wm, eye: V3, dir: V3) {
        let Some(mut drag) = state.drag else { return };
        let Some(placed) = state.placed(drag.window).cloned() else {
            state.drag = None;
            return;
        };
        let Some(at) = space::on_front(&placed.pose, eye, dir) else {
            return;
        };
        let dx = (at[0] - drag.from[0]) / placed.pixel;
        let dy = (at[1] - drag.from[1]) / placed.pixel;
        let e = drag.edges;
        // The opposite edges stay put (`Placed::anchor`): a dragged edge follows the crosshair.
        let w = drag.size[0]
            + if e.right {
                dx
            } else if e.left {
                -dx
            } else {
                0.0
            };
        let h = drag.size[1]
            + if e.top {
                dy
            } else if e.bottom {
                -dy
            } else {
                0.0
            };
        let size = [
            w.max(MIN_SIZE[0]).round() as i32,
            h.max(MIN_SIZE[1]).round() as i32,
        ];
        if (size[0] - drag.asked[0]).abs() >= 8 || (size[1] - drag.asked[1]).abs() >= 8 {
            drag.asked = size;
            wm.send(Input::Resize {
                window: drag.window,
                size,
            });
            state.drag = Some(drag);
        }
    }

    fn mine(&self, state: &mut State, wm: &Wm) {
        let Some((window, since)) = state.mining else {
            return;
        };
        let on_it = state
            .gaze
            .is_some_and(|g| g.window == window && g.distance <= HAND_REACH);
        if !on_it {
            state.mining = None;
            return;
        }
        if since.elapsed().as_secs_f64() >= MINE_TIME {
            state.mining = None;
            self.pocket(state, wm, window);
            state.pick_up.push(window);
        }
    }

    /// Put a window away: out of the world, its program told nobody sees it.
    fn pocket(&self, state: &mut State, wm: &Wm, window: WindowId) {
        if let Some(placed) = state.placed_mut(window) {
            placed.pocketed = true;
        }
        state.pocket.retain(|w| *w != window);
        state.pocket.push(window);
        if state.focus == Some(window) {
            state.focus = None;
            wm.send(Input::Focus(None));
        }
        wm.send(Input::Visible {
            window,
            visible: false,
        });
        state.changed();
    }

    /// The last window put away comes back, in front of the player. With none put away, the
    /// farthest window does: a window lost behind a wall or far away is never out of reach.
    fn recall(&self, state: &mut State, wm: &Wm, windows: &[Window]) {
        let in_pocket = state
            .pocket
            .iter()
            .rposition(|w| !state.itemised.contains(w))
            .map(|i| state.pocket[i]);
        // Then a window standing in another world.
        let elsewhere = state
            .placed
            .iter()
            .find(|p| !p.pocketed && p.world != state.world)
            .map(|p| p.window);
        let farthest = state.eye.and_then(|(eye, ..)| {
            state
                .placed
                .iter()
                .filter(|p| state.here(p))
                .max_by(|a, b| {
                    let d = |p: &Placed| {
                        let v = space::sub(p.pose.centre, eye);
                        space::dot(v, v)
                    };
                    d(a).total_cmp(&d(b))
                })
                .map(|p| p.window)
        });
        let Some(window) = in_pocket.or(elsewhere).or(farthest) else {
            return;
        };
        state.pocket.retain(|p| *p != window);
        let pose = windows
            .iter()
            .find(|w| w.id == window)
            .map(|w| self.place_new(state, w));
        let world = state.world.clone();
        if let (Some(pose), Some(placed)) = (pose, state.placed_mut(window)) {
            placed.pose = pose;
            placed.pocketed = false;
            placed.world = world;
        }
        wm.send(Input::Visible {
            window,
            visible: true,
        });
        state.changed();
    }

    /// Ask the server for the items of windows just picked up; close the windows whose items
    /// were destroyed.
    fn item_traffic(
        &self,
        state: &mut State,
        wm: &Wm,
        windows: &[Window],
        host: &mut dyn ClientHost,
    ) {
        for window in std::mem::take(&mut state.pick_up) {
            let Some(info) = windows.iter().find(|w| w.id == window) else {
                continue;
            };
            let message = items::Message::PickedUp {
                key: items::WindowKey {
                    session: state.instance,
                    window: window.0,
                },
                title: info.title.clone(),
                app_id: info.app_id.clone(),
            };
            match host.send_payload_to_server(&items::channel(), message.bytes()) {
                Ok(_) => {
                    state.itemised.insert(window);
                }
                // A server without this mod: the window stays put away, for the recall key.
                Err(error) => log::info!("wm_world: no window item ({error:?}): put away only"),
            }
        }
        let launches =
            std::mem::take(&mut *items::LAUNCHES.lock().unwrap_or_else(|p| p.into_inner()));
        for (exec, at, yaw, pitch) in launches {
            log::info!("wm_world: a dispenser starts `{exec}`");
            wm.spawn(&exec);
            state.place_next = Some((
                Pose {
                    centre: at,
                    yaw,
                    pitch,
                },
                Instant::now(),
            ));
        }
        let closes = std::mem::take(&mut *items::CLOSES.lock().unwrap_or_else(|p| p.into_inner()));
        for key in closes {
            if let Some(window) = items::local_window(key, state.instance) {
                wm.send(Input::Close(window));
            }
        }
    }

    /// A window item was used: its window comes back in front of the player, or, gone, its
    /// program starts again.
    fn use_window_item(&self, key: items::WindowKey, app_id: &str) {
        let Some(wm) = wayland_wm::wm() else { return };
        let windows = wm.windows();
        let mut state = self.lock();
        match items::local_window(key, state.instance)
            .filter(|w| windows.iter().any(|x| x.id == *w))
        {
            Some(window) => {
                state.itemised.remove(&window);
                state.pocket.retain(|w| *w != window);
                let pose = windows
                    .iter()
                    .find(|w| w.id == window)
                    .map(|w| self.place_new(&state, w));
                let world = state.world.clone();
                if let (Some(pose), Some(placed)) = (pose, state.placed_mut(window)) {
                    placed.pose = pose;
                    placed.pocketed = false;
                    placed.world = world;
                }
                wm.send(Input::Visible {
                    window,
                    visible: true,
                });
                state.changed();
            }
            None if !app_id.is_empty() => {
                log::info!("wm_world: the window of this item is gone: starting {app_id} again");
                wm.spawn(&format!("gtk-launch {app_id} 2>/dev/null || {app_id}"));
            }
            None => {}
        }
    }

    /// The mod's own keys.
    fn keys(&self, state: &mut State, wm: &Wm, host: &mut dyn ClientHost) {
        if host.consume_click(MOD_ID, "recall") {
            let windows = wm.windows();
            self.recall(state, wm, &windows);
        }
        if host.consume_click(MOD_ID, "terminal") {
            let command = std::env::var("HORIZON_WM_TERMINAL")
                .ok()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| {
                    "for t in foot alacritty kitty wezterm konsole gnome-terminal xterm; do \
                     command -v $t >/dev/null && exec $t; done"
                        .into()
                });
            wm.spawn(&command);
        }
    }

    /// A mouse button. `sneaking`: the sneak mapping is held.
    fn mouse(
        &self,
        host: &mut dyn ClientHost,
        glfw_button: i32,
        pressed: bool,
        sneaking: bool,
    ) -> Flow {
        let Some(wm) = wayland_wm::wm() else {
            return Flow::Continue;
        };
        let button = match glfw_button {
            0 => button::LEFT,
            1 => button::RIGHT,
            2 => button::MIDDLE,
            3 => button::SIDE,
            4 => button::EXTRA,
            _ => return Flow::Continue,
        };
        wm.player_input(host);
        let mut state = self.lock();
        log::debug!(
            "wm_world: button {button:#x} {} gaze {:?} sneaking {sneaking}",
            if pressed { "down" } else { "up" },
            state.gaze,
        );
        if !pressed {
            // Releases go wherever their press went.
            if state.buttons.remove(&button) {
                wm.send(Input::PointerButton {
                    button,
                    pressed: false,
                });
                return Flow::Cancel;
            }
            if button == button::LEFT && state.mining.take().is_some() {
                return Flow::Cancel;
            }
            if button == button::RIGHT && state.drag.take().is_some() {
                state.changed();
                return Flow::Cancel;
            }
            return Flow::Continue;
        }
        if !host.playing() {
            return Flow::Continue;
        }
        // Setting a carried window down: beside another, it snaps to it.
        if button == button::RIGHT
            && let Some(carry) = state.carry.take()
        {
            let arc = if host.setting(MOD_ID, CURVE_SETTING).as_deref() == Some("false") {
                0.0
            } else {
                ROW_CURVE
            };
            snap(&mut state, carry.window, arc);
            state.changed();
            return Flow::Cancel;
        }
        let Some(gaze) = state.gaze else {
            return Flow::Continue;
        };
        match gaze.part {
            Part::Screen(at) => {
                if state.focus != Some(gaze.window) {
                    state.focus = Some(gaze.window);
                    wm.send(Input::Focus(Some(gaze.window)));
                }
                wm.send(Input::PointerMotion {
                    window: gaze.window,
                    at,
                });
                state.pointer = Some((gaze.window, at));
                wm.send(Input::PointerButton {
                    button,
                    pressed: true,
                });
                state.buttons.insert(button);
                Flow::Cancel
            }
            _ if gaze.distance > HAND_REACH => Flow::Continue,
            part => {
                let Some(placed) = state.placed(gaze.window).cloned() else {
                    return Flow::Continue;
                };
                match (button, part) {
                    (b, _) if b == button::LEFT => {
                        state.mining = Some((gaze.window, Instant::now()));
                    }
                    (b, Part::Frame(edges)) if b == button::RIGHT && sneaking => {
                        let (eye, dir) = state.eye.map(|(e, d, ..)| (e, d)).unwrap_or_default();
                        if let Some(from) = space::on_front(&placed.pose, eye, dir) {
                            if let Some(placed) = state.placed_mut(gaze.window) {
                                placed.anchor = Edges {
                                    left: edges.right,
                                    right: edges.left,
                                    top: edges.bottom || !edges.top,
                                    bottom: edges.top,
                                };
                            }
                            state.drag = Some(Drag {
                                window: gaze.window,
                                edges,
                                from,
                                size: [f64::from(placed.size[0]), f64::from(placed.size[1])],
                                asked: placed.size,
                            });
                        }
                    }
                    (b, _) if b == button::RIGHT => {
                        let (eye, dir) = state.eye.map(|(e, d, ..)| (e, d)).unwrap_or_default();
                        let point = space::add(eye, space::mul(dir, gaze.distance));
                        let local = placed.pose.local(point);
                        state.carry = Some(Carry {
                            window: gaze.window,
                            distance: gaze.distance,
                            offset: [local[0], local[1]],
                        });
                    }
                    _ => {}
                }
                Flow::Cancel
            }
        }
    }
}

/// Set `window` against the side of the nearest window it was put down beside: within half a
/// block of that side, turned much the same way. `arc` turns it towards the row's viewer.
fn snap(state: &mut State, window: WindowId, arc: f32) {
    const REACH: f64 = 0.5;
    let Some(moved) = state.placed(window).cloned() else {
        return;
    };
    let half = |p: &Placed| {
        [
            f64::from(p.size[0]) * p.pixel / 2.0 + space::BEZEL,
            f64::from(p.size[1]) * p.pixel / 2.0 + space::BEZEL,
        ]
    };
    let mh = half(&moved);
    let mut best: Option<(f64, Pose)> = None;
    for other in state
        .placed
        .iter()
        .filter(|p| p.window != window && state.here(p))
    {
        let turn = (moved.pose.yaw - other.pose.yaw + 540.0).rem_euclid(360.0) - 180.0;
        if turn.abs() > 60.0 || (moved.pose.pitch - other.pose.pitch).abs() > 30.0 {
            continue;
        }
        let oh = half(other);
        for (side, sign) in [(space::Side::Right, 1.0), (space::Side::Left, -1.0)] {
            // The other's side, and the moved window's facing side, at mid height.
            let theirs = other.pose.at(sign * oh[0], 0.0, 0.0);
            let ours = moved.pose.at(-sign * mh[0], 0.0, 0.0);
            let d = space::sub(theirs, ours);
            let gap = space::dot(d, d).sqrt();
            if gap < REACH && best.is_none_or(|(g, _)| gap < g) {
                best = Some((gap, space::snapped(&other.pose, oh, mh, side, arc)));
            }
        }
    }
    if let Some((_, pose)) = best
        && let Some(placed) = state.placed_mut(window)
    {
        placed.pose = pose;
    }
}

/// The nearest window part along the view, if no block is nearer.
fn gaze(state: &State, windows: &[Window], eye: V3, dir: V3, block: Option<f64>) -> Option<Gaze> {
    let mut best: Option<Gaze> = None;
    for placed in state.placed.iter().filter(|p| state.here(p)) {
        // A carried window is under the crosshair by construction: look through it.
        if state.carry.is_some_and(|c| c.window == placed.window) {
            continue;
        }
        let Some(window) = windows.iter().find(|w| w.id == placed.window) else {
            continue;
        };
        let rects = layer_rects(window);
        let Some((distance, part)) = space::hit(&placed.pose, &placed.shape(&rects), eye, dir)
        else {
            continue;
        };
        if distance > POINT_REACH || best.is_some_and(|b| b.distance <= distance) {
            continue;
        }
        best = Some(Gaze {
            window: placed.window,
            part,
            distance,
        });
    }
    best.filter(|g| block.is_none_or(|b| g.distance < b))
}

/// Draws the windows in the world, and what the player is doing with them on the HUD.
struct Drawer(&'static WmWorld);

impl Presenter for Drawer {
    fn draw_world(
        &self,
        _host: &dyn ClientHost,
        _wm: &Wm,
        windows: &[Window],
        event: &mut RenderLevelStageEvent<'_>,
    ) {
        let state = self.0.lock();
        draw::world(&state, windows, event);
    }

    fn draw_gui(
        &self,
        host: &dyn ClientHost,
        _wm: &Wm,
        windows: &[Window],
        draw: &mut dyn mod_api::DrawContext,
    ) {
        let state = self.0.lock();
        draw::hud(host, &state, windows, draw);
    }
}

static MOD: OnceLock<&'static WmWorld> = OnceLock::new();

/// The mod's one instance: hooks get `&self`, the presenter needs `'static`.
fn this() -> &'static WmWorld {
    MOD.get_or_init(|| Box::leak(Box::default()))
}

/// App items in a dispenser start their program, the window in front of the dispenser.
struct Dispensable;

#[mod_api::horizon_part]
impl mod_api::ItemPart for Dispensable {
    fn dispense_item(
        &self,
        host: &mut dyn mod_api::ServerHost,
        event: &mut mod_api::DispenseItemEvent<'_>,
    ) -> Option<mod_api::Dispensed> {
        let exec = apps::exec_of(event.stack)?;
        items::dispensed(host, event.dimension, event.pos, event.facing, exec);
        // The item stays; the dispenser clicks.
        Some((Some(event.stack.clone()), 1000))
    }
}

/// The hooks, forwarding to [`this`].
#[derive(Default)]
pub struct Hooked;

#[mod_api::horizon_mod]
impl Hooks for Hooked {
    fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
        // Optional: a server without this mod still lets the player in (windows are put away
        // instead of becoming items).
        items::set_key(r.item_key(items::WINDOW_ITEM));
        let app = r.item_key(apps::APP_ITEM);
        apps::set_key(app);
        r.item_part(app, Dispensable);
        let (server, client) = items::handlers();
        r.payload_handlers()
            .registrar("1")
            .optional()
            .play_bidirectional(items::channel(), server, Some(client));
        let hints = mod_api::SettingSpec::toggle(HINTS_SETTING, "wm_world.setting.hints", true)
            .tooltip("wm_world.setting.hints.tooltip");
        if let Err(error) = r.register_setting(hints) {
            log::warn!("wm_world: the hints setting wasn't registered: {error:?}");
        }
        let curve =
            mod_api::SettingSpec::toggle(CURVE_SETTING, "wm_world.setting.curved_rows", true)
                .tooltip("wm_world.setting.curved_rows.tooltip");
        if let Err(error) = r.register_setting(curve) {
            log::warn!("wm_world: the curved rows setting wasn't registered: {error:?}");
        }
        r.register_key_mapping(
            KeyRegistration::new("recall", "key.keyboard.r").label("key.wm_world.recall"),
        );
        r.register_key_mapping(
            KeyRegistration::new("release", "key.keyboard.left.win").label("key.wm_world.release"),
        );
        r.register_key_mapping(
            KeyRegistration::new("terminal", "key.keyboard.grave.accent")
                .label("key.wm_world.terminal"),
        );
    }

    fn items(&self, _context: &RegistrationContext, event: &mut mod_api::ItemsEvent<'_>) {
        items::declare(event.items);
        apps::declare(event.items);
    }

    fn entity_join_level(
        &self,
        host: &mut dyn mod_api::ServerHost,
        event: &mut mod_api::EntityJoinLevelEvent<'_>,
    ) -> Flow {
        items::joined(host, event.dimension, event.entity, event.kind, event.pos);
        Flow::Continue
    }

    fn entity_leave_level(
        &self,
        _host: &mut dyn mod_api::ServerHost,
        event: &mut mod_api::EntityLeaveLevelEvent<'_>,
    ) {
        if event.kind == "minecraft:item" {
            items::left(event.entity);
        }
    }

    fn item_expire(
        &self,
        host: &mut dyn mod_api::ServerHost,
        event: &mut mod_api::ItemExpireEvent<'_>,
    ) {
        items::expired(host, event.entity, event.stack);
    }

    fn server_tick(
        &self,
        host: &mut dyn mod_api::ServerHost,
        event: &mut mod_api::ServerTickEvent<'_>,
    ) {
        if matches!(event.phase, mod_api::ServerTickPhase::ServerTickEnd { .. }) {
            items::tick(host);
        }
    }

    fn hopper_pickup_item(
        &self,
        _level: &dyn mod_api::LevelHost,
        event: &mut mod_api::HopperPickupItemEvent<'_>,
    ) -> Flow {
        // A window item in a hopper would look destroyed: hoppers leave them be.
        if event.item == items::WINDOW_ITEM {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    fn right_click_item(
        &self,
        host: mod_api::HostSide,
        event: &mut mod_api::RightClickItemEvent<'_>,
    ) -> Flow {
        if let Some(exec) = apps::exec_of(event.click.stack) {
            // An app item starts its program on the client; it isn't used up.
            if let mod_api::HostSide::Client(_) = host
                && let Some(wm) = wayland_wm::wm()
            {
                log::info!("wm_world: starting `{exec}`");
                wm.spawn(&exec);
            }
            *event.cancellation_result = mod_api::InteractionResult::Success;
            return Flow::Cancel;
        }
        let Some((key, app_id)) = items::key_of(event.click.stack) else {
            return Flow::Continue;
        };
        match host {
            // The client brings the window back...
            mod_api::HostSide::Client(_) => this().use_window_item(key, &app_id),
            // ...and the server uses the item up.
            mod_api::HostSide::Server(host) => {
                if let Some(player) = host.player(event.click.player) {
                    let slot = match event.click.hand {
                        mod_api::Hand::Off => Some(40),
                        _ => player.selected_slot(&*host).map(|s| s as u32),
                    };
                    if let Some(slot) = slot {
                        let _ = player.set_inventory_slot(host, slot, None);
                    }
                }
            }
        }
        *event.cancellation_result = mod_api::InteractionResult::Success;
        Flow::Cancel
    }

    fn client_level_load(
        &self,
        _host: &mut dyn ClientHost,
        event: &mut mod_api::ClientLevelLoadEvent<'_>,
    ) {
        this().lock().dimension = event.dimension.to_string();
    }

    fn build_creative_mode_tab_contents(
        &self,
        _host: &mut dyn ClientHost,
        event: &mut mod_api::BuildCreativeModeTabContentsEvent<'_>,
    ) {
        if event.tab == "minecraft:tools_and_utilities" {
            event
                .items
                .extend(apps::installed().iter().filter_map(apps::stack));
        }
    }

    fn render_frame_post(&self, host: &mut dyn ClientHost, _event: &mut RenderFramePostEvent) {
        let this = this();
        if wayland_wm::wm().is_some() {
            this.register_presenter();
        }
        this.frame(host);
    }

    fn mouse_button_pre(
        &self,
        host: &mut dyn ClientHost,
        event: &mut mod_api::MouseButtonPreEvent,
    ) -> Flow {
        // Sneaking as the game knows it: the sneak mapping, wherever it's bound (or toggled).
        let sneaking = matches!(host.mapping("key.sneak"), Ok(Some(m)) if m.down);
        this().mouse(host, event.button, event.pressed, sneaking)
    }

    fn mouse_scrolling(
        &self,
        host: &mut dyn ClientHost,
        event: &mut mod_api::MouseScrollingEvent,
    ) -> Flow {
        let Some(wm) = wayland_wm::wm() else {
            return Flow::Continue;
        };
        if !host.playing() {
            return Flow::Continue;
        }
        let mut state = this().lock();
        if let Some(carry) = &mut state.carry {
            carry.distance = (carry.distance + event.scroll[1] * 0.25).clamp(1.0, 24.0);
            return Flow::Cancel;
        }
        match state.gaze {
            Some(Gaze {
                part: Part::Screen(_),
                ..
            }) => {
                // Wayland scrolls down for positive values; the wheel's up is positive here.
                wm.send(Input::PointerAxis {
                    horizontal: event.scroll[0] * 15.0,
                    vertical: -event.scroll[1] * 15.0,
                    discrete: Some([event.accumulated[0], -event.accumulated[1]]),
                });
                Flow::Cancel
            }
            _ => Flow::Continue,
        }
    }

    fn key_capture(
        &self,
        host: &mut dyn ClientHost,
        event: &mut mod_api::KeyCaptureEvent<'_>,
    ) -> Flow {
        let Some(wm) = wayland_wm::wm() else {
            return Flow::Continue;
        };
        wm.player_input(host);
        let mut state = this().lock();
        let code = event.raw_code;
        // The release key (Super unless rebound) gives the keyboard back to the game, whatever
        // window has it; it never reaches the window.
        let release = host.mod_key(MOD_ID, "release").map(|(key, _)| key);
        if release.as_deref() == Some(event.key) {
            match event.action {
                KeyAction::Press if state.focus.is_some() => {
                    this().release_focus(&mut state, wm);
                    state.release_held = true;
                    return Flow::Cancel;
                }
                KeyAction::Repeat | KeyAction::Release if state.release_held => {
                    if matches!(event.action, KeyAction::Release) {
                        state.release_held = false;
                    }
                    return Flow::Cancel;
                }
                _ => {}
            }
        }
        log::debug!(
            "wm_world: key {} {code} {:?} captured {} focus {:?} gaze {:?}",
            event.key,
            event.action,
            state.captured,
            state.focus,
            state.gaze
        );
        match event.action {
            KeyAction::Press if state.captured => {
                if code == KEY_ESC {
                    state.escape_since = Some(Instant::now());
                }
                state.keys.insert(code);
                wm.send(Input::Key {
                    code,
                    pressed: true,
                });
                Flow::Cancel
            }
            // Programs repeat keys themselves.
            KeyAction::Repeat if state.captured || state.keys.contains(&code) => Flow::Cancel,
            KeyAction::Release if state.keys.remove(&code) => {
                if code == KEY_ESC {
                    state.escape_since = None;
                }
                wm.send(Input::Key {
                    code,
                    pressed: false,
                });
                Flow::Cancel
            }
            _ => Flow::Continue,
        }
    }

    fn calculate_player_turn(
        &self,
        _host: &dyn ClientHost,
        event: &mut mod_api::CalculatePlayerTurnEvent,
    ) {
        // Pixel-true aim: over the window being typed into, one mouse step moves about one of its
        // pixels, as on a desktop. Vanilla turns (0.6 s + 0.2)³ × 8 × 0.15 degrees a count.
        let state = this().lock();
        let Some(gaze) = state
            .gaze
            .filter(|g| state.captured && matches!(g.part, Part::Screen(_)))
        else {
            return;
        };
        let Some(placed) = state.placed(gaze.window) else {
            return;
        };
        let degrees_per_pixel = (placed.pixel / gaze.distance.max(0.5)).to_degrees();
        let f = (degrees_per_pixel / 0.15 / 8.0).cbrt();
        let sensitivity = ((f - 0.2) / 0.6).max(0.0);
        if sensitivity < event.mouse_sensitivity {
            event.mouse_sensitivity = sensitivity;
        }
    }
}
