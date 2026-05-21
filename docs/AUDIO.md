<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Audio bridge

Synbad's audio bridge streams real-time audio between paired LAN peers as a
sidecar to the keyboard/mouse/clipboard share. When you're driving a remote
machine through Synbad you can hear its system audio on the machine you're
sitting at, and your microphone can be sent the other way — without
bolting on Discord, Zoom, or platform-specific audio tunnels.

The bridge is **off by default** and entirely separate from the Deskflow
Core wire protocol. Toggling it doesn't disturb input sharing.

## Goals (v1)

| Goal | Status |
|------|--------|
| Microphone on machine A → speakers on machine B | ✅ Wired |
| System audio output on machine B → speakers on machine A (loopback) | ✅ Linux/Windows; ⚠ macOS needs [BlackHole](#macos) |
| LAN-only — no STUN, no TURN, no cloud signaling server | ✅ |
| Authenticated + encrypted end-to-end, reusing pairing trust | ✅ |
| Per-host device selection + global enable/disable in the GUI | ✅ |
| Per-peer routing overrides | 🟡 Schema in place; UI exposes globals only in v1 |

## Non-goals (v1, deliberately deferred)

- **Opus encoding** — we ship raw L16 PCM (~1.5 Mbps stereo). Cheap on a
  LAN; saves a libopus C dep. Revisit when bandwidth becomes a complaint.
- **Echo cancellation / noise suppression** — WebRTC's AEC lives in
  libwebrtc (C++), not in [webrtc-rs](https://github.com/webrtc-rs/webrtc).
  Use OS-level alternatives (PipeWire `echo-cancel`, Win11 Voice Clarity).
- **Multi-channel** — mono only.
- **Auto follow-the-focused-screen routing** — initial UX is static
  per-peer toggles. Hooking it into the Core's "active screen" event needs
  a new IPC plumbing pass.
- **NAT traversal** — by design. Synbad is LAN-only.

## Architecture

```
+--------------------+        TCP (synbad-crypto authenticated)        +--------------------+
| synbadd (peer A)   |  <----------------- signaling ----------------> | synbadd (peer B)   |
|  ├─ AudioBridge    |     SDP offer/answer + ICE candidates            |  ├─ AudioBridge    |
|  ├─ cpal capture   |     (newline-delimited JSON, see below)          |  ├─ cpal capture   |
|  └─ cpal playback  |                                                   |  └─ cpal playback  |
|         │ │        |                                                   |         │ │        |
|         ▼ ▲        |                                                   |         ▼ ▲        |
|     +-------+      |    direct UDP (DTLS+SRTP, RTP payload type 96)    |     +-------+      |
|     | wrtc  | <==================== media =====================>      | wrtc  |             |
|     +-------+      |                                                   |     +-------+      |
+--------------------+                                                   +--------------------+
```

- **Transport: `webrtc-rs` 0.17.** Pure Rust, no libwebrtc/C++. Picks up
  ICE host candidates from local interfaces, runs DTLS+SRTP, packetizes
  audio into RTP.
- **Codec: L16/48k/mono** (RFC 3551 §4.5.7) registered on `MediaEngine`
  as payload type 96. webrtc-rs ships built-in payloaders for Opus and
  G722 only; L16 packetization is hand-rolled on top of the `rtp` crate.
- **Audio I/O: `cpal` 0.17.** Cross-platform shim over ALSA / CoreAudio /
  WASAPI. Capture and playback each run on cpal's real-time thread and
  shuttle 20 ms (`960 i16`) frames to/from tokio via bounded channels.
- **Signaling: reuses `synbad-crypto::CipherStream`.** The same
  authenticated + AEAD-encrypted transport that backs pairing and config
  sync; an audio signaling session is just an additional listener on its
  own TCP port (`audio.signal_port`, default 24852).

### Signaling protocol

Each peer runs a listener on `audio.signal_port`. Inbound TCP connections
go through `synbad-crypto`'s authenticated handshake (`HandshakeMode::
Authenticated`); an untrusted peer is rejected before any audio bytes
flow. After the handshake the connection becomes an `AudioSignal` channel:

```jsonc
{ "kind": "offer",          "session_id": "...", "sdp": "..." }
{ "kind": "answer",         "session_id": "...", "sdp": "..." }
{ "kind": "ice_candidate",  "session_id": "...", "candidate": "...",
  "sdp_mid": "0", "sdp_mline_index": 0 }
{ "kind": "ice_complete",   "session_id": "..." }
{ "kind": "close",          "session_id": "...", "reason": "..." }
```

Audio signaling and config sync each bind their own TCP port, so a
misconnected client lands on a listener that simply rejects the
opposite protocol's JSON. The cipher transport itself is the same
handshake the rest of Synbad uses; cross-protocol confusion is
prevented by port separation and application-layer schema checks.

### Why no STUN/TURN

Synbad peers discover each other via mDNS on the same LAN and exchange
ICE host candidates over the trusted signaling channel. Internet NAT
traversal is out of scope, so the `RTCConfiguration.ice_servers` list
is empty. This also keeps the bridge offline-capable.

## Configuration

The `[audio]` section in `synbad.toml`:

```toml
[audio]
enabled = true
input_device = "Built-in Microphone"   # or omit for OS default
output_device = "Built-in Speakers"    # or omit for OS default
signal_port = 24852

# Optional per-peer override. Absent = bidirectional default whenever
# `enabled = true`. Useful for muting one direction (or the whole link)
# for a specific peer without changing the master switch.
[audio.per_peer."peer-uuid-abc"]
enabled = true
send_to_peer = true
receive_from_peer = false
```

`enabled` is hot-reloadable — flipping it from the GUI brings the
listener and bridge up (or tears them down) live, no restart needed.
Device picks and the per-peer map also reload live.

## Platform notes

### Linux

- ALSA, PulseAudio, and PipeWire all work through cpal's ALSA backend.
- Loopback (client-speakers → server) uses the `<sink>.monitor` source
  that PipeWire/PulseAudio exposes for every sink. cpal lists these as
  ordinary input devices; the GUI flags them as `(loopback)`.
- Build dep: `libasound2-dev`. Runtime dep on `libasound2` (already
  declared in the `.deb` metadata once this PR lands).

### Windows

- WASAPI. Loopback capture works natively — pick the relevant output
  device as the *input* in the GUI and cpal opens it in loopback mode.
- No additional manifest changes are required — WASAPI access doesn't
  prompt the user.

### macOS

- CoreAudio. Microphone capture works natively but **requires the
  `NSMicrophoneUsageDescription` key in `Info.plist`** (declared in
  `dist/macos/Info.plist`) and the user must approve the TCC prompt the
  first time `synbadd` opens an input stream.
- **Loopback** is not exposed by any first-party CoreAudio API older
  than macOS 13's ScreenCaptureKit (which cpal doesn't surface). To send
  Mac system audio to a peer, install a virtual audio device such as
  [BlackHole](https://github.com/ExistentialAudio/BlackHole) and select
  it as the input device in the Audio tab. Without it, the GUI shows a
  clear error rather than silently failing.

## Threat model

The audio path inherits the existing pairing/trust model:

- The signaling channel runs over `synbad-crypto`'s authenticated
  ChaCha20-Poly1305 transport. An unpaired peer is rejected at handshake.
- DTLS-SRTP keys are negotiated through that authenticated channel, so a
  network attacker can neither inject media nor decrypt it.
- Revoking trust for a peer terminates the audio session within a few
  seconds (the next signaling read on the now-broken trust returns an
  error and the bridge tears the session down).
- No microphone audio is sent before the user has explicitly enabled
  the audio bridge (`[audio] enabled = true`) and confirmed the pairing.

## Implementation map

| Crate / file | Responsibility |
|--------------|----------------|
| `synbad-audio/src/bridge.rs` | Top-level orchestration; command/event channels. |
| `synbad-audio/src/session.rs` | Per-peer PeerConnection + signaling state machine. |
| `synbad-audio/src/rtc.rs` | webrtc-rs `MediaEngine`/`API` setup, L16 codec registration. |
| `synbad-audio/src/protocol.rs` | `AudioSignal` wire types + domain string. |
| `synbad-audio/src/capture.rs` | cpal input → 20 ms `i16` PCM frames. |
| `synbad-audio/src/playback.rs` | `i16` PCM frames → cpal output. |
| `synbad-audio/src/devices.rs` | Device enumeration + loopback detection. |
| `synbadd/src/audio.rs` | TCP listener; hands authenticated streams to the bridge. |
| `synbadd/src/supervisor/mod.rs` | Owns the bridge handle and the listener task. |
| `synbadd/src/supervisor/requests.rs` | IPC handlers for the new Audio* requests. |
| `synbad-gui/src/app/views.rs` | `draw_audio` — the Audio tab UI. |
| `synbad-config/src/lib.rs` | `AudioConfig` + `PeerAudioRouting` schema. |
| `synbad-ipc/src/lib.rs` | `AudioDeviceInfo`, `PeerAudioStatus`, audio Request/Response/Event variants. |
