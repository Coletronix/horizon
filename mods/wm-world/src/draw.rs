//! Drawing: each window's slab and screen in the world, and the hints on the HUD.
use crate::space::{self, BEZEL, DEPTH, Part, V3};
use crate::{HAND_REACH, MINE_TIME, Placed, State};
use mod_api::{
    ClientHost, DrawContext, PipelineQuad, RenderLevelStage, RenderLevelStageEvent, Text,
    WorldDepth, WorldLine, WorldQuad,
};
use wayland_wm::Window;

const BEZEL_COLOUR: u32 = 0xFF1D1D22;
const BEZEL_HOVER: u32 = 0xFF34343C;
const BEZEL_FOCUS: u32 = 0xFF2E5AAC;
const BEZEL_MINED: u32 = 0xFF8C2F2A;
const SIDE_COLOUR: u32 = 0xFF17171B;
const BACK_COLOUR: u32 = 0xFF26262C;
const SCREEN_OFF: u32 = 0xFF050506;

fn lerp(a: u32, b: u32, t: f64) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let (x, y) = (
            f64::from((a >> shift) & 0xFF),
            f64::from((b >> shift) & 0xFF),
        );
        ((x + (y - x) * t).round() as u32) << shift
    };
    ch(24) | ch(16) | ch(8) | ch(0)
}

/// Every placed window, farthest first: plain quads don't write depth, so nearer slabs must come
/// later to cover farther ones.
pub fn world(state: &State, windows: &[Window], event: &mut RenderLevelStageEvent<'_>) {
    // After clouds and weather: plain quads write no depth, so later sky layers would cover them.
    if event.stage != RenderLevelStage::AfterWeather {
        return;
    }
    let Some((eye, ..)) = state.eye else { return };
    let mut shown: Vec<(&Placed, &Window)> = state
        .placed
        .iter()
        .filter(|p| !p.pocketed)
        .filter_map(|p| {
            windows
                .iter()
                .find(|w| w.id == p.window && w.mapped())
                .map(|w| (p, w))
        })
        .collect();
    let distance = |p: &Placed| {
        let d = space::sub(p.pose.centre, eye);
        space::dot(d, d)
    };
    shown.sort_by(|a, b| distance(b.0).total_cmp(&distance(a.0)));
    for (placed, window) in shown {
        slab(state, placed, eye, event);
        layers(placed, window, event);
    }
    if let Some(drag) = state.drag
        && let Some(placed) = state.placed(drag.window)
    {
        ghost(placed, drag.asked, event);
    }
}

/// The slab: back and sides, the bezel and the dark screen behind the window's pixels.
fn slab(state: &State, placed: &Placed, eye: V3, event: &mut RenderLevelStageEvent<'_>) {
    let hw = f64::from(placed.size[0]) * placed.pixel / 2.0;
    let hh = f64::from(placed.size[1]) * placed.pixel / 2.0;
    let (ow, oh, hz) = (hw + BEZEL, hh + BEZEL, DEPTH / 2.0);
    let at = |x: f64, y: f64, z: f64| placed.pose.at(x, y, z);
    let gazed = state.gaze.filter(|g| g.window == placed.window);
    let mut bezel = if state.focus == Some(placed.window) {
        BEZEL_FOCUS
    } else if gazed.is_some_and(|g| !matches!(g.part, Part::Screen(_)) && g.distance <= HAND_REACH)
        || state.carry.is_some_and(|c| c.window == placed.window)
    {
        BEZEL_HOVER
    } else {
        BEZEL_COLOUR
    };
    if let Some((window, since)) = state.mining
        && window == placed.window
    {
        bezel = lerp(
            bezel,
            BEZEL_MINED,
            since.elapsed().as_secs_f64() / MINE_TIME,
        );
    }
    // Faces as (corners, colour), each listed top left, bottom left, bottom right, top right.
    let rect = |x0: f64, y0: f64, x1: f64, y1: f64, z: f64| {
        [at(x0, y1, z), at(x0, y0, z), at(x1, y0, z), at(x1, y1, z)]
    };
    let back = rect(-ow, -oh, ow, oh, -hz);
    let sides = [
        [
            at(-ow, oh, hz),
            at(-ow, -oh, hz),
            at(-ow, -oh, -hz),
            at(-ow, oh, -hz),
        ],
        [
            at(ow, oh, -hz),
            at(ow, -oh, -hz),
            at(ow, -oh, hz),
            at(ow, oh, hz),
        ],
        [
            at(-ow, oh, -hz),
            at(-ow, oh, hz),
            at(ow, oh, hz),
            at(ow, oh, -hz),
        ],
        [
            at(-ow, -oh, hz),
            at(-ow, -oh, -hz),
            at(ow, -oh, -hz),
            at(ow, -oh, hz),
        ],
    ];
    let front = [
        (rect(-ow, hh, ow, oh, hz), bezel),
        (rect(-ow, -oh, ow, -hh, hz), bezel),
        (rect(-ow, -hh, -hw, hh, hz), bezel),
        (rect(hw, -hh, ow, hh, hz), bezel),
        (rect(-hw, -hh, hw, hh, hz), SCREEN_OFF),
    ];
    // Faces turned away from the eye first.
    let facing = |corners: &[V3; 4]| {
        let centre = space::mul(
            space::add(
                space::add(corners[0], corners[1]),
                space::add(corners[2], corners[3]),
            ),
            0.25,
        );
        let normal = space::cross(
            space::sub(corners[1], corners[0]),
            space::sub(corners[2], corners[1]),
        );
        space::dot(normal, space::sub(eye, centre)) > 0.0
    };
    let mut faces: Vec<([V3; 4], u32)> = Vec::new();
    faces.push((back, BACK_COLOUR));
    faces.extend(sides.iter().map(|s| (*s, SIDE_COLOUR)));
    faces.extend(front);
    faces.sort_by_key(|(corners, _)| facing(corners));
    let quads: Vec<WorldQuad> = faces
        .into_iter()
        .map(|(corners, colour)| WorldQuad::new(corners, colour, WorldDepth::Tested))
        .collect();
    let _ = event.draw.quads(&quads);
}

