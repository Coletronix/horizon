//! The session's processes: programs it starts, and the game it keeps running.
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// The environment that makes toolkits open their windows in this session.
pub fn session_command(command: &str, socket: &str, x_display: Option<u32>) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .env("WAYLAND_DISPLAY", socket)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        // X11 programs open in the session's XWayland, never on another desktop.
        .env_remove("DISPLAY");
    if let Some(display) = x_display {
        cmd.env("DISPLAY", format!(":{display}"));
    }
    cmd
}

/// Programs started here, reaped when they exit.
#[derive(Default)]
pub struct Programs {
    children: Vec<Child>,
}

impl Programs {
    pub fn spawn(&mut self, command: &str, socket: &str, x_display: Option<u32>) {
        match session_command(command, socket, x_display).spawn() {
            Ok(child) => {
                log::info!("started `{command}` (pid {})", child.id());
                self.children.push(child);
            }
            Err(error) => log::warn!("`{command}` didn't start: {error}"),
        }
    }

    pub fn reap(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

/// What the supervised game did.
pub enum GameExit {
    /// Still running, or being started again.
    Running,
    /// It quit on purpose: the session ends.
    Quit,
}

/// The game this session started, started again when it crashes, unless it keeps crashing.
pub struct Supervised {
    args: Vec<String>,
    control: String,
    /// More environment for the game: with a screen, the session's `WAYLAND_DISPLAY`.
    env: Vec<(String, String)>,
    child: Option<Child>,
    crashes: Vec<Instant>,
    restart_at: Option<Instant>,
}

/// Crashes within this window count towards giving up.
const CRASH_WINDOW: Duration = Duration::from_secs(120);
const MAX_CRASHES: usize = 4;

impl Supervised {
    pub fn new(args: Vec<String>, control: String, env: Vec<(String, String)>) -> Self {
        let mut game = Supervised {
            args,
            control,
            env,
            child: None,
            crashes: Vec::new(),
            restart_at: None,
        };
        game.start();
        game
    }

    fn start(&mut self) {
        let Some((program, rest)) = self.args.split_first() else {
            return;
        };
        match Command::new(program)
            .args(rest)
            .env(horizon_wm_ipc::SESSION_ENV, &self.control)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .spawn()
        {
            Ok(child) => {
                log::info!("started the game (pid {})", child.id());
                self.child = Some(child);
            }
            Err(error) => {
                log::error!("the game didn't start: {error}");
                self.crashed();
            }
        }
    }

    fn crashed(&mut self) {
        let now = Instant::now();
        self.crashes
            .retain(|t| now.duration_since(*t) < CRASH_WINDOW);
        self.crashes.push(now);
        if self.crashes.len() >= MAX_CRASHES {
            log::error!(
                "the game crashed {} times in {}s: not starting it again. Programs keep running; \
                 start the game yourself with {}=<this session's control socket>",
                self.crashes.len(),
                CRASH_WINDOW.as_secs(),
                horizon_wm_ipc::SESSION_ENV
            );
            self.restart_at = None;
        } else {
            // 1 s, 2 s, 4 s...
            let delay = Duration::from_secs(1 << (self.crashes.len() - 1));
            log::warn!("starting the game again in {}s", delay.as_secs());
            self.restart_at = Some(now + delay);
        }
    }

    /// Ask the game to quit (SIGTERM) as the session ends, and stop starting it again.
    pub fn stop(&mut self) {
        self.restart_at = None;
        if let Some(child) = &self.child
            && let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }

    pub fn poll(&mut self) -> GameExit {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    self.child = None;
                    if quit_on_purpose(status) {
                        log::info!("the game quit: ending the session");
                        return GameExit::Quit;
                    }
                    log::warn!("the game stopped: {status}");
                    self.crashed();
                }
                Err(error) => log::warn!("couldn't check on the game: {error}"),
            }
        }
        if self.restart_at.is_some_and(|t| Instant::now() >= t) {
            self.restart_at = None;
            self.start();
        }
        GameExit::Running
    }
}

fn quit_on_purpose(status: ExitStatus) -> bool {
    status.success()
}
