//! Per-peer audio session state machine.
//!
//! One session per paired peer with audio enabled. The session owns:
//! - the WebRTC [`RTCPeerConnection`]
//! - the local capture stream (mic or loopback) feeding into the
//!   outbound RTP track
//! - the local playback stream draining the inbound RTP track
//! - the [`synbad_crypto::CipherStream`] (split into read + write halves)
//!   carrying signaling messages
//!
//! ## Roles
//!
//! Exactly one peer in a pair drives the SDP offer/answer. We pick by
//! TCP direction: the side that dials is the [`SessionRole::Offerer`]
//! and the side that accepts is the [`SessionRole::Answerer`]. The
//! supervisor enforces a glare rule (only dial when our `machine_id` is
//! lexicographically smaller than the peer's) so two peers never both
//! end up as offerer.
//!
//! ## Concurrency
//!
//! The session task spawns three sub-tasks:
//!
//! 1. **Signaling writer** — owns the [`synbad_crypto::CipherWriter`]
//!    half and drains an internal `mpsc<AudioSignal>` queue. The PC's
//!    `on_ice_candidate` callback pushes into the queue directly.
//! 2. **Capture pump** — drains the mpsc from `capture::start_capture`
//!    and writes one RTP packet per 20 ms PCM frame to the outbound
//!    track (with L16 byte-swap from host-LE to wire-BE).
//! 3. **Inbound pump** (per remote track) — spawned from
//!    `pc.on_track`, reads RTP packets, byte-swaps BE → host-LE, and
//!    pushes mono `i16` frames into the playback sender.
//!
//! The main session task drives negotiation: offer/answer over the
//! [`synbad_crypto::CipherReader`] half, then loops handling inbound
//! signals (ICE candidates, IceComplete, Close).

#![allow(clippy::result_large_err)] // AudioError carries Strings; size dominates.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use bytes::Bytes;
use cpal::Stream;
use synbad_config::AudioConfig;
use synbad_crypto::{CipherReader, CipherStream, CipherWriter};
use synbad_ipc::PeerAudioStatus;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::capture::{self, PcmFrame, FRAME_SAMPLES};
use crate::errors::AudioError;
use crate::playback;
use crate::protocol::AudioSignal;
use crate::rtc::{self, L16_PAYLOAD_TYPE, SAMPLES_PER_PACKET};

