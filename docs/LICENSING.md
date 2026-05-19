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

### Open decision: the LICENSE file

| Option | When it applies | Note |
|--------|-----------------|------|
| GPLv2 | Safe default; matches the Core exactly | Simplest to defend |
| GPLv2-or-later | If we want future GPL flexibility | Check Core headers say "or later" |
| Permissive (MIT/Apache-2.0) | Only valid if aggregation holds and we never link/modify the Core | Higher risk; avoid for v1 |

**Recommended for v1: GPLv2** (or GPLv2-or-later if the Core's per-file headers
permit "or later"). Revisit only with legal review.

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
- [ ] Commit the chosen `LICENSE` file (Phase 0).
- [ ] Add a `NOTICE`/attribution file crediting the Core and Deskflow upstream.
- [ ] Legal review before first public release.
