#!/usr/bin/env bash
# Sign nested executables before their app. Identifiers never include a version.
set -euo pipefail
app="${1:?Synbad.app required}"
sign_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
identity="${SYNBAD_SIGN_IDENTITY:--}"
if [ "${REQUIRE_DEVELOPER_ID:-false}" = true ]; then
  case "$identity" in
    'Developer ID Application:'*) ;;
    *) echo 'Published macOS releases require a Developer ID Application identity in SYNBAD_SIGN_IDENTITY.' >&2; exit 1 ;;
  esac
fi
flags=(--force --sign "$identity")
if [ "$identity" != - ]; then flags+=(--options runtime --timestamp); fi
for item in 'synbadd:dev.synbad.synbadd' 'synbad-gui:dev.synbad.synbad' \
            'deskflow-client:dev.synbad.deskflow.client' 'deskflow-server:dev.synbad.deskflow.server'; do
  case "${item%%:*}" in
    synbadd|synbad-gui)
      codesign "${flags[@]}" --entitlements "$sign_dir/audio-input.entitlements" --identifier "${item#*:}" "$app/Contents/MacOS/${item%%:*}"
      ;;
    *) codesign "${flags[@]}" --identifier "${item#*:}" "$app/Contents/MacOS/${item%%:*}" ;;
  esac
done
codesign "${flags[@]}" --entitlements "$sign_dir/audio-input.entitlements" --identifier dev.synbad.synbad "$app"
codesign --verify --deep --strict "$app"
