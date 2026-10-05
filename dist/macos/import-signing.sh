#!/usr/bin/env bash
set -euo pipefail
# CI secrets are passed through the environment and never printed.
if [ -z "${MACOS_CERT_P12_BASE64:-}" ]; then exit 0; fi
cert_dir="${RUNNER_TEMP:?CI temp directory required}/synbad-signing"
mkdir -p "$cert_dir"
printf '%s' "$MACOS_CERT_P12_BASE64" | base64 -D > "$cert_dir/certificate.p12"
password="$(openssl rand -hex 32)"
security create-keychain -p "$password" "$cert_dir/signing.keychain-db"
security set-keychain-settings -lut 21600 "$cert_dir/signing.keychain-db"
security unlock-keychain -p "$password" "$cert_dir/signing.keychain-db"
security import "$cert_dir/certificate.p12" -k "$cert_dir/signing.keychain-db" \
  -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$cert_dir/signing.keychain-db" >/dev/null
security list-keychains -d user -s "$cert_dir/signing.keychain-db" login.keychain-db
rm -f "$cert_dir/certificate.p12"
