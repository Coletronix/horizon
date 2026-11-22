//! A debug presenter: every window stood up in the world as a panel, side by side, three blocks
//! north of where the camera first was, facing south. It takes no input; it shows that windows
//! draw in the world. Turned on by `HORIZON_WM_WORLD=1`.
use crate::{Presenter, Window, Wm};
use mod_api::{PipelineQuad, RenderLevelStage, RenderLevelStageEvent, WorldDepth};
use std::sync::OnceLock;

/// Window pixels per block.
const PIXELS_PER_BLOCK: f64 = 256.0;

pub(crate) fn enabled() -> bool {
    std::env::var("HORIZON_WM_WORLD").is_ok_and(|v| v == "1")
}

#[derive(Default)]
pub(crate) struct Panels {
    /// The top left corner of the first panel, fixed at the first frame.
    anchor: OnceLock<[f64; 3]>,
}

impl Presenter for Panels {
    fn draw_world(&self, _wm: &Wm, windows: &[Window], event: &mut RenderLevelStageEvent<'_>) {
        if event.stage != RenderLevelStage::AfterTranslucentBlocks {
            return;
        }
        let camera = event.view.camera;
        let [ax, ay, az] = *self
            .anchor
            .get_or_init(|| [camera[0] - 1.0, camera[1] + 2.0, camera[2] - 3.0]);
        let mut x = ax;
        for window in windows.iter().filter(|w| w.mapped()) {
            for layer in &window.layers {
                let [lx, ly] = layer.at.map(|v| v as f64 / PIXELS_PER_BLOCK);
                let [lw, lh] = layer.size.map(|v| v as f64 / PIXELS_PER_BLOCK);
                let (x0, x1) = (x + lx, x + lx + lw);
                let (y0, y1) = (ay - ly, ay - ly - lh);
                // Popups a hair in front, so they don't fight the window for depth.
                let z = az + if layer.popup { 0.002 } else { 0.0 };
                let quad = PipelineQuad::new(
                    [[x0, y0, z], [x0, y1, z], [x1, y1, z], [x1, y0, z]],
                    [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
                    0xFFFFFFFF,
                );
                let _ = event
                    .draw
                    .live_quads(&layer.texture, &[quad], WorldDepth::Tested);
            }
            x += window.size[0] as f64 / PIXELS_PER_BLOCK + 0.25;
        }
    }
}
