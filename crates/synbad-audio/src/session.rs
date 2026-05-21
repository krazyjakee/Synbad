//! Per-peer audio session state machine.
//!
//! One session per paired peer with audio enabled. The session owns:
//! - a [`str0m::Rtc`] driving WebRTC (DTLS-SRTP, ICE, RTP framing)
//! - a [`tokio::net::UdpSocket`] for the media path
//! - the local capture stream (cpal mic or loopback) feeding into the
//!   outbound Opus encoder
//! - the local playback stream draining inbound Opus frames
//! - the [`synbad_crypto::CipherStream`] (split into read + write halves)
//!   carrying SDP offer/answer signaling
//!
//! ## Roles
//!
//! Exactly one peer in a pair drives the SDP offer/answer. We pick by
//! TCP direction: the side that dials is the [`SessionRole::Offerer`]
//! and the side that accepts is the [`SessionRole::Answerer`]. The
//! supervisor enforces a glare rule (only dial when our `machine_id`
//! is lexicographically smaller than the peer's) so two peers never
//! both end up as offerer.
//!
//! ## Driver loop
//!
//! str0m is sans-I/O — it never touches the network or a clock on its
//! own, just transforms `Input` events into `Output` events. The
//! driver task is the I/O wrapper:
//!
//! 1. Initial handshake: do the SDP offer/answer over the cipher
//!    stream. str0m bakes ICE candidates into the SDP itself
//!    (`add_local_candidate` before `sdp_api().apply()`), so no
//!    separate trickle is needed.
//! 2. Main loop: `tokio::select!` over UDP recv, capture frame
//!    arrival, scheduled timeout, signal-stream read, and the close
//!    signal. After each input, drain `Rtc::poll_output` until it
//!    returns `Timeout`, dispatching `Transmit` to the UDP socket and
//!    `Event::MediaData` to the Opus decoder.
//!
//! ## Why str0m
//!
//! v0.1.4 used webrtc-rs 0.17, which we found derives mismatched
//! DTLS-SRTP keys in our two-PC scenario — every inbound packet
//! failed AEAD/HMAC authentication. The Phase 1 validation test
//! (`tests/str0m_validation.rs`) proved str0m completes the same
//! bidirectional bring-up cleanly. Phase 2 (this file plus
//! `rtc.rs` / `protocol.rs`) is the full port.

#![allow(clippy::result_large_err)] // AudioError carries Strings; size dominates.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use cpal::Stream;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::{Codec, PayloadParams};
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use synbad_config::AudioConfig;
use synbad_crypto::{CipherReader, CipherStream, CipherWriter};
use synbad_ipc::PeerAudioStatus;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::capture::{self, PcmFrame};
use crate::errors::AudioError;
use crate::playback as playback_mod;
use crate::protocol::AudioSignal;
use crate::rtc::{
    build_opus_decoder, build_opus_encoder, build_rtc, FRAME_SAMPLES, OPUS_MAX_DECODE_SAMPLES,
};

/// Maximum Opus encoded packet we'll ever produce. 4000 bytes is well
/// above libopus's hard limit (~1275 at 510 kbps for 20 ms frames);
/// we allocate this once and reuse it across encode calls.
const OPUS_ENCODE_BUF: usize = 4000;

/// MTU-ish allocation for the UDP recv buffer. WebRTC media packets
/// rarely exceed 1200 bytes once DTLS overhead is included, but we
/// pad for jumbo frames on LANs that allow them.
const UDP_RECV_BUF: usize = 2000;

/// Which side of a session this is. Determines who creates the SDP
/// offer vs. who replies with the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// Side that initiated the TCP connection. Creates the offer.
    Offerer,
    /// Side that accepted the TCP connection. Replies with an answer.
    Answerer,
}

