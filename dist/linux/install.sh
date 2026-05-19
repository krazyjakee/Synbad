#!/usr/bin/env bash
# Install Synbad as a per-user systemd service.
#
# Builds the release binaries, drops them into ~/.local/bin, installs the
# user-level systemd unit, and enables it so `synbadd` autostarts at login.
# Re-running is safe; it reinstalls in place.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")"/../.. && pwd)"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"

echo "[synbad] building release binaries"
cd "${REPO_ROOT}"
cargo build --release -p synbadd -p synbad-gui

echo "[synbad] installing binaries to ${BIN_DIR}"
mkdir -p "${BIN_DIR}"
install -m 755 target/release/synbadd "${BIN_DIR}/synbadd"
install -m 755 target/release/synbad-gui "${BIN_DIR}/synbad-gui"

echo "[synbad] installing systemd user unit"
mkdir -p "${SYSTEMD_USER_DIR}"
install -m 644 "${REPO_ROOT}/dist/linux/synbadd.service" "${SYSTEMD_USER_DIR}/synbadd.service"

# `daemon-reload` so systemd picks up the new unit; `enable --now` makes it
# autostart at login *and* starts it immediately.
systemctl --user daemon-reload
systemctl --user enable --now synbadd.service

echo
echo "[synbad] installed. Useful commands:"
echo "  systemctl --user status synbadd"
echo "  journalctl --user -u synbadd -f"
echo "  systemctl --user restart synbadd"
