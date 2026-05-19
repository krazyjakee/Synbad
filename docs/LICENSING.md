# Licensing & Trademark

> Working notes, not legal advice. Have the final license choice and the Core's
> per-file headers reviewed by someone qualified before public release.

## The Core

The upstream Synergy Core (this is what Synbad orchestrates) is licensed
**GPLv2**. GPLv2 grants the right to build, modify, and distribute derivative
and combined works, provided downstream stays GPL-compatible and complete
corresponding source is offered.

## What this means for Synbad

- Synbad uses **process orchestration**, not linking (see ARCHITECTURE.md):
  Synbad spawns the unmodified Core binaries as child processes and talks to
  them over IPC. Separate processes at arm's length is arguably **mere
  aggregation**, which would *not* force Synbad's own code under the GPL.
- Regardless, Synbad is intended to be **fully open source and
  GPLv2-compatible**, so this distinction is a safety margin, not a loophole
  we depend on.

### Decision: MIT, with runtime fetch of the Core

Synbad's own source is **MIT** (see `LICENSE` at the repo root). This works
under our distribution model because **Synbad does not redistribute the
Core**: at runtime, `synbadd` queries
`github.com/deskflow/deskflow/releases/latest`, downloads the platform-
appropriate release asset, verifies it against the upstream `sums.txt`,
and extracts `deskflow-core` into a per-user cache. The implementation is
in `crates/synbadd/src/binaries.rs`. The combined work only exists on the
end user's system, which is the textbook "mere aggregation" scenario the
GPL FAQ permits.

| Option | When it applies | Note |
|--------|-----------------|------|
| GPLv2 | If we ever ship Core bytes in our release artifacts | Safest if we bundle |
| GPLv2-or-later | If we want future GPL flexibility | Check Core headers say "or later" |
| **MIT** (chosen) | Synbad source only; Core fetched at runtime | Keeps GPL combination off our distribution path |

If we later decide to bundle Core binaries in our installers (instead of
fetching them at runtime), we must revisit this — bundling pulls the
release artifact into GPL territory.

### Conditions we must meet if we distribute Core binaries

- Provide or offer the **complete corresponding source** of the Core (and any
  modifications — we plan none in Phase 1).
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

- [ ] Pull the Core's actual `LICENSE` + sample per-file headers; record exact
      GPL version and any OpenSSL exception here.
- [x] Commit the chosen `LICENSE` file (Phase 0) — MIT.
- [ ] Add a `NOTICE`/attribution file crediting the Core and Deskflow upstream.
- [ ] Legal review before first public release — confirm the runtime-fetch
      model holds for our chosen upstream (Deskflow's GitHub releases) and
      that we don't ship Core bytes in any Synbad release artifact.
