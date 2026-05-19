# Synbad brand assets

Vector masters live here; raster outputs are generated on demand by
[`scripts/generate-icons.sh`](scripts/generate-icons.sh) and committed only
when needed by platform packaging (e.g. `synbad.ico` for Windows, `synbad.icns`
for the macOS bundle).

| File | Purpose |
|------|---------|
| [`icon.svg`](icon.svg) | Square mark — app icon, tray icon, favicon source |
| [`logo.svg`](logo.svg) | Mark + wordmark — README hero, site header |
| [`scripts/generate-icons.sh`](scripts/generate-icons.sh) | Rasterize to PNG/ICO/ICNS |
| `png/` | Generated PNGs at standard sizes (not committed; output of the script) |

## Regenerating

```sh
# Install at least one renderer:
#   Linux:   sudo apt install librsvg2-bin icoutils   (or imagemagick)
#   macOS:   brew install librsvg                     (iconutil ships with Xcode)
#   Windows: use Inkscape, or run this script in WSL
bash assets/scripts/generate-icons.sh
```

## Brand colors

| Token        | Hex       | Use |
|--------------|-----------|-----|
| Primary 600  | `#7c3aed` | Brand gradient start (violet) |
| Primary 700  | `#2563eb` | Brand gradient end (blue) |
| Surface      | `#ffffff` | Screen fills on the mark |
| Ink          | `#0f172a` | Body text on light backgrounds |
| Muted        | `#475569` | Secondary text / taglines |

## Usage rules (Synbad mark only)

The Synbad mark and wordmark are licensed alongside the source ([MIT](../LICENSE))
for documentation, packaging, and distributions of Synbad itself. Don't use
them to suggest endorsement of unrelated projects, and don't pair the mark
with "Synergy" branding — see [docs/LICENSING.md](../docs/LICENSING.md) for
trademark constraints around the upstream Symless mark.
