# Synbad user guide

A walkthrough for installing Synbad, pairing two machines, and sharing
input across them. If you're looking for the technical design, see
[ARCHITECTURE.md](ARCHITECTURE.md) and [SECURITY.md](SECURITY.md).

## What Synbad does

Synbad lets you share one keyboard, mouse, and clipboard across two or
more computers on the same LAN. You move the cursor off the edge of one
screen and it appears on another — same as the open-source Synergy /
Deskflow Core that Synbad supervises underneath.

Synbad adds:

- A modern Rust GUI for the screen layout.
- **LAN auto-discovery** — machines find each other automatically over
  mDNS. No IP entry, no cloud account.
- **LAN config sync** — change the screen layout on any machine and the
  change replicates to every other paired peer, peer-to-peer over the
  LAN.
- **Encrypted, authenticated transport** between paired peers — see
  [SECURITY.md](SECURITY.md).

## Install

Pick the script that matches your OS — each one builds Synbad from this
repo and registers `synbadd` to start at login. See
[`dist/README.md`](../dist/README.md) for uninstall instructions.

| OS      | Command |
|---------|---------|
| Linux   | `bash dist/linux/install.sh` |
| macOS   | `bash dist/macos/install.sh` |
| Windows | `powershell.exe -ExecutionPolicy Bypass -File dist\windows\install.ps1` |

After installation:

- The daemon `synbadd` is running in the background (and will restart at
  login).
- The GUI is on your PATH as `synbad-gui`.
- The Deskflow Core binary is **not** bundled — `synbadd` fetches a
  pinned upstream release on first start. See the README for the
  runtime dependencies (Qt6 etc) you'll need installed.

## First-run flow

1. Launch `synbad-gui` on each machine.
2. Each machine appears in the other's **Discovered peers** list with a
   "discovered, untrusted" badge.
3. On machine A, click **Pair** next to machine B's entry. The pairing
   handshake runs. Both screens display a six-byte **verification
   code** (e.g. `ab-cd-ef-12-34-56`).
4. **Compare the codes on the two screens.** If they match, click
   **Accept** on both. If they don't match, click **Decline** — a
   network MITM is in flight (see [SECURITY.md](SECURITY.md)).
5. Both peers persist each other in the trust store. They can now
   participate in input sharing and config sync.

## Building a screen layout

The GUI's **Layout** tab gives you a grid where each paired peer can be
placed:

- Drag a screen tile into the position that matches its physical
  location relative to the others.
- A cursor crossing the right edge of the leftmost screen reappears on
  the left edge of the next screen to its right — directions are
  inferred from the grid. (Cardinal links are also editable directly if
  you need diagonals or non-adjacent jumps.)
- The local machine's tile is highlighted so you don't accidentally
  drop yourself into limbo.

When you save the layout it propagates to every paired peer
automatically. Each peer regenerates the Core's `.conf` and the running
Core process is restarted in place.

## Start sharing input

1. On the machine you want to use as the **server** (the source of the
   keyboard/mouse), set its role to *Server* in the GUI and click
   **Start**.
2. On every other paired machine, set the role to *Client*, enter the
   server's hostname (or pick it from the discovered list), and click
   **Start**.
3. The status chip in the GUI flips to *Running* on both sides, and the
   log tail shows the Core's connect messages.

## Troubleshooting

| Symptom | Likely cause | Where to look |
|---------|--------------|---------------|
| Peer doesn't appear in the discovered list | mDNS blocked on the LAN, or both machines on different VLANs / VPNs | Add the peer manually via *Add peer* in the GUI |
| Pairing fails with "session timed out" | Firewall is blocking TCP/24850 (default service port) | Open the port, or set a different `service_port` in `~/.config/synbad/config.toml` |
| Pairing fails with "user declined" | One side hit Decline (often: code mismatch) | Try again from a known-trusted network |
| Core crashes immediately on Start | Missing runtime libraries (Qt6 on Linux, etc) | `journalctl --user -u synbadd -f` on Linux; `/tmp/synbadd.err.log` on macOS |
| Configs out of sync between peers | A peer is offline or the firewall is blocking TCP/24851 (sync port) | Wait for the peer to come back; sync resumes automatically |
| Want to forget a paired peer | Revoke trust in the GUI (or remove `~/.config/synbad/trusted-peers.json` to forget all) | After revoking, the peer must re-pair |
| Want to rotate this machine's identity | Stop `synbadd`, delete `~/.config/synbad/identity/`, restart | Every peer must re-pair with you afterward |

## Files Synbad reads/writes

```text
~/.config/synbad/
    config.toml             user-editable settings (role, ports, screens)
    config.versions.json    Lamport stamps used by config sync (do not edit)
    trusted-peers.json      paired peers (public keys only — no secrets)
    identity/
        machine-id          stable UUID
        ed25519.secret      private key, mode 0600
        ed25519.public      public key

~/.local/share/synbad/
    bin/<tag>/deskflow-core fetched Core binary
    synergy.conf            generated layout (regenerated from config.toml)
    deskflow.ini            generated QSettings INI for the Core
    synbadd-<user>.sock     GUI ↔ daemon IPC (Unix)
```

On Windows, replace `~/.config/synbad/` with
`%APPDATA%\synbad\synbad\` and the socket with the named pipe
`\\.\pipe\synbadd-<user>`.

## Uninstalling

See [`dist/README.md`](../dist/README.md).
