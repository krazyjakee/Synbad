<p align="center">
  <img src="assets/logo.svg" alt="Synbad" width="520">
</p>

# Changelog

All notable changes to Synbad land here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches 1.0.

## [Unreleased]

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

Initial public scaffold. See [docs/ROADMAP.md](docs/ROADMAP.md) for the phase
plan; nothing is shipping in 0.1.0 yet beyond crate skeletons and design docs.
