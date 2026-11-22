//! The private protocol between `horizon-session` and the game.
//!
//! `horizon-session` is the Wayland compositor: it owns the socket programs connect to, their
//! surfaces and their buffers, so the programs outlive the game (a crash, a restart, a world
//! change). The game connects to the session's control socket ([`SESSION_ENV`]) and gets every
//! window's surfaces: shared-memory pixels inline, GPU buffers (dmabufs) as file descriptors. It
//! sends back the player's input and when it has shown a frame.
//!
//! A message is a frame on a Unix stream socket: three little-endian `u32`s (header length,
//! payload length, number of file descriptors), the header (a [`ToGame`] or [`ToSession`] in
//! postcard), then the payload (raw pixels, kept out of postcard). File descriptors ride with the
//! frame's first bytes (`SCM_RIGHTS`).
#![deny(unsafe_code)]

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};

/// Bumped when a message changes shape; both ends refuse a peer with another version.
pub const VERSION: u32 = 1;

/// The variable the session sets for the game it starts: its control socket's path.
pub const SESSION_ENV: &str = "HORIZON_SESSION";

/// A window's identity, unique while the session runs (it outlives the game's restarts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// A surface's identity, unique while the session runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SurfaceId(pub u64);

/// Mouse buttons, by their Linux input codes (`BTN_LEFT`...).
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
    pub const SIDE: u32 = 0x113;
    pub const EXTRA: u32 = 0x114;
}

/// The player's input and the window management the game decides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Input {
    /// The pointer is over `window` at `at` (logical pixels of its space).
    PointerMotion { window: WindowId, at: [f64; 2] },
    /// The pointer left every window.
    PointerLeave,
    /// A button (a [`button`] code) went down or up where the pointer is.
    PointerButton { button: u32, pressed: bool },
    /// Scrolling where the pointer is, in logical pixels (`discrete` in wheel steps, if a wheel).
    PointerAxis {
        horizontal: f64,
        vertical: f64,
        discrete: Option<[i32; 2]>,
    },
    /// A key went down or up, by its Linux input code (`KEY_A` is 30).
    Key { code: u32, pressed: bool },
    /// Give `window` the keyboard, or no window.
    Focus(Option<WindowId>),
    /// Ask `window` to take this size in logical pixels (0 lets it choose).
    Resize { window: WindowId, size: [i32; 2] },
    /// Ask `window` to close (its program may ask to save first).
    Close(WindowId),
    /// Tell `window` whether anyone can see it: a hidden window is told it's suspended and gets
    /// no frame callbacks, so its program stops drawing.
    Visible { window: WindowId, visible: bool },
}

/// How a shared-memory buffer's pixels are laid out, 4 bytes each, as Wayland names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// `ARGB8888`: B, G, R, A in memory, premultiplied.
    Bgra8,
    /// `XRGB8888`: B, G, R, ignored.
    Bgrx8,
    /// `ABGR8888`: R, G, B, A in memory, premultiplied.
    Rgba8,
    /// `XBGR8888`: R, G, B, ignored.
    Rgbx8,
}

/// A rectangle of buffer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One surface of a window, in the window's space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerInfo {
    pub surface: SurfaceId,
    /// Its top left corner, logical pixels.
    pub at: [i32; 2],
    /// Its size, logical pixels.
    pub size: [i32; 2],
    /// A popup (menu, tooltip) rather than part of the window.
    pub popup: bool,
}

/// A window as it was at its last commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub app_id: String,
    /// Its visible size in logical pixels.
    pub size: [i32; 2],
    pub focused: bool,
    /// The window this one belongs to (a dialog's main window).
    pub parent: Option<WindowId>,
    /// Its surfaces, bottom first.
    pub layers: Vec<LayerInfo>,
}

/// A dmabuf layout: a DRM fourcc and modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

/// What the game's GPU imports: its DRM device and the layouts it samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufCaps {
    pub device: u64,
    pub formats: Vec<DmabufFormat>,
}

