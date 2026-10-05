#!/usr/bin/env bash
# Install Synbad as a per-user launchd agent on macOS.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")"/../.. && pwd)"
PLIST_SRC="${REPO_ROOT}/dist/macos/dev.synbad.synbadd.plist"
PLIST_DST="${HOME}/Library/LaunchAgents/dev.synbad.synbadd.plist"
APP_DST="/Applications/Synbad.app"
BIN_DST="${APP_DST}/Contents/MacOS/synbadd"
GUI_DST="${APP_DST}/Contents/MacOS/synbad-gui"

echo "[synbad] building native release binaries and patched Core"
cd "${REPO_ROOT}"
core_arch=x86_64
rust_target=x86_64-apple-darwin
if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
  core_arch=arm64
  rust_target=aarch64-apple-darwin
fi
rustup target add "$rust_target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cargo build --release --target-dir "$stage/target" --target "$rust_target" -p synbadd -p synbad-gui
bash dist/macos/build-core.sh "$stage/core" "$core_arch"
app="$stage/Synbad.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$stage/target/$rust_target/release/synbadd" "$stage/target/$rust_target/release/synbad-gui" "$app/Contents/MacOS/"
cp "$stage/core/deskflow-client" "$stage/core/deskflow-server" "$app/Contents/MacOS/"
cp "$stage/core/DESKFLOW-LICENSE" "$app/Contents/Resources/"
cp "$stage/core/DESKFLOW-LICENSE-EXCEPTION" "$app/Contents/Resources/"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
sed "s/@VERSION@/$version/g" dist/macos/Info.plist > "$app/Contents/Info.plist"
bash assets/scripts/generate-icons.sh
cp assets/synbad.icns "$app/Contents/Resources/"
bash dist/macos/sign.sh "$app"
echo "[synbad] installing signed app to $APP_DST (sudo)"
new_app="/Applications/.synbad-install-$$.app"
old_app="/Applications/.synbad-previous-$$.app"
sudo ditto "$app" "$new_app"
codesign --verify --deep --strict "$new_app"
if [ -e "$APP_DST" ]; then sudo mv "$APP_DST" "$old_app"; fi
if ! sudo mv "$new_app" "$APP_DST"; then
  if [ -e "$old_app" ]; then sudo mv "$old_app" "$APP_DST"; fi
  exit 1
fi
if [ -e "$old_app" ]; then sudo rm -rf "$old_app"; fi
sudo mkdir -p /usr/local/bin
sudo ln -sf "$BIN_DST" /usr/local/bin/synbadd
sudo ln -sf "$GUI_DST" /usr/local/bin/synbad-gui

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
echo "  open /Applications/Synbad.app"
