#!/usr/bin/env bash
# Build the exact Deskflow source and teardown patch shipped by Synbad.
set -euo pipefail
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")"/../.. && pwd)"
out="${1:?output directory required}"
arch="${2:?arm64 or x86_64 required}"
case "$arch" in arm64|x86_64) ;; *) exit 2 ;; esac
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --branch v1.17.0 https://github.com/deskflow/deskflow.git "$work/source"
git -C "$work/source" checkout --quiet 44bd69fdc8df726909f35b7efeeebc554bfde1c5
git -C "$work/source" apply --check "$repo/dist/deskflow/teardown.patch"
git -C "$work/source" apply "$repo/dist/deskflow/teardown.patch"
cmake -S "$work/source" -B "$work/build" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES="$arch" \
  -DCMAKE_OSX_SYSROOT="$(xcrun --show-sdk-path)" \
  -DCMAKE_OSX_DEPLOYMENT_TARGET=12.0 -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DCMAKE_PREFIX_PATH="$(brew --prefix qt)" \
  -DOPENSSL_ROOT_DIR="$(brew --prefix openssl@3)" \
  -DBUILD_GUI=OFF -DBUILD_INSTALLER=OFF -DBUILD_TESTS="${SYNBAD_CORE_TESTS:-OFF}"
cmake --build "$work/build" --target deskflow-server deskflow-client --parallel 3
if [ "${SYNBAD_CORE_TESTS:-OFF}" = ON ]; then
  cmake --build "$work/build" --target core-teardown-tests --parallel 3
  "$work/build/bin/core-teardown-tests"
fi
for name in deskflow-server deskflow-client; do
  cp "$work/build/bin/$name" "$out/$name"
  lipo "$out/$name" -verify_arch "$arch"
  # A release must work on machines without Homebrew; the Core's OpenSSL
  # is static and all remaining dependencies must be Apple system libraries.
  if otool -L "$out/$name" | tail -n +2 | awk '{print $1}' | grep -Ev '^(/System/Library/|/usr/lib/)' ; then
    echo "Unexpected non-system Core dependency" >&2
    exit 1
  fi
done
cp "$work/source/LICENSE" "$out/DESKFLOW-LICENSE"
cp "$work/source/LICENSE_EXCEPTION" "$out/DESKFLOW-LICENSE-EXCEPTION"
tar --exclude=.git -czf "$out/deskflow-source-1.17.0-synbad-${arch}.tar.gz" -C "$work" source
