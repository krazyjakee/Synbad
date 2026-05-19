# Security model

This document describes what Synbad protects against, what it *doesn't*
protect against, and how the cryptographic pieces fit together. It's a
companion to [ARCHITECTURE.md](ARCHITECTURE.md), [DISCOVERY.md](DISCOVERY.md),
and [CONFIG-SYNC.md](CONFIG-SYNC.md).

## TL;DR

- **Identity** is an ed25519 keypair generated on first run and persisted
  to `~/.config/synbad/identity/`. The private key never leaves the
  machine.
- **Pairing** is user-confirmed via a short authentication string (SAS).
  Two peers compare a six-byte code on screen; a network MITM that
  splices the TCP stream sees the codes diverge.
- **Transport** between trusted peers is X25519 + ChaCha20-Poly1305 with
  the static ed25519 identities authenticating each session. A passive
  observer on the LAN sees only ciphertext.
- **No cloud, no accounts.** Trust is local and per-user; nothing leaves
  the LAN.

## Threat model

### Attackers we defend against

| Attacker | Defence |
|----------|---------|
| Passive eavesdropper on the LAN reading sync traffic | All sync sessions run over ChaCha20-Poly1305 AEAD ([`synbad-crypto`](../crates/synbad-crypto/)) — they see only ciphertext. |
| Network MITM that splices a TCP connection during pairing | The pairing SAS is bound to both sides' ed25519 keys; an MITM forces different keys into each half, so the on-screen codes diverge and the user catches it. |
| Hostile but unpaired LAN host attempting to push config | Sync sessions begin with a mutual ed25519 handshake. An unpaired peer is rejected by the encrypted transport before any application bytes are exchanged. |
| Replayed sync frame from a prior session | Each session ECDH-derives fresh AEAD keys, and each frame is bound to a per-direction counter mixed into the AEAD nonce. A replay decrypts with the wrong nonce → tag fails → session aborts. |
| Tampering with sync frames in flight | AEAD tags fail; the session aborts at the next `recv`. |
| Malformed config injected by a *trusted* peer | The merged config is re-validated before it's persisted, and the previous on-disk config is restored if validation fails — see `synbadd::supervisor::handle_sync_op`. |

### Attackers we **do not** defend against

| Out of scope | Rationale |
|--------------|-----------|
| A compromised endpoint (root on either peer) | They can read the private key from `~/.config/synbad/identity/ed25519.secret` and impersonate the user. Local OS security is the prerequisite. |
| WAN / internet attackers | Synbad is LAN-only by design. Don't expose the daemon ports to the internet — they are not hardened against the open web. |
| Side-channel attacks against the underlying primitives | We rely on RustCrypto / dalek crates. Their guarantees are inherited; we don't add our own constant-time discipline. |
| Denial of service on the LAN | An attacker who controls the LAN can drop packets. Synbad falls back to "last good config" but can't keep the link up against an active disruptor. |
| Trust store backups leaking peer identities | The trust store contains only *public* keys + machine_ids — not secrets. Loss-of-confidentiality there is not a credential compromise. |

## Identity

Each `synbadd` instance generates a stable identity the first time it
runs and persists it to:

```text
~/.config/synbad/identity/
    machine-id         text UUID, used as the `id` TXT key in mDNS
    ed25519.secret     32 raw bytes, file mode 0600 on Unix
    ed25519.public     32 raw bytes (cached; re-derivable from .secret)
```

The implementation is in
[`crates/synbad-discovery/src/identity.rs`](../crates/synbad-discovery/src/identity.rs).

The user-facing **fingerprint** (shown during pairing) is
`SHA-256(public_key)` truncated to 8 bytes and rendered as
`aaaa-bbbb-cccc-dddd`. 64 bits of entropy is short enough to read aloud
and long enough that an attacker can't accidentally produce a colliding
key in a single pairing session.

### Rotating identity

Deleting the `identity/` directory and restarting `synbadd` generates a
fresh keypair. All previously-paired peers lose trust with this machine
and must re-pair from scratch. There is no "rekey while keeping trust"
flow — pairing is intended to be an infrequent, conscious action.

## Pairing

Pairing is the bootstrapping step that lets two peers become *trusted*
to each other. The wire-level protocol is implemented in
[`crates/synbad-discovery/src/pairing.rs`](../crates/synbad-discovery/src/pairing.rs)
and the TCP plumbing is in
[`crates/synbadd/src/pairing.rs`](../crates/synbadd/src/pairing.rs).

### Steps

1. The TCP connection is wrapped in an **anonymous** encrypted channel
   (X25519 ephemeral handshake — no long-term keys yet). This stops a
   passive observer from reading display names / machine_ids / public
   keys during the handshake.
