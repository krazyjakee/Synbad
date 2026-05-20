//! IPC between the GUI (`synbad-gui`) and the daemon (`synbadd`).
//!
//! Wire format: newline-delimited JSON. One JSON object per line. Each line
//! is a [`Message`]. A connection starts in request/response mode. After the
//! client sends [`Request::Subscribe`], the server keeps the connection open
//! and pushes [`Event`]s as they occur (e.g. log lines, state changes).

use serde::{Deserialize, Serialize};

use synbad_config::{AudioConfig, Config};

/// A peer discovered on the local network via mDNS. The fields are
/// populated from the peer's `_synbad._tcp.local.` TXT record. The peer
/// is **not trusted** until the user completes pairing — `trusted` lives
/// alongside this in the daemon's per-peer state, not on the wire type.
///
/// Defined here (not in `synbad-discovery`) so the GUI can use this type
/// without transitively compiling the mDNS / crypto stack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// Stable machine UUID from the peer's `id` TXT key.
    pub machine_id: String,
    /// User-facing display name (mDNS instance name).
    pub display_name: String,
    /// Reachable host (resolved IP, or `hostname.local`).
    pub host: String,
    /// Port of the Synbad daemon on the peer — used for pairing handshake
    /// and (eventually) config sync.
    pub service_port: u16,
    /// Port of the Synergy/Deskflow Core on the peer — used when the GUI
    /// adds this peer to its screen layout. Zero if the peer didn't
    /// advertise one.
    pub core_port: u16,
    /// Port the peer's `synbadd` listens on for **config sync** sessions.
    /// Separate from `service_port` (which carries pairing) so the sync
    /// listener can be kept lifecycle-independent from pairing. Zero if
    /// the peer didn't advertise one — sync to that peer isn't possible.
    #[serde(default)]
    pub sync_port: u16,
    /// Port the peer's `synbadd` listens on for the **audio bridge**
    /// signaling protocol. Zero if the peer hasn't enabled audio or
    /// didn't advertise one — outbound audio dial is skipped in that
    /// case. Same kind of role as `sync_port` (per-protocol port,
    /// lifecycle-independent from pairing).
    #[serde(default)]
    pub audio_port: u16,
    /// Short public-key fingerprint — what the user compares at pair time.
    pub fingerprint: String,
    /// Synbad discovery protocol version advertised by the peer.
    pub protocol_version: u32,
    /// Short hash of the peer's `VersionedConfig` head, as advertised
    /// in the `cfg` TXT key. Empty if the peer hasn't advertised one. The
    /// daemon uses this to detect divergence and trigger a pull-sync.
    #[serde(default)]
    pub config_head: String,
}

pub mod client;
pub mod log_parse;
#[cfg(feature = "tokio-server")]
pub mod server;

/// Top-level message exchanged on the IPC socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Return the daemon's current status and a tail of recent log lines.
    GetStatus,
    /// Return the current Synbad config.
    GetConfig,
    /// Replace the Synbad config. The daemon persists it, regenerates the
    /// Core `.conf`, and restarts the Core if it was running.
    SetConfig { config: Config },
    /// Spawn the Core process (according to current config role).
    Start,
    /// Terminate the Core process.
    Stop,
    /// Stop and start the Core process.
    Restart,
    /// Convert this connection to a streaming subscription. The server
    /// replies once with `Response::Ok`, then pushes `Event` messages until
    /// the client disconnects.
    Subscribe,
    /// Return all peers currently visible via mDNS. Peers are unsorted —
    /// the GUI sorts for display.
    ListPeers,
    /// Get the local machine's identity (UUID + user-facing fingerprint).
    /// Used by the GUI to render "this is us" / the fingerprint to share
    /// during pairing.
    GetLocalIdentity,
    /// Begin a pairing session with the discovered peer named by
    /// `machine_id`. The daemon dials the peer's Synbad service port and
    /// runs the handshake; once the SAS is computed, it emits
    /// `Event::PairingProposed`, then waits for the user's
    /// `ConfirmPairing` response.
    StartPairing { machine_id: String },
    /// Reply to a `PairingProposed` event with the user's verdict.
    ConfirmPairing { session_id: String, accept: bool },
    /// Return all peers the user has paired with.
    ListTrustedPeers,
    /// Forget a previously-paired peer.
    RevokeTrust { machine_id: String },
    /// Enumerate available audio input and output devices on the local
    /// machine. The GUI uses the result to populate device-picker
    /// dropdowns in the Audio tab.
    ListAudioDevices,
    /// Replace the audio sub-section of the config without touching the
    /// rest of it. Cheaper than `SetConfig` when the user is just
    /// toggling audio on/off, since it won't restart the Core.
    SetAudioConfig { config: AudioConfig },
    /// Return a snapshot of per-peer audio session status (sending /
    /// receiving / RTT / last error).
    GetAudioStatus,
    /// Stop the Core process and terminate the daemon. The daemon replies
    /// `Response::Ok`, then exits. Sent by the GUI when the user quits so a
    /// GUI-spawned `synbadd` doesn't outlive the window.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status {
        state: DaemonState,
        recent_log: Vec<String>,
    },
    Config {
        config: Config,
    },
    Peers {
        peers: Vec<DiscoveredPeer>,
    },
    LocalIdentity {
        machine_id: String,
        fingerprint: String,
    },
    TrustedPeers {
        peers: Vec<TrustedPeer>,
    },
    AudioDevices {
        input: Vec<AudioDeviceInfo>,
        output: Vec<AudioDeviceInfo>,
    },
    AudioStatus {
        peers: Vec<PeerAudioStatus>,
    },
    Error {
        message: String,
    },
}

