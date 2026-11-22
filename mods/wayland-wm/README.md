# wayland_wm: desktop windows in the game

The windows belong to [`horizon-session`](../../crates/horizon-session), the Wayland compositor
that owns the desktop's programs, so they outlive the game: a crash, a restart, another world. This
native mod connects the game to the session (starting one if none runs), and every surface of every
window becomes a `LiveTexture` the game's renderer draws.

```text
programs ──Wayland──▶ horizon-session ──control socket (pixels, dmabuf fds, window list)──▶ game + wayland_wm
                         ▲                                                                    │
                         └──────────────── input, frames shown, buffers read ◀────────────────┘
```

- **A session started the game** (`horizon-session -- GAME ARGS`): the game finds it in
  `HORIZON_SESSION`. The session starts the game again when it crashes (1 s, 2 s, 4 s; it stops
  after 4 crashes in 2 minutes) and ends when the game quits.
- **The game started first**: it looks for a session on `$XDG_RUNTIME_DIR/horizon-session.sock` and
  starts one there if nobody listens. That session stays 60 s after the game is gone, so a game that
  crashes and is started again finds its programs still open.
- **Nothing gets stuck on the game**: the session never waits for it. A game that stops reading is
  dropped; when it goes, every key and button it held is let go, and programs keep getting a frame
  a second.

This mod decides nothing about how windows look or how the player uses them. **Presenter mods**
do ([`wm_world`](../wm-world) stands them in the world as things the player looks at, clicks,
carries and puts away): they depend on `wayland_wm`, read the windows each frame, draw their layers where they like and
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
- **Clipboard**: call `Wm::player_input(host)` from your key and click hooks (the only moments
  the game lets a mod reach the clipboard). Text a program copies then goes on the desktop's
  clipboard, and the desktop's text is offered to programs when a window gets the keyboard.
- **Programs**: `Wm::spawn` asks the session to start a shell command with its `WAYLAND_DISPLAY`
  and the toolkit variables that make GTK, Qt, SDL, Firefox and Electron use it. The program
  belongs to the session.

## Performance

- GPU programs (Mesa EGL and Vulkan clients, GTK 4, Firefox, Chromium) are offered
  `zwp_linux_dmabuf` with the formats the game's GPU imports. Their buffers reach the renderer
  without a CPU copy: one GPU copy into the window's texture, after the client's GPU work on the
  buffer is done, and the buffer goes back to the client as soon as that copy has run.
- Shared-memory programs are copied on the CPU, only the regions they damaged, sent to the game over
  the control socket, and uploaded, only those regions again.
- Textures nobody draws aren't uploaded, and are freed on the GPU.

## Running it in this checkout

`wayland-wm-dev` is the game with this mod linked in, as the launcher's native build would link it:

```sh
cargo build --profile fast -p horizon-session -p wayland-wm-dev
# The session runs the game, and starts it again if it crashes:
HORIZON_WM_OVERLAY=1 target/fast/horizon-session --spawn "GSK_RENDERER=cairo zenity --info" -- \
  target/fast/wayland-wm-dev --resources .local/horizon-client/26.3 --world W
# Or the game alone: it starts a session beside its executable.
HORIZON_WM_OVERLAY=1 HORIZON_WM_SPAWN=foot \
  target/fast/wayland-wm-dev --resources .local/horizon-client/26.3 --world W
```

- `HORIZON_WM_OVERLAY=1`: a debug presenter drawing every window side by side on the HUD.
- `HORIZON_WM_WORLD=1`: a debug presenter standing every window up in the world, in front of where
  the camera started.
- `HORIZON_WM_SPAWN`: a program to start in the compositor when the game starts.

- **X11 programs** run through XWayland, which the session starts once a game has said which GPU
  buffers it imports (so X11 programs are accelerated too); `DISPLAY` points at it for programs
  the session starts. X11 menus and tooltips are popups of the window they belong to.

Not there yet: drag and drop, cursor images, popup grabs,
fractional scale, running as the login session itself (DRM/KMS, libinput, a fallback desktop).
