//! Where windows stand, kept beside the session (in `$XDG_RUNTIME_DIR`) so a game that restarts
//! puts them back. A file from another run of the session names other windows: it is ignored.
use crate::space::{Edges, Pose};
use crate::{Placed, State};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use wayland_wm::WindowId;

#[derive(Serialize, Deserialize)]
struct Stored {
    instance: u64,
    windows: Vec<StoredWindow>,
    pocket: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
struct StoredWindow {
    id: u64,
    centre: [f64; 3],
    yaw: f32,
    pitch: f32,
    pixel: f64,
    size: [i32; 2],
    pocketed: bool,
    #[serde(default)]
    world: String,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("horizon-wm-world.json"))
}

pub fn save(state: &State) {
    let Some(path) = path() else { return };
    let stored = Stored {
        instance: state.instance,
        windows: state
            .placed
            .iter()
            .map(|p| StoredWindow {
                id: p.window.0,
                centre: p.pose.centre,
                yaw: p.pose.yaw,
                pitch: p.pose.pitch,
                pixel: p.pixel,
                size: p.size,
                pocketed: p.pocketed,
                world: p.world.clone(),
            })
            .collect(),
        pocket: state.pocket.iter().map(|w| w.0).collect(),
    };
    let Ok(json) = serde_json::to_vec(&stored) else {
        return;
    };
    // Written whole, then renamed: a crash mid-write leaves the last good file.
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, json).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

/// The placements kept for session run `instance`, if any.
pub fn load(instance: u64) -> Option<State> {
    let bytes = std::fs::read(path()?).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    if stored.instance != instance {
        return None;
    }
    Some(State {
        placed: stored
            .windows
            .into_iter()
            .map(|w| Placed {
                window: WindowId(w.id),
                pose: Pose {
                    centre: w.centre,
                    yaw: w.yaw,
                    pitch: w.pitch,
                },
                pixel: w.pixel,
                size: w.size,
                pocketed: w.pocketed,
                world: w.world,
                anchor: Edges {
                    top: true,
                    ..Edges::default()
                },
            })
            .collect(),
        pocket: stored.pocket.into_iter().map(WindowId).collect(),
        ..State::default()
    })
}
