//! The game's connection to the session.
use horizon_wm_ipc::{Reader, Received, ToGame, ToSession, Writer};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

/// A game that stops reading is dropped once this much waits for it: the session must never
/// block on it, or every program would hang with it.
const BACKLOG_LIMIT: usize = 512 << 20;

pub struct GameConn {
    /// Tells this connection's socket events from an earlier one's.
    pub id: u64,
    stream: UnixStream,
    reader: Reader,
    writer: Writer,
    /// It said hello with our version: windows go to it.
    pub ready: bool,
    /// It fell too far behind, or its socket failed: drop it.
    pub broken: bool,
}

impl GameConn {
    pub fn new(id: u64, stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(GameConn {
            id,
            stream,
            reader: Reader::default(),
            writer: Writer::default(),
            ready: false,
            broken: false,
        })
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub fn send(&mut self, message: &ToGame, payload: &[u8], fds: Vec<OwnedFd>) {
        if self.broken {
            return;
        }
        if let Err(error) = self.writer.push(message, payload, fds) {
            log::warn!("a message for the game couldn't be encoded: {error}");
        }
        if self.writer.queued() > BACKLOG_LIMIT {
            log::warn!("the game stopped reading: dropping it");
            self.broken = true;
        }
    }

    /// Write what the socket takes now.
    pub fn flush(&mut self) {
        if self.broken || self.writer.is_empty() {
            return;
        }
        if let Err(error) = self.writer.flush(&self.stream) {
            log::warn!("the game's socket failed: {error}");
            self.broken = true;
        }
    }

    pub fn pending(&self) -> bool {
        !self.writer.is_empty()
    }

    /// Read what has arrived; the messages, or `Err` when the game is gone.
    pub fn read(&mut self) -> io::Result<Vec<Received<ToSession>>> {
        match self.reader.fill(&self.stream) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        let mut messages = Vec::new();
        while let Some(message) = self.reader.next_message()? {
            messages.push(message);
        }
        Ok(messages)
    }
}
