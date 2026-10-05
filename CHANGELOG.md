<p align="center">
  <img src="assets/logo.svg" alt="Synbad" width="520">
</p>

# Changelog

All notable changes to Synbad land here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches 1.0.

## [Unreleased]

## [0.1.11] - 2026-10-05

### Fixed
- macOS releases bundle a native, patched Deskflow Core for both arm64 and
  Intel. Socket callbacks are joined before Carbon event-queue teardown;
  early events are rejected safely before the queue is initialized.
- A Core that stays alive without connecting or opening its server listener
  is restarted after 30 seconds. Five consecutive failures open a cooldown
  of 60 seconds, doubling to a five-minute cap. Retry budgets reset only
  after a minute of readiness or an explicit Start/Restart.
- Core stdout/stderr and daemon diagnostics persist in size-capped logs,
  including final refusal messages and signal exits. Older oversized logs
  are trimmed on open.
- mDNS excludes macOS utun, AWDL, LLW and loopback interfaces, caps browse
  refresh at 30 seconds, reacts to LAN address changes and deduplicates
  unchanged peer announcements.
- Pairing commits and syncs `trusted-peers.json` before reporting success;
  failed saves no longer change in-memory trust.
- macOS app and helper signatures use stable identifiers, with Developer ID
  signing when configured and ad-hoc signing otherwise. Updates replace the
  complete verified app, including its native Core and signature.

## [0.1.10] - 2026-10-01

### Fixed
- Windows IPC commands wait for replies instead of treating an empty
  nonblocking named-pipe read as a disconnect.
- Connection recovery across IPC, config sync, pairing, and audio: bounded
  requests/negotiation, automatic retries after listener bind failures,
  safe subscription recovery after event loss, and cleanup of ended audio
  sessions and owned connection tasks.
- Fragmented encrypted signaling reads retain progress when interrupted by
  media/timer events; maximum-sized encrypted frames include tag overhead.
- Server Core startup keeps retrying after repeated failures, allowing
  recovery from temporary port contention without another Start click.
- LAN dialing tries all discovered interfaces within one deadline and
  supports IPv4/IPv6 listeners. GUI reconnection restores current peer and
  audio status instead of retaining stale connection indicators.

## [0.1.9] - 2026-09-30

