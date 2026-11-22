//! Monitor blocks: a flat rectangle of them is a screen a window fills, as a wall of item frames
//! with maps is a map wall. Right-click one with a window item: the window stands on the wall's
//! face, sized to fit it, and stays there (as waxed).
use crate::space::{BEZEL, DEPTH, Pose};
use std::collections::BTreeSet;

pub const SCREEN_BLOCK: &str = "wm_world:screen";

/// The block declaration: a dark panel, broken like concrete.
pub fn declare(blocks: &mut Vec<mod_api::Block>) {
    blocks.push(mod_api::Block::new(
        SCREEN_BLOCK,
        "minecraft:black_concrete",
    ));
}

/// A wall of monitor blocks: its blocks' centre, its size in blocks across (right, up as its
/// viewer sees it), and the view that sees its face straight on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wall {
    pub centre: [f64; 3],
    pub size: [u32; 2],
    pub yaw: f32,
    pub pitch: f32,
}

/// The wall `start` belongs to on its `face` side: the monitor blocks connected to it in the
/// face's plane, if they make a full rectangle (at most 16 by 16). `is_screen` says whether a
/// block is a monitor block.
pub fn wall(start: [i32; 3], face: [i32; 3], is_screen: impl Fn([i32; 3]) -> bool) -> Option<Wall> {
    // The two axes across the face: (right, up) as a viewer facing the face sees them.
    let (right, up, yaw, pitch): ([i32; 3], [i32; 3], f32, f32) = match face {
        [0, 0, -1] => ([-1, 0, 0], [0, 1, 0], 0.0, 0.0),
        [0, 0, 1] => ([1, 0, 0], [0, 1, 0], 180.0, 0.0),
        [-1, 0, 0] => ([0, 0, 1], [0, 1, 0], -90.0, 0.0),
        [1, 0, 0] => ([0, 0, -1], [0, 1, 0], 90.0, 0.0),
        [0, 1, 0] => ([-1, 0, 0], [0, 0, 1], 0.0, 90.0),
        [0, -1, 0] => ([-1, 0, 0], [0, 0, -1], 0.0, -90.0),
        _ => return None,
    };
    let add =
        |p: [i32; 3], d: [i32; 3], k: i32| [p[0] + d[0] * k, p[1] + d[1] * k, p[2] + d[2] * k];
    // Flood fill in the plane.
    let mut seen = BTreeSet::new();
    let mut todo = vec![(0i32, 0i32)];
    while let Some((a, b)) = todo.pop() {
        if seen.len() > 256 || !seen.insert((a, b)) {
            continue;
        }
        let at = add(add(start, right, a), up, b);
        if !is_screen(at) {
            seen.remove(&(a, b));
            continue;
        }
        todo.extend([(a + 1, b), (a - 1, b), (a, b + 1), (a, b - 1)]);
    }
    let (a0, a1) = (
        seen.iter().map(|p| p.0).min()?,
        seen.iter().map(|p| p.0).max()?,
    );
    let (b0, b1) = (
        seen.iter().map(|p| p.1).min()?,
        seen.iter().map(|p| p.1).max()?,
    );
    let (w, h) = ((a1 - a0 + 1) as u32, (b1 - b0 + 1) as u32);
    if seen.len() as u32 != w * h || w > 16 || h > 16 {
        return None;
    }
    let mid = |lo: i32, hi: i32| f64::from(lo + hi) / 2.0;
    let (ma, mb) = (mid(a0, a1), mid(b0, b1));
    let centre = [
        f64::from(start[0]) + 0.5 + f64::from(right[0]) * ma + f64::from(up[0]) * mb,
        f64::from(start[1]) + 0.5 + f64::from(right[1]) * ma + f64::from(up[1]) * mb,
        f64::from(start[2]) + 0.5 + f64::from(right[2]) * ma + f64::from(up[2]) * mb,
    ];
    Some(Wall {
        centre,
        size: [w, h],
        yaw,
        pitch,
    })
}

impl Wall {
    /// The pose of a window standing on the wall's face, just off it.
    pub fn pose(&self, face: [i32; 3]) -> Pose {
        let off = 0.5 + DEPTH / 2.0 + 0.01;
        Pose {
            centre: [
                self.centre[0] + f64::from(face[0]) * off,
                self.centre[1] + f64::from(face[1]) * off,
                self.centre[2] + f64::from(face[2]) * off,
            ],
            yaw: self.yaw,
            pitch: self.pitch,
        }
    }

    /// The window size that fills the wall, bezel included, at `pixel` blocks a pixel.
    pub fn window_size(&self, pixel: f64) -> [i32; 2] {
        let px = |blocks: u32| ((f64::from(blocks) - 2.0 * BEZEL) / pixel).round() as i32;
        [px(self.size[0]), px(self.size[1])]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rectangle_of_monitors_is_a_wall_and_a_ragged_one_isnt() {
        // A 3 wide, 2 high wall in the plane z = 5, facing north (-z), from x = 0..2, y = 64..65.
        let blocks: BTreeSet<[i32; 3]> = (0..3)
            .flat_map(|x| (64..66).map(move |y| [x, y, 5]))
            .collect();
        let found = wall([1, 64, 5], [0, 0, -1], |p| blocks.contains(&p)).unwrap();
        assert_eq!(found.size, [3, 2]);
        assert_eq!(found.centre, [1.5, 65.0, 5.5]);
        assert_eq!(found.yaw, 0.0);
        // The window stands just north of the face.
        assert!(found.pose([0, 0, -1]).centre[2] < 5.0);
        // A missing corner: not a rectangle.
        let ragged: BTreeSet<[i32; 3]> = blocks
            .iter()
            .copied()
            .filter(|p| *p != [2, 65, 5])
            .collect();
        assert!(wall([0, 64, 5], [0, 0, -1], |p| ragged.contains(&p)).is_none());
    }
}
