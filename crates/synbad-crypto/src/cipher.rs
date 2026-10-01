//! Post-handshake AEAD framing.
//!
//! A [`CipherStream`] wraps a [`TcpStream`] and provides length-delimited,
//! ChaCha20-Poly1305 encrypted frames. Each direction has its own key and
//! nonce prefix; nonces are `nonce_prefix (4 B) || frame_counter (8 B BE)`.
//!
//! The counter is monotonically increasing on send and **strictly**
//! checked on receive — out-of-order or replayed frames decrypt to a
//! different AEAD tag, so tampering aborts the session at the next
//! [`recv`](CipherStream::recv) call.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// Per-frame ciphertext cap. Frames larger than this are rejected before
/// allocation, so a hostile peer can't OOM us with a 4 GiB length prefix.
/// Matches the existing application-layer cap in `synbadd::sync` so the
/// transport adds no new tighter bound the caller has to worry about.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame is {0} bytes (max {})", MAX_FRAME_BYTES)]
    Oversize(usize),
    #[error("AEAD decrypt failed (tampered or out-of-order)")]
    BadCiphertext,
    #[error("AEAD encrypt failed")]
    EncryptFailed,
    #[error("counter overflow — session has exhausted its nonce space")]
    NonceExhaustion,
}

pub struct CipherStream {
    stream: TcpStream,
    send_cipher: ChaCha20Poly1305,
    recv_cipher: ChaCha20Poly1305,
    send_prefix: [u8; 4],
    recv_prefix: [u8; 4],
    /// Per-direction monotonic counter mixed into the nonce. Bumped after
    /// each successful send/recv. ChaCha20-Poly1305's nonce is 12 bytes
    /// total — 4 from the prefix + 8 from this counter — so a session
    /// can safely emit `2^64` frames before exhausting nonce space (and
    /// we cap to a `u64` overflow check below in case of a runaway).
    send_counter: u64,
    recv_counter: u64,
    recv_frame: PendingFrame,
    /// The handshake transcript hash. Same on both peers; useful for
    /// higher-layer channel binding. Populated by the handshake code
    /// after constructing the stream — defaults to zero until set.
    pub(crate) transcript: [u8; 32],
}

impl CipherStream {
    /// Build a stream from a fresh pair of AEAD keys.
    pub(crate) fn new(
        stream: TcpStream,
        send_key: [u8; 32],
        recv_key: [u8; 32],
        send_prefix: [u8; 4],
        recv_prefix: [u8; 4],
    ) -> Self {
        CipherStream {
            stream,
            send_cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
            recv_cipher: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
            send_prefix,
            recv_prefix,
            send_counter: 0,
            recv_counter: 0,
            recv_frame: PendingFrame::default(),
            transcript: [0u8; 32],
        }
    }

    /// SHA-256 of the handshake transcript. Both peers see the same
    /// bytes; useful for higher-layer channel binding (e.g. tying the
    /// pairing SAS to the transport so a MITM that splices the TCP
    /// channel can't reuse SAS material across the two halves).
    pub fn transcript_hash(&self) -> [u8; 32] {
        self.transcript
    }

    /// Underlying TCP local address. Used by the audio bridge to
    /// figure out which interface IP to advertise as the host
    /// candidate for the WebRTC media socket — the same NIC that
    /// successfully terminated our signaling TCP is the natural one
    /// to use for the UDP media path.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.local_addr()
    }

    /// Underlying TCP peer address.
    pub fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    /// Encrypt and frame `payload`. Each call emits exactly one frame.
    /// Returns an error if the payload exceeds [`MAX_FRAME_BYTES`] or
    /// if the per-direction counter would overflow.
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), FrameError> {
        if payload.len() > MAX_FRAME_BYTES {
            return Err(FrameError::Oversize(payload.len()));
        }
        let nonce = build_nonce(&self.send_prefix, self.send_counter);
        // No associated data — the counter is implicit in the nonce
        // and we don't need to bind any other field to the AEAD tag.
        let ct = self
            .send_cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: payload,
                    aad: b"",
                },
            )
            .map_err(|_| FrameError::EncryptFailed)?;
        let len_be = (ct.len() as u32).to_be_bytes();
        self.stream.write_all(&len_be).await?;
        self.stream.write_all(&ct).await?;
        self.stream.flush().await?;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or(FrameError::NonceExhaustion)?;
        Ok(())
    }

    /// Read exactly one frame and decrypt it. Returns the plaintext.
    pub async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        let ct = self.recv_frame.read(&mut self.stream).await?;

        let nonce = build_nonce(&self.recv_prefix, self.recv_counter);
        let pt = self
            .recv_cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: b"" })
            .map_err(|_| FrameError::BadCiphertext)?;
        self.recv_counter = self
            .recv_counter
            .checked_add(1)
            .ok_or(FrameError::NonceExhaustion)?;
        Ok(pt)
    }
}