/// What the game tells the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToSession {
    /// The first message: the game's protocol version and the dmabufs its GPU imports.
    Hello {
        version: u32,
        caps: Option<DmabufCaps>,
    },
    Input(Input),
    /// The game showed a frame: windows may draw their next.
    FrameDone,
    /// The game's GPU has read `surface`'s dmabuf number `seq` (and every one before it): the
    /// session may give it back to its program.
    Consumed {
        surface: SurfaceId,
        seq: u64,
    },
    /// Start a shell command as a program of this session.
    Spawn {
        command: String,
    },
}

/// What the session tells the game.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToGame {
    /// The answer to [`ToSession::Hello`]: the session's version and its `WAYLAND_DISPLAY`.
    Hello { version: u32, socket: String },
    /// New shared-memory pixels for `surface`, a `width` × `height` buffer in `format`. The payload
    /// holds each region's rows in turn, `region.width * 4` bytes each; no regions means the whole
    /// buffer, `width * 4` bytes a row.
    Pixels {
        surface: SurfaceId,
        width: u32,
        height: u32,
        format: PixelFormat,
        regions: Vec<Region>,
    },
    /// A GPU buffer for `surface`, its one plane's file descriptor attached. `buffer` names the
    /// buffer while it lives (programs cycle through a few); `seq` counts this surface's buffers,
    /// for [`ToSession::Consumed`].
    Dmabuf {
        surface: SurfaceId,
        buffer: u64,
        seq: u64,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
        y_inverted: bool,
    },
    /// `surface` is gone: its texture can be freed.
    Forget { surface: SurfaceId },
    /// Every window now, in the order they were opened.
    Windows(Vec<WindowInfo>),
}

const PREFIX: usize = 12;
/// The most file descriptors one message carries.
pub const MAX_FDS: usize = 4;
/// The most bytes one message may claim: more is a broken peer.
const MAX_MESSAGE: usize = 256 << 20;

