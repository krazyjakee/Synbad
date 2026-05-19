<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Packaging & autostart

Per-platform installers that build Synbad and wire `synbadd` to start
automatically when the user logs in. Each script is idempotent — re-run it
to upgrade an existing install in place.

| Platform | Installer | Mechanism |
|----------|-----------|-----------|
| Linux    | [`linux/install.sh`](linux/install.sh)     | systemd **user** service (`synbadd.service`) |
| macOS    | [`macos/install.sh`](macos/install.sh)     | per-user launchd agent (`dev.synbad.synbadd.plist`) |
| Windows  | [`windows/install.ps1`](windows/install.ps1) | Task Scheduler entry triggered `AtLogOn` |

All three are **per-user**, not system-wide:

- `synbadd` needs access to the user's graphical session (the Core binary
  talks to the display server), so a system-level daemon running as root /
  SYSTEM can't share input with the logged-in user reliably.
- A per-user install also keeps the trust store and identity in the user's
  home directory, where they belong — Synbad never asks for elevated
  privileges.

## Uninstalling

| Platform | Command |
|----------|---------|
| Linux    | `systemctl --user disable --now synbadd && rm ~/.config/systemd/user/synbadd.service` |
| macOS    | `launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/dev.synbad.synbadd.plist && rm ~/Library/LaunchAgents/dev.synbad.synbadd.plist` |
| Windows  | `Unregister-ScheduledTask -TaskName SynbadDaemon -Confirm:$false` |

Identity, trust store, and config under `~/.config/synbad/` (or
`%APPDATA%\synbad\` on Windows) are **not** removed automatically — delete
that directory to forget all paired peers and rotate the local identity.
