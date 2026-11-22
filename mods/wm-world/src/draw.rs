//! Drawing: each window's slab and screen in the world, and the hints on the HUD.
use crate::space::{self, BEZEL, DEPTH, Part, V3};
use crate::{HAND_REACH, MINE_TIME, Placed, State};
use mod_api::{
    ClientHost, DrawContext, PipelineQuad, RenderLevelStage, RenderLevelStageEvent, Text,
    WorldDepth, WorldLine,
};
use wayland_wm::{LivePixelFormat, LiveTexture, Window};

const BEZEL_COLOUR: u32 = 0xFF1D1D22;
const BEZEL_HOVER: u32 = 0xFF34343C;
const BEZEL_FOCUS: u32 = 0xFF2E5AAC;
const BEZEL_MINED: u32 = 0xFF8C2F2A;
const SIDE_COLOUR: u32 = 0xFF17171B;
const BACK_COLOUR: u32 = 0xFF26262C;
const SCREEN_OFF: u32 = 0xFF050506;

/// One white texel, tinted to colour the slabs.
fn white() -> &'static LiveTexture {
    static WHITE: std::sync::OnceLock<LiveTexture> = std::sync::OnceLock::new();
    WHITE.get_or_init(|| {
        let texture = LiveTexture::new(1, 1);
        texture.write(1, 1, 4, LivePixelFormat::Rgba8, &[255; 4], &[]);
        texture
    })
}

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
        .filter(|p| state.here(p))
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
        // From behind (or edge on) a window is its back: its screen isn't seen through it.
        if placed.pose.local(eye)[2] > DEPTH / 2.0 {
            layers(placed, window, event);
        }
    }
    if let Some(drag) = state.drag
        && let Some(placed) = state.placed(drag.window)
    {
        ghost(placed, drag.asked, event);
    }
    leads(state, eye, event);
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
    // Faces turned away from the eye first, each by its outward normal.
    let basis = placed.pose.basis();
    let neg = |v: V3| space::mul(v, -1.0);
    let mut faces: Vec<([V3; 4], u32, V3)> = Vec::new();
    faces.push((back, BACK_COLOUR, neg(basis.out)));
    let side_normals = [neg(basis.right), basis.right, basis.up, neg(basis.up)];
    faces.extend(
        sides
            .iter()
            .zip(side_normals)
            .map(|(s, n)| (*s, SIDE_COLOUR, n)),
    );
    faces.extend(front.into_iter().map(|(c, colour)| (c, colour, basis.out)));
    let facing =
        |corners: &[V3; 4], normal: V3| space::dot(normal, space::sub(eye, corners[0])) > 0.0;
    faces.sort_by_key(|(corners, _, normal)| facing(corners, *normal));
    // Drawn as live quads, as the screens are: one queue, in order, so a nearer window's slab
    // covers a farther window's screen (plain quads would all go before every screen).
    let quads: Vec<PipelineQuad> = faces
        .into_iter()
        .map(|(corners, colour, _)| PipelineQuad::spanning(corners, colour))
        .collect();
    let _ = event.draw.live_quads(white(), &quads, WorldDepth::Tested);
}

/// The window's surfaces on the front face, popups a little in front of the window. The window's
/// own surfaces are cut to its geometry: their client-side shadows would lie over the bezel.
fn layers(placed: &Placed, window: &Window, event: &mut RenderLevelStageEvent<'_>) {
    let hw = f64::from(placed.size[0]) * placed.pixel / 2.0;
    let hh = f64::from(placed.size[1]) * placed.pixel / 2.0;
    let size = [f64::from(placed.size[0]), f64::from(placed.size[1])];
    for (i, layer) in window.layers.iter().enumerate() {
        let z = DEPTH / 2.0 + 0.002 + if layer.popup { 0.004 } else { 0.0 } + i as f64 * 1e-4;
        let (at, extent) = (
            [f64::from(layer.at[0]), f64::from(layer.at[1])],
            [f64::from(layer.size[0]), f64::from(layer.size[1])],
        );
        if extent[0] <= 0.0 || extent[1] <= 0.0 {
            continue;
        }
        // The part shown, in window pixels.
        let (mut a, mut b) = (at, [at[0] + extent[0], at[1] + extent[1]]);
        if !layer.popup {
            a = [a[0].max(0.0), a[1].max(0.0)];
            b = [b[0].min(size[0]), b[1].min(size[1])];
            if b[0] <= a[0] || b[1] <= a[1] {
                continue;
            }
        }
        let u = |x: f64| ((x - at[0]) / extent[0]) as f32;
        let v = |y: f64| ((y - at[1]) / extent[1]) as f32;
        let corner = |x: f64, y: f64| {
            placed
                .pose
                .at(-hw + x * placed.pixel, hh - y * placed.pixel, z)
        };
        let quad = PipelineQuad::new(
            [
                corner(a[0], a[1]),
                corner(a[0], b[1]),
                corner(b[0], b[1]),
                corner(b[0], a[1]),
            ],
            [
                [u(a[0]), v(a[1])],
                [u(a[0]), v(b[1])],
                [u(b[0]), v(b[1])],
                [u(b[0]), v(a[1])],
            ],
            0xFFFFFFFF,
        );
        let _ = event
            .draw
            .live_quads(&layer.texture, &[quad], WorldDepth::Tested);
    }
}

