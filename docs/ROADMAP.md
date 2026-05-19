<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Roadmap

Delivery is phased so each phase is independently usable.

## Phase 0 — Foundations
- [ ] Cargo workspace + crate skeletons (see ARCHITECTURE.md layout)
- [ ] Decide and pin the GUI framework (open decision below)
- [ ] Finalize and commit the LICENSE file (see LICENSING.md)
- [ ] CI: build + lint on Linux/macOS/Windows
- [ ] Vendor or document how to obtain the Core binaries per platform

## Phase 1 — Client + Core supervision
- [ ] `synbad-config`: config model, (de)serialization, Core `.conf` generation
- [ ] `synbadd`: spawn/supervise `synergys`/`synergyc`, restart on crash
- [ ] `synbad-ipc`: daemon <-> Core IPC client (logs/status/reload)
- [ ] Manual config path works end-to-end (parity with Synergy 1, our branding)

## Phase 2 — GUI
- [ ] Tray icon + show/hide config window
- [ ] Screen-layout editor (drag screens into a grid)
- [ ] Live status: connected peers, log tail, start/stop
- [ ] GUI <-> `synbadd` IPC

## Phase 3 — LAN auto-discovery
- [ ] `synbad-discovery`: mDNS/DNS-SD advertise + browse (see DISCOVERY.md)
- [ ] Discovered peers surface in the GUI; one-click add to layout
- [ ] Pairing/trust handshake before a peer can join

## Phase 4 — LAN config sync
- [x] `synbad-sync`: peer-to-peer replication of the config model
       (see CONFIG-SYNC.md)
- [x] Conflict resolution + convergence across all peers
- [x] Config changes on any node propagate and regenerate the Core `.conf`

## Phase 5 — Hardening
- [x] Transport encryption + authenticated pairing for discovery/sync
       (see SECURITY.md; implementation in `synbad-crypto`)
- [x] Packaging/installers per platform; autostart of `synbadd`
       (see `dist/` — systemd user unit, launchd plist, scheduled task)
- [x] Docs: user guide, security model (USER-GUIDE.md, SECURITY.md)

## Possible future
- [ ] Native-Rust protocol implementation to drop the Core-binary dependency

---

## Open decisions (resolve in Phase 0)

1. **GUI framework.** Candidates: `egui` (simple, immediate-mode, easy tray),
   `Tauri` (web UI, heavier), `Slint`, `iced`. **Recommended default: `egui`**
   for a small config/tray app with minimal dependencies — revisit if the
   layout editor needs richer widgets.
2. **License file.** GPLv2 vs GPLv2-or-later vs (if aggregation holds) a
   permissive license. Decision and rationale tracked in LICENSING.md.
3. **Core binary distribution.** Bundle prebuilt Core per-platform, build from
   the upstream repo in CI, or require user-installed Core. Affects packaging
   and the licensing/source-offer obligation.
4. **Discovery service type + record schema** — finalized in DISCOVERY.md.
5. **Sync conflict model** (LWW vs version vector) — finalized in CONFIG-SYNC.md.
