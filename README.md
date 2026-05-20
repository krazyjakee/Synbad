<p align="center">
  <img src="assets/logo.svg" alt="Synbad" width="520">
</p>

<p align="center">
  <a href="https://github.com/krazyjakee/Synbad/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/krazyjakee/Synbad/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/krazyjakee/Synbad/actions/workflows/audit.yml"><img alt="Audit" src="https://github.com/krazyjakee/Synbad/actions/workflows/audit.yml/badge.svg"></a>
  <a href="https://github.com/krazyjakee/Synbad/releases"><img alt="Release" src="https://img.shields.io/github/v/release/krazyjakee/Synbad?include_prereleases&sort=semver"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust 1.75+" src="https://img.shields.io/badge/rust-1.75%2B-orange.svg">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-informational">
</p>

# Synbad

A free, open-source desktop client and GUI for sharing one keyboard, mouse, and
clipboard across nearby computers — built on the open-source Synergy Core
(GPLv2, upstream of [Deskflow](https://github.com/deskflow/deskflow)).

Synbad is **not** affiliated with or endorsed by Symless. "Synergy" is a
trademark of Symless; Synbad uses only the open-source Core under its GPLv2
license and ships its own independent name, branding, and GUI.

## What Synbad does

- **Client + server engine** — share input between machines (via the open Core).
- **Native GUI** — a modern, cross-platform configuration UI (Rust).
- **LAN auto-discovery** — machines find each other automatically on the local
  network, no manual IP/hostname entry. *(Our own open implementation — the
  Synergy 3 discovery service is proprietary; we do not use or interoperate
  with it.)*
- **LAN config sync** — screen layout and settings stay consistent across all
  peers, synced peer-to-peer over the LAN with no cloud account.

## Status

Early planning. See [`docs/`](docs/) for the architecture, roadmap, and the
licensing rationale.

| Doc | Purpose |
|-----|---------|
| [docs/USER-GUIDE.md](docs/USER-GUIDE.md) | Install, pair, share input across machines |
| [docs/SECURITY.md](docs/SECURITY.md) | Threat model, identity, pairing, encrypted transport |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased delivery plan and open decisions |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Component layout and Core integration strategy |
| [docs/DISCOVERY.md](docs/DISCOVERY.md) | LAN auto-discovery design |
| [docs/CONFIG-SYNC.md](docs/CONFIG-SYNC.md) | LAN config-sync design |
| [docs/LICENSING.md](docs/LICENSING.md) | License obligations and trademark constraints |
| [dist/README.md](dist/README.md) | Per-platform packaging / autostart |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to build, test, and submit changes |
| [CHANGELOG.md](CHANGELOG.md) | Release notes |
| [assets/README.md](assets/README.md) | Brand assets and icon generation |

## Building

The workspace is a standard Cargo project.

```sh
cargo build --workspace          # default build (no tray)
cargo build -p synbad-gui --features tray   # GUI with system-tray support
cargo test  --workspace          # unit tests
```

### Build-time dependencies

Building `synbad-gui` with `--features tray` on Linux pulls in GTK3, libxdo,
and libappindicator development headers. On Debian/Ubuntu:

```sh
sudo apt install libgtk-3-dev libxdo-dev libayatana-appindicator3-dev
```

No extra system packages are needed for the default (no-tray) build.

### Runtime dependencies (Deskflow Core)

Synbad does **not** redistribute the Deskflow Core. On first run, `synbadd`
downloads a pinned upstream release into `~/.local/share/synbad/bin/<tag>/`
and supervises it as a child process. Synbad currently pins
**[Deskflow v1.17.0](https://github.com/deskflow/deskflow/releases/tag/v1.17.0)**
because it's the last upstream release that ships a Linux build compatible
with Ubuntu 24.04's Qt 6.4 — Deskflow 1.19+ target Qt 6.7 / 6.8.

#### Linux

The downloaded `deskflow-core` is the CLI binary, but it's still linked
against (non-GUI parts of) Qt 6, plus libei and libportal. On Ubuntu 24.04 /
Debian 12 you'll need:

```sh
sudo apt install libqt6core6t64 libqt6dbus6t64 libqt6xml6t64 libei1 libportal1
```

(On older Debian/Ubuntu releases without the `t64` ABI transition, drop the
`t64` suffix from the Qt packages.)

#### macOS

The upstream `.dmg` ships `Deskflow.app` with the Qt frameworks vendored
inside `Contents/Frameworks/`, so **no extra Homebrew install is needed**.
`synbadd` copies the whole bundle to
`~/Library/Application Support/dev.synbad.synbad/bin/<tag>/Deskflow.app/`
and runs the binary from inside it; the
`@executable_path/../Frameworks/...` rpaths resolve to the bundled Qt.

What macOS *does* require is granting permissions to `synbadd` the first
time you click **Start**:

- **System Settings → Privacy & Security → Accessibility** — needed to
  inject keyboard/mouse events (server role) and to read modifier state
  reliably.
- **System Settings → Privacy & Security → Input Monitoring** — needed to
  observe the keyboard at all on macOS 11+.

If the Core fast-fails immediately on first launch, the bundle may be
quarantined by Gatekeeper. Clear it once:

```sh
xattr -dr com.apple.quarantine \
  "$HOME/Library/Application Support/dev.synbad.synbad/bin/"
```

#### Windows

The pinned tag (v1.17.0) only ships an `.msi` installer for Windows, which
`synbadd` can't extract in pure Rust. Until the pin is bumped to a release
that publishes the `win-x64-portable.7z`, point `binaries.core` at a
hand-built `deskflow-core.exe` (see below).

#### Override

To use a hand-built or differently-versioned `deskflow-core`, set
`binaries.core` in `~/.config/synbad/config.toml` to its absolute path —
`synbadd` short-circuits the download entirely when that's set. This is the
right escape hatch if you're on a distro whose Qt is too new (rather than
too old) for the pinned release.

### IPC transport

`synbad-gui` talks to `synbadd` over a local socket abstracted by
[`interprocess`]: a Unix domain socket on Linux/macOS (under
`$XDG_STATE_HOME/synbad/`) and a named pipe on Windows
(`\\.\pipe\synbadd-<user>`). Wire format is newline-delimited JSON. There's
a small request/response smoke test:

```sh
cargo run -p synbadd            # in one terminal
cargo run -p synbad-ipc --example smoke   # in another
```

[`interprocess`]: https://docs.rs/interprocess

### Core process status

The daemon parses Synergy/Deskflow Core stderr to surface structured
`PeerConnected` / `PeerDisconnected` / `ActiveScreen` events alongside the
raw log feed, without speaking the Core's native binary IPC. See
[`crates/synbad-ipc/src/log_parse.rs`](crates/synbad-ipc/src/log_parse.rs).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, commit style, and
the surfaces that need an issue before a PR. Security issues go through
GitHub's [private advisory flow](https://github.com/krazyjakee/Synbad/security/advisories/new),
not public issues.

## License

Synbad's own source is **MIT** (see [LICENSE](LICENSE)). The Synergy Core is
GPLv2 and is fetched at runtime on the user's machine — Synbad does not
redistribute Core binaries. See [docs/LICENSING.md](docs/LICENSING.md) for
the rationale and the trademark constraints (the "Synergy" name is a Symless
trademark and Synbad does not use it for branding).
