//! A debug presenter: every window drawn side by side on the HUD, scaled to fit. It takes no
//! input; it shows that windows reach the game. Turned on by `HORIZON_WM_OVERLAY=1`.
use crate::{Presenter, Window, Wm};
use mod_api::DrawContext;

pub(crate) fn enabled() -> bool {
    std::env::var("HORIZON_WM_OVERLAY").is_ok_and(|v| v == "1")
}

pub(crate) struct Overlay;

impl Presenter for Overlay {
    fn draw_gui(&self, _wm: &Wm, windows: &[Window], draw: &mut dyn DrawContext) {
        let shown: Vec<&Window> = windows.iter().filter(|w| w.mapped()).collect();
        if shown.is_empty() {
            return;
        }
        let [gui_w, gui_h] = draw.size();
        let margin = 8.0;
        let cell_w = (gui_w - margin) / shown.len() as f32 - margin;
        let cell_h = gui_h * 0.6;
        for (i, window) in shown.iter().enumerate() {
            let [w, h] = window.size.map(|v| v as f32);
            let scale = (cell_w / w).min(cell_h / h);
            let x0 = margin + i as f32 * (cell_w + margin);
            let y0 = margin + 10.0;
            let border = if window.focused { 0xFF55AAFF } else { 0xFF333333 };
            draw.fill([x0 - 1.0, y0 - 1.0, w * scale + 2.0, h * scale + 2.0], border);
            draw.text(&window.title, [x0, y0 - 10.0], 0xFFFFFFFF, true);
            for layer in &window.layers {
                let [lx, ly] = layer.at.map(|v| v as f32);
                let [lw, lh] = layer.size.map(|v| v as f32);
                draw.live_texture(
                    &layer.texture,
                    [x0 + lx * scale, y0 + ly * scale, lw * scale, lh * scale],
                    [0.0, 0.0, 1.0, 1.0],
                    0xFFFFFFFF,
                );
            }
        }
    }
}
