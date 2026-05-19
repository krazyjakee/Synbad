<p align="center">
  <img src="assets/logo.svg" alt="Synbad" width="520">
</p>

# Changelog

All notable changes to Synbad land here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches 1.0.

## [Unreleased]

### Added
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
