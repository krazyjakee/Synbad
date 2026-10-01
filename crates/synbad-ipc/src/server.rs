//! Tokio-based IPC server, used by `synbadd`. Each accepted connection is
//! handled by an async task; broadcasts are delivered via a `tokio::sync::broadcast`
//! channel owned by the daemon.
//!
//! Backed by [`interprocess::local_socket::tokio`] so the same code path works
//! on Unix (filesystem sockets) and Windows (named pipes).

use std::path::{Path, PathBuf};
use std::time::Duration;

use interprocess::local_socket::{
    tokio::{prelude::*, Stream as IpcStream},
    GenericFilePath, ListenerOptions,
};
use serde_json as json;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};

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
/// On Unix, recovers a stale socket only after proving no listener owns it;
/// on Windows, `interprocess`
/// manages named-pipe lifecycle internally. The returned [`Listener`] yields
/// one [`IncomingRequest`] per protocol message — the caller drives request
/// handling and returns a [`Response`].
pub struct Listener {
    rx: mpsc::Receiver<IncomingRequest>,
    task: JoinHandle<()>,
    #[cfg(unix)]
    _bind_lock: std::fs::File,
}

impl Listener {
    pub async fn bind(
        socket_path: &Path,
        event_bus: broadcast::Sender<Event>,
    ) -> Result<Self, Error> {
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        #[cfg(unix)]
        let bind_lock = {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            let mut lock_path = socket_path.as_os_str().to_os_string();
            lock_path.push(".lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(lock_path)?;
            // Serialize stale-probe/unlink/bind across daemon launches. The
            // kernel releases ownership even if the daemon crashes.
            // SAFETY: file owns a valid descriptor; flock retains no pointer.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            file
        };

        let path_str = socket_path
            .to_str()
            .ok_or_else(|| Error::BadPath(socket_path.to_path_buf()))?;
        let name = path_str.to_fs_name::<GenericFilePath>()?;
        let listener = match ListenerOptions::new().name(name).create_tokio() {
            Ok(listener) => listener,
            #[cfg(unix)]
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Never unlink a live daemon's socket. Only recover a stale
                // endpoint after connect explicitly reports refusal.
                use std::os::unix::fs::FileTypeExt;
                if !tokio::fs::symlink_metadata(socket_path)
                    .await?
                    .file_type()
                    .is_socket()
                {
                    return Err(e.into());
                }
                let name = path_str.to_fs_name::<GenericFilePath>()?;
                match tokio::time::timeout(Duration::from_secs(1), IpcStream::connect(name)).await {
                    Ok(Err(probe)) if probe.kind() == std::io::ErrorKind::ConnectionRefused => {
                        tokio::fs::remove_file(socket_path).await?;
                        let name = path_str.to_fs_name::<GenericFilePath>()?;
                        ListenerOptions::new().name(name).create_tokio()?
                    }
                    _ => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        };

        let (tx, rx) = mpsc::channel::<IncomingRequest>(64);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept(), if connections.len() < 64 => {
                        match accepted {
                            Ok(stream) => { connections.spawn(handle_connection(stream, tx.clone(), event_bus.clone())); }
                            Err(e) => {
                                tracing::warn!(?e, "ipc accept failed");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(Listener {
            rx,
            task,
            #[cfg(unix)]
            _bind_lock: bind_lock,
        })
    }

    pub async fn next_request(&mut self) -> Option<IncomingRequest> {
        self.rx.recv().await
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const OP_TIMEOUT: Duration = Duration::from_secs(10);

async fn handle_connection(
    stream: IpcStream,
    tx: mpsc::Sender<IncomingRequest>,
    event_bus: broadcast::Sender<Event>,
) {
    let (read_half, mut writer) = stream.split();
    let mut reader = BufReader::new(read_half);
    let mut buffer = Vec::new();
    let mut subscription: Option<broadcast::Receiver<Event>> = None;
    loop {
        let is_subscribed = subscription.is_some();
        let line = tokio::select! {
            line = async {
                if !is_subscribed {
                    tokio::time::timeout(OP_TIMEOUT, read_message(&mut reader, &mut buffer)).await
                        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
                } else {
                    read_message(&mut reader, &mut buffer).await
                }
            } => match line {
                Ok(Some(line)) => line,
                _ => break,
            },
            event = async {
                match subscription.as_mut() {
                    Some(sub) => sub.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok(ev) => {
                        if send_message(&mut writer, &Message::Event(ev)).await.is_err() { break; }
                    }
                    // A lost event may be a state change. Close so clients
                    // reconnect and fetch complete snapshots, rather than
                    // silently keeping an incomplete view forever.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "ipc subscriber lagged; reconnect required");
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
                continue;
            }
        };
        let req = match json::from_slice::<Message>(&line) {
            Ok(Message::Request(req)) => req,
            other => {
                let response = Message::Response(Response::Error {
                    message: format!("expected valid request: {other:?}"),
                });
                if send_message(&mut writer, &response).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let is_subscribe = matches!(req, Request::Subscribe);
        // Subscribe before enqueuing the request so its events are buffered,
        // but don't write any event until the acknowledgement is flushed.
        let pending_subscription = is_subscribe.then(|| event_bus.subscribe());
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        let response = tokio::time::timeout(OP_TIMEOUT, async {
            tx.send(IncomingRequest {
                request: req,
                reply,
                subscribe: None,
            })
            .await
            .ok()?;
            reply_rx.await.ok()
        })
        .await;
        let Ok(Some(response)) = response else {
            break;
        };
        if send_message(&mut writer, &Message::Response(response))
            .await
            .is_err()
        {
            break;
        }
        if is_subscribe {
            subscription = pending_subscription;
        }
    }
}

// fill_buf is cancellation-safe. Progress lives in buffer because this
// future competes with broadcast events in select!.
async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>> {
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let n = end.map_or(bytes.len(), |i| i + 1);
        if buffer.len() + n > crate::MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC message too large",
            ));
        }
        buffer.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if end.is_some() {
            return Ok(Some(std::mem::take(buffer)));
        }
    }
}

async fn send_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> std::io::Result<()> {
    let mut line = json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    if line.len() > crate::MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC message too large",
        ));
    }
    tokio::time::timeout(OP_TIMEOUT, async {
        writer.write_all(&line).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Connection;

    fn socket_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "synbad-ipc-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        #[cfg(unix)]
        {
            std::env::temp_dir().join(name)
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"\\.\pipe\{name}"))
        }
    }

    #[tokio::test]
    async fn subscribe_ack_precedes_events_and_lag_forces_reconnect() {
        let path = socket_path();
        let (events, _) = broadcast::channel(2);
        let mut listener = Listener::bind(&path, events.clone()).await.unwrap();
        let client_path = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::connect(&client_path).unwrap();
            conn.send(Request::Subscribe).unwrap();
            // The burst deliberately occurs before the daemon replies.
            assert!(matches!(
                conn.recv().unwrap(),
                Message::Response(Response::Ok)
            ));
            // Lag closes the transport; it must not remain silently idle.
            assert!(conn.recv().is_err());
        });
        let request = listener.next_request().await.unwrap();
        for i in 0..10 {
            events
                .send(Event::Log {
                    line: i.to_string(),
                })
                .unwrap();
        }
        request.reply.send(Response::Ok).unwrap();
        tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        // Recovery on a fresh connection.
        let client_path = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::connect(&client_path).unwrap();
            assert!(matches!(
                conn.request(Request::GetStatus).unwrap(),
                Response::Ok
            ));
        });
        listener
            .next_request()
            .await
            .unwrap()
            .reply
            .send(Response::Ok)
            .unwrap();
        client.await.unwrap();
        drop(listener);
    }

    #[tokio::test]
    async fn dropping_listener_closes_active_subscribers() {
        let path = socket_path();
        let (events, _) = broadcast::channel(16);
        let mut listener = Listener::bind(&path, events).await.unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let client = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::connect(&path).unwrap();
            conn.send(Request::Subscribe).unwrap();
            assert!(matches!(
                conn.recv().unwrap(),
                Message::Response(Response::Ok)
            ));
            ready_tx.send(()).unwrap();
            assert!(conn.recv().is_err());
        });
        listener
            .next_request()
            .await
            .unwrap()
            .reply
            .send(Response::Ok)
            .unwrap();
        ready_rx.await.unwrap();
        drop(listener);
        tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_bind_preserves_live_socket_and_stale_socket_recovers() {
        let path = socket_path();
        let (events, _) = broadcast::channel(16);
        let mut listener = Listener::bind(&path, events.clone()).await.unwrap();
        assert!(Listener::bind(&path, events.clone()).await.is_err());
        let client_path = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::connect(&client_path).unwrap();
            assert!(matches!(
                conn.request(Request::GetStatus).unwrap(),
                Response::Ok
            ));
        });
        listener
            .next_request()
            .await
            .unwrap()
            .reply
            .send(Response::Ok)
            .unwrap();
        client.await.unwrap();
        drop(listener);
        tokio::task::yield_now().await;
        // A raw Unix listener leaves an actual stale filesystem entry.
        let _ = tokio::fs::remove_file(&path).await;
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let listener = Listener::bind(&path, events).await.unwrap();
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_recovery_preserves_regular_files_and_external_live_sockets() {
        let path = socket_path();
        let (events, _) = broadcast::channel(16);
        tokio::fs::write(&path, b"keep me").await.unwrap();
        assert!(Listener::bind(&path, events.clone()).await.is_err());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"keep me");
        tokio::fs::remove_file(&path).await.unwrap();
        let external = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(Listener::bind(&path, events).await.is_err());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        drop(external);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn simultaneous_stale_recovery_has_only_one_owner() {
        let path = socket_path();
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let (events, _) = broadcast::channel(16);
        let (a, b) = tokio::join!(
            Listener::bind(&path, events.clone()),
            Listener::bind(&path, events)
        );
        assert_ne!(
            a.is_ok(),
            b.is_ok(),
            "only one launch may claim the stale socket"
        );
        let mut listener = a.or(b).unwrap();
        let client = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::connect(&path).unwrap();
            assert!(matches!(
                conn.request(Request::GetStatus).unwrap(),
                Response::Ok
            ));
        });
        listener
            .next_request()
            .await
            .unwrap()
            .reply
            .send(Response::Ok)
            .unwrap();
        client.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_unterminated_message_is_rejected() {
        let (mut write, read) = tokio::io::duplex(4096);
        let sender = tokio::spawn(async move {
            let body = vec![b'x'; crate::MAX_MESSAGE_BYTES + 1];
            let _ = write.write_all(&body).await;
        });
        let mut reader = BufReader::new(read);
        let mut buffer = Vec::new();
        let error = read_message(&mut reader, &mut buffer).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(buffer.len() <= crate::MAX_MESSAGE_BYTES);
        sender.abort();
    }
}
