# LAN Auto-Discovery

> Independent, open implementation. The Synergy 3 discovery service is
> proprietary and cloud/account-backed; Synbad does **not** use or interoperate
> with it. Synbad discovery is LAN-only and account-free.

## Goal

A new machine running Synbad on the same LAN appears in every other peer's GUI
automatically, with no manual IP/hostname/port entry.

## Mechanism: mDNS / DNS-SD (Zeroconf)

- Each `synbadd` **advertises** a service instance and **browses** for others.
- Service type: **`_synbad._tcp`** (local domain).
- Instance name: user-facing machine name (e.g. `Jake-Desktop`).
- Rust crate candidate: `mdns-sd` (pure-Rust, no Avahi/Bonjour dependency).

### TXT record schema (advertised attributes)

| Key   | Meaning                                              |
|-------|------------------------------------------------------|
| `v`   | Synbad discovery protocol version                    |
| `id`  | Stable per-machine UUID (persisted locally)          |
| `host`| Hostname for the Core connection                     |
| `port`| Core listen port                                     |
| `fp`  | Public-key fingerprint for pairing/trust             |
| `cfg` | Config version vector head (see CONFIG-SYNC.md)      |

`id` is generated once and persisted so a machine keeps its identity across
renames and IP changes. `fp` lets a peer verify identity before trusting it.

## Trust / pairing

Discovery only makes a peer *visible*. It must not auto-join input sharing.

1. Peer B appears in Peer A's GUI as "discovered, untrusted".
2. User initiates pairing; a short code / fingerprint is shown on both ends.
3. User confirms the matching fingerprint -> public keys exchanged and stored.
4. Only trusted peers may participate in config sync and input sessions.

This blocks a hostile machine on the LAN from silently joining.

## Failure / edge handling

- mDNS blocked (some corp/VPN networks): provide a **manual add** fallback
  (enter host:port) — discovery is an enhancement, not the only path.
- Multiple NICs / VPN: advertise on intended interfaces only; make the
  interface set configurable.
- Name collisions: disambiguate by `id`, not by display name.
- Stale records: rely on mDNS goodbye packets + TTL; reap peers not seen within
  a timeout.

## Out of scope

WAN/internet discovery, relay/rendezvous servers, any account or cloud
component. Synbad discovery is deliberately LAN-only.
