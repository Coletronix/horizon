# wm_world: desktop windows as things in the world

A presenter for [`wayland_wm`](../wayland-wm): every window of the desktop session stands in the
world as a slab, with its screen on the front, a bezel around it and a back. The player uses windows
with the game's own controls.

| Doing | How |
|---|---|
| Point | Look at a screen: the pointer is where the crosshair is |
| Click | Click a screen: the window gets the keyboard, and the click reaches it |
| Type | While the crosshair is on that window, every key is the window's, Escape included |
| Stop typing | Look away (keys are the game's again; look back and they're the window's), or press the release key (Super, rebindable in Controls), or hold Escape for a second |
| Scroll | The wheel over a screen scrolls it |
| Drag | Hold a button and look: past a window's edge the drag goes on along its plane; onto another window, it carries what you drag there (drag and drop between windows) |
| Carry | Right-click the bezel, back or a side; the wheel brings it nearer or pushes it away; right-click sets it down |
| Line up | Set a carried window down beside another: it snaps against its side, tops level, turned a little towards you so a row curves around you ("Curved rows" turns that off) |
| Herd | Right-click a window's bezel holding a lead: it follows you (any number of them, pushing each other apart as they go). Right-click a block with the lead to tie them there; right-click a window with the lead again to let it go |
| Lock in place | Right-click the bezel with honeycomb: the waxed window (honey-coloured bezel) can't be carried, resized or pushed; an axe scrapes the wax off |
| Resize | Sneak and right-click the bezel near an edge or corner, drag, let go |
| Put away | Hold attack on the bezel: the window leaves the world and its program is suspended |
| Bring back | The recall key (R) brings the last window put away in front of you; with none put away, the farthest window (one lost behind a wall or far away) |
| Hints | On the HUD at the top; turn them off with "Hints on the HUD" on the mod's page in Options > Mods |
| Open a terminal | The terminal key (`` ` ``): `HORIZON_WM_TERMINAL`, else the first of foot, alacritty, kitty, wezterm, konsole, gnome-terminal, xterm |

- **New windows** open in front of the player and face them, stepping aside from windows already
  there. A dialog opens just in front of its window.
- **Windows don't overlap**: two that touch push each other apart, a little each frame, the short
  way. The window being carried, resized or typed into stands firm and pushes the others along.
- **Pixel-true aim**: over the window being typed into, mouse sensitivity drops so that one mouse
  step moves about one of its pixels.
- **Sharp windows** (a setting): programs draw at twice the pixels, so text stays crisp up close.
- **Size**: 256 window pixels to a block. A window is 2/16 of a block thick, with a 1.5/16 bezel.
- **Kept across restarts**: where windows stand is written beside the session
  (`$XDG_RUNTIME_DIR/horizon-wm-world.json`), so a game that crashes and starts again puts them back.

## Engine hooks it relies on

- `key_capture` (added on this branch): a key before the game handles it, cancellable. It's how
  typing into a window keeps WASD, Escape and the hotbar keys from the game.
- `mouse_button_pre`, `mouse_scrolling`, `calculate_player_turn`, `render_level_stage`,
  `gui_render_post`, `render_frame_post`; the control socket's `mouse` now goes through
  `mouse_button_pre` as a window's click does.

## Not yet

Survival items (a picked-up window as an item, closing when the item is destroyed), app items and
the launcher block, monitor blocks, wax and item frames, leads, snapping and arcs, cursor images,
drag and drop, per-world placement.