/// Outbound queue depth for signals heading to the peer. Trickled ICE
/// candidates appear in bursts of a few; 32 is comfortably above the
/// realistic peak.
const SIGNAL_QUEUE_DEPTH: usize = 32;

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
/// Dropping the [`AudioSession`] aborts every owned task and drops the
/// owned cpal streams, which tears down the PeerConnection's interceptor
/// pipeline and stops audio I/O. The struct deliberately holds the
/// `Stream` handles rather than passing them to other tasks because cpal
/// streams aren't `Send` on every platform.
pub struct AudioSession {
    pub session_id: String,
    pub peer_machine_id: String,
    peer_connection: Arc<RTCPeerConnection>,
    /// Held to keep the cpal capture callback running. `Stream` isn't
    /// `Send` on all hosts so it stays on the supervisor's task.
    _capture_stream: Option<Stream>,
    /// Held to keep the cpal playback callback running.
    _playback_stream: Option<Stream>,
    /// Held so the playback sink remains writable for the on_track pump
    /// installed on the PeerConnection.
    _playback_keepalive: Option<mpsc::Sender<PcmFrame>>,
    /// Live status shared with the per-session sub-tasks. Updated by the
    /// capture pump on first send, the inbound pump on first receive, and
    /// the PeerConnection state-change handler. Read by the bridge in
    /// `AudioCommand::QueryStatus`.
    status: Arc<StdMutex<PeerAudioStatus>>,
    /// Aborted on drop.
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

impl AudioSession {
    /// Start a fully-driven session.
    ///
    /// Builds the PeerConnection, attaches the outbound L16 track, opens
    /// the local capture and playback streams, and spawns the
    /// negotiation driver. The returned [`AudioSession`] keeps every
    /// owned resource alive; drop it to tear the link down.
    pub async fn start(
        peer_machine_id: String,
        signal: CipherStream,
        cfg: &AudioConfig,
        role: SessionRole,
        events: mpsc::Sender<crate::bridge::AudioEvent>,
    ) -> Result<Self, AudioError> {
        let session_id = Uuid::new_v4().to_string();
        info!(
            peer = %peer_machine_id,
            session = %session_id,
            ?role,
            "audio session starting"
        );

        let api = rtc::build_api()?;
        let pc = rtc::new_peer_connection(&api).await?;

        let status = Arc::new(StdMutex::new(PeerAudioStatus {
            machine_id: peer_machine_id.clone(),
            display_name: peer_machine_id.clone(),
            sending_to_peer: false,
            receiving_from_peer: false,
            rtt_ms: None,
            last_error: None,
        }));

        install_pc_state_handler(&pc, Arc::clone(&status), events.clone());

        // Outbound: cpal mic/loopback → L16 packets → outbound track.
        let outbound_track = rtc::build_outbound_track();
        pc.add_track(outbound_track.clone())
            .await
            .map_err(|e| AudioError::WebRtc(format!("add outbound track: {e}")))?;

        // Local capture is only opened if this peer is supposed to send
        // audio (either because the global toggle is on or this peer
        // has an override). Skipping the open keeps headless / mic-less
        // hosts quiet on the answerer side.
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

        // Inbound: PC `on_track` fires once the answerer's track is
        // negotiated. We open the playback stream up-front so the
        // first packets aren't dropped by an unbuilt sink.
        let want_recv = peer_wants_recv(cfg, &peer_machine_id);
        let (playback_stream, playback_tx) = if want_recv {
            match playback::start_playback(cfg.output_device.as_deref()) {
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

        // Split the signaling channel so the writer task can drain the
        // outgoing queue while the driver task blocks on inbound reads
        // — they touch different halves of the underlying TCP stream,
        // so neither can cancel-bug the other.
        let (signal_reader, signal_writer) = signal.split();
        let (out_tx, out_rx) = mpsc::channel::<AudioSignal>(SIGNAL_QUEUE_DEPTH);

        // Hook the PC's ICE gatherer to push every local candidate into
        // the outbound queue. A `None` candidate marks gathering complete
        // per RFC 8838.
        install_ice_handler(&pc, &session_id, out_tx.clone());

        // Hook on_track to spawn an inbound-RTP pump per remote track.
        if let Some(tx) = playback_tx.clone() {
            install_on_track(&pc, tx, &session_id, Arc::clone(&status), events.clone());
        }

        // Spawn the writer task. It owns the CipherWriter and quits when
        // the outbound queue is closed.
        let writer_task = tokio::spawn(signal_writer_task(
            signal_writer,
            out_rx,
            peer_machine_id.clone(),
        ));

        // Spawn the capture pump if we opened a capture stream.
        let capture_task = capture_rx.map(|rx| {
            tokio::spawn(capture_pump_task(
                rx,
                outbound_track.clone(),
                peer_machine_id.clone(),
                Arc::clone(&status),
                events.clone(),
            ))
        });

        // Spawn the negotiation driver task — runs the offer/answer
        // dance and then loops on inbound signals.
        let driver_task = tokio::spawn(driver_task(
            session_id.clone(),
            peer_machine_id.clone(),
            pc.clone(),
            signal_reader,
            out_tx,
            role,
        ));

        let mut tasks = vec![writer_task, driver_task];
        if let Some(t) = capture_task {
            tasks.push(t);
        }

        Ok(Self {
            session_id,
            peer_machine_id,
            peer_connection: pc,
            _capture_stream: capture_stream,
            _playback_stream: playback_stream,
            _playback_keepalive: playback_tx,
            status,
            tasks,
        })
    }

    /// Snapshot of this session's current peer status. Cheap (one lock +
    /// clone) so it's safe to call per-`QueryStatus`.
    pub fn status(&self) -> PeerAudioStatus {
        self.status.lock().expect("peer status poisoned").clone()
    }

    /// Tear down the PeerConnection and stop both audio streams.
    pub async fn close(self, reason: Option<String>) {
        debug!(
            peer = %self.peer_machine_id,
            session = %self.session_id,
            reason = ?reason,
            "audio session closing"
        );
        // Closing the PC stops the interceptor pipeline; tasks notice
        // their channels close and exit; Drop aborts whatever remains.
        let _ = self.peer_connection.close().await;
    }
}

fn peer_wants_send(cfg: &AudioConfig, peer: &str) -> bool {
    cfg.per_peer
        .get(peer)
        .map(|p| p.enabled && p.send_to_peer)
        .unwrap_or(cfg.send_mic_to_peers)
}

fn peer_wants_recv(cfg: &AudioConfig, peer: &str) -> bool {
    cfg.per_peer
        .get(peer)
        .map(|p| p.enabled && p.receive_from_peer)
        .unwrap_or(cfg.receive_peer_audio)
}

fn install_ice_handler(
    pc: &Arc<RTCPeerConnection>,
    session_id: &str,
    out_tx: mpsc::Sender<AudioSignal>,
) {
    let sid = session_id.to_string();
    pc.on_ice_candidate(Box::new(move |candidate| {
        let sid = sid.clone();
        let out_tx = out_tx.clone();
        Box::pin(async move {
            let signal = match candidate {
                Some(c) => match c.to_json() {
                    Ok(init) => AudioSignal::IceCandidate {
                        session_id: sid,
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                    },
                    Err(e) => {
                        warn!(?e, "ICE candidate serialise failed");
                        return;
                    }
                },
                None => AudioSignal::IceComplete { session_id: sid },
            };
            if out_tx.send(signal).await.is_err() {
                debug!("outbound signal queue closed; ICE handler dropping candidate");
            }
        })
    }));
}

fn install_on_track(
    pc: &Arc<RTCPeerConnection>,
    playback_tx: mpsc::Sender<PcmFrame>,
    session_id: &str,
    status: Arc<StdMutex<PeerAudioStatus>>,
    events: mpsc::Sender<crate::bridge::AudioEvent>,
) {
    let session_id = session_id.to_string();
    pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let playback_tx = playback_tx.clone();
        let session_id = session_id.clone();
        let status = Arc::clone(&status);
        let events = events.clone();
        Box::pin(async move {
            if track.payload_type() != L16_PAYLOAD_TYPE {
                warn!(
                    pt = track.payload_type(),
                    session = %session_id,
                    "remote track has unexpected payload type"
                );
                return;
            }
            inbound_pump(track, playback_tx, session_id, status, events).await;
        })
    }));
}

