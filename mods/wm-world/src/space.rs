//! Where windows are in the world, and what the view's ray hits on them.
//!
//! A window is a slab: its screen on the front face, a bezel around the screen, a back and four
//! sides. Its pose is its centre and the view (yaw, pitch, Minecraft's degrees) that sees the
//! screen straight on, so a window faces whoever stands where that view comes from.

pub type V3 = [f64; 3];

pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub fn mul(a: V3, k: f64) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}
pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Minecraft's look direction for `yaw` and `pitch` in degrees: yaw 0 looks south (+z), positive
/// pitch looks down.
pub fn forward(yaw: f32, pitch: f32) -> V3 {
    let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ]
}

/// Thickness of a window, in blocks: a painting's, so it is never a flat sheet.
pub const DEPTH: f64 = 2.0 / 16.0;
/// The bezel around the screen, in blocks: where the window is grabbed, mined and resized.
pub const BEZEL: f64 = 1.5 / 16.0;
/// Window pixels per block, at the scale windows open with.
pub const PIXELS_PER_BLOCK: f64 = 256.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub centre: V3,
    pub yaw: f32,
    pub pitch: f32,
}

/// A window's axes: `right` and `up` across its screen as its viewer sees it, `out` from the
/// screen towards the viewer.
#[derive(Clone, Copy, Debug)]
pub struct Basis {
    pub right: V3,
    pub up: V3,
    pub out: V3,
}

impl Pose {
    pub fn basis(&self) -> Basis {
        let f = forward(self.yaw, self.pitch);
        let yaw = f64::from(self.yaw).to_radians();
        let right = [-yaw.cos(), 0.0, -yaw.sin()];
        let up = cross(right, f);
        Basis {
            right,
            up,
            out: mul(f, -1.0),
        }
    }

    /// The world point at `x` right, `y` up and `z` out of the centre.
    pub fn at(&self, x: f64, y: f64, z: f64) -> V3 {
        let b = self.basis();
        add(
            self.centre,
            add(mul(b.right, x), add(mul(b.up, y), mul(b.out, z))),
        )
    }

    /// `p` in the window's axes.
    pub fn local(&self, p: V3) -> V3 {
        let b = self.basis();
        let d = sub(p, self.centre);
        [dot(d, b.right), dot(d, b.up), dot(d, b.out)]
    }

    fn local_dir(&self, d: V3) -> V3 {
        let b = self.basis();
        [dot(d, b.right), dot(d, b.up), dot(d, b.out)]
    }
}

/// Edges of the bezel a hit is near, for resizing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Edges {
    pub fn any(&self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// What part of a window a ray hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Part {
    /// The screen (or a popup over it), at this point in window pixels.
    Screen([f64; 2]),
    /// The bezel on the front, near these edges (none: its middle part, which can't be).
    Frame(Edges),
    /// The back or a side.
    Body,
}

/// A window's shape for hit testing: its screen size in blocks, and its layers' rectangles in
/// window pixels (popups may lie outside the screen).
pub struct Shape<'a> {
    pub size_px: [f64; 2],
    pub pixel: f64,
    pub layers: &'a [[f64; 4]],
}

impl Shape<'_> {
    pub fn half(&self) -> [f64; 2] {
        [
            self.size_px[0] * self.pixel / 2.0,
            self.size_px[1] * self.pixel / 2.0,
        ]
    }

    /// Window pixels for a point `x`, `y` on the front, in the window's axes.
    pub fn to_pixels(&self, x: f64, y: f64) -> [f64; 2] {
        let [hw, hh] = self.half();
        [(x + hw) / self.pixel, (hh - y) / self.pixel]
    }
}

