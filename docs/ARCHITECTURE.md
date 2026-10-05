<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Architecture

## Guiding principle

Synbad is a **GUI + LAN orchestration layer** around the unmodified open-source
Synergy Core. We do not fork or patch the Core. This keeps the Core swappable,
minimizes C++ build pain, and keeps the licensing boundary clean (see
[LICENSING.md](LICENSING.md)).

## Components

```
+-----------------------------------------------------------+
|  Synbad GUI (Rust)                                         |
|  - tray icon + config window                               |
|  - screen-layout editor                                    |
|  - shows discovered peers, applies synced config           |
+----------------------+------------------------------------+
                       | spawns + supervises (child process)
                       | generates synbad.conf
                       v
+-----------------------------------------------------------+
|  Synergy Core binaries (synergys / synergyc, unmodified)   |
|  - input capture/injection, clipboard, wire protocol       |
+-----------------------------------------------------------+
                       ^
                       | local IPC socket (log/status/control)
                       |
+----------------------+------------------------------------+
|  Synbad daemon (Rust)                                       |
|  - LAN auto-discovery (mDNS/DNS-SD)                          |
|  - LAN config sync (peer-to-peer)                           |
|  - Core process supervision                                 |
+-----------------------------------------------------------+
```

## Core integration strategy

We adopt **process orchestration** (the same model the reference Qt GUI uses),
not FFI or linking:

1. Synbad generates the Core config file (screen layout) from synced state.
2. The Synbad daemon spawns `synergys`/`synergyc` as child processes with the
   appropriate CLI args.
3. Synbad connects to the Core's local IPC socket for logs, status, and
   restart/reload control.

Consequences:

- No C++ in the Synbad binary; no `bindgen`/shim maintenance.
- Loose coupling — arguably *mere aggregation* for GPL purposes, though Synbad
  is intended to be GPLv2-compatible regardless.
- The Core can be upgraded independently.

A future phase *may* add a native-Rust protocol implementation to drop the
Core-binary dependency entirely; that is out of scope for the initial release.

## Audio bridge (optional sidecar)

`synbad-audio` is a self-contained subsystem that runs alongside the Core
wrapper. It uses [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) for
RTP/DTLS/SRTP and [`cpal`](https://github.com/RustAudio/cpal) for device
I/O. The signaling channel reuses the same authenticated, encrypted
transport (`synbad-crypto`) and trust store (`synbad-discovery`) that
already back pairing and config-sync, with its own listener port and
protocol domain (`b"synbad-audio-v1"`). `ice_servers` is empty —
host-candidates only on the LAN. See [AUDIO.md](AUDIO.md).

## Process model

- **`synbad`** — GUI application (user session). Talks to `synbadd` over a
  local socket.
- **`synbadd`** — background daemon: owns discovery, config sync, and Core
  supervision. Runs per-user (no root needed for the common case).
- **Core binaries** — launched and supervised by `synbadd`.

Splitting GUI from daemon lets discovery/sync keep running headless and keeps
the GUI restartable without dropping input-sharing sessions.

## Workspace layout

```
crates/
  synbad-gui/        # Rust GUI (egui)
  synbadd/           # daemon: supervision + discovery + sync
  synbad-discovery/  # mDNS/DNS-SD service (see DISCOVERY.md)
  synbad-config/     # config model, serialization, Core .conf generation
  synbad-sync/       # LAN peer-to-peer config sync (see CONFIG-SYNC.md)
  synbad-crypto/     # authenticated, encrypted transport
  synbad-audio/      # LAN audio bridge (see AUDIO.md)
  synbad-ipc/        # GUI <-> daemon IPC, and Core IPC client
  synbad-update/     # in-app update checks
docs/
```

Boundaries to respect: the **config model is the single source of truth**;
discovery feeds peers into it, sync replicates it, and the Core `.conf` is a
*generated artifact* — never hand-edited at runtime.

## Connection recovery

The daemon retains the user's Start/Stop intent across temporary failures.
Recovery is automatic while sharing is started; an explicit Stop cancels
Core restarts and tears down audio. The last persisted config and trust
store remain available during an outage.

| Connection | Failure detection and recovery |
|------------|--------------------------------|
| GUI → daemon IPC | Connect attempts are bounded to 1 s; request reads/writes to 5 s each. Reconnect uses 500 ms–10 s exponential backoff. Subscription acknowledgements precede events; lost broadcast events close the stream so the GUI fetches fresh config, peer, audio, and Core status snapshots. Mutating commands are not replayed after an ambiguous response failure. |
| Core process | Exits, client disconnects and failure to connect/open a listener within 30 s trigger restart. Five failures open a cooldown of 60 s doubling to 300 s. Only 60 s of actual readiness or explicit Start/Restart resets retries. User Stop cancels retries. |
| Discovery and LAN listeners | A 5 s reconcile retries failed pairing, sync, and audio binds, detects ended listener tasks, applies port changes, and refreshes mDNS endpoints/config heads. TCP listeners prefer IPv4/IPv6 dual stack with IPv4 fallback; outbound dials race up to 16 discovered addresses under one connect deadline. |
| Config sync | One outbound session per peer; failed sessions retry with capped 1–60 s backoff on the 5 s reconcile tick. Success is tied to the local head at dial time so edits made during a session receive another push. Sessions have a 10 s budget after connecting. |
| Pairing | Connect, transport handshake, and Hello exchange have 10 s limits; writes have a 5 s limit. The session has a 120 s budget after connect for both users' confirmations. Disconnects and peer declines fail the pending session. Pairing requires a fresh user action after failure. |
| Audio | Connect/authentication have 3 s/5 s budgets; SDP negotiation has a 10 s deadline. Initial media setup has a 15 s budget; disconnected ICE receives a 5 s grace window. Ended drivers are reaped every 250 ms and the designated dialing peer reconnects with capped backoff. Routing/device edits rebuild affected sessions; gains update live. |

IPC frames are limited to 1 MiB. Encrypted frames allow 256 KiB of plaintext
plus the 16-byte authentication tag. Partial frame reads retain their
progress when an audio timer or media packet interrupts them. Each IPC,
sync, and audio accept loop owns at most 64 connection tasks, and aborting
its owner closes the associated streams. Pairing accepts are also bounded
to 64 active inbound sessions. Accept errors back off rather than spinning.

Tests exercise fragmented/cancelled reads, oversized and truncated frames,
subscription ordering and lag, stalled IPC, duplicate and stale socket
binding and concurrent daemon startup, unavailable addresses, dual-stack audio,
remote close, negotiation
timeouts, and real daemon recovery after startup port contention and
repeated Core failures. Real daemon tests use isolated XDG directories and
a fake Core on Linux. Cross-platform socket behavior and actual two-machine
Wi-Fi loss, suspend/resume, and native audio-device failures still need
platform testing; recovery cannot prevent the interruption itself.