async fn inbound_pump(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    playback_tx: mpsc::Sender<PcmFrame>,
    session_id: String,
    status: Arc<StdMutex<PeerAudioStatus>>,
    events: mpsc::Sender<crate::bridge::AudioEvent>,
) {
    debug!(session = %session_id, "inbound RTP pump started");
    let mut first_packet = true;
    loop {
        match track.read_rtp().await {
            Ok((pkt, _attrs)) => {
                let frame = depayload_l16(&pkt.payload);
                if frame.is_empty() {
                    continue;
                }
                if first_packet {
                    first_packet = false;
                    update_status(&status, &events, |s| s.receiving_from_peer = true);
                }
                if playback_tx.send(frame).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                debug!(?e, "inbound RTP read ended");
                break;
            }
        }
    }
    update_status(&status, &events, |s| s.receiving_from_peer = false);
}

/// Installs the PeerConnection state-change handler that mirrors transport
/// failures into the shared status. `Connected` clears any sticky error;
/// `Failed`/`Disconnected` set one. `Closed` zeroes the send/receive flags
/// so the GUI can show "idle" rather than the last `true` it saw.
fn install_pc_state_handler(
    pc: &Arc<RTCPeerConnection>,
    status: Arc<StdMutex<PeerAudioStatus>>,
    events: mpsc::Sender<crate::bridge::AudioEvent>,
) {
    pc.on_peer_connection_state_change(Box::new(move |state| {
        let status = Arc::clone(&status);
        let events = events.clone();
        Box::pin(async move {
            debug!(?state, "audio peer connection state change");
            update_status(&status, &events, |s| match state {
                RTCPeerConnectionState::Connected => {
                    s.last_error = None;
                }
                RTCPeerConnectionState::Failed => {
                    s.last_error = Some("connection failed".to_string());
                }
                RTCPeerConnectionState::Disconnected => {
                    s.last_error = Some("connection disconnected".to_string());
                }
                RTCPeerConnectionState::Closed => {
                    s.sending_to_peer = false;
                    s.receiving_from_peer = false;
                }
                // New / Connecting / Unspecified: no user-visible change.
                _ => {}
            });
        })
    }));
}