### Security
- `rustls` 0.23.40 → 0.23.45 (RUSTSEC-2026-0285, TLS 1.3 handshake messages
  accepted across encryption levels). (#86)
- Dropped the unused `ringbuf` dependency from `synbad-audio`
  (RUSTSEC-2026-0293). (#85)
- RUSTSEC-2026-0257 (`webbrowser`, pinned by egui-winit 0.27) acknowledged in
  the audit ignore lists; the GUI now only opens release-notes links shaped
  `https://github.com/…`, so the vulnerable path is unreachable. (#82)
- Moved off yanked `chacha20` 0.10.1.

### Changed
- Sharing now starts automatically when the daemon launches, instead of
  sitting at "stopped" until Start is clicked. An explicit **Stop** is
  remembered (a per-machine `user-stopped` marker in the state dir, never
  synced) and honoured across restarts until the next Start.
- Client reconnect backoff is capped at 10 s (was 30 s) so a client picks the
  server back up within seconds of it returning. It still retries forever.
- The audio bridge now follows the run state: it comes up with Start (or the
  launch auto-start) and goes down with Stop, and toggling audio on or off
  takes effect live instead of asking for a daemon restart.

### Fixed
- ~100 ms whole-desktop frame hitch every 5 s on X11: the GUI's periodic
  monitor enumeration issued `RRGetScreenResources`, which makes Xorg re-probe
  every output while blocking all clients. It now uses `RRGetMonitors`, which
  reads the server's cached layout (~0.3 ms). (#65) Side effect: the
  `Xft.dpi` scale is read more tolerantly (whitespace, long resource
  databases) and virtual monitors without an output are no longer dropped, so
  a few setups may see corrected monitor sizes in the layout editor.
- A Core restart no longer fails when GitHub is unreachable (offline, network
  not up yet at boot, rate-limited) once the release-check cache expired —
  the pinned Deskflow build is used straight from the local cache. Failed
  Core resolution is now retried with backoff instead of stranding the daemon
  in "crashed".
- The daemon no longer blocks its event loop while waiting out a restart
  backoff (up to 30 s), so Stop/Restart and other GUI requests respond
  immediately while the client is reconnecting.
- Stopping `synbadd` with SIGTERM (systemd stop, logout, `kill`) or SIGHUP no
  longer orphans the Deskflow Core — it's now shut down cleanly, and on Linux
  the kernel also terminates it if the daemon dies hard (SIGKILL, OOM). An
  orphaned server kept the Core port, so the next daemon's server failed to
  bind and gave up.
- A Core that took longer than 2 s to exit during a stop/restart had its late
  exit applied to its replacement, flipping state to reconnecting and losing
  the handle needed to stop it. Exits are now matched by pid.
- Restarts triggered by a config change (e.g. a new server address) start
  with a fresh retry budget instead of inheriting a backed-off delay.
- A restart requested while the first-run Core download was in flight was
  dropped (and the GUI showed "stopped"); the Core then launched with the
  pre-edit layout. The generated Core config is now written right before
  spawning, from the live config.
- Failing to spawn the Core or write its generated config now retries with
  backoff instead of leaving sharing off with nothing pending.
- A crash mid-extraction of the Deskflow download no longer leaves a
  truncated binary that is reused forever; extraction is staged and moved
  into place only once complete.
- Ctrl-C arriving while the daemon was busy (e.g. stopping the Core) could be
  lost; the signal handlers are now installed once for the daemon's lifetime.
- The systemd user unit shipped in the `.deb` pointed at `~/.local/bin/synbadd`;
  it now targets `/usr/bin/synbadd`, and `install.sh` rewrites it for its own
  install location.

## [0.1.8] - 2026-07-18

### Changed
- Playback jitter buffer cap dropped from 200 ms to 60 ms with an explicit
  30 ms post-drop target, so the pump converges on the target instead of
  oscillating up to the cap. Knocks ~100 ms off steady-state perceived input
  lag without touching any other audio path; telemetry gains a
  `target_samples` field. (#49)
- Client role now retries the server **indefinitely** with capped exponential
  backoff instead of giving up after 3 reconnect attempts. A paired, enabled
  link is level-triggered: if the server reboots, hits a network blip, or
  simply starts after the client, the client recovers on its own with no
  restart. A new non-terminal `DaemonState::Reconnecting { attempt,
  next_retry_secs }` surfaces "reconnecting to server…" in the UI instead of a
  terminal "crashed". Server role is unchanged (still gives up after
  `MAX_FAST_FAILS` startup failures). The reconnect backoff now actually
  escalates across consecutive fast-fails — it was previously reset to the
  minimum on every spawn. (#50)

### Security
- `memmap2` 0.9.10 → 0.9.11 (RUSTSEC-2026-0186: unchecked pointer offset). (#68)
- `quinn-proto` 0.11.14 → 0.11.16 (RUSTSEC-2026-0185: unbounded out-of-order
  stream reassembly). (#67)
- `anyhow` is on 1.0.103, which carries the fix for RUSTSEC-2026-0190
  (`Error::downcast_mut` unsoundness). (#75)
- Added `.cargo/audit.toml` so `cargo audit` / the audit-check workflow agree
  with `deny.toml` on the set of transitive advisories with no in-range fix
  (GTK3 stack, `glib`, `paste`, `proc-macro-error`, `fxhash`, `audiopus_sys`,
  `ttf-parser`, `quick-xml` build/`xcb`-only reach). These are acknowledged and
  tracked to the next eframe / opus / str0m bump, not left unhandled.
  (#51–#62, #70–#74)

## [0.1.7] - 2026-05-26

### Added
- Audio bridge gains input and output volume sliders (0–200 % linear),
  applied live without restarting peer sessions. Persists alongside the
  existing audio device picks in `AudioConfig`. (#45)
- Periodic playback-buffer telemetry: every ~2 s the pump task emits a
  `debug` log with peak jitter-buffer depth (samples and ms), mpsc
  queue pressure, and cumulative dropped-sample count, so post-mortems
  on perceived audio latency have something concrete to point at. (#44)

## [0.1.6] - 2026-05-21

### Documentation
- README now points macOS users at [BlackHole](https://github.com/ExistentialAudio/BlackHole)
  (`brew install blackhole-2ch`) for routing system-audio output into the
  Synbad audio bridge, since CoreAudio doesn't expose a first-party
  loopback source on macOS &lt; 13.

## [0.1.5] - 2026-05-21

### Changed
- Audio bridge ported from `webrtc-rs` 0.17 to `str0m` 0.19. On-wire
  codec switched from a hand-rolled L16 carrier to Opus at 48 kHz mono,
  64 kbps. Bandwidth drops by an order of magnitude and the LAN session
  bring-up settles in ~140 ms on loopback.

### Fixed
- Two-machine audio between v0.1.4 hosts was broken because every
  inbound RTP packet failed AEAD/HMAC authentication immediately after
  DTLS-SRTP handshake completed — a key-derivation bug in `webrtc-rs`
  0.17 that survived several attempted workarounds (AES-CM-HMAC profile
  preference, deferred `add_track`, bundled-ICE SDP, mDNS off, loopback
  candidates on). Validated against `str0m` with a side-by-side
  bidirectional smoke test, then swapped the whole stack. The
  un-`#[ignore]`'d loopback integration test (`tests/loopback.rs`) is
  the regression net.
- macOS CI/release builds no longer fail on `audiopus_sys`'s bundled
  libopus cmake build (which doesn't compile on aarch64-apple-darwin).
  The workflows now `brew install opus pkg-config` so the crate picks
  up the system library and skips the bundled build.

### Notes
- **Wire-incompatible audio** with v0.1.4 and earlier — the codec
  changed. Pairing, sync, and input sharing remain wire-compatible, so
  mixed clusters keep working for everything except audio.
- New runtime dependency on `libopus`. Linux distros already ship it;
  the macOS release bundle pulls it via brew during build.

## [0.1.4] - 2026-05-21

### Changed
- Audio bridge routing model simplified to "bidirectional when enabled."
  Dropped the `send_mic_to_peers` / `receive_peer_audio` per-side
  toggles from `AudioConfig` and the GUI Audio tab — they had to be
  ticked on both ends for a session to form, which never matched the
  user model of "flip Enabled, audio works." The master switch is now
  the only thing needed on each end; `[audio.per_peer."<machine-id>"]`
  remains as the way to mute one direction or a whole link for a
  specific peer. Legacy TOMLs that still carry the removed keys load
  cleanly (regression test covers it).

### Fixed
- Per-peer status table in the Audio tab stopped showing stale rows.
  `AudioEvent::SessionClosed` was previously dropped on the floor in
  the supervisor with a comment claiming the GUI would see the change
  from "the next snapshot" — but the GUI only ever requests one
  snapshot, on tab open. A new `Event::AudioPeerRemoved { machine_id }`
  propagates closures so disconnected peers drop out of the table, and
  the reconcile loop's redial repopulates them when the session comes
  back up.
- Per-peer table now renders without depending on the system font's
  unicode coverage. Replaced the `✓ → ←` glyphs (which rendered as
  tofu boxes on some Linux setups) with plain ASCII column headers
  (`Peer | Send | Receive | State`) and `on`/`off` cells. The new
  State column surfaces `last_error` in red, `connected` in green, or
  `negotiating…` so the row carries real information instead of
  flashing an empty Error column.

## [0.1.3] - 2026-05-21

### Fixed
- Auto-update on Linux/macOS appeared to update only the daemon. The
  on-disk swap was actually doing both binaries, but the running GUI
  process stayed on the old inode: closing the window only hides to
  tray, and re-launching forwards to the still-running process via the
  single-instance socket. The post-install dialog now offers a
  **Relaunch now** button that bounces the daemon supervisor
  (`systemctl --user restart synbadd.service` on Linux,
  `launchctl kickstart -k gui/$UID/dev.synbad.synbadd` on macOS,
  `schtasks /End` + `/Run SynbadDaemon` on Windows), unlinks the
  single-instance socket, spawns a fresh `synbad-gui`, and exits — so
  both halves of the install actually take effect without the user
  having to chase down systemd / launchctl by hand.
- GUI flashed a misleading "could not connect to synbadd" error on cold
  start with autostart on, because the IPC loop tried to connect before
  spawning the daemon. The loop now pre-spawns the daemon at the top of
  the boot dance, holds a 5 s soft "Launching…" grace window for connect
  failures during that period, and surfaces real spawn failures with the
  attempted binary path (e.g. `could not launch synbadd at
  /usr/bin/synbadd: No such file or directory`) instead of looping the
  generic connect error.
- Client Core could stall on a silent "NOTE: disconnected from server"
  log line — modern `deskflow-core` keeps the process alive after the
  link drops without retrying, so the supervisor's exit-driven
  reconnect never fired. The supervisor now watches the log stream for
  that line (client role only), force-kills the child, and lets the
  existing `MAX_CLIENT_RECONNECTS = 3` path take over.
- Audio bridge is now level-triggered with retry instead of edge-only.
  A 5 s reconcile loop in the supervisor walks every visible peer and
  redials any that should have a session but don't; failed dials arm
  per-peer exponential backoff (1 s → 60 s cap), and `SessionClosed` /
  `PeerStatus` events evict liveness so a dropped session recovers
  within a tick or two. Toggling `audio.enabled` is hot-reloadable —
  the bridge and listener are built (or torn down) live instead of
  requiring a daemon restart.

### Changed
- Release workflow is faster: a single `cargo build -p synbadd -p
  synbad-gui` invocation replaces the two sequential builds, `mold` is
  used as the linker on Linux, and apt + Homebrew downloads are cached
  between runs. `rust-cache` is now scoped to tag pushes only so PR/CI
  activity can't evict release-target entries from the per-repo cache
  budget.

## [0.1.2] - 2026-05-21

### Fixed
- Audio playback dropped roughly 75% of received samples: the cpal
  output callback called `Vec::drain(..)` across the whole shared
  buffer but only consumed the per-callback slice, so the rest was
  discarded on drop. The drain range is now bounded to what's actually
  consumed, with a regression test. Replaces the 1 s clear-on-overrun
  with a 200 ms jitter cap that drops only the oldest samples down to
  half-cap, and removes a blocking `std::thread::sleep` from the
  bridge task.
- Toggling `audio.enabled` was a silent no-op until daemon restart.
  The supervisor now emits an `Event::AudioError` so the GUI banner
  explains the restart requirement instead of looking like the change
  took.
- Audio settings edits no longer fake an unsaved-changes state. The
  GUI was marking the config `dirty` after `SetAudioConfig`, so the
  daemon's `ConfigChanged` round-trip lit up Apply/Revert and the
  orange "remote config" banner for a change the user had just made.
- Audio toggles no longer bounce active input sharing.
  `core_inputs_differ` now zeroes `c.audio` alongside the existing
  `service_port` / `sync_port` exclusions, so an audio change doesn't
  trigger `stop_core()` + `start_core()`.

### Changed
- Client-role Core caps at 3 consecutive fast-fail reconnects instead
  of retrying forever. A child that survives past `FAST_FAIL_WINDOW`
  still resets the counter, so a transient mid-session drop gets a
  fresh budget of 3 — only a truly unreachable server gives up.
  Reconnect log lines include "(attempt N of 3)". Walks back the
  retry-indefinitely policy from 0.1.1.
- Audio tab: send/receive/device controls gray out when the master
  switch is off, matching their runtime behavior. The always-blank
  "RTT" column is removed from the per-peer status grid; `rtt_ms`
  stays on the IPC type for future RTCP wiring.
- `cargo-deny` audit is green again: extend the license allowlist for
  permissive transitive deps (0BSD, BSL-1.0, CDLA-Permissive-2.0,
  bzip2-1.0.6, OFL-1.1, LicenseRef-UFL-1.0), mark workspace crates as
  `publish = false` with `allow-wildcard-paths = true`, and ignore
  the unmaintained gtk-rs / proc-macro-error / fxhash advisories
  (all transitive through eframe / winit / webrtc with no upstream
  migration path right now).

### Removed
- Dead `SIGNAL_DOMAIN` constant from the audio bridge. AUDIO.md
  claimed it was mixed into the transport transcript but it wasn't;
  the doc paragraph now describes what actually keeps the audio and
  sync protocols apart (port separation + application-layer schema).

## [0.1.1] - 2026-05-20

### Added
- LAN audio bridge under [`crates/synbad-audio`](crates/synbad-audio/):
  streams mic + system audio between paired peers over webrtc-rs
  (RTP/DTLS/SRTP) with cpal device I/O, reusing the existing
  `synbad-crypto` authenticated transport for SDP/ICE signaling on a
  separate port. Includes rubato resampling to 48 kHz on both capture
  and playback paths, RFC 3551 §4.5.7 L16 packetisation, glare-rule
  offerer selection, and per-peer routing config. mDNS peers advertise
  `audio_port` in their TXT record so the supervisor knows when
  outbound dial is possible. Opt-in subsystem with GUI controls, IPC
  plumbing for device enumeration and per-peer status, and config-sync
  schema additions. On macOS, surfaces a `LoopbackUnavailable` error
  with a link to the BlackHole installer when no loopback-capable
  input device exists. Bumps workspace MSRV to 1.85 and adds
  `libasound2` to the Debian package depends.
- "Share clipboard" checkbox on the Settings tab, backed by a new
  top-level `clipboard_sharing: bool` on `Config` (default `true`,
  serde-defaulted so existing configs upgrade silently). Off-state
  emits `clipboardSharing = false` into the generated `synergy.conf`,
  which the Synergy/Deskflow Core honours to suppress clipboard relay
  in both directions. The field syncs across trusted peers via LWW
  and triggers a Core restart when toggled.

### Changed
- Layout editor is now monitor-aware: each `Screen` carries a
  `MonitorInfo` list (populated from `display-info` on startup and a
  5s reconcile pass) so screens are sized by the real bounding box of
  their monitors and multi-monitor setups draw per-display sub-rects.
  The reconcile loop also appends any trusted peer not already in
  `config.screens`, with an immediate trigger on `PairingCompleted`,
  so paired machines slot into the layout without manual intervention.
- Client-role Core now reconnects indefinitely instead of giving up
  after 5 sub-2-second exits. A fast-exit in client role is almost
  always "server unreachable" (DNS miss, server down, network drop) —
  exactly the case where the user wants us to keep retrying. Server
  role keeps the fast-fail cap since fast-exit there usually means
  port-in-use or missing libs that won't self-resolve. Each reconnect
  attempt records a user-visible log line so the GUI doesn't look
  frozen on the Crashed chip during the backoff window.

## [0.1.0-alpha.4] - 2026-05-20

### Changed
- Internal refactor: the supervisor and GUI app modules are now split into
  directories.
  [`crates/synbadd/src/supervisor.rs`](crates/synbadd/src/supervisor/)
  (1,319 lines) became `supervisor/{mod,requests,config_edit,core_proc}.rs`
  — one file per concern (event loop, IPC dispatch, config edits, Core
  child lifecycle + `build_command` tests).
  [`crates/synbad-gui/src/app.rs`](crates/synbad-gui/src/app/) (1,097
  lines) became `app/{mod,views}.rs`, separating state + the `eframe::App`
  frame loop from per-tab rendering. No behavior change; all 51 workspace
  tests still pass.

## [0.1.0-alpha.3] - 2026-05-19

### Added
- In-app auto-updater under [`crates/synbad-update`](crates/synbad-update/),
  reachable from the tray menu and the Settings tab. Queries the GitHub
  Releases API for the latest tag, downloads the matching archive for the
  host triple, and atomically swaps both `synbadd` and `synbad-gui` in
  place. When the install directory isn't writable (e.g. `/usr/bin` on
  Linux), the file-swap step re-launches the sibling binary under
  `pkexec` / `sudo` / `osascript` / UAC with a hidden
  `__apply-update --plan <path>` subcommand.
- Release pipeline now produces native installers alongside the existing
  archives: `.deb` and `.AppImage` for Linux, `.dmg` for macOS (per
  architecture), and `.msi` for Windows. Each ships with a SHA-256 sidecar.
  Installers are unsigned for now — Gatekeeper and SmartScreen will warn
  on first launch until code signing and notarization are wired up.
- WiX source under [`dist/windows/synbad.wxs`](dist/windows/synbad.wxs),
  macOS `Info.plist` under [`dist/macos/Info.plist`](dist/macos/Info.plist),
  and a `synbad.desktop` entry under
  [`dist/linux/synbad.desktop`](dist/linux/synbad.desktop) — used by the
  per-platform packaging steps in the release workflow.

## [0.1.0-alpha.2] - 2026-05-19

### Added
- GitHub Actions: CI (fmt/clippy/test on Linux/macOS/Windows), release
  artifact build matrix, weekly `cargo-audit` + `cargo-deny`, GitHub Pages
  deploy.
- Brand assets: SVG icon and wordmark logo, raster/ICO/ICNS generator script.
- Project hygiene: dependabot, issue and PR templates, `CONTRIBUTING.md`,
  `rust-toolchain.toml`, `deny.toml`.
- Landing page scaffold under `site/` for GitHub Pages.
- Wordmark logo header on every top-level doc and the landing page hero.

### Changed
- Dependabot: group minor + patch updates into one weekly PR; skip major
  bumps to stop the per-major PR flood.
- Audit workflow scoped to `main`/`master` so it doesn't double-fire on
  dependabot branch pushes.

### Fixed
- Windows release build: `synbadd`'s `sevenz` feature called
  `sevenz_rust2::Error::io`, which is `pub(crate)` in 0.18.0. Replaced with
  `?` (the crate's public `From<std::io::Error>` impl).

## [0.1.0] - TBD

Initial public scaffold; nothing is shipping in 0.1.0 yet beyond crate
skeletons and design docs.
