//! Blocking IPC client used by the GUI.
//!
//! Kept synchronous so the GUI crate doesn't pull in tokio. A background
//! thread in the GUI handles the long-lived subscription connection.
//!
//! Backed by [`interprocess::local_socket`] so the same code path works on
//! Unix (filesystem sockets) and Windows (named pipes). The endpoint path is
//! produced by [`synbad_config::paths::ipc_socket`] and interpreted as a
//! [`GenericFilePath`] name — that variant accepts both Unix socket paths and
//! `\\.\pipe\...` named-pipe paths.

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, ConnectOptions, GenericFilePath, Stream};
use interprocess::ConnectWaitMode;
use serde_json as json;

use crate::{Message, Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] json::Error),
    #[error("daemon: {0}")]
    Daemon(String),
    #[error("unexpected message: {0}")]
    Unexpected(String),
    #[error("connection closed")]
    Closed,
    #[error("ipc path is not valid UTF-8: {0:?}")]
    BadPath(std::path::PathBuf),
}

pub struct Connection {
    reader: BufReader<Stream>,
    timeout: Option<Duration>,
}

impl Connection {
    pub fn connect(socket_path: &Path) -> Result<Self, Error> {
        let path_str = socket_path
            .to_str()
            .ok_or_else(|| Error::BadPath(socket_path.to_path_buf()))?;
        let name = path_str.to_fs_name::<GenericFilePath>()?;
        let stream = ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(Duration::from_secs(1)))
            .nonblocking_stream(true)
            .connect_sync()?;
        Ok(Connection {
            reader: BufReader::new(stream),
            timeout: Some(Duration::from_secs(5)),
        })
    }

    /// Disable the receive deadline for an idle event subscription.
    pub fn make_blocking(&mut self) -> Result<(), Error> {
        self.reader.get_ref().set_nonblocking(false)?;
        self.timeout = None;
        Ok(())
    }

    fn wait(&self, deadline: Option<Instant>) -> Result<(), Error> {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC operation timed out").into());
        }
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }

    pub fn send(&mut self, req: Request) -> Result<(), Error> {
        let mut line = json::to_vec(&Message::Request(req))?;
        line.push(b'\n');
        if line.len() > crate::MAX_MESSAGE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC message too large").into());
        }
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let mut sent = 0;
        while sent < line.len() {
            match self.reader.get_ref().write(&line[sent..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(deadline)?,
                Err(e) => return Err(e.into()),
            }
            if deadline.is_some_and(|d| Instant::now() >= d) && sent < line.len() {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
        }
        Ok(())
    }

    /// Read one bounded message. A timeout invalidates the connection;
    /// callers must reconnect rather than reuse a partially read message.
    pub fn recv(&mut self) -> Result<Message, Error> {
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let mut buf = Vec::new();
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
            match self.reader.fill_buf() {
                Ok([]) => return Err(Error::Closed),
                Ok(bytes) => {
                    let end = bytes.iter().position(|b| *b == b'\n');
                    let n = end.map_or(bytes.len(), |i| i + 1);
                    if buf.len() + n > crate::MAX_MESSAGE_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "IPC message too large",
                        )
                        .into());
                    }
                    buf.extend_from_slice(&bytes[..n]);
                    self.reader.consume(n);
                    if end.is_some() {
                        return Ok(json::from_slice(&buf)?);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(deadline)?,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Send a request, then read one [`Response`]. Events arriving on this
    /// connection before the response are surfaced as
    /// `Error::Unexpected` — call [`Self::recv`] in a loop instead if you've
    /// subscribed.
    pub fn request(&mut self, req: Request) -> Result<Response, Error> {
        self.send(req)?;
        match self.recv()? {
            Message::Response(r) => match r {
                Response::Error { message } => Err(Error::Daemon(message)),
                other => Ok(other),
            },
            Message::Event(e) => Err(Error::Unexpected(format!("event before response: {:?}", e))),
            Message::Request(_) => Err(Error::Unexpected("request from server".into())),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stalled_daemon_cannot_block_a_command_indefinitely() {
        let path = std::env::temp_dir().join(format!("synbad-stall-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut conn = Connection::connect(&path).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        conn.timeout = Some(Duration::from_millis(50));
        let start = Instant::now();
        assert!(
            matches!(conn.request(Request::GetStatus), Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
    }
}
