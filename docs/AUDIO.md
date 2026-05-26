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

- **Echo cancellation / noise suppression** — WebRTC's AEC lives in
  libwebrtc (C++); neither [str0m](https://github.com/algesten/str0m) nor
  any other pure-Rust stack ships an equivalent. Use OS-level alternatives
  (PipeWire `echo-cancel`, Win11 Voice Clarity).
- **Multi-channel / stereo** — mono only. Each direction is a single mic
  stream; stereo would double bandwidth for no gain on speech.
- **Codec negotiation beyond Opus** — we offer Opus only. str0m supports
  PCMU/PCMA too but they're 8 kHz telephony-grade and not worth the
  branching for a LAN bridge.
- **Auto follow-the-focused-screen routing** — initial UX is static
  per-peer toggles. Hooking it into the Core's "active screen" event needs
  a new IPC plumbing pass.
- **NAT traversal** — by design. Synbad is LAN-only, so the ICE candidate
  set is host-only and there are no STUN/TURN servers.

## Architecture

```
+--------------------+        TCP (synbad-crypto authenticated)        +--------------------+
| synbadd (peer A)   |  <----------------- signaling ----------------> | synbadd (peer B)   |
|  ├─ AudioBridge    |        SDP offer/answer (ICE candidates           |  ├─ AudioBridge    |
|  ├─ cpal capture   |         embedded in the SDP — no trickle)         |  ├─ cpal capture   |
|  └─ cpal playback  |        (length-prefixed JSON, see below)          |  └─ cpal playback  |
|         │ │        |                                                   |         │ │        |
|         ▼ ▲        |                                                   |         ▼ ▲        |
|     +-------+      |  direct UDP (DTLS+SRTP, Opus 48k/mono, PT 111)    |     +-------+      |
|     | str0m | <==================== media =====================>      | str0m |             |
|     +-------+      |                                                   |     +-------+      |
+--------------------+                                                   +--------------------+
```

- **Transport: [`str0m`](https://github.com/algesten/str0m) 0.19.** A
  sans-I/O WebRTC stack in pure Rust (no libwebrtc/C++). We feed it
  network bytes + a clock and it emits `Output::Transmit` / `Event::
  MediaData` events; the session task in `session.rs` is the I/O wrapper
  around a `tokio::net::UdpSocket`. Crypto runs through `aws-lc-rs`
  (enabled via the `aws-lc-rs` Cargo feature). We migrated here from
  webrtc-rs 0.17 in v0.1.5 because that stack derived mismatched
  DTLS-SRTP keys in our bidirectional bring-up and every inbound packet
  failed AEAD authentication; str0m completes the same handshake
  cleanly (see `tests/str0m_validation.rs`).
- **Codec: Opus 48 kHz mono, ~64 kbps.** libopus via the `opus` crate,
  20 ms frames (960 samples), application = `Voip`. 64 kbps is well
  below libopus's 510 kbps ceiling but comfortable for mono speech;
  thinning starts around 32 kbps. Opus is the only str0m audio codec
  well-suited to LAN microphone audio — PCMU/PCMA are 8 kHz telephony.
- **Audio I/O: `cpal` 0.17.** Cross-platform shim over ALSA / CoreAudio /
  WASAPI. Capture and playback each run on cpal's real-time callback
  thread and shuttle 20 ms (`960 i16`) PCM frames to/from the session
  task via a lock-free `ringbuf` SPSC queue. Devices whose native sample
  rate isn't 48 kHz are resampled with `rubato` before encode / after
  decode.
- **Signaling: reuses `synbad-crypto::CipherStream`.** The same
  authenticated + AEAD-encrypted (ChaCha20-Poly1305) transport that backs
  pairing and config sync; an audio signaling session is just an
  additional listener on its own TCP port (`audio.signal_port`,
  default 24852).

### Signaling protocol

Each peer runs a listener on `audio.signal_port`. Inbound TCP connections
go through `synbad-crypto`'s authenticated handshake (`HandshakeMode::
Authenticated`); an untrusted peer is rejected before any audio bytes
flow. After the handshake the connection becomes an `AudioSignal` channel:

```jsonc
{ "kind": "offer",  "session_id": "...", "sdp": "..." }
{ "kind": "answer", "session_id": "...", "sdp": "..." }
{ "kind": "close",  "session_id": "...", "reason": "..." }
```

There are no `ice_candidate` / `ice_complete` messages: str0m bundles
every host candidate into the SDP itself (driven by
`add_local_candidate` before `sdp_api().apply()`), so the two peers
have nothing to trickle. Keeping the protocol to three variants
removes a class of "candidate arrived before remote-description" races
that the earlier webrtc-rs design had to defend against.

Audio signaling and config sync each bind their own TCP port, so a
misconnected client lands on a listener that simply rejects the
opposite protocol's JSON. The cipher transport itself is the same
handshake the rest of Synbad uses; cross-protocol confusion is
prevented by port separation and application-layer schema checks.

### Why no STUN/TURN

Synbad peers discover each other via mDNS on the same LAN and exchange
ICE host candidates (embedded in the SDP) over the trusted signaling
channel. Internet NAT traversal is out of scope, so we never call any
STUN/TURN servers — the only candidates str0m sees are the host
candidates we add for each local interface. This also keeps the bridge
offline-capable.

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
| `synbad-audio/src/session.rs` | Per-peer `str0m::Rtc` + UDP socket + SDP signaling state machine. |
| `synbad-audio/src/rtc.rs` | `str0m::Rtc` builder + Opus encoder/decoder helpers (48 kHz mono, 20 ms frames). |
| `synbad-audio/src/protocol.rs` | `AudioSignal` wire types (Offer / Answer / Close). |
| `synbad-audio/src/capture.rs` | cpal input → 20 ms `i16` PCM frames (resampled to 48 kHz via `rubato` when needed). |
| `synbad-audio/src/playback.rs` | `i16` PCM frames → cpal output (resampled from 48 kHz when needed). |
| `synbad-audio/src/devices.rs` | Device enumeration + loopback detection. |
| `synbadd/src/audio.rs` | TCP listener; hands authenticated streams to the bridge. |
| `synbadd/src/supervisor/mod.rs` | Owns the bridge handle and the listener task. |
| `synbadd/src/supervisor/requests.rs` | IPC handlers for the new Audio* requests. |
| `synbad-gui/src/app/views.rs` | `draw_audio` — the Audio tab UI. |
| `synbad-config/src/lib.rs` | `AudioConfig` + `PeerAudioRouting` schema. |
| `synbad-ipc/src/lib.rs` | `AudioDeviceInfo`, `PeerAudioStatus`, audio Request/Response/Event variants. |
