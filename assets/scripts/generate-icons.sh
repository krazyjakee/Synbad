#!/usr/bin/env bash
# Rasterize assets/icon.svg into the per-platform sizes Synbad ships.
#
# Outputs:
#   assets/png/synbad-{16,32,48,64,128,256,512,1024}.png
#   assets/synbad.ico          (Windows multi-resolution)
#   assets/synbad.icns         (macOS, only when iconutil is available)
#
# Requires one of: rsvg-convert (preferred), inkscape, or magick/convert.
# Optional: icotool (icoutils) for .ico, iconutil (macOS) for .icns.

set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
src="${here}/icon.svg"
out="${here}/png"
mkdir -p "${out}"

renderer=""
for cmd in rsvg-convert inkscape magick convert; do
  if command -v "$cmd" >/dev/null 2>&1; then renderer="$cmd"; break; fi
done

if [ -z "$renderer" ]; then
  echo "error: need rsvg-convert, inkscape, or imagemagick on PATH" >&2
  exit 1
fi

render() {
  local size="$1" dst="$2"
  case "$renderer" in
    rsvg-convert) rsvg-convert -w "$size" -h "$size" "$src" -o "$dst" ;;
    inkscape)     inkscape "$src" --export-type=png --export-filename="$dst" -w "$size" -h "$size" >/dev/null ;;
    magick)       magick -background none -density 384 "$src" -resize "${size}x${size}" "$dst" ;;
    convert)      convert -background none -density 384 "$src" -resize "${size}x${size}" "$dst" ;;
  esac
}

for size in 16 32 48 64 128 256 512 1024; do
  render "$size" "${out}/synbad-${size}.png"
  echo "  -> ${out}/synbad-${size}.png"
done

# Windows .ico (multi-resolution)
if command -v icotool >/dev/null 2>&1; then
  icotool -c \
    "${out}/synbad-16.png" \
    "${out}/synbad-32.png" \
    "${out}/synbad-48.png" \
    "${out}/synbad-64.png" \
    "${out}/synbad-128.png" \
    "${out}/synbad-256.png" \
    -o "${here}/synbad.ico"
  echo "  -> ${here}/synbad.ico"
elif command -v magick >/dev/null 2>&1; then
  magick "${out}/synbad-16.png" "${out}/synbad-32.png" "${out}/synbad-48.png" \
         "${out}/synbad-64.png" "${out}/synbad-128.png" "${out}/synbad-256.png" \
         "${here}/synbad.ico"
  echo "  -> ${here}/synbad.ico"
else
  echo "  (skipped synbad.ico — install icoutils or imagemagick)" >&2
fi

# macOS .icns (only on Darwin with iconutil)
if [ "$(uname -s)" = "Darwin" ] && command -v iconutil >/dev/null 2>&1; then
  iconset="${here}/synbad.iconset"
  rm -rf "$iconset"; mkdir -p "$iconset"
  cp "${out}/synbad-16.png"   "${iconset}/icon_16x16.png"
  cp "${out}/synbad-32.png"   "${iconset}/icon_16x16@2x.png"
  cp "${out}/synbad-32.png"   "${iconset}/icon_32x32.png"
  cp "${out}/synbad-64.png"   "${iconset}/icon_32x32@2x.png"
  cp "${out}/synbad-128.png"  "${iconset}/icon_128x128.png"
  cp "${out}/synbad-256.png"  "${iconset}/icon_128x128@2x.png"
  cp "${out}/synbad-256.png"  "${iconset}/icon_256x256.png"
  cp "${out}/synbad-512.png"  "${iconset}/icon_256x256@2x.png"
  cp "${out}/synbad-512.png"  "${iconset}/icon_512x512.png"
  cp "${out}/synbad-1024.png" "${iconset}/icon_512x512@2x.png"
  iconutil -c icns "$iconset" -o "${here}/synbad.icns"
  rm -rf "$iconset"
  echo "  -> ${here}/synbad.icns"
fi