2. Each side sends a `PairHello` containing its ed25519 public key, a
   16-byte random nonce, its machine_id, and its display name.
3. Both sides derive a **canonical transcript** by concatenating the
   two halves in lexicographic order of the public keys. The
   transcript is order-independent — both peers compute the same bytes.
4. Each side derives a 6-byte **SAS** code (`sha256(transcript ||
   "synbad-pair-sas-v1")[..6]`) and shows it to its user.
5. The user compares the two codes on the two screens. If they match,
   both users click **Accept**.
6. Each side ed25519-signs the transcript and sends a `PairConfirm`.
   The other side verifies the signature against the public key from
   the matching `PairHello`. A successful verification persists the
   peer in the trust store.

### Why this resists network MITM

An attacker that splices the TCP connection performs two independent
handshakes — one with each side — using its own ed25519 keypair. The
two halves see different public keys, so the SAS codes diverge. The
user comparing codes on the two screens catches the attack. The signed
transcript binds the SAS to the exchanged keys, so the attacker can't
replay signatures to make the codes match retroactively.

The anonymous transport handshake doesn't authenticate anyone — that's
the *point* of pairing. It exists only to give us confidentiality
against passive listeners while pairing is happening. Active attacks
are caught at the SAS layer.

## Encrypted transport

Implementation: [`crates/synbad-crypto/`](../crates/synbad-crypto/).

Both pairing and sync use the same transport crate, in different modes:

| Use | Mode | What's verified |
|-----|------|-----------------|
| Pairing | `HandshakeMode::Anonymous` | Confidentiality only; trust comes from the SAS layer. |
| Sync | `HandshakeMode::Authenticated` | Each side proves possession of the long-term ed25519 key paired with the peer. Unpaired peers are rejected by the transport. |

### Wire-level flow (sync / authenticated)

```text
Initiator                                            Responder
  │                                                    │
  │ ──── version, auth_flag=1, eph_pub_i, nonce_i ───▶ │
  │ ◀───────────── eph_pub_r, nonce_r ──────────────── │
  │                                                    │
  │     shared = X25519(eph_sk, peer_eph_pk)           │
  │     transcript = SHA-256(domain || pubs || nonces) │
  │     keys = HKDF-SHA256(shared, salt=transcript)    │
  │                                                    │
  │ ──── ENC( AuthFrame_i ) ─────────────────────────▶ │ verify sig over transcript
  │ ◀──── ENC( AuthFrame_r ) ────────────────────────── │ resolver looks up trust store
  │                                                    │
  │      … encrypted sync frames …                     │
```

The `AuthFrame` carries the sender's machine_id, ed25519 public key,
and an ed25519 signature over the SHA-256 transcript. Each side
verifies the signature against the long-term key it *expected* to see
(from the trust store) and aborts if the bytes differ — so a hostile
peer can't substitute its own key even if it knows the target's
machine_id.

### Frame format (post-handshake)

Each direction is independent ChaCha20-Poly1305 with a 12-byte nonce
constructed as `nonce_prefix (4 B) || counter (8 B BE)`. Counters are
monotonically increasing; replaying or reordering a frame causes
AEAD tag verification to fail.

```text
[u32 BE ciphertext_len] [ciphertext + 16 B tag]
```

`ciphertext_len ≤ MAX_FRAME_BYTES (256 KiB)` — both sides reject
oversized frames before allocating, so a hostile peer can't OOM us
with a 4 GiB length prefix.

## Application-layer signatures

The sync protocol *also* ed25519-signs each `SyncFrame` (see
[`crates/synbad-sync/src/protocol.rs`](../crates/synbad-sync/src/protocol.rs)).
This predates the encrypted transport and is now redundant for
authenticity (the transport already provides it), but we keep it as
defense in depth — a future protocol mistake at the transport layer is
caught by the application layer, and vice versa.

## Validation on merge

A trusted peer that pushes a malformed config is still a risk: a bug
or a compromised peer could ship a config that's syntactically valid
JSON but semantically broken (e.g. a `server_name` that points at no
existing screen). The supervisor re-validates the merged config
**before** persisting and rolls back to the on-disk copy if
validation fails — see
[`crates/synbadd/src/supervisor.rs`](../crates/synbadd/src/supervisor.rs)
(`handle_sync_op`).

## Audit log

Every pairing, every trust revocation, every config sync, and every
Core start/stop is emitted as a structured `Event` on the IPC bus. The
GUI surfaces these; advanced users can stream them via
`synbad-ipc::client::subscribe`. Synbad does not write its own
encrypted audit log to disk — journal/launchd/Event Log is the system
of record for daemon activity.

## Reporting vulnerabilities

Open a private security advisory on the GitHub repository, or email
the maintainer directly. Please don't file a public issue for security
bugs.
