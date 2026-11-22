//! The game's end of the session: a connection to `horizon-session`, which owns the programs and
//! their windows. Surfaces arrive as pixels or dmabufs and go into [`LiveTexture`]s; the window list
//! arrives whole. Input and frames go back.
//!
//! Which session: `HORIZON_SESSION` names its control socket when a session started this game.
//! Without it the game looks at `$XDG_RUNTIME_DIR/horizon-session.sock` and, if nobody listens,
//! starts `horizon-session` there itself (it stays a while after the game is gone, so a game that
//! crashes and is started again finds its programs still open). When the session goes away the
//! game keeps trying to reach it again.
use crate::{Layer, LiveTexture, Window, Wm};
use horizon_wm_ipc::{
    DmabufCaps, DmabufFormat, PixelFormat, Reader, Region, SESSION_ENV, SurfaceId, ToGame,
    ToSession, VERSION, Writer,
};
use mod_api::{LiveDmabuf, LiveDmabufPlane, LivePixelFormat, LiveRegion};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a write to the session may block before the game gives up on that connection.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a session the game started stays without a game.
const LINGER_SECS: u32 = 60;

/// The game's link to the session.
#[derive(Default)]
pub(crate) struct Link {
    /// The connection's writing half, while connected.
    out: Mutex<Option<(UnixStream, Writer)>>,
    /// Dmabufs given to textures, not yet read by the renderer: `(surface, seq, texture, generation)`.
    pending: Mutex<Vec<(SurfaceId, u64, LiveTexture, u64)>>,
}

impl Link {
    pub(crate) fn send(&self, message: ToSession) {
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let Some((stream, writer)) = out.as_mut() else {
            return;
        };
        let result = writer
            .push(&message, &[], Vec::new())
            .and_then(|()| writer.flush(&*stream));
        if let Err(error) = result {
            log::warn!("wayland_wm: the session stopped listening: {error}");
            let _ = stream.shutdown(std::net::Shutdown::Both);
            *out = None;
        }
    }

    /// Tell the session which dmabufs the renderer has read, then that a frame was shown.
    pub(crate) fn frame_done(&self) {
        let consumed: Vec<(SurfaceId, u64)> = {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let mut done = Vec::new();
            pending.retain(|(surface, seq, texture, generation)| {
                let read = texture.consumed() >= *generation;
                if read {
                    done.push((*surface, *seq));
                }
                !read
            });
            done
        };
        for (surface, seq) in consumed {
            self.send(ToSession::Consumed { surface, seq });
        }
        self.send(ToSession::FrameDone);
    }
}