/// The nearest hit of the ray from `origin` along `dir` (unit length) on a window: its distance
/// and part, or `None`.
pub fn hit(pose: &Pose, shape: &Shape, origin: V3, dir: V3) -> Option<(f64, Part)> {
    let o = pose.local(origin);
    let d = pose.local_dir(dir);
    let [hw, hh] = shape.half();
    let hz = DEPTH / 2.0;
    // The screen face first: popups reach past the box.
    let mut best: Option<(f64, Part)> = None;
    if d[2] < 0.0 && o[2] > hz {
        let t = (hz - o[2]) / d[2];
        let (x, y) = (o[0] + d[0] * t, o[1] + d[1] * t);
        let at = shape.to_pixels(x, y);
        let on_layer = shape
            .layers
            .iter()
            .any(|r| at[0] >= r[0] && at[0] < r[0] + r[2] && at[1] >= r[1] && at[1] < r[1] + r[3]);
        let on_screen = x.abs() < hw && y.abs() < hh;
        if on_layer || on_screen {
            best = Some((t, Part::Screen(at)));
        }
    }
    // Then the slab, bezel included.
    let ext = [hw + BEZEL, hh + BEZEL, hz];
    let (mut t0, mut t1, mut axis) = (f64::NEG_INFINITY, f64::INFINITY, 0usize);
    let mut sign = 0.0;
    for i in 0..3 {
        if d[i].abs() < 1e-12 {
            if o[i].abs() > ext[i] {
                return best;
            }
            continue;
        }
        let (a, b) = ((-ext[i] - o[i]) / d[i], (ext[i] - o[i]) / d[i]);
        let (near, far) = if a < b { (a, b) } else { (b, a) };
        if near > t0 {
            t0 = near;
            axis = i;
            sign = if d[i] < 0.0 { 1.0 } else { -1.0 };
        }
        t1 = t1.min(far);
    }
    if t0 > t1 || t0 < 0.0 {
        return best;
    }
    if best.is_some_and(|(t, _)| t <= t0 + 1e-9) {
        return best;
    }
    let p = [o[0] + d[0] * t0, o[1] + d[1] * t0];
    let part = if axis == 2 && sign > 0.0 {
        let edges = Edges {
            left: p[0] < -hw,
            right: p[0] > hw,
            top: p[1] > hh,
            bottom: p[1] < -hh,
        };
        if edges.any() {
            Part::Frame(edges)
        } else {
            // Inside the screen's outline but not on a layer: a window not yet drawn.
            Part::Screen(shape.to_pixels(p[0], p[1]))
        }
    } else {
        Part::Body
    };
    Some((t0, part))
}

/// Where the ray meets the plane of the window's front face, in the window's axes.
pub fn on_front(pose: &Pose, origin: V3, dir: V3) -> Option<[f64; 2]> {
    let o = pose.local(origin);
    let d = pose.local_dir(dir);
    if d[2].abs() < 1e-9 {
        return None;
    }
    let t = (DEPTH / 2.0 - o[2]) / d[2];
    (t > 0.0).then(|| [o[0] + d[0] * t, o[1] + d[1] * t])
}

/// How far `b` must move for two slabs to stop overlapping, and which way: the shortest way out
/// along either slab's axes. `None` when they don't overlap. `half` is each slab's half size along
/// its right, up and out axes.
pub fn separation(a: &Pose, a_half: V3, b: &Pose, b_half: V3) -> Option<V3> {
    let (ab, bb) = (a.basis(), b.basis());
    let a_axes = [ab.right, ab.up, ab.out];
    let b_axes = [bb.right, bb.up, bb.out];
    let d = sub(b.centre, a.centre);
    let mut best: Option<(f64, V3)> = None;
    for axis in a_axes.iter().chain(&b_axes) {
        let reach = |axes: &[V3; 3], half: V3| {
            (0..3)
                .map(|i| dot(axes[i], *axis).abs() * half[i])
                .sum::<f64>()
        };
        let along = dot(d, *axis);
        let overlap = reach(&a_axes, a_half) + reach(&b_axes, b_half) - along.abs();
        if overlap <= 0.0 {
            return None;
        }
        if best.is_none_or(|(o, _)| overlap < o) {
            let sign = if along < 0.0 { -1.0 } else { 1.0 };
            best = Some((overlap, mul(*axis, sign * overlap)));
        }
    }
    best.map(|(_, push)| push)
}

/// Which side of a window another is snapped to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Where a window of half size `half` (its bezel included) goes to stand against `side` of
/// `target` (half size `target_half`), tops aligned, turned `arc` degrees towards whoever faces
/// the row, with a small gap so their slabs don't touch.
pub fn snapped(target: &Pose, target_half: [f64; 2], half: [f64; 2], side: Side, arc: f32) -> Pose {
    const GAP: f64 = 0.04;
    let sign = match side {
        Side::Right => 1.0,
        Side::Left => -1.0,
    };
    let yaw = target.yaw + arc * sign as f32;
    let pose = Pose {
        centre: [0.0; 3],
        yaw,
        pitch: target.pitch,
    };
    // The shared edge, top-aligned: on the target's side, at its top less the new one's half.
    let edge = target.at(
        sign * (target_half[0] + GAP / 2.0),
        target_half[1] - half[1],
        0.0,
    );
    let right = pose.basis().right;
    Pose {
        centre: add(edge, mul(right, sign * (half[0] + GAP / 2.0))),
        ..pose
    }
}

