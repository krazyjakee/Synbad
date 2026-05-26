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