/// A message received: its header, its payload and its file descriptors.
#[cfg(unix)]
pub struct Received<M> {
    pub message: M,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Encode one frame (without its file descriptors).
pub fn encode<M: Serialize>(message: &M, payload: &[u8], fds: usize) -> io::Result<Vec<u8>> {
    let header = postcard::to_allocvec(message).map_err(invalid)?;
    let mut frame = Vec::with_capacity(PREFIX + header.len() + payload.len());
    frame.extend_from_slice(&(header.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(fds as u32).to_le_bytes());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Frames read from a socket, with the file descriptors that came with them.
#[cfg(unix)]
#[derive(Default)]
pub struct Reader {
    buffer: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

#[cfg(unix)]
impl Reader {
    /// Read what the socket has (blocking if it blocks). `Ok(0)` is the end of the stream.
    pub fn fill(&mut self, socket: impl AsFd) -> io::Result<usize> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
        use std::mem::MaybeUninit;
        let mut chunk = vec![0u8; 256 << 10];
        let mut space = vec![MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(16))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let received = recvmsg(
            &socket,
            &mut [io::IoSliceMut::new(&mut chunk)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        )?;
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                self.fds.extend(fds);
            }
        }
        self.buffer.extend_from_slice(&chunk[..received.bytes]);
        Ok(received.bytes)
    }

    /// The next whole message, if it has arrived.
    pub fn next_message<M: DeserializeOwned>(&mut self) -> io::Result<Option<Received<M>>> {
        if self.buffer.len() < PREFIX {
            return Ok(None);
        }
        let word = |i: usize| {
            u32::from_le_bytes(self.buffer[i * 4..i * 4 + 4].try_into().expect("4 bytes")) as usize
        };
        let (header, payload, fds) = (word(0), word(1), word(2));
        if header + payload > MAX_MESSAGE || fds > MAX_FDS {
            return Err(invalid("a message larger than the protocol allows"));
        }
        if self.buffer.len() < PREFIX + header + payload {
            return Ok(None);
        }
        if self.fds.len() < fds {
            return Err(invalid("a message's file descriptors didn't arrive"));
        }
        let message =
            postcard::from_bytes(&self.buffer[PREFIX..PREFIX + header]).map_err(invalid)?;
        let payload_bytes = self.buffer[PREFIX + header..PREFIX + header + payload].to_vec();
        self.buffer.drain(..PREFIX + header + payload);
        Ok(Some(Received {
            message,
            payload: payload_bytes,
            fds: self.fds.drain(..fds).collect(),
        }))
    }
}

/// Frames waiting to be written, each with the file descriptors it carries.
#[cfg(unix)]
#[derive(Default)]
pub struct Writer {
    queue: VecDeque<(Vec<u8>, Vec<OwnedFd>, usize)>,
    queued: usize,
}

#[cfg(unix)]
impl Writer {
    /// Queue a message.
    pub fn push<M: Serialize>(
        &mut self,
        message: &M,
        payload: &[u8],
        fds: Vec<OwnedFd>,
    ) -> io::Result<()> {
        debug_assert!(fds.len() <= MAX_FDS);
        let frame = encode(message, payload, fds.len())?;
        self.queued += frame.len();
        self.queue.push_back((frame, fds, 0));
        Ok(())
    }

    /// Bytes queued and not yet written.
    pub fn queued(&self) -> usize {
        self.queued
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Write as much as the socket takes. On a non-blocking socket, stops at `WouldBlock` with
    /// `Ok`; on a blocking one, returns once everything is written.
    pub fn flush(&mut self, socket: impl AsFd) -> io::Result<()> {
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
        use std::mem::MaybeUninit;
        while let Some((frame, fds, sent)) = self.queue.front_mut() {
            let result = if *sent == 0 && !fds.is_empty() {
                let borrowed: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
                let mut space = vec![
                    MaybeUninit::<u8>::uninit();
                    rustix::cmsg_space!(ScmRights(borrowed.len()))
                ];
                let mut control = SendAncillaryBuffer::new(&mut space);
                control.push(SendAncillaryMessage::ScmRights(&borrowed));
                sendmsg(
                    &socket,
                    &[io::IoSlice::new(frame)],
                    &mut control,
                    SendFlags::NOSIGNAL,
                )
            } else {
                rustix::net::send(&socket, &frame[*sent..], SendFlags::NOSIGNAL)
            };
            match result {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    *sent += n;
                    self.queued -= n;
                    if *sent == frame.len() {
                        self.queue.pop_front();
                    }
                }
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn messages_and_fds_cross_in_order() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut writer = Writer::default();
        let (pipe_r, _pipe_w) = std::io::pipe().unwrap();
        writer
            .push(
                &ToGame::Hello {
                    version: VERSION,
                    socket: "wayland-9".into(),
                },
                &[],
                vec![],
            )
            .unwrap();
        let pixels = vec![7u8; 600_000];
        writer
            .push(
                &ToGame::Pixels {
                    surface: SurfaceId(3),
                    width: 1,
                    height: 1,
                    format: PixelFormat::Bgra8,
                    regions: vec![],
                },
                &pixels,
                vec![OwnedFd::from(pipe_r)],
            )
            .unwrap();
        let send = std::thread::spawn(move || writer.flush(&a).map(|()| a));
        let mut reader = Reader::default();
        let mut got = Vec::new();
        while got.len() < 2 {
            while let Some(message) = reader.next_message::<ToGame>().unwrap() {
                got.push(message);
            }
            if got.len() < 2 {
                assert!(reader.fill(&b).unwrap() > 0);
            }
        }
        send.join().unwrap().unwrap();
        assert!(matches!(&got[0].message, ToGame::Hello { socket, .. } if socket == "wayland-9"));
        assert!(got[0].fds.is_empty());
        assert_eq!(got[1].payload, pixels);
        assert_eq!(got[1].fds.len(), 1);
    }
}
