<p align="center">
  <img src="assets/logo.svg" alt="Synbad" width="520">
</p>

# Contributing to Synbad

Thanks for your interest! The most useful contributions right now are
design feedback, packaging fixes, and small focused PRs.

## Before you start

- **Read the design docs first.** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md),
  [`docs/SECURITY.md`](docs/SECURITY.md), and the per-component design notes
  exist so PRs don't have to re-derive the constraints in review.
- **Open an issue for non-trivial work.** Anything that changes the IPC
  protocol, on-disk format, trust store, or pairing flow should be discussed
  before implementation — these surfaces are stability-sensitive.
- **Synergy / Symless trademarks.** Synbad uses only the open-source Synergy
  Core under GPLv2 and ships its own branding. PRs that introduce "Synergy"
  branding, or that bundle/redistribute Core binaries, will be declined. See
  [`docs/LICENSING.md`](docs/LICENSING.md).

## Development setup

```sh
# Default build (no tray)
cargo build --workspace
cargo test  --workspace

# GUI with system-tray support (Linux: needs GTK/xdo/appindicator)
sudo apt install libgtk-3-dev libxdo-dev libayatana-appindicator3-dev   # Debian/Ubuntu
cargo build -p synbad-gui --features tray
```

The repo pins a stable Rust toolchain via [`rust-toolchain.toml`](rust-toolchain.toml).

## Before pushing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
```

CI runs the same commands across Linux/macOS/Windows plus
[`cargo-audit`](https://github.com/rustsec/rustsec) and
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) — keep new
dependencies permissively-licensed (see [`deny.toml`](deny.toml)).

## Commit style

- Imperative mood, scoped prefix where it helps: `daemon: restart Core on
  stderr EOF`, `gui: drag handle for screen layout`, `docs: clarify pairing
  flow`.
- One logical change per PR. If your branch grew, split it.
- Reference issues with `Refs #123` / `Closes #123`.

## Security disclosures

Don't file public issues for vulnerabilities. Use GitHub's private security
advisory flow: <https://github.com/krazyjakee/Synbad/security/advisories/new>.
See [`docs/SECURITY.md`](docs/SECURITY.md) for the threat model.

## Licensing of your contribution

Synbad's source is MIT-licensed. By submitting a PR you agree your
contribution is offered under the same MIT terms — no separate CLA.