/// The window's surfaces on the front face, popups a little in front of the window.
fn layers(placed: &Placed, window: &Window, event: &mut RenderLevelStageEvent<'_>) {
    let hw = f64::from(placed.size[0]) * placed.pixel / 2.0;
    let hh = f64::from(placed.size[1]) * placed.pixel / 2.0;
    for (i, layer) in window.layers.iter().enumerate() {
        let z = DEPTH / 2.0 + 0.002 + if layer.popup { 0.004 } else { 0.0 } + i as f64 * 1e-4;
        let x0 = -hw + f64::from(layer.at[0]) * placed.pixel;
        let x1 = x0 + f64::from(layer.size[0]) * placed.pixel;
        let y0 = hh - f64::from(layer.at[1]) * placed.pixel;
        let y1 = y0 - f64::from(layer.size[1]) * placed.pixel;
        let at = |x: f64, y: f64| placed.pose.at(x, y, z);
        let quad =
            PipelineQuad::spanning([at(x0, y0), at(x0, y1), at(x1, y1), at(x1, y0)], 0xFFFFFFFF);
        let _ = event
            .draw
            .live_quads(&layer.texture, &[quad], WorldDepth::Tested);
    }
}

/// The size a resize asked for, outlined from the edges that stay put.
fn ghost(placed: &Placed, size: [i32; 2], event: &mut RenderLevelStageEvent<'_>) {
    let (hw, hh) = (
        f64::from(placed.size[0]) * placed.pixel / 2.0,
        f64::from(placed.size[1]) * placed.pixel / 2.0,
    );
    let (w, h) = (
        f64::from(size[0]) * placed.pixel,
        f64::from(size[1]) * placed.pixel,
    );
    let a = placed.anchor;
    let x0 = match (a.left, a.right) {
        (true, false) => -hw,
        (false, true) => hw - w,
        _ => -w / 2.0,
    };
    let y1 = match (a.top, a.bottom) {
        (true, false) => hh,
        (false, true) => -hh + h,
        _ => h / 2.0,
    };
    let z = DEPTH / 2.0 + 0.01;
    let c = [
        placed.pose.at(x0, y1, z),
        placed.pose.at(x0, y1 - h, z),
        placed.pose.at(x0 + w, y1 - h, z),
        placed.pose.at(x0 + w, y1, z),
    ];
    let lines: Vec<WorldLine> = (0..4)
        .map(|i| {
            WorldLine::new(
                c[i],
                c[(i + 1) % 4],
                0xFFFFFFFF,
                2.0,
                WorldDepth::ThroughWalls,
            )
        })
        .collect();
    let _ = event.draw.lines(&lines);
}

/// One line of hint text, centred near the top of the screen.
fn hint(host: &dyn ClientHost, draw: &mut dyn DrawContext, text: Text, row: f32) {
    let Ok(line) = host.text_string(&text) else {
        return;
    };
    let [w, _] = draw.size();
    let width = draw.text_width(&line);
    draw.fill(
        [(w - width) / 2.0 - 3.0, 4.0 + row * 12.0, width + 6.0, 11.0],
        0x90000000,
    );
    draw.text(
        &line,
        [(w - width) / 2.0, 5.0 + row * 12.0],
        0xFFFFFFFF,
        true,
    );
}

/// What the player is doing with windows, and how to stop.
pub fn hud(host: &dyn ClientHost, state: &State, windows: &[Window], draw: &mut dyn DrawContext) {
    let title = |id| {
        windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.title.clone())
            .unwrap_or_default()
    };
    let mut row = 0.0;
    if state.captured
        && let Some(focus) = state.focus
    {
        hint(
            host,
            draw,
            Text::translate("wm_world.hud.typing", vec![Text::literal(title(focus))]),
            row,
        );
        row += 1.0;
    } else if state.carry.is_some() {
        hint(
            host,
            draw,
            Text::translate("wm_world.hud.carrying", vec![]),
            row,
        );
        row += 1.0;
    } else if state.drag.is_some() {
        hint(
            host,
            draw,
            Text::translate("wm_world.hud.resizing", vec![]),
            row,
        );
        row += 1.0;
    } else if let Some(gaze) = state.gaze
        && !matches!(gaze.part, Part::Screen(_))
        && gaze.distance <= HAND_REACH
    {
        hint(
            host,
            draw,
            Text::translate("wm_world.hud.frame", vec![]),
            row,
        );
        row += 1.0;
    }
    if !state.pocket.is_empty() {
        hint(
            host,
            draw,
            Text::translate(
                "wm_world.hud.pocket",
                vec![Text::literal(state.pocket.len().to_string())],
            ),
            row,
        );
    }
}
