//! A debug presenter: every window stood up in the world as a panel, side by side, three blocks
//! in front of where the camera first looked, facing it. It takes no input; it shows that windows
//! draw in the world. Turned on by `HORIZON_WM_WORLD=1`.
use crate::{Presenter, Window, Wm};
use mod_api::{ClientHost, PipelineQuad, RenderLevelStage, RenderLevelStageEvent, WorldDepth};
use std::sync::OnceLock;

/// Window pixels per block.
const PIXELS_PER_BLOCK: f64 = 256.0;

pub(crate) fn enabled() -> bool {
    std::env::var("HORIZON_WM_WORLD").is_ok_and(|v| v == "1")
}

/// Where the panels stand: the top left corner of the first, and the direction to the right as
/// the camera saw them.
#[derive(Clone, Copy)]
struct Anchor {
    corner: [f64; 3],
    right: [f64; 3],
}

#[derive(Default)]
pub(crate) struct Panels {
    anchor: OnceLock<Anchor>,
}

impl Presenter for Panels {
    fn draw_world(
        &self,
        host: &dyn ClientHost,
        _wm: &Wm,
        windows: &[Window],
        event: &mut RenderLevelStageEvent<'_>,
    ) {
        if event.stage != RenderLevelStage::AfterTranslucentBlocks {
            return;
        }
        let Some(camera) = host.camera() else { return };
        let Anchor { corner, right } = *self.anchor.get_or_init(|| {
            // Minecraft's yaw 0 looks south (+z); right of that is west (-x).
            let yaw = f64::from(camera.yaw).to_radians();
            let forward = [-yaw.sin(), 0.0, yaw.cos()];
            let right = [-yaw.cos(), 0.0, -yaw.sin()];
            let eye = [
                camera.position[0],
                camera.position[1] + f64::from(camera.eye_height),
                camera.position[2],
            ];
            let width: f64 = windows
                .iter()
                .filter(|w| w.mapped())
                .map(|w| w.size[0] as f64 / PIXELS_PER_BLOCK + 0.25)
                .sum();
            let centre = [eye[0] + forward[0] * 3.0, eye[1] + 0.5, eye[2] + forward[2] * 3.0];
            let half = width.max(1.0) / 2.0;
            Anchor {
                corner: [centre[0] - right[0] * half, centre[1], centre[2] - right[2] * half],
                right,
            }
        });
        let at = |x: f64, y: f64| [corner[0] + right[0] * x, corner[1] - y, corner[2] + right[2] * x];
        let mut x = 0.0;
        for window in windows.iter().filter(|w| w.mapped()) {
            for layer in &window.layers {
                let [lx, ly] = layer.at.map(|v| v as f64 / PIXELS_PER_BLOCK);
                let [lw, lh] = layer.size.map(|v| v as f64 / PIXELS_PER_BLOCK);
                let (x0, x1, y0, y1) = (x + lx, x + lx + lw, ly, ly + lh);
                let quad = PipelineQuad::spanning(
                    [at(x0, y0), at(x0, y1), at(x1, y1), at(x1, y0)],
                    0xFFFFFFFF,
                );
                // Popups draw after the window, over it: the same plane, later in the stage.
                let _ = event
                    .draw
                    .live_quads(&layer.texture, &[quad], WorldDepth::Tested);
            }
            x += window.size[0] as f64 / PIXELS_PER_BLOCK + 0.25;
        }
    }
}
