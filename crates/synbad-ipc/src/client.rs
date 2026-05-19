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

use interprocess::local_socket::{
    prelude::*, GenericFilePath, RecvHalf, SendHalf, Stream,
};
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
    sender: SendHalf,
    reader: BufReader<RecvHalf>,
}

impl Connection {
    pub fn connect(socket_path: &Path) -> Result<Self, Error> {
        let path_str = socket_path
            .to_str()
            .ok_or_else(|| Error::BadPath(socket_path.to_path_buf()))?;
        let name = path_str.to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name)?;
        let (recv, send) = stream.split();
        Ok(Connection {
            sender: send,
            reader: BufReader::new(recv),
        })
    }

    /// No-op kept for source compatibility with the previous client. The
    /// underlying transport is always blocking; we don't impose request
    /// timeouts at this layer.
    pub fn make_blocking(&mut self) -> Result<(), Error> {
        Ok(())
    }

    pub fn send(&mut self, req: Request) -> Result<(), Error> {
        let line = json::to_string(&Message::Request(req))?;
        self.sender.write_all(line.as_bytes())?;
        self.sender.write_all(b"\n")?;
        // interprocess streams treat flush as a no-op; call anyway for clarity.
        self.sender.flush()?;
        Ok(())
    }

    /// Read one [`Message`] from the connection.
    pub fn recv(&mut self) -> Result<Message, Error> {
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(Error::Closed);
        }
        let msg: Message = json::from_str(buf.trim_end())?;
        Ok(msg)
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