/// Each lead, from the top of its window to what holds it: the player's hand or a post.
fn leads(state: &State, eye: V3, event: &mut RenderLevelStageEvent<'_>) {
    const LEAD_COLOUR: u32 = 0xFF6B4A2B;
    let lines: Vec<WorldLine> = state
        .leashed
        .iter()
        .filter_map(|(window, tether)| {
            let placed = state.placed(*window).filter(|p| state.here(p))?;
            let top = placed.pose.at(
                0.0,
                f64::from(placed.size[1]) * placed.pixel / 2.0 + BEZEL,
                0.0,
            );
            let end = match tether {
                crate::Tether::Player => [eye[0], eye[1] - 0.5, eye[2]],
                crate::Tether::Post(p) => [
                    f64::from(p[0]) + 0.5,
                    f64::from(p[1]) + 0.75,
                    f64::from(p[2]) + 0.5,
                ],
            };
            Some(WorldLine::new(
                top,
                end,
                LEAD_COLOUR,
                2.0,
                WorldDepth::Tested,
            ))
        })
        .collect();
    if !lines.is_empty() {
        let _ = event.draw.lines(&lines);
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

/// A hint, centred near the top of the screen, wrapped to fit it; `row` is the first line's, and
/// the next free row comes back.
fn hint(host: &dyn ClientHost, draw: &mut dyn DrawContext, text: Text, row: f32) -> f32 {
    let Ok(text) = host.text_string(&text) else {
        return row;
    };
    let [w, _] = draw.size();
    let room = (w - 16.0).max(40.0);
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if draw.text_width(&format!("{line} {word}")) <= room => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    let mut row = row;
    for line in lines {
        let width = draw.text_width(&line);
        let y = 4.0 + row * 12.0;
        draw.fill([(w - width) / 2.0 - 3.0, y, width + 6.0, 11.0], 0x90000000);
        draw.text(&line, [(w - width) / 2.0, y + 1.0], 0xFFFFFFFF, true);
        row += 1.0;
    }
    row
}

/// The release key's name as Controls shows it.
fn release_key(host: &dyn ClientHost) -> Text {
    match host.mod_key(crate::MOD_ID, "release") {
        Some((key, _)) => Text::translate(key, vec![]),
        None => Text::literal(""),
    }
}

/// Whether the player wants the hints (the `hints` setting on the mod's page in Options > Mods).
fn hints_on(host: &dyn ClientHost) -> bool {
    host.setting(crate::MOD_ID, crate::HINTS_SETTING).as_deref() != Some("false")
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
    if !hints_on(host) {
        return;
    }
    let mut row = 0.0;
    if state.captured
        && let Some(focus) = state.focus
    {
        row = hint(
            host,
            draw,
            Text::translate(
                "wm_world.hud.typing",
                vec![Text::literal(title(focus)), release_key(host)],
            ),
            row,
        );
    } else if state.carry.is_some() {
        row = hint(
            host,
            draw,
            Text::translate("wm_world.hud.carrying", vec![]),
            row,
        );
    } else if state.drag.is_some() {
        row = hint(
            host,
            draw,
            Text::translate("wm_world.hud.resizing", vec![]),
            row,
        );
    } else if let Some(gaze) = state.gaze
        && !matches!(gaze.part, Part::Screen(_))
        && gaze.distance <= HAND_REACH
    {
        row = hint(
            host,
            draw,
            Text::translate("wm_world.hud.frame", vec![]),
            row,
        );
    }
    // Windows the recall key brings back (the ones that became items come back by their item).
    let recallable = state
        .pocket
        .iter()
        .filter(|w| !state.itemised.contains(w))
        .count();
    if recallable > 0 {
        hint(
            host,
            draw,
            Text::translate(
                "wm_world.hud.pocket",
                vec![Text::literal(recallable.to_string())],
            ),
            row,
        );
    }
}