/// Mutate the shared status and, if the mutation actually changed
/// something, push a `PeerStatus` event onto the bridge's event channel.
/// `try_send` so a clogged channel can't stall a callback task.
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

/// Decode RFC 3551 §4.5.7 L16: payload is big-endian i16 samples. We
/// convert to little-endian `i16` for downstream cpal playback.
fn depayload_l16(payload: &[u8]) -> PcmFrame {
    let n = payload.len() / 2;
    let mut samples = Vec::with_capacity(n);
    for chunk in payload.chunks_exact(2) {
        samples.push(i16::from_be_bytes([chunk[0], chunk[1]]));
    }
    PcmFrame::from(samples)
}

async fn signal_writer_task(
    mut writer: CipherWriter,
    mut queue: mpsc::Receiver<AudioSignal>,
    peer_machine_id: String,
) {
    while let Some(signal) = queue.recv().await {
        let body = match serde_json::to_vec(&signal) {
            Ok(b) => b,
            Err(e) => {
                warn!(?e, peer = %peer_machine_id, "signal serialise failed");
                continue;
            }
        };
        if let Err(e) = writer.send(&body).await {
            debug!(?e, peer = %peer_machine_id, "signaling writer ended");
            break;
        }
    }
    debug!(peer = %peer_machine_id, "signaling writer task exiting");
}

async fn capture_pump_task(
    mut rx: mpsc::Receiver<PcmFrame>,
    track: Arc<webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP>,
    peer_machine_id: String,
    status: Arc<StdMutex<PeerAudioStatus>>,
    events: mpsc::Sender<crate::bridge::AudioEvent>,
) {
    // RTP sequence + timestamp need to start at random values per RFC
    // 3550; the synchronization source (SSRC) is set by the binding
    // when the track is bound to the PeerConnection so we leave it 0.
    let mut sequence_number: u16 = rand::random();
    let mut timestamp: u32 = rand::random();
    let mut first_packet = true;

    debug!(peer = %peer_machine_id, "capture pump started");
    while let Some(frame) = rx.recv().await {
        if frame.len() != FRAME_SAMPLES {
            // Capture only emits exact-size frames in v1, but be
            // defensive: skip anything else rather than ship a
            // partial RTP packet.
            warn!(len = frame.len(), "unexpected PCM frame size; dropping");
            continue;
        }
        let payload = payload_l16(&frame);
        let pkt = rtp::packet::Packet {
            header: rtp::header::Header {
                version: 2,
                padding: false,
                extension: false,
                marker: false,
                payload_type: L16_PAYLOAD_TYPE,
                sequence_number,
                timestamp,
                ssrc: 0, // filled in by TrackBinding
                csrc: Vec::new(),
                extension_profile: 0,
                extensions: Vec::new(),
                extensions_padding: 0,
            },
            payload,
        };
        // `write_rtp_with_extensions` returns Err when no bindings are
        // attached yet (pre-negotiation) — that's expected for the
        // first few frames on the offerer side, so swallow rather than
        // warn. We pass an empty extension list because L16 doesn't
        // carry any header extensions.
        match track.write_rtp_with_extensions(&pkt, &[]).await {
            Ok(_) => {
                if first_packet {
                    first_packet = false;
                    update_status(&status, &events, |s| s.sending_to_peer = true);
                }
            }
            Err(e) => {
                debug!(?e, peer = %peer_machine_id, "RTP write failed");
            }
        }
        sequence_number = sequence_number.wrapping_add(1);
        timestamp = timestamp.wrapping_add(SAMPLES_PER_PACKET);
    }
    update_status(&status, &events, |s| s.sending_to_peer = false);
    debug!(peer = %peer_machine_id, "capture pump exiting");
}

