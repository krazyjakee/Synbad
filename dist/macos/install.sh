#!/usr/bin/env bash
# Install Synbad as a per-user launchd agent on macOS.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")"/../.. && pwd)"
PLIST_SRC="${REPO_ROOT}/dist/macos/dev.synbad.synbadd.plist"
PLIST_DST="${HOME}/Library/LaunchAgents/dev.synbad.synbadd.plist"
BIN_DST="/usr/local/bin/synbadd"
GUI_DST="/usr/local/bin/synbad-gui"

echo "[synbad] building release binaries"
cd "${REPO_ROOT}"
cargo build --release -p synbadd -p synbad-gui

# /usr/local/bin needs sudo on most macOS installs. We could put binaries
# under ~/.local/bin instead and patch the plist, but /usr/local/bin keeps
# the plist constant across users.
echo "[synbad] installing binaries to /usr/local/bin (sudo)"
sudo install -m 755 target/release/synbadd "${BIN_DST}"
sudo install -m 755 target/release/synbad-gui "${GUI_DST}"

echo "[synbad] installing launchd plist"
mkdir -p "$(dirname "${PLIST_DST}")"
install -m 644 "${PLIST_SRC}" "${PLIST_DST}"

# `bootstrap` registers the agent with the current GUI session; if it was
# already loaded, bootout first so re-runs are idempotent.
UID_NUM="$(id -u)"
if launchctl print "gui/${UID_NUM}/dev.synbad.synbadd" >/dev/null 2>&1; then
  launchctl bootout "gui/${UID_NUM}" "${PLIST_DST}" || true
fi
launchctl bootstrap "gui/${UID_NUM}" "${PLIST_DST}"
launchctl enable "gui/${UID_NUM}/dev.synbad.synbadd"

echo
echo "[synbad] installed. Useful commands:"
echo "  launchctl print gui/${UID_NUM}/dev.synbad.synbadd"
echo "  launchctl kickstart -k gui/${UID_NUM}/dev.synbad.synbadd  # restart"
echo "  tail -f /tmp/synbadd.err.log"