/// A single negotiated audio link with one peer.
///
/// Dropping the [`AudioSession`] aborts every owned task and drops
/// the cpal streams, which tears down the cpal callbacks and stops
/// audio I/O. The struct deliberately holds the `Stream` handles
/// rather than passing them to other tasks because cpal streams
/// aren't `Send` on every platform.
pub struct AudioSession {
    pub session_id: String,
    pub peer_machine_id: String,
    /// Sender used to ask the driver to tear down gracefully. The
    /// driver also exits if `close_tx` is dropped (channel closes),
    /// so we don't have to call this — `Drop` is sufficient.
    close_tx: Option<oneshot::Sender<()>>,
    /// Held to keep the cpal capture callback running. `Stream` isn't
    /// `Send` on all hosts so it stays on the supervisor's task.
    _capture_stream: Option<Stream>,
    /// Held to keep the cpal playback callback running.
    _playback_stream: Option<Stream>,
    /// Held so the playback sink remains writable for the driver task.
    _playback_keepalive: Option<mpsc::Sender<PcmFrame>>,
    /// Live status shared with the driver task. Updated as media flows
    /// in either direction and when the underlying ICE/DTLS state
    /// changes. Read by the bridge in `AudioCommand::QueryStatus`.
    status: Arc<StdMutex<PeerAudioStatus>>,
    /// Driver task handle; aborted on drop.
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        // Sending on close_tx is best-effort — if the driver has
        // already exited the receive side is gone, which is fine.
        if let Some(tx) = self.close_tx.take() {
            let _ = tx.send(());
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

/// Internal bundle of "where do PCM frames come from / where do they
/// go" passed into [`AudioSession::start_inner`]. Production builds
/// this from cpal in [`AudioSession::start`]; integration tests build
/// it from in-memory `mpsc` channels via
/// [`AudioSession::start_for_test`].
struct SessionIo {
    capture_rx: Option<mpsc::Receiver<PcmFrame>>,
    playback_tx: Option<mpsc::Sender<PcmFrame>>,
    capture_stream: Option<Stream>,
    playback_stream: Option<Stream>,
}

impl AudioSession {
    /// Start a fully-driven session.
    ///
    /// Builds the str0m `Rtc`, binds a UDP socket for the media path,
    /// opens the local capture and playback cpal streams, and spawns
    /// the driver task. The returned [`AudioSession`] keeps every
    /// owned resource alive; drop it to tear the link down.
    pub async fn start(
        peer_machine_id: String,
        signal: CipherStream,
        cfg: &AudioConfig,
        role: SessionRole,
        events: mpsc::Sender<crate::bridge::AudioEvent>,
    ) -> Result<Self, AudioError> {
        let want_send = peer_wants_send(cfg, &peer_machine_id);
        let (capture_stream, capture_rx) = if want_send {
            match capture::start_capture(cfg.input_device.as_deref()) {
                Ok((s, rx)) => (Some(s), Some(rx)),
                Err(e) => {
                    warn!(?e, "audio capture unavailable; sending silence-free");
                    let _ = events
                        .send(crate::bridge::AudioEvent::Error {
                            peer: Some(peer_machine_id.clone()),
                            message: format!("capture: {e}"),
                        })
                        .await;
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let want_recv = peer_wants_recv(cfg, &peer_machine_id);
        let (playback_stream, playback_tx) = if want_recv {
            match playback_mod::start_playback(cfg.output_device.as_deref()) {
                Ok((s, tx)) => (Some(s), Some(tx)),
                Err(e) => {
                    warn!(?e, "audio playback unavailable; receive will be discarded");
                    let _ = events
                        .send(crate::bridge::AudioEvent::Error {
                            peer: Some(peer_machine_id.clone()),
                            message: format!("playback: {e}"),
                        })
                        .await;
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        Self::start_inner(
            peer_machine_id,
            signal,
            role,
            events,
            SessionIo {
                capture_rx,
                playback_tx,
                capture_stream,
                playback_stream,
            },
        )
        .await
    }

    /// Test-only entry point that hands in `mpsc` channels for
    /// capture and playback instead of opening cpal devices. The
    /// loopback integration test uses this to drive two
    /// `AudioSession`s on one machine without touching the host's
    /// microphone or speakers.
    #[doc(hidden)]
    pub async fn start_for_test(
        peer_machine_id: String,
        signal: CipherStream,
        role: SessionRole,
        events: mpsc::Sender<crate::bridge::AudioEvent>,
        capture_rx: Option<mpsc::Receiver<PcmFrame>>,
        playback_tx: Option<mpsc::Sender<PcmFrame>>,
    ) -> Result<Self, AudioError> {
        Self::start_inner(
            peer_machine_id,
            signal,
            role,
            events,
            SessionIo {
                capture_rx,
                playback_tx,
                capture_stream: None,
                playback_stream: None,
            },
        )
        .await
    }

    async fn start_inner(
        peer_machine_id: String,
        signal: CipherStream,
        role: SessionRole,
        events: mpsc::Sender<crate::bridge::AudioEvent>,
        io: SessionIo,
    ) -> Result<Self, AudioError> {
        let session_id = Uuid::new_v4().to_string();
        info!(
            peer = %peer_machine_id,
            session = %session_id,
            ?role,
            "audio session starting"
        );

        // Figure out which local IP to use for the media path. The
        // signaling TCP already crossed the LAN successfully — its
        // local endpoint is by definition a routable interface on
        // this host. Falling back to the unspecified address would
        // be fine for binding but doesn't make for a useful ICE host
        // candidate.
        let signal_local = signal
            .local_addr()
            .map_err(|e| AudioError::WebRtc(format!("signal local_addr: {e}")))?;

        // Bind a UDP socket on the same interface, ephemeral port.
        // We don't reuse the signaling TCP port — STUN/DTLS-SRTP
        // multiplex on a different transport.
        let udp = UdpSocket::bind((signal_local.ip(), 0))
            .await
            .map_err(|e| AudioError::WebRtc(format!("bind media udp: {e}")))?;
        let media_addr = udp
            .local_addr()
            .map_err(|e| AudioError::WebRtc(format!("media local_addr: {e}")))?;

        debug!(
            peer = %peer_machine_id,
            session = %session_id,
            %media_addr,
            "media socket bound"
        );

        let status = Arc::new(StdMutex::new(PeerAudioStatus {
            machine_id: peer_machine_id.clone(),
            display_name: peer_machine_id.clone(),
            sending_to_peer: false,
            receiving_from_peer: false,
            rtt_ms: None,
            last_error: None,
        }));

        let SessionIo {
            capture_rx,
            playback_tx,
            capture_stream,
            playback_stream,
        } = io;

        let (close_tx, close_rx) = oneshot::channel::<()>();

        let driver = tokio::spawn(driver_task(
            session_id.clone(),
            peer_machine_id.clone(),
            role,
            udp,
            media_addr,
            signal,
            capture_rx,
            playback_tx.clone(),
            events.clone(),
            Arc::clone(&status),
            close_rx,
        ));

        Ok(Self {
            session_id,
            peer_machine_id,
            close_tx: Some(close_tx),
            _capture_stream: capture_stream,
            _playback_stream: playback_stream,
            _playback_keepalive: playback_tx,
            status,
            tasks: vec![driver],
        })
    }

    /// Snapshot of this session's current peer status. Cheap (one
    /// lock + clone) so it's safe to call per `QueryStatus`.
    pub fn status(&self) -> PeerAudioStatus {
        self.status.lock().expect("peer status poisoned").clone()
    }

    /// Tear down the session. The driver task observes `close_tx`
    /// dropping, drains any in-flight str0m output, and exits.
    pub async fn close(mut self, reason: Option<String>) {
        debug!(
            peer = %self.peer_machine_id,
            session = %self.session_id,
            reason = ?reason,
            "audio session closing"
        );
        // Best-effort signal; if the driver already exited the
        // receive side is gone, which is fine.
        if let Some(tx) = self.close_tx.take() {
            let _ = tx.send(());
        }
        // `Drop` will abort the task; await briefly so any flushable
        // output (RTCP BYE, ICE close) actually makes it onto the
        // wire before we tear the UDP socket down.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn peer_wants_send(cfg: &AudioConfig, peer: &str) -> bool {
    // No per-peer override = bidirectional default whenever the
    // bridge is enabled. The bridge would never have started this
    // session if `enabled` were false, so we don't double-check it
    // here.
    cfg.per_peer
        .get(peer)
        .map(|p| p.enabled && p.send_to_peer)
        .unwrap_or(true)
}

fn peer_wants_recv(cfg: &AudioConfig, peer: &str) -> bool {
    cfg.per_peer
        .get(peer)
        .map(|p| p.enabled && p.receive_from_peer)
        .unwrap_or(true)
}

/// Mutate the shared status and, if the mutation actually changed
/// something, push a `PeerStatus` event onto the bridge's event
/// channel. `try_send` so a clogged channel can't stall the driver.
fn update_status<F>(
    status: &Arc<StdMutex<PeerAudioStatus>>,
    events: &mpsc::Sender<crate::bridge::AudioEvent>,
    update: F,
) where
    F: FnOnce(&mut PeerAudioStatus),
{
    let snapshot;
    let changed;
    {
        let mut guard = status.lock().expect("peer status poisoned");
        let before = guard.clone();
        update(&mut guard);
        changed = *guard != before;
        snapshot = guard.clone();
    }
    if changed {
        let _ = events.try_send(crate::bridge::AudioEvent::PeerStatus(snapshot));
    }
}

#[allow(clippy::too_many_arguments)]
async fn driver_task(
    session_id: String,
    peer_machine_id: String,
    role: SessionRole,
    udp: UdpSocket,
    media_addr: SocketAddr,
    signal: CipherStream,
    capture_rx: Option<mpsc::Receiver<PcmFrame>>,
    playback_tx: Option<mpsc::Sender<PcmFrame>>,
    events: mpsc::Sender<crate::bridge::AudioEvent>,
    status: Arc<StdMutex<PeerAudioStatus>>,
    close_rx: oneshot::Receiver<()>,
) {
    let (signal_reader, signal_writer) = signal.split();
    if let Err(e) = run_driver(
        &session_id,
        &peer_machine_id,
        role,
        udp,
        media_addr,
        signal_reader,
        signal_writer,
        capture_rx,
        playback_tx,
        &events,
        &status,
        close_rx,
    )
    .await
    {
        warn!(
            peer = %peer_machine_id,
            session = %session_id,
            ?e,
            "audio driver ended with error"
        );
        let _ = events
            .send(crate::bridge::AudioEvent::Error {
                peer: Some(peer_machine_id),
                message: format!("{e}"),
            })
            .await;
    } else {
        debug!(
            peer = %peer_machine_id,
            session = %session_id,
            "audio driver ended cleanly"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_driver(
    session_id: &str,
    peer_machine_id: &str,
    role: SessionRole,
    udp: UdpSocket,
    media_addr: SocketAddr,
    mut signal_reader: CipherReader,
    mut signal_writer: CipherWriter,
    mut capture_rx: Option<mpsc::Receiver<PcmFrame>>,
    playback_tx: Option<mpsc::Sender<PcmFrame>>,
    events: &mpsc::Sender<crate::bridge::AudioEvent>,
    status: &Arc<StdMutex<PeerAudioStatus>>,
    mut close_rx: oneshot::Receiver<()>,
) -> Result<(), AudioError> {
    let start = Instant::now();
    let mut rtc = build_rtc(start);

    // Add our host candidate before the SDP exchange so the offer or
    // answer carries it inline (str0m bundles candidates into the SDP
    // when this is set ahead of `sdp_api().apply()`).
    let candidate = Candidate::host(media_addr, Protocol::Udp)
        .map_err(|e| AudioError::WebRtc(format!("host candidate: {e}")))?;
    rtc.add_local_candidate(candidate);

    // SDP exchange. For the offerer, we know the mid up front
    // (returned by `add_media`); for the answerer, str0m will emit
    // it as a `MediaAdded` event after `accept_offer`.
    let mut mid: Option<Mid> = match role {
        SessionRole::Offerer => {
            let mut change = rtc.sdp_api();
            let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
            let (offer, pending) = change
                .apply()
                .ok_or_else(|| AudioError::WebRtc("sdp_api apply() returned None".into()))?;
            send_signal(
                &mut signal_writer,
                &AudioSignal::Offer {
                    session_id: session_id.to_string(),
                    sdp: offer.to_sdp_string(),
                },
            )
            .await?;
            let answer = wait_for_answer(&mut signal_reader).await?;
            let parsed = SdpAnswer::from_sdp_string(&answer)
                .map_err(|e| AudioError::WebRtc(format!("parse answer: {e}")))?;
            rtc.sdp_api()
                .accept_answer(pending, parsed)
                .map_err(|e| AudioError::WebRtc(format!("accept_answer: {e}")))?;
            Some(mid)
        }
        SessionRole::Answerer => {
            let offer_sdp = wait_for_offer(&mut signal_reader).await?;
            let offer = SdpOffer::from_sdp_string(&offer_sdp)
                .map_err(|e| AudioError::WebRtc(format!("parse offer: {e}")))?;
            let answer = rtc
                .sdp_api()
                .accept_offer(offer)
                .map_err(|e| AudioError::WebRtc(format!("accept_offer: {e}")))?;
            send_signal(
                &mut signal_writer,
                &AudioSignal::Answer {
                    session_id: session_id.to_string(),
                    sdp: answer.to_sdp_string(),
                },
            )
            .await?;
            None
        }
    };

    info!(
        peer = %peer_machine_id,
        session = %session_id,
        "audio negotiation complete"
    );

    // Resolve the Opus PT from the codec config. The PT is the same
    // on both sides after SDP negotiation; we pin it now and reuse
    // for every outbound write.
    let opus_pt: Pt = opus_payload_type(&rtc)
        .ok_or_else(|| AudioError::WebRtc("Opus codec missing from negotiated config".into()))?;

    let mut encoder = build_opus_encoder()?;
    let mut decoder = build_opus_decoder()?;
    let mut encode_buf = vec![0u8; OPUS_ENCODE_BUF];
    let mut decode_buf = vec![0i16; OPUS_MAX_DECODE_SAMPLES];
    let mut udp_buf = vec![0u8; UDP_RECV_BUF];
    let mut first_send = true;
    let mut first_recv = true;

    loop {
        // Drain everything str0m wants to emit right now. Each
        // iteration moves outbound packets to the UDP socket and
        // turns inbound media into PCM on the playback queue. The
        // loop terminates either via an `Output::Timeout` (the
        // happy path — that timeout is what we sleep on below) or
        // a `poll_output` error (we propagate and tear down).
        let next_timeout: Instant = loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let dest = t.destination;
                    let contents: Vec<u8> = t.contents.into();
                    if let Err(e) = udp.send_to(&contents, dest).await {
                        debug!(?e, peer = %peer_machine_id, "udp send_to failed");
                    }
                }
                Ok(Output::Event(ev)) => match ev {
                    Event::Connected => {
                        update_status(status, events, |s| {
                            s.last_error = None;
                        });
                        debug!(peer = %peer_machine_id, "rtc connected");
                    }
                    Event::MediaAdded(m) if mid.is_none() => {
                        mid = Some(m.mid);
                    }
                    Event::MediaData(data) => {
                        if let Some(tx) = &playback_tx {
                            match decoder.decode(&data.data, &mut decode_buf, false) {
                                Ok(n) => {
                                    let frame: PcmFrame = Arc::from(&decode_buf[..n]);
                                    if first_recv {
                                        first_recv = false;
                                        update_status(status, events, |s| {
                                            s.receiving_from_peer = true;
                                        });
                                    }
                                    // try_send so a stalled cpal
                                    // playback can't back up the
                                    // whole driver loop.
                                    let _ = tx.try_send(frame);
                                }
                                Err(e) => {
                                    debug!(
                                        ?e,
                                        peer = %peer_machine_id,
                                        "opus decode failed; dropping packet"
                                    );
                                }
                            }
                        }
                    }
                    Event::IceConnectionStateChange(state) => {
                        debug!(?state, peer = %peer_machine_id, "ice state change");
                        use str0m::IceConnectionState as I;
                        match state {
                            I::Disconnected => {
                                update_status(status, events, |s| {
                                    s.last_error = Some("ice disconnected".to_string());
                                });
                            }
                            I::Connected | I::Completed => {
                                update_status(status, events, |s| {
                                    s.last_error = None;
                                });
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                },
                Ok(Output::Timeout(t)) => break t,
                Err(e) => {
                    warn!(?e, peer = %peer_machine_id, "rtc poll_output error");
                    return Err(AudioError::WebRtc(format!("poll_output: {e}")));
                }
            }
        };

        // Now wait for the next thing that needs the driver's
        // attention. The select! arms cover every event source:
        // graceful close, inbound UDP, outbound capture frame, the
        // scheduled timeout str0m gave us, and the signaling stream
        // (which we mostly use to detect a `Close` signal from the
        // peer).
        let now = Instant::now();
        let sleep = tokio::time::sleep(next_timeout.saturating_duration_since(now));
        tokio::pin!(sleep);

        tokio::select! {
            biased;

            _ = &mut close_rx => {
                debug!(peer = %peer_machine_id, "close requested");
                let _ = send_signal(
                    &mut signal_writer,
                    &AudioSignal::Close {
                        session_id: session_id.to_string(),
                        reason: Some("local close".into()),
                    },
                ).await;
                return Ok(());
            }

            recv = udp.recv_from(&mut udp_buf) => {
                match recv {
                    Ok((n, src)) => {
                        let now = Instant::now();
                        let view = match udp_buf[..n].try_into() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Err(e) = rtc.handle_input(Input::Receive(
                            now,
                            Receive {
                                proto: Protocol::Udp,
                                source: src,
                                destination: media_addr,
                                contents: view,
                            },
                        )) {
                            debug!(?e, "rtc handle_input(Receive) failed");
                        }
                    }
                    Err(e) => {
                        debug!(?e, "udp recv_from error");
                    }
                }
            }

            frame = recv_capture_frame(&mut capture_rx) => {
                let Some(frame) = frame else {
                    // Capture closed — keep the session up (we can
                    // still receive even if we can't send) but stop
                    // polling this arm.
                    continue;
                };
                if frame.len() != FRAME_SAMPLES {
                    warn!(
                        len = frame.len(),
                        peer = %peer_machine_id,
                        "unexpected PCM frame size; dropping"
                    );
                    continue;
                }
                let Some(mid) = mid else {
                    // We haven't seen MediaAdded yet — drop the
                    // pre-negotiation frame instead of buffering.
                    continue;
                };
                match encoder.encode(&frame, &mut encode_buf) {
                    Ok(n) => {
                        let now = Instant::now();
                        let rtp_time = now.saturating_duration_since(start);
                        let Some(writer) = rtc.writer(mid) else {
                            continue;
                        };
                        if let Err(e) = writer.write(
                            opus_pt,
                            now,
                            rtp_time.into(),
                            encode_buf[..n].to_vec(),
                        ) {
                            debug!(?e, peer = %peer_machine_id, "writer.write failed");
                        } else if first_send {
                            first_send = false;
                            update_status(status, events, |s| {
                                s.sending_to_peer = true;
                            });
                        }
                    }
                    Err(e) => {
                        debug!(?e, "opus encode failed; dropping frame");
                    }
                }
            }

            _ = &mut sleep => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    debug!(?e, "rtc handle_input(Timeout) failed");
                }
            }

            sig = signal_reader.recv() => {
                match sig {
                    Ok(bytes) => {
                        match serde_json::from_slice::<AudioSignal>(&bytes) {
                            Ok(AudioSignal::Close { reason, .. }) => {
                                debug!(?reason, "peer requested close");
                                return Ok(());
                            }
                            Ok(other) => {
                                debug!(?other, "ignoring late signaling message");
                            }
                            Err(e) => {
                                debug!(?e, "signal decode failed");
                            }
                        }
                    }
                    Err(e) => {
                        debug!(?e, "signaling reader ended");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Polled inside `select!` to draw the next PCM frame. Returns
/// `None` if the capture channel is closed or there's no capture at
/// all (in which case we want the arm to pend forever rather than
/// fire repeatedly with `None`).
async fn recv_capture_frame(capture_rx: &mut Option<mpsc::Receiver<PcmFrame>>) -> Option<PcmFrame> {
    match capture_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn send_signal(writer: &mut CipherWriter, signal: &AudioSignal) -> Result<(), AudioError> {
    let body = serde_json::to_vec(signal)?;
    writer
        .send(&body)
        .await
        .map_err(|_| AudioError::SignalClosed)?;
    Ok(())
}

async fn wait_for_offer(reader: &mut CipherReader) -> Result<String, AudioError> {
    loop {
        let bytes = reader.recv().await.map_err(|_| AudioError::SignalClosed)?;
        let signal: AudioSignal = serde_json::from_slice(&bytes)?;
        match signal {
            AudioSignal::Offer { sdp, .. } => return Ok(sdp),
            other => {
                debug!(?other, "discarding pre-offer signal");
            }
        }
    }
}

async fn wait_for_answer(reader: &mut CipherReader) -> Result<String, AudioError> {
    loop {
        let bytes = reader.recv().await.map_err(|_| AudioError::SignalClosed)?;
        let signal: AudioSignal = serde_json::from_slice(&bytes)?;
        match signal {
            AudioSignal::Answer { sdp, .. } => return Ok(sdp),
            other => {
                debug!(?other, "discarding pre-answer signal");
            }
        }
    }
}

fn opus_payload_type(rtc: &Rtc) -> Option<Pt> {
    rtc.codec_config()
        .iter()
        .find(|p| p.spec().codec == Codec::Opus)
        .map(PayloadParams::pt)
}
