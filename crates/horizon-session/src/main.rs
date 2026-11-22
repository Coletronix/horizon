//! `horizon-session`: the Wayland compositor that owns the desktop's programs.
//!
//! Programs connect to its Wayland socket; the game connects to its control socket
//! ([`horizon_wm_ipc`]) and shows their windows. The programs belong to this process, not to the
//! game, so a game that crashes, restarts or changes worlds loses nothing.
//!
//! ```text
//! horizon-session [--control PATH] [--linger SECS] [--spawn CMD]... [-- GAME ARGS...]
//! ```
//!
//! - `-- GAME ARGS...`: start the game and keep it running: it gets the control socket in
//!   `HORIZON_SESSION`, and is started again if it crashes. The session ends when the game quits.
//! - `--linger SECS`: with no game of its own, end once no game has been connected this long (the
//!   game started the session itself and is gone).
//! - `--spawn CMD`: a program to start at once.
//! - `--control PATH`: the control socket (default `$XDG_RUNTIME_DIR/horizon-session.sock`).
//! - `--display window|drm`: the session shows itself: in a window on another desktop, or on the
//!   computer's screens from a text console. The game is then a program of the session, shown
//!   full screen; Ctrl+Alt+Backspace switches to a plain desktop of every window and back.
//!   Without it, the game shows the windows itself, wherever it runs.
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod compositor;
#[cfg(target_os = "linux")]
mod game;
mod logger;
#[cfg(target_os = "linux")]
mod programs;

/// Where the session shows itself, with `--display`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// A window on the desktop the session runs on.
    Window,
    /// The computer's screens, from a text console.
    Drm,
}

/// What the command line asked for.
#[derive(Debug, Default, PartialEq)]
pub struct Options {
    pub display: Option<Screen>,
    pub control: Option<PathBuf>,
    pub linger: Option<Duration>,
    pub spawn: Vec<String>,
    pub game: Vec<String>,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Options::default();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
            match arg.as_str() {
                "--control" => options.control = Some(value("--control")?.into()),
                "--linger" => {
                    let secs: u64 = value("--linger")?
                        .parse()
                        .map_err(|_| "--linger takes whole seconds".to_string())?;
                    options.linger = Some(Duration::from_secs(secs));
                }
                "--spawn" => options.spawn.push(value("--spawn")?),
                "--display" => {
                    options.display = Some(match value("--display")?.as_str() {
                        "window" => Screen::Window,
                        "drm" => Screen::Drm,
                        other => {
                            return Err(format!("--display takes window or drm, not {other:?}"));
                        }
                    })
                }
                "--" => {
                    options.game = args.collect();
                    break;
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(options)
    }

    /// The control socket's path.
    pub fn control_path(&self) -> Result<PathBuf, String> {
        match &self.control {
            Some(path) => Ok(path.clone()),
            None => std::env::var_os("XDG_RUNTIME_DIR")
                .map(|dir| PathBuf::from(dir).join("horizon-session.sock"))
                .ok_or_else(|| "XDG_RUNTIME_DIR isn't set; pass --control".to_string()),
        }
    }
}

fn main() -> ExitCode {
    logger::init();
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("horizon-session: {error}");
            return ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    match compositor::run(options) {
        Ok(code) => code,
        Err(error) => {
            log::error!("{error}");
            ExitCode::FAILURE
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = options;
        eprintln!("horizon-session: Wayland sessions need Linux");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_arguments_follow_the_separator() {
        let args = [
            "--linger", "30", "--spawn", "foot", "--", "game", "--linger", "x",
        ];
        let options = Options::parse(args.iter().map(|s| s.to_string())).unwrap();
        assert_eq!(options.linger, Some(Duration::from_secs(30)));
        assert_eq!(options.spawn, ["foot"]);
        assert_eq!(options.game, ["game", "--linger", "x"]);
    }
}
