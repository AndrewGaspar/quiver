#!/bin/bash
# Render the app icon (contrib/quiver.svg) to PNG at the standard hicolor sizes, into
# contrib/icons/hicolor/<n>x<n>/apps/quiver.png, for consumers that only read raster icons.
# The PNGs are committed (packaging needs no SVG renderer); rerun after changing the SVG:
#
#   tools/render-icons.sh
#
# Uses librsvg's rsvg-convert (librsvg is on any desktop with GTK).
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
command -v rsvg-convert >/dev/null || { echo "render-icons.sh: needs rsvg-convert (librsvg)" >&2; exit 1; }

for n in 16 24 32 48 64 128 256 512; do
    dir=$repo/contrib/icons/hicolor/${n}x$n/apps
    mkdir -p "$dir"
    rsvg-convert -w "$n" -h "$n" "$repo/contrib/quiver.svg" -o "$dir/quiver.png"
done
echo "rendered contrib/icons/hicolor/*/apps/quiver.png"