/// Keep progress outside the read future: audio races recv against UDP,
/// capture and timers, so read_exact with a stack-local buffer loses bytes.
#[derive(Default)]
struct PendingFrame {
    header: [u8; 4],
    header_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl PendingFrame {
    async fn read<R: AsyncRead + Unpin>(&mut self, stream: &mut R) -> Result<Vec<u8>, FrameError> {
        while self.header_read < 4 {
            let n = stream.read(&mut self.header[self.header_read..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.header_read += n;
        }
        if self.body.is_empty() {
            let len = u32::from_be_bytes(self.header) as usize;
            if !(16..=MAX_FRAME_BYTES + 16).contains(&len) {
                return Err(FrameError::Oversize(len));
            }
            self.body.resize(len, 0);
        }
        while self.body_read < self.body.len() {
            let n = stream.read(&mut self.body[self.body_read..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.body_read += n;
        }
        self.header_read = 0;
        self.body_read = 0;
        Ok(std::mem::take(&mut self.body))
    }
}

fn build_nonce(prefix: &[u8; 4], counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(prefix);
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

impl CipherStream {
    /// Split into independently-owned read and write halves.
    ///
    /// Useful when two tokio tasks need to read and write concurrently —
    /// [`Self::send`] and [`Self::recv`] both take `&mut self`, so a
    /// single owner can only do one at a time. The signaling protocol in
    /// `synbad-audio` needs a long-lived `recv` loop on the same channel
    /// it must write trickled ICE candidates into; splitting avoids
    /// cancelling a partial read mid-frame.
    pub fn split(self) -> (CipherReader, CipherWriter) {
        let (read_half, write_half) = self.stream.into_split();
        let reader = CipherReader {
            stream: read_half,
            cipher: self.recv_cipher,
            prefix: self.recv_prefix,
            counter: self.recv_counter,
            frame: self.recv_frame,
        };
        let writer = CipherWriter {
            stream: write_half,
            cipher: self.send_cipher,
            prefix: self.send_prefix,
            counter: self.send_counter,
        };
        (reader, writer)
    }
}

/// Read half of a split [`CipherStream`].
pub struct CipherReader {
    stream: OwnedReadHalf,
    frame: PendingFrame,
    cipher: ChaCha20Poly1305,
    prefix: [u8; 4],
    counter: u64,
}

impl CipherReader {
    /// Cancellation-safe: partial frame bytes survive a dropped recv future.
    /// Read and decrypt one frame. Same wire format as
    /// [`CipherStream::recv`].
    pub async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        let ct = self.frame.read(&mut self.stream).await?;

        let nonce = build_nonce(&self.prefix, self.counter);
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: b"" })
            .map_err(|_| FrameError::BadCiphertext)?;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(FrameError::NonceExhaustion)?;
        Ok(pt)
    }
}

/// Write half of a split [`CipherStream`].
pub struct CipherWriter {
    stream: OwnedWriteHalf,
    cipher: ChaCha20Poly1305,
    prefix: [u8; 4],
    counter: u64,
}

impl CipherWriter {
    /// Encrypt and frame `payload`. Same wire format as
    /// [`CipherStream::send`].
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), FrameError> {
        if payload.len() > MAX_FRAME_BYTES {
            return Err(FrameError::Oversize(payload.len()));
        }
        let nonce = build_nonce(&self.prefix, self.counter);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: payload,
                    aad: b"",
                },
            )
            .map_err(|_| FrameError::EncryptFailed)?;
        let len_be = (ct.len() as u32).to_be_bytes();
        self.stream.write_all(&len_be).await?;
        self.stream.write_all(&ct).await?;
        self.stream.flush().await?;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(FrameError::NonceExhaustion)?;
        Ok(())
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    async fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        (client.unwrap(), server.unwrap().0)
    }

    #[tokio::test]
    async fn cancelled_reads_preserve_partial_header_and_ciphertext() {
        let (receiver, mut sender) = sockets().await;
        let (mut reader, _writer) =
            CipherStream::new(receiver, [1; 32], [2; 32], [3; 4], [4; 4]).split();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&[2; 32]));
        let nonce = build_nonce(&[4; 4], 0);
        let body = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                b"fragmented signaling".as_slice(),
            )
            .unwrap();
        let header = (body.len() as u32).to_be_bytes();
        sender.write_all(&header[..2]).await.unwrap();
        assert!(timeout(Duration::from_millis(20), reader.recv())
            .await
            .is_err());
        sender.write_all(&header[2..]).await.unwrap();
        sender.write_all(&body[..7]).await.unwrap();
        assert!(timeout(Duration::from_millis(20), reader.recv())
            .await
            .is_err());
        sender.write_all(&body[7..]).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), reader.recv())
                .await
                .unwrap()
                .unwrap(),
            b"fragmented signaling"
        );
        assert_eq!(reader.counter, 1);
    }

    #[tokio::test]
    async fn maximum_payload_roundtrips_with_tag_overhead() {
        let (a, b) = sockets().await;
        let mut sender = CipherStream::new(a, [1; 32], [2; 32], [3; 4], [4; 4]);
        let mut receiver = CipherStream::new(b, [2; 32], [1; 32], [4; 4], [3; 4]);
        let payload = vec![7; MAX_FRAME_BYTES];
        let (sent, received) = tokio::join!(sender.send(&payload), receiver.recv());
        sent.unwrap();
        assert_eq!(received.unwrap(), payload);
    }

    #[tokio::test]
    async fn truncated_and_oversized_frames_fail_promptly() {
        let (a, mut b) = sockets().await;
        let mut receiver = CipherStream::new(a, [1; 32], [2; 32], [3; 4], [4; 4]);
        b.write_all(&32u32.to_be_bytes()).await.unwrap();
        b.write_all(&[0; 3]).await.unwrap();
        drop(b);
        assert!(
            matches!(receiver.recv().await, Err(FrameError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof)
        );
        let (a, mut b) = sockets().await;
        let (mut receiver, _) = CipherStream::new(a, [1; 32], [2; 32], [3; 4], [4; 4]).split();
        b.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(
            receiver.recv().await,
            Err(FrameError::Oversize(_))
        ));
        assert!(receiver.frame.body.is_empty());
    }
}
