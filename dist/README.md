<p align="center">
  <img src="../assets/logo.svg" alt="Synbad" width="520">
</p>

# Packaging & autostart

Two ways to install Synbad:

1. **Prebuilt installers** from the [GitHub Releases page][releases] —
   `.deb` / `.AppImage` for Linux, `.dmg` for macOS, `.msi` for Windows.
   These are produced by [`.github/workflows/release.yml`](../.github/workflows/release.yml)
   and wire up autostart the same way the scripts below do. **Unsigned**
   for now: Gatekeeper / SmartScreen will warn on first launch.
2. **From-source scripts** in this directory — useful for dev installs and
   for distros where the prebuilt artifacts don't fit. Each script is
   idempotent; re-run it to upgrade an existing install in place.

| Platform | Prebuilt | From source | Autostart mechanism |
|----------|----------|-------------|---------------------|
| Linux    | `.deb`, `.AppImage` | [`linux/install.sh`](linux/install.sh)     | systemd **user** service ([`synbadd.service`](linux/synbadd.service)) |
| macOS    | `.dmg`              | [`macos/install.sh`](macos/install.sh)     | per-user launchd agent ([`dev.synbad.synbadd.plist`](macos/dev.synbad.synbadd.plist)) |
| Windows  | `.msi`              | [`windows/install.ps1`](windows/install.ps1) | `HKCU\…\Run` registry entry (`.msi`) or Task Scheduler `AtLogOn` (`install.ps1`) |

The packaging templates that drive the MSI/DMG/AppImage builds live next to
the install scripts:
[`linux/synbad.desktop`](linux/synbad.desktop),
[`macos/Info.plist`](macos/Info.plist),
[`windows/synbad.wxs`](windows/synbad.wxs).

[releases]: https://github.com/krazyjakee/Synbad/releases

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
