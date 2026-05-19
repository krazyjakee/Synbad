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
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased delivery plan and open decisions |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Component layout and Core integration strategy |
| [docs/DISCOVERY.md](docs/DISCOVERY.md) | LAN auto-discovery design |
| [docs/CONFIG-SYNC.md](docs/CONFIG-SYNC.md) | LAN config-sync design |
| [docs/LICENSING.md](docs/LICENSING.md) | License obligations and trademark constraints |

## License

To be finalized — see [docs/LICENSING.md](docs/LICENSING.md). The intended
license is GPLv2-compatible and fully open source.