/// Where the session's control socket is, and whether a session started this game.
fn control_path() -> Option<(PathBuf, bool)> {
    if let Some(path) = std::env::var_os(SESSION_ENV) {
        return Some((path.into(), true));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some((PathBuf::from(dir).join("horizon-session.sock"), false))
}

/// Start the thread that keeps the game connected.
pub(crate) fn start(wm: &'static Wm) -> Result<(), String> {
    let (path, given) =
        control_path().ok_or("XDG_RUNTIME_DIR isn't set, and no session was given")?;
    std::thread::Builder::new()
        .name("Wayland session link".into())
        .spawn(move || {
            let mut started = false;
            loop {
                match UnixStream::connect(&path) {
                    Ok(stream) => {
                        started = false;
                        if let Err(error) = serve(wm, stream) {
                            log::warn!("wayland_wm: the session's connection ended: {error}");
                            // A session that refuses this game (another protocol) isn't asked
                            // again at once.
                            std::thread::sleep(Duration::from_secs(5));
                        }
                        wm.disconnected();
                    }
                    Err(_) if !given && !started => {
                        started = true;
                        start_session(&path);
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Start `horizon-session` on `path`, in its own process group so the game's terminal signals
/// don't end it with the game.
fn start_session(path: &PathBuf) {
    use std::os::unix::process::CommandExt;
    let beside = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("horizon-session"))
        .filter(|p| p.exists());
    let program = beside.unwrap_or_else(|| PathBuf::from("horizon-session"));
    match std::process::Command::new(&program)
        .arg("--control")
        .arg(path)
        .arg("--linger")
        .arg(LINGER_SECS.to_string())
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(child) => log::info!(
            "wayland_wm: started a session (pid {}) on {}",
            child.id(),
            path.display()
        ),
        Err(error) => log::warn!("wayland_wm: couldn't start {}: {error}", program.display()),
    }
}

/// One surface's texture, and its shared-memory pixels for damage to land in.
struct Surface {
    texture: LiveTexture,
    pixels: Vec<u8>,
}

/// Talk to the session over `stream` until it goes away.
fn serve(wm: &Wm, stream: UnixStream) -> std::io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let caps = mod_api::live_dmabuf_caps().map(|caps| DmabufCaps {
        device: caps.device,
        formats: caps
            .formats
            .iter()
            .map(|f| DmabufFormat {
                fourcc: f.fourcc,
                modifier: f.modifier,
            })
            .collect(),
    });
    let mut writer = Writer::default();
    writer.push(
        &ToSession::Hello {
            version: VERSION,
            caps,
        },
        &[],
        Vec::new(),
    )?;
    writer.flush(&stream)?;
    *wm.link.out.lock().unwrap_or_else(|p| p.into_inner()) = Some((stream.try_clone()?, writer));

    let mut reader = Reader::default();
    let mut surfaces: HashMap<SurfaceId, Surface> = HashMap::new();
    let connected = Instant::now();
    loop {
        if reader.fill(&stream)? == 0 {
            return Ok(());
        }
        while let Some(received) = reader.next_message::<ToGame>()? {
            match received.message {
                ToGame::Hello {
                    version,
                    socket,
                    instance,
                } => {
                    if version != VERSION {
                        return Err(std::io::Error::other(format!(
                            "the session speaks protocol {version}, the game {VERSION}"
                        )));
                    }
                    log::info!(
                        "wayland_wm: connected to the session in {:?}: WAYLAND_DISPLAY={socket}",
                        connected.elapsed()
                    );
                    wm.connected_to(socket, instance);
                }
                ToGame::Pixels {
                    surface,
                    width,
                    height,
                    format,
                    regions,
                } => {
                    let entry = surfaces.entry(surface).or_insert_with(|| Surface {
                        texture: LiveTexture::new(0, 0),
                        pixels: Vec::new(),
                    });
                    let row = width as usize * 4;
                    let format = match format {
                        PixelFormat::Bgra8 => LivePixelFormat::Bgra8,
                        PixelFormat::Bgrx8 => LivePixelFormat::Bgrx8,
                        PixelFormat::Rgba8 => LivePixelFormat::Rgba8,
                        PixelFormat::Rgbx8 => LivePixelFormat::Rgbx8,
                    };
                    if regions.is_empty() {
                        entry.pixels = received.payload;
                    } else if !patch(&mut entry.pixels, row, height, &regions, &received.payload) {
                        log::warn!("wayland_wm: damage for a surface whose pixels never came");
                        continue;
                    }
                    let damage: Vec<LiveRegion> = regions
                        .iter()
                        .map(|r| LiveRegion::new(r.x, r.y, r.width, r.height))
                        .collect();
                    entry
                        .texture
                        .write(width, height, row, format, &entry.pixels, &damage);
                }
                ToGame::Dmabuf {
                    surface,
                    buffer,
                    seq,
                    width,
                    height,
                    fourcc,
                    modifier,
                    offset,
                    stride,
                    y_inverted,
                } => {
                    let Some(fd) = received.fds.into_iter().next() else {
                        continue;
                    };
                    let entry = surfaces.entry(surface).or_insert_with(|| Surface {
                        texture: LiveTexture::new(0, 0),
                        pixels: Vec::new(),
                    });
                    entry.pixels = Vec::new();
                    let generation = entry.texture.set_dmabuf(LiveDmabuf {
                        id: buffer,
                        width,
                        height,
                        fourcc,
                        modifier,
                        planes: vec![LiveDmabufPlane { fd, offset, stride }],
                        y_inverted,
                    });
                    wm.link
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push((surface, seq, entry.texture.clone(), generation));
                }
                ToGame::Forget { surface } => {
                    surfaces.remove(&surface);
                    wm.link
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .retain(|(s, ..)| *s != surface);
                }
                ToGame::Clipboard { text } => wm.copied(text),
                ToGame::Windows(list) => {
                    let windows = list
                        .into_iter()
                        .map(|info| Window {
                            id: info.id,
                            title: info.title,
                            app_id: info.app_id,
                            size: info.size,
                            focused: info.focused,
                            parent: info.parent,
                            layers: info
                                .layers
                                .iter()
                                .filter_map(|layer| {
                                    Some(Layer {
                                        texture: surfaces.get(&layer.surface)?.texture.clone(),
                                        at: layer.at,
                                        size: layer.size,
                                        popup: layer.popup,
                                    })
                                })
                                .collect(),
                        })
                        .collect();
                    wm.publish(windows);
                }
            }
        }
    }
}

/// Copy damaged regions, packed row after row in `payload`, into `pixels` (rows of `row` bytes).
/// False when `pixels` isn't a whole picture of that size, or `payload` is short.
fn patch(pixels: &mut [u8], row: usize, height: u32, regions: &[Region], payload: &[u8]) -> bool {
    if pixels.len() != row * height as usize {
        return false;
    }
    let mut at = 0;
    for region in regions {
        let (x, w) = (region.x as usize * 4, region.width as usize * 4);
        if x + w > row || region.y + region.height > height {
            return false;
        }
        for y in region.y as usize..(region.y + region.height) as usize {
            let Some(src) = payload.get(at..at + w) else {
                return false;
            };
            pixels[y * row + x..y * row + x + w].copy_from_slice(src);
            at += w;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_lands_where_it_was_drawn() {
        // A 3x2 picture, 4 bytes a pixel; damage the middle column.
        let mut pixels = vec![0u8; 3 * 4 * 2];
        let region = Region {
            x: 1,
            y: 0,
            width: 1,
            height: 2,
        };
        let payload = [1, 1, 1, 1, 2, 2, 2, 2];
        assert!(patch(&mut pixels, 12, 2, &[region], &payload));
        assert_eq!(&pixels[4..8], &[1, 1, 1, 1]);
        assert_eq!(&pixels[16..20], &[2, 2, 2, 2]);
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
        // Damage outside the picture is refused.
        assert!(!patch(
            &mut pixels,
            12,
            2,
            &[Region {
                x: 2,
                y: 1,
                width: 2,
                height: 1
            }],
            &payload
        ));
    }
}
