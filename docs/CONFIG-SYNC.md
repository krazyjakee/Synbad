# LAN Config Sync

> Independent, open implementation. Peer-to-peer over the LAN, no cloud, no
> account. Does not use or interoperate with Synergy 3's proprietary sync.

## Goal

A change made on any trusted peer (screen layout, options, hotkeys) converges
to an identical config on all trusted peers, then regenerates each node's Core
`.conf`.

## Model

The synced unit is the **Synbad config document** owned by `synbad-config`:

- the screen grid / layout (which machine is where),
- per-screen options and global options,
- the set of trusted peers (identities + fingerprints).

The Core `.conf` is a **generated artifact** derived from this document — never
the source of truth, never hand-merged.

## Topology

Peer-to-peer over LAN among **trusted** peers (from DISCOVERY.md pairing).
No elected master required for correctness; any peer may accept an edit. (An
optional "coordinator" role may be added later purely as an optimization.)

## Conflict resolution (open decision — Phase 4)

Two candidates, to be finalized:

1. **Per-field Last-Write-Wins** keyed by a Lamport/`(counter, machine-id)`
   timestamp. Simple; deterministic; loses one side of a true concurrent edit
   to the same field.
2. **Version vectors** for change detection + field-level merge, falling back
   to deterministic tie-break. More robust to concurrent edits; more code.

**Leaning LWW for v1** (config edits are infrequent and rarely concurrent),
with the version-vector head already advertised in discovery TXT (`cfg`) so we
can detect divergence and upgrade the strategy later without a protocol break.

## Sync protocol sketch

1. Each peer keeps a monotonically versioned config doc + change log.
2. Discovery TXT advertises the current version head (`cfg`).
3. On seeing a peer with a newer/divergent head, open an authenticated channel
   (keys from pairing) and exchange deltas since the last common version.
4. Merge per the conflict policy -> new version -> regenerate Core `.conf` ->
   signal `synbadd` to reload the Core.

## Safety

- Only **trusted/paired** peers exchange config (no unauthenticated writes).
- Encrypted + authenticated transport (Phase 5 hardening).
- Apply atomically: validate merged config before regenerating `.conf`;
  keep the previous good config for rollback.
- A malformed/oversized doc from a peer is rejected, not applied.

## Out of scope

Cross-LAN/internet sync, cloud backup, account-tied profiles.
