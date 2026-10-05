<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Licensing & Trademark

> Working notes, not legal advice. Have the final license choice and the Core's
> per-file headers reviewed by someone qualified before public release.

## The Core

Synbad orchestrates Deskflow Core v1.17.0 at commit
`44bd69fdc8df726909f35b7efeeebc554bfde1c5`. Its source headers specify
**GPL-2.0-or-later**, and upstream's `LICENSE_EXCEPTION` supplies an OpenSSL
linking exception. The modifications Synbad ships are recorded in
[`dist/deskflow/teardown.patch`](../dist/deskflow/teardown.patch).

## What this means for Synbad

- Synbad uses **process orchestration**, not linking (see ARCHITECTURE.md):
  Synbad spawns the Core binaries as child processes and talks to
  them over IPC. Separate processes at arm's length is arguably **mere
  aggregation**, which would *not* force Synbad's own code under the GPL.
- Regardless, Synbad is intended to be **fully open source and
  GPLv2-compatible**, so this distinction is a safety margin, not a loophole
  we depend on.

### Distribution

Synbad's own source is **MIT** (see `LICENSE` at the repo root). The
separate Deskflow Core retains its upstream license and copyright notices.

macOS app and Core archives include the patched native executables,
`DESKFLOW-LICENSE`, and `DESKFLOW-LICENSE-EXCEPTION`. The same release
publishes `deskflow-source-1.17.0-synbad-{arch}.tar.gz`, containing the exact
patched source used for that build. This archive is produced after CMake
configuration and excludes only Git metadata. Core packaging is described
in [`dist/deskflow/README.md`](../dist/deskflow/README.md).

Linux and Windows retain runtime fetching of pinned upstream binaries.
The resolver verifies downloaded assets using the release checksum or
the pinned known checksum before extracting them into a per-user cache.

### Conditions we must meet if we distribute Core binaries

- Provide or offer the **complete corresponding source** of the Core (and any
  modifications).
- Preserve copyright and license notices; mark any modifications.
- Add no further restrictions.
- **TLS/OpenSSL caveat:** GPLv2 + OpenSSL has a historical incompatibility;
  synergy-core traditionally carried an OpenSSL linking exception. If we ship
  Core builds with TLS, confirm the per-file exception is present.
- Check whether Core headers say **"GPLv2"** vs **"GPLv2 or later"** — it
  constrains which GPL version Synbad may adopt.

## Trademark (separate from copyright — GPL does not grant trademark rights)

- "Synergy" and the Synergy logo are **Symless trademarks**. GPLv2 covers
  copyright only.
- Synbad therefore ships its **own name, icon, and branding**, and must not be
  presented as Synergy or imply official affiliation.
- Permitted: factual statements like "built on the open-source Synergy Core".
- Not permitted: naming the product "Synergy*", using Symless logos, or
  implying endorsement.
- We also do **not** use the "Synergy 3" name or its proprietary
  discovery/sync services; Synbad's discovery and sync are independent LAN-only
  reimplementations.

## Action items

- [x] Record the pinned Core's license headers and OpenSSL exception.
- [x] Commit the chosen `LICENSE` file (Phase 0) — MIT.
- [x] Preserve upstream Core license and exception notices, and document
      the patched source and attribution in `dist/deskflow/README.md`.
- [ ] Legal review before first public release — confirm the runtime-fetch
      model holds for our chosen upstream (Deskflow's GitHub releases) and
      the macOS bundled-Core distribution and corresponding source artifacts.
