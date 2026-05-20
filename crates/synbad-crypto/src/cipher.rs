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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        let mut len_be = [0u8; 4];
        self.stream.read_exact(&mut len_be).await?;
        let ct_len = u32::from_be_bytes(len_be) as usize;
        // 16 bytes is the AEAD tag; a valid frame includes it, so the
        // minimum legal `ct_len` is 16. Smaller values can't be real
        // ciphertext; reject before reading.
        if !(16..=MAX_FRAME_BYTES).contains(&ct_len) {
            return Err(FrameError::Oversize(ct_len));
        }
        let mut ct = vec![0u8; ct_len];
        self.stream.read_exact(&mut ct).await?;

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
    cipher: ChaCha20Poly1305,
    prefix: [u8; 4],
    counter: u64,
}

impl CipherReader {
    /// Read and decrypt one frame. Same wire format as
    /// [`CipherStream::recv`].
    pub async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        let mut len_be = [0u8; 4];
        self.stream.read_exact(&mut len_be).await?;
        let ct_len = u32::from_be_bytes(len_be) as usize;
        if !(16..=MAX_FRAME_BYTES).contains(&ct_len) {
            return Err(FrameError::Oversize(ct_len));
        }
        let mut ct = vec![0u8; ct_len];
        self.stream.read_exact(&mut ct).await?;

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
