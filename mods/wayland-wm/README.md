# wayland_wm: a Wayland compositor inside the game

A native mod that runs a Wayland server on its own thread. Linux programs started with its
`WAYLAND_DISPLAY` open their windows into the game, and every surface of every window becomes a
`LiveTexture` the game's renderer draws.

This mod decides nothing about how windows look or how the player uses them. **Presenter mods**
do: they depend on `wayland_wm`, read the windows each frame, draw their layers where they like and
send the player's input back. Several presenters can run at once.

## For presenter mods

```rust,ignore
use std::sync::Arc;
use wayland_wm::{Input, Presenter, Window, Wm, register_presenter};

struct OnABlock;

impl Presenter for OnABlock {
    fn draw_world(&self, wm: &Wm, windows: &[Window], event: &mut mod_api::RenderLevelStageEvent<'_>) {
        // For each window and each of its layers: event.draw.live_quads(&layer.texture, &quads, depth)
    }
    fn draw_gui(&self, wm: &Wm, windows: &[Window], draw: &mut dyn mod_api::DrawContext) {
        // draw.live_texture(&layer.texture, rect, [0.0, 0.0, 1.0, 1.0], 0xFFFFFFFF)
    }
}

// Once, from a client hook:
register_presenter(Arc::new(OnABlock));
// The player's input, in the window's own logical pixels:
if let Some(wm) = wayland_wm::wm() {
    wm.send(Input::PointerMotion { window, at: [x, y] });
    wm.send(Input::PointerButton { button: wayland_wm::button::LEFT, pressed: true });
    wm.send(Input::Key { code: raw_code, pressed: true }); // Linux input codes
}
```

- **Windows** (`Wm::windows`): a shared snapshot, cheap to take each frame. `Wm::generation` changes
  when a window opens, closes, is retitled or resized, or gains a surface. A new frame in an existing
  surface doesn't change it: that surface's texture has it.
- **Layers** (`Window::layers`): the window's surfaces bottom first (the main surface, subsurfaces,
  then popups), each with its place and size in the window's logical pixels. `(0, 0)` is the top
  left of the window's visible area; client-side shadows lie outside it.
- **Input** (`Input`): pointer motion over a window, buttons, scrolling, keys, keyboard focus,
  resize and close requests.
- **Programs**: `Wm::spawn` starts a shell command with this compositor's `WAYLAND_DISPLAY` and the
  toolkit variables that make GTK, Qt, SDL, Firefox and Electron use it.

## Performance

- GPU programs (Mesa EGL and Vulkan clients, GTK 4, Firefox, Chromium) are offered
  `zwp_linux_dmabuf` with the formats the game's GPU imports. Their buffers reach the renderer
  without a CPU copy: one GPU copy into the window's texture, after the client's GPU work on the
  buffer is done, and the buffer goes back to the client as soon as that copy has run.
- Shared-memory programs are copied on the CPU, only the regions they damaged, and uploaded, only
  those regions again.
- Textures nobody draws aren't uploaded, and are freed on the GPU.

## Running it in this checkout

`wayland-wm-dev` is the game with this mod linked in, as the launcher's native build would link it:

```sh
HORIZON_WM_OVERLAY=1 HORIZON_WM_SPAWN="GSK_RENDERER=cairo zenity --info" \
  cargo run --profile fast -p wayland-wm-dev -- --resources .local/horizon-client/26.3 --world W
```

- `HORIZON_WM_OVERLAY=1`: a debug presenter drawing every window side by side on the HUD.
- `HORIZON_WM_WORLD=1`: a debug presenter standing every window up in the world, in front of where
  the camera started.
- `HORIZON_WM_SPAWN`: a program to start in the compositor when the game starts.

Not there yet: XWayland (X11 programs), clipboard and drag and drop, cursor images, popup grabs,
fractional scale, the presenters' own input model.