/// Encode 960 little-endian `i16` PCM samples as an L16 RTP payload
/// (big-endian on the wire per RFC 3551 §4.5.7).
fn payload_l16(frame: &[i16]) -> Bytes {
    let mut out = Vec::with_capacity(frame.len() * 2);
    for s in frame {
        out.extend_from_slice(&s.to_be_bytes());
    }
    Bytes::from(out)
}

async fn driver_task(
    session_id: String,
    peer_machine_id: String,
    pc: Arc<RTCPeerConnection>,
    mut reader: CipherReader,
    out_tx: mpsc::Sender<AudioSignal>,
    role: SessionRole,
) {
    if let Err(e) = run_driver(
        &session_id,
        &peer_machine_id,
        &pc,
        &mut reader,
        &out_tx,
        role,
    )
    .await
    {
        warn!(peer = %peer_machine_id, session = %session_id, ?e, "audio driver ended with error");
    } else {
        debug!(peer = %peer_machine_id, session = %session_id, "audio driver ended cleanly");
    }
    // Drop out_tx so the writer task finishes. (We're holding the last
    // clone outside the PC's ICE handler — that one gets dropped when
    // we close the PC.)
    let _ = pc.close().await;
}

async fn run_driver(
    session_id: &str,
    peer_machine_id: &str,
    pc: &Arc<RTCPeerConnection>,
    reader: &mut CipherReader,
    out_tx: &mpsc::Sender<AudioSignal>,
    role: SessionRole,
) -> Result<(), AudioError> {
    match role {
        SessionRole::Offerer => {
            let offer = pc
                .create_offer(None)
                .await
                .map_err(|e| AudioError::WebRtc(format!("create_offer: {e}")))?;
            pc.set_local_description(offer.clone())
                .await
                .map_err(|e| AudioError::WebRtc(format!("set_local_description(offer): {e}")))?;
            out_tx
                .send(AudioSignal::Offer {
                    session_id: session_id.to_string(),
                    sdp: offer.sdp,
                })
                .await
                .map_err(|_| AudioError::SignalClosed)?;
            wait_for_answer(reader, pc).await?;
        }
        SessionRole::Answerer => {
            let remote_sdp = wait_for_offer(reader).await?;
            let offer = RTCSessionDescription::offer(remote_sdp)
                .map_err(|e| AudioError::WebRtc(format!("wrap offer: {e}")))?;
            pc.set_remote_description(offer)
                .await
                .map_err(|e| AudioError::WebRtc(format!("set_remote_description(offer): {e}")))?;
            let answer = pc
                .create_answer(None)
                .await
                .map_err(|e| AudioError::WebRtc(format!("create_answer: {e}")))?;
            pc.set_local_description(answer.clone())
                .await
                .map_err(|e| AudioError::WebRtc(format!("set_local_description(answer): {e}")))?;
            out_tx
                .send(AudioSignal::Answer {
                    session_id: session_id.to_string(),
                    sdp: answer.sdp,
                })
                .await
                .map_err(|_| AudioError::SignalClosed)?;
        }
    }

    info!(peer = %peer_machine_id, session = %session_id, "audio negotiation complete");

    // Stay in the ICE candidate exchange loop until the peer Closes the
    // session or the channel drops.
    loop {
        let bytes = match reader.recv().await {
            Ok(b) => b,
            Err(e) => {
                debug!(?e, "signaling reader ended");
                return Ok(());
            }
        };
        let signal: AudioSignal = serde_json::from_slice(&bytes)?;
        match signal {
            AudioSignal::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..
            } => {
                let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                    username_fragment: None,
                };
                if let Err(e) = pc.add_ice_candidate(init).await {
                    debug!(?e, "add_ice_candidate failed");
                }
            }
            AudioSignal::IceComplete { .. } => {
                let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                    candidate: String::new(),
                    sdp_mid: None,
                    sdp_mline_index: None,
                    username_fragment: None,
                };
                if let Err(e) = pc.add_ice_candidate(init).await {
                    debug!(?e, "add_ice_candidate(end-of-candidates) failed");
                }
            }
            AudioSignal::Close { reason, .. } => {
                debug!(?reason, "remote requested session close");
                return Ok(());
            }
            other => {
                debug!(?other, "ignoring unexpected signal in steady state");
            }
        }
    }
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

