//! tracing → in-app log bridge.
//!
//! Synbad's own `tracing::info!`/`warn!` (including everything from
//! `synbad-audio` and the sync/pairing modules) previously only went to
//! stderr. The in-app log view subscribes to `Event::Log`, which is only
//! fed by Core's stdout/stderr — so audio bridge messages, sync timeouts,
//! and similar Synbad-side errors were invisible to users debugging from
//! the GUI.
//!
//! This `MakeWriter` plugs into `tracing_subscriber::fmt::layer().with_writer(...)`
//! and forwards each formatted event line into the same `log_tx` channel
//! that Core's stdout reader uses, prefixed with `[synbad]` so the user
//! can tell daemon-side lines apart from Core output. The channel is
//! bounded and we `try_send` — if the GUI isn't draining (or the channel
//! is full) we silently drop the line rather than stalling the tracing
//! pipeline.
//!
//! Stderr logging is unaffected: it's a parallel fmt layer on the same
//! subscriber.

use std::io;

use tokio::sync::mpsc;

const LINE_PREFIX: &str = "[synbad] ";

/// Writer handed out per-event by [`ChannelMakeWriter`]. Owns a small
/// buffer because tracing-subscriber may issue several `write` calls for
/// one event, though in practice the fmt formatter currently writes the
/// whole event (terminated by `\n`) in a single call.
pub(crate) struct ChannelWriter {
    sender: mpsc::Sender<String>,
    buf: Vec<u8>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            self.buf.extend_from_slice(&buf[..pos]);
            // CRLF tolerance — fmt outputs LF on every platform, but the
            // trim keeps us honest if that ever changes.
            let line = String::from_utf8_lossy(&self.buf)
                .trim_end_matches('\r')
                .to_string();
            self.buf.clear();
            buf = &buf[pos + 1..];
            let _ = self.sender.try_send(format!("{LINE_PREFIX}{line}"));
        }
        self.buf.extend_from_slice(buf);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `MakeWriter` implementation. tracing's fmt layer calls `make_writer`
/// once per event; we hand back a fresh `ChannelWriter` cloning the
/// shared sender. The writer carries its own scratch buffer, so events
/// don't interleave across threads.
#[derive(Clone)]
pub(crate) struct ChannelMakeWriter {
    sender: mpsc::Sender<String>,
}

impl ChannelMakeWriter {
    pub(crate) fn new(sender: mpsc::Sender<String>) -> Self {
        Self { sender }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ChannelMakeWriter {
    type Writer = ChannelWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ChannelWriter {
            sender: self.sender.clone(),
            buf: Vec::new(),
        }
    }
}