/// `degrees` to the nearest multiple of `step`.
pub fn snap(degrees: f32, step: f32) -> f32 {
    (degrees / step).round() * step
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(layers: &[[f64; 4]]) -> Shape<'_> {
        Shape {
            size_px: [256.0, 128.0],
            pixel: 1.0 / 256.0,
            layers,
        }
    }

    /// A window two blocks south of the origin, facing a viewer at the origin looking south.
    fn ahead() -> Pose {
        Pose {
            centre: [0.0, 0.0, 2.0],
            yaw: 0.0,
            pitch: 0.0,
        }
    }

    #[test]
    fn a_look_at_the_screen_lands_on_its_pixels() {
        let layers = [[0.0, 0.0, 256.0, 128.0]];
        // Straight at the centre: the middle pixel.
        let (t, part) = hit(&ahead(), &shape(&layers), [0.0; 3], [0.0, 0.0, 1.0]).unwrap();
        assert!((t - (2.0 - DEPTH / 2.0)).abs() < 1e-9);
        assert_eq!(part, Part::Screen([128.0, 64.0]));
        // A quarter block to the viewer's right (west, -x) and up: right of centre, higher up.
        let dir = sub([-0.25, 0.125, 2.0 - DEPTH / 2.0], [0.0; 3]);
        let n = dot(dir, dir).sqrt();
        let (_, part) = hit(&ahead(), &shape(&layers), [0.0; 3], mul(dir, 1.0 / n)).unwrap();
        let Part::Screen([x, y]) = part else {
            panic!("{part:?}")
        };
        assert!(
            (x - 192.0).abs() < 1e-6 && (y - 32.0).abs() < 1e-6,
            "{x} {y}"
        );
    }

    #[test]
    fn the_bezel_and_the_back_are_not_the_screen() {
        let layers = [[0.0, 0.0, 256.0, 128.0]];
        // Just past the right edge of the screen, inside the bezel.
        let x = -(0.5 + BEZEL / 2.0);
        let (_, part) = hit(&ahead(), &shape(&layers), [x, 0.0, 0.0], [0.0, 0.0, 1.0]).unwrap();
        assert_eq!(
            part,
            Part::Frame(Edges {
                right: true,
                ..Edges::default()
            })
        );
        // From behind: the back.
        let (_, part) = hit(&ahead(), &shape(&layers), [0.0, 0.0, 4.0], [0.0, 0.0, -1.0]).unwrap();
        assert_eq!(part, Part::Body);
        // Past the bezel: nothing.
        assert!(hit(&ahead(), &shape(&layers), [-0.7, 0.0, 0.0], [0.0, 0.0, 1.0]).is_none());
    }

    #[test]
    fn overlapping_windows_are_pushed_apart_the_short_way() {
        let a = ahead();
        let half = [0.5, 0.25, DEPTH / 2.0];
        // Side by side, overlapping by a tenth of a block: b goes further along a's right (-x).
        let b = Pose {
            centre: [-0.9, 0.0, 2.0],
            ..a
        };
        let push = separation(&a, half, &b, half).unwrap();
        assert!(
            (push[0] + 0.1).abs() < 1e-9 && push[1].abs() < 1e-9,
            "{push:?}"
        );
        // Apart: nothing to do.
        let c = Pose {
            centre: [-1.2, 0.0, 2.0],
            ..a
        };
        assert!(separation(&a, half, &c, half).is_none());
        // One just in front of the other: out along the depth, the shortest way.
        let d = Pose {
            centre: [0.0, 0.0, 2.0 - DEPTH / 2.0],
            ..a
        };
        let push = separation(&a, half, &d, half).unwrap();
        assert!(push[2] < 0.0 && push[0].abs() < 1e-9, "{push:?}");
    }

    #[test]
    fn a_snapped_window_stands_against_the_side_tops_aligned() {
        let target = ahead();
        let pose = snapped(&target, [0.5, 0.25], [0.25, 0.125], Side::Right, 0.0);
        // Right of the target as its viewer sees it (-x), its left edge on the target's right
        // one with the gap between, tops level, in the same plane.
        assert!(
            (pose.centre[0] - -(0.5 + 0.04 + 0.25)).abs() < 1e-9,
            "{pose:?}"
        );
        assert!((pose.centre[1] - (0.25 - 0.125)).abs() < 1e-9);
        assert!((pose.centre[2] - 2.0).abs() < 1e-9);
        // Curved: turned towards the viewer, the shared edge staying put.
        let curved = snapped(&target, [0.5, 0.25], [0.25, 0.125], Side::Right, 20.0);
        assert_eq!(curved.yaw, 20.0);
        let edge = curved.at(-0.25 - 0.02, 0.0, 0.0);
        assert!(
            (edge[0] - -(0.5 + 0.02)).abs() < 1e-9 && (edge[2] - 2.0).abs() < 1e-9,
            "{edge:?}"
        );
        // Separate slabs: the gap keeps them from pushing each other.
        let half = |w: f64, h: f64| [w, h, DEPTH / 2.0];
        assert!(separation(&target, half(0.5, 0.25), &pose, half(0.25, 0.125)).is_none());
    }

    #[test]
    fn a_popup_past_the_screen_still_takes_the_pointer() {
        // A menu hanging 40 pixels below the window.
        let layers = [[0.0, 0.0, 256.0, 128.0], [10.0, 100.0, 80.0, 68.0]];
        let y = -(64.0 + 20.0) / 256.0;
        let (_, part) = hit(&ahead(), &shape(&layers), [0.4, y, 0.0], [0.0, 0.0, 1.0]).unwrap();
        assert!(matches!(part, Part::Screen([_, py]) if (py - 148.0).abs() < 1e-6));
    }
}