/// Per-peer trust state, surfaced alongside [`DiscoveredPeer`] in the
/// daemon's reply. The wire-side peer struct stays trust-free so the
/// trust decision can't be spoofed by the peer itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// We've seen this peer but the user hasn't paired with it yet.
    #[default]
    Unverified,
    /// The user has confirmed the fingerprint; this peer's public key is
    /// stored locally and may participate in input sharing.
    Trusted,
}

/// A peer the user has paired with. The struct intentionally only carries
/// public material — the local secret key never leaves `Identity` in
/// `synbad-discovery`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedPeer {
    pub machine_id: String,
    pub display_name: String,
    /// Hex-encoded ed25519 public key, 64 chars.
    pub public_key_hex: String,
    /// Cached fingerprint for fast display (rederivable from pubkey).
    pub fingerprint: String,
    /// Unix seconds at the moment trust was established.
    pub paired_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A line of stderr/stdout from the supervised Core process.
    Log { line: String },
    /// The daemon's view of the Core process state changed.
    State { state: DaemonState },
    /// The Synbad config was updated (by us, or by an external editor — the
    /// daemon also watches the file).
    ConfigChanged,
    /// A remote screen completed the Synergy/Deskflow handshake. Derived
    /// by parsing Core stderr — see [`log_parse`].
    PeerConnected { name: String },
    /// A remote screen disconnected.
    PeerDisconnected { name: String },
    /// The cursor moved onto a different screen (server role only; emitted
    /// for both local and remote screens).
    ActiveScreen { name: String },
    /// A new mDNS peer became visible on the LAN. Does not imply trust —
    /// the user must still confirm a fingerprint before this peer can
    /// participate in input sharing.
    PeerDiscovered { peer: DiscoveredPeer },
    /// A previously-visible mDNS peer is gone (TTL expiry or goodbye).
    PeerLost { machine_id: String },
    /// A pairing handshake has reached the user-confirmation step. The
    /// GUI MUST surface `verification_code` to the user and reply with
    /// `Request::ConfirmPairing` carrying `accept: true|false`.
    PairingProposed {
        session_id: String,
        peer_machine_id: String,
        peer_display_name: String,
        peer_fingerprint: String,
        verification_code: String,
    },
    /// Pairing completed successfully — the peer is now in
    /// [`TrustedPeerStore`](crate::TrustedPeer)'s namespace.
    PairingCompleted { peer: TrustedPeer },
    /// Pairing did not complete (peer declined, signature failure,
    /// timeout, network drop). `session_id` may be empty for failures
    /// that occur before a session id is assigned.
    PairingFailed { session_id: String, reason: String },
    /// The user revoked trust for a previously-paired peer.
    TrustRevoked { machine_id: String },
    /// A config-sync session with `peer_machine_id` started. Emitted for
    /// both inbound (peer connected to us) and outbound (we dialed peer).
    SyncStarted {
        peer_machine_id: String,
        direction: SyncDirection,
    },
    /// A merge with `peer_machine_id` completed; `updated` is `true` iff
    /// at least one local field was overwritten by the peer's state.
    SyncCompleted {
        peer_machine_id: String,
        direction: SyncDirection,
        updated: bool,
        new_head: String,
    },
    /// A sync session failed before completing — connection error,
    /// signature failure, untrusted peer, etc.
    SyncFailed {
        peer_machine_id: String,
        direction: SyncDirection,
        reason: String,
    },
    /// Local audio device set changed (plug/unplug). The GUI should
    /// re-issue `Request::ListAudioDevices` to refresh dropdowns.
    AudioDevicesChanged,
    /// Per-peer audio session status update — push from the bridge as
    /// connections come and go.
    AudioPeerStatus { status: PeerAudioStatus },
    /// Audio subsystem error. `peer` is `None` for global errors
    /// (e.g. cpal initialization failed), `Some(machine_id)` for a
    /// peer-scoped failure.
    AudioError {
        peer: Option<String>,
        message: String,
    },
}

/// Whether a sync session was initiated by us or accepted from a peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncDirection {
    Inbound,
    Outbound,
}

/// One audio input or output device as the GUI sees it. Mirrors the
/// fields cpal exposes, with `is_loopback` flagged for inputs that
/// actually capture system audio (PipeWire `.monitor`, WASAPI loopback).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub native_sample_rate: u32,
    pub channels: u16,
    /// True iff this is a monitor / loopback input — relevant for the
    /// "client speakers → server" direction. Always `false` for outputs.
    #[serde(default)]
    pub is_loopback: bool,
}

/// Snapshot of one peer's audio session state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerAudioStatus {
    pub machine_id: String,
    pub display_name: String,
    pub sending_to_peer: bool,
    pub receiving_from_peer: bool,
    /// Most recent round-trip estimate from RTCP receiver reports.
    #[serde(default)]
    pub rtt_ms: Option<u32>,
    /// Sticky error string, cleared once a session re-establishes.
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonState {
    Stopped,
    Starting,
    Running { pid: u32 },
    Crashed { exit_code: Option<i32> },
}

impl DaemonState {
    pub fn is_running(&self) -> bool {
        matches!(self, DaemonState::Running { .. } | DaemonState::Starting)
    }
}
