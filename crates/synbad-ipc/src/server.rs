//! Tokio-based IPC server, used by `synbadd`. Each accepted connection is
//! handled by an async task; broadcasts are delivered via a `tokio::sync::broadcast`
//! channel owned by the daemon.
//!
//! Backed by [`interprocess::local_socket::tokio`] so the same code path works
//! on Unix (filesystem sockets) and Windows (named pipes).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use interprocess::local_socket::{
    tokio::{prelude::*, SendHalf, Stream as IpcStream},
    GenericFilePath, ListenerOptions,
};
use serde_json as json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::{Event, Message, Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] json::Error),
    #[error("ipc path is not valid UTF-8: {0:?}")]
    BadPath(PathBuf),
}

/// A request from a connected client, plus a one-shot reply channel.
pub struct IncomingRequest {
    pub request: Request,
    pub reply: tokio::sync::oneshot::Sender<Response>,
    /// If the request is `Subscribe`, the handler should also retain this
    /// sender side; the server task forwards broadcast events through it.
    pub subscribe: Option<mpsc::Sender<Event>>,
}

/// Bind a local socket at `path`.
///
/// On Unix, removes any stale socket file first; on Windows, `interprocess`
/// manages named-pipe lifecycle internally. The returned [`Listener`] yields
/// one [`IncomingRequest`] per protocol message — the caller drives request
/// handling and returns a [`Response`].
pub struct Listener {
    rx: mpsc::Receiver<IncomingRequest>,
    _socket_path: PathBuf,
}

impl Listener {
    pub async fn bind(
        socket_path: &Path,
        event_bus: broadcast::Sender<Event>,
    ) -> Result<Self, Error> {
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        // On Unix, a stale socket file from a previous run causes bind() to
        // fail with EADDRINUSE; remove it first. On Windows the path is a
        // `\\.\pipe\...` namespace name with no on-disk artefact.
        #[cfg(unix)]
        {
            let _ = tokio::fs::remove_file(socket_path).await;
        }

        let path_str = socket_path
            .to_str()
            .ok_or_else(|| Error::BadPath(socket_path.to_path_buf()))?;
        let name = path_str.to_fs_name::<GenericFilePath>()?;
        let listener = ListenerOptions::new().name(name).create_tokio()?;

        let (tx, rx) = mpsc::channel::<IncomingRequest>(64);
        let owned_path = socket_path.to_path_buf();

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        let tx = tx.clone();
                        let bus = event_bus.clone();
                        tokio::spawn(handle_connection(stream, tx, bus));
                    }
                    Err(e) => {
                        tracing::warn!(?e, "ipc accept failed");
                    }
                }
            }
        });

        Ok(Listener {
            rx,
            _socket_path: owned_path,
        })
    }

    pub async fn next_request(&mut self) -> Option<IncomingRequest> {
        self.rx.recv().await
    }
}

async fn handle_connection(
    stream: IpcStream,
    tx: mpsc::Sender<IncomingRequest>,
    event_bus: broadcast::Sender<Event>,
) {
    let (read_half, write_half) = stream.split();
    let mut reader = BufReader::new(read_half).lines();
    let writer: Arc<Mutex<SendHalf>> = Arc::new(Mutex::new(write_half));

    // After Subscribe, this task forwards broadcast events onto the connection.
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;

    while let Ok(Some(line)) = reader.next_line().await {
        let msg: Message = match json::from_str(line.trim_end()) {
            Ok(m) => m,
            Err(e) => {
                let _ = send_response(
                    &writer,
                    Response::Error {
                        message: format!("malformed message: {}", e),
                    },
                )
                .await;
                continue;
            }
        };
        let req = match msg {
            Message::Request(r) => r,
            _ => {
                let _ = send_response(
                    &writer,
                    Response::Error {
                        message: "expected request".into(),
                    },
                )
                .await;
                continue;
            }
        };

        let is_subscribe = matches!(req, Request::Subscribe);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let subscribe = if is_subscribe {
            let (etx, mut erx) = mpsc::channel::<Event>(256);
            let writer2 = writer.clone();
            let bus = event_bus.clone();
            // Drop any previous forwarder if the client resubscribes.
            if let Some(h) = forwarder.take() {
                h.abort();
            }
            forwarder = Some(tokio::spawn(async move {
                // Bridge: broadcast bus -> per-connection channel.
                let bridge = tokio::spawn(async move {
                    let mut sub = bus.subscribe();
                    while let Ok(ev) = sub.recv().await {
                        if etx.send(ev).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(ev) = erx.recv().await {
                    let line = match json::to_string(&Message::Event(ev)) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let mut w = writer2.lock().await;
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    if w.write_all(b"\n").await.is_err() {
                        break;
                    }
                }
                bridge.abort();
            }));
            None
        } else {
            None
        };

        if tx
            .send(IncomingRequest {
                request: req,
                reply: reply_tx,
                subscribe,
            })
            .await
            .is_err()
        {
            // Daemon dropped the receiver.
            break;
        }

        let response = match reply_rx.await {
            Ok(r) => r,
            Err(_) => Response::Error {
                message: "daemon dropped request".into(),
            },
        };
        if send_response(&writer, response).await.is_err() {
            break;
        }
    }

    if let Some(h) = forwarder {
        h.abort();
    }
}

async fn send_response(writer: &Arc<Mutex<SendHalf>>, resp: Response) -> std::io::Result<()> {
    let line = json::to_string(&Message::Response(resp)).map_err(std::io::Error::other)?;
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}