async fn wait_for_answer(
    reader: &mut CipherReader,
    pc: &Arc<RTCPeerConnection>,
) -> Result<(), AudioError> {
    loop {
        let bytes = reader.recv().await.map_err(|_| AudioError::SignalClosed)?;
        let signal: AudioSignal = serde_json::from_slice(&bytes)?;
        match signal {
            AudioSignal::Answer { sdp, .. } => {
                let answer = RTCSessionDescription::answer(sdp)
                    .map_err(|e| AudioError::WebRtc(format!("wrap answer: {e}")))?;
                pc.set_remote_description(answer).await.map_err(|e| {
                    AudioError::WebRtc(format!("set_remote_description(answer): {e}"))
                })?;
                return Ok(());
            }
            AudioSignal::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..
            } => {
                // Candidates may arrive before the answer; queue them
                // into the PC, but `add_ice_candidate` requires a
                // remote description so this will warn until we land
                // the answer. Stash and re-apply isn't worth it on a
                // LAN where the answer arrives within ~ms.
                let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                    username_fragment: None,
                };
                let _ = pc.add_ice_candidate(init).await;
            }
            other => {
                debug!(?other, "discarding pre-answer signal");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_status() -> Arc<StdMutex<PeerAudioStatus>> {
        Arc::new(StdMutex::new(PeerAudioStatus {
            machine_id: "peer-A".into(),
            display_name: "peer-A".into(),
            sending_to_peer: false,
            receiving_from_peer: false,
            rtt_ms: None,
            last_error: None,
        }))
    }

    #[test]
    fn update_status_emits_event_on_change() {
        let status = fresh_status();
        let (events_tx, mut events_rx) = mpsc::channel(8);

        update_status(&status, &events_tx, |s| s.sending_to_peer = true);

        let ev = events_rx.try_recv().expect("event should be emitted");
        match ev {
            crate::bridge::AudioEvent::PeerStatus(s) => assert!(s.sending_to_peer),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn update_status_skips_event_when_unchanged() {
        let status = fresh_status();
        status.lock().unwrap().sending_to_peer = true;
        let (events_tx, mut events_rx) = mpsc::channel(8);

        // Same value → no event.
        update_status(&status, &events_tx, |s| s.sending_to_peer = true);
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn payload_l16_round_trips() {
        let original: Vec<i16> = (0..960).map(|i| (i as i16).wrapping_mul(37)).collect();
        let payload = payload_l16(&original);
        let back = depayload_l16(&payload);
        assert_eq!(back.as_ref(), original.as_slice());
    }

    #[test]
    fn payload_l16_is_big_endian() {
        let frame = vec![0x1234i16];
        let payload = payload_l16(&frame);
        assert_eq!(&payload[..], &[0x12, 0x34]);
    }
}
