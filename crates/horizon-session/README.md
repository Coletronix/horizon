# horizon-session: the desktop session

The Wayland compositor that owns the desktop's programs. The game connects to it and shows their
windows ([`wayland_wm`](../../mods/wayland-wm), [`wm_world`](../../mods/wm-world)); the programs
belong to the session, so the game can crash, restart or change worlds and nothing is lost.

## Three ways to run it

| How | What it is for |
|---|---|
| `horizon-session -- GAME ARGS` | Inside another desktop: the game is a window there; programs open in the game's world |
| `horizon-session --display window -- GAME ARGS` | Trying the real session inside another desktop: the session shows itself in a window, the game full screen inside it |
| `horizon-session --display drm -- GAME ARGS` | The real session, from a text console or a login manager: the session is the desktop |

The session starts the game and starts it again if it crashes (1 s, 2 s, 4 s; after 4 crashes in
2 minutes it stops trying), and ends when the game quits on purpose. Programs given with
`--spawn CMD` start once the game is up.

## Never stuck

With `--display`, whatever the game or a program does:

- **Ctrl+Alt+Backspace** switches to the **fallback desktop**: every window side by side on a plain
  background, used with the mouse and keyboard directly, and back to the game. It is also what the
  screen shows while the game is starting, has crashed, or isn't there.
- **Ctrl+Alt+F1 to F12** switch text consoles (`--display drm`).
- **`pkill -USR1 horizon-session`** (from another console, or over ssh) switches to the fallback
  desktop too; **`pkill horizon-session`** ends the session, asking programs to close first.

## Locking

Screen lockers that speak `ext-session-lock` (swaylock, hyprlock...) lock a session with a screen:
it then shows only the locker, and only the locker hears the keyboard and pointer (console
switching still works; the escape chord doesn't get past a lock). Not yet confirmed with a
locker: hyprlock connects but waited without locking in the nested test.

## Trying the real session

From a text console (Ctrl+Alt+F3, log in):

```sh
horizon-session --display drm --spawn foot -- \
  target/fast/wayland-wm-dev --resources .local/horizon-client/26.3 --world Desktop
```

As a login manager entry: copy `dist/horizon.desktop` to `/usr/share/wayland-sessions/` and make
`horizon-session-start` a script on the `PATH` that runs the line above.

`--display drm` has been built but not yet run on real hardware: the first try should be from a
text console you can switch away from.

## For tests

- `HORIZON_SESSION_LOG=debug|trace`: more log.
- `HORIZON_SESSION_SHOTS=DIR`: a file `DIR/shot.request` asks for the next frame of the session's
  own screen as `DIR/shot.png`.
