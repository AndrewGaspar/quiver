#!/usr/bin/env bash
# Lay out a built Quiver under a staging root. Shared by the Arch package (packaging/arch/
# PKGBUILD) and the per-user install (tools/install-user.sh), so both ship the same files.
#
#   packaging/layout.sh <cargo target dir> <staging root>
#
# Final locations come from the environment (absolute paths, as installed; files land at
# <staging root><path>):
#   BINDIR      quiver, quiverd, quiver-app, ubi             (/usr/bin)
#   LIBDIR      ubi-bridge.exe, sight/<arch>/libquiver_sight_layer.so  (/usr/lib/quiver)
#   DATADIR     applications/quiver.desktop, icons/hicolor/,  (/usr/share)
#               vulkan/explicit_layer.d/VK_LAYER_QUIVER_sight.<arch>.json
#   SIGHT_LAYER_I686=1  also the 32-bit Sight layer (built for i686-unknown-linux-gnu)
#   UNITDIR     quiverd.service (systemd user unit)          (/usr/lib/systemd/user)
#   LICENSEDIR  LICENSE and third-party licenses             (/usr/share/licenses/quiver)
#   EXEC_START  the unit's ExecStart                         ($BINDIR/quiverd)
#
# quiverd finds the bridge at <its dir>/../lib/quiver/ubi-bridge.exe, so LIBDIR must be
# $BINDIR/../lib/quiver (crates/ubi-core/src/launcher.rs, bridge_candidates).
set -euo pipefail

target=${1:?usage: layout.sh <cargo target dir> <staging root>}
root=${2:?usage: layout.sh <cargo target dir> <staging root>}
src=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

: "${BINDIR:=/usr/bin}" "${LIBDIR:=/usr/lib/quiver}" "${DATADIR:=/usr/share}"
: "${UNITDIR:=/usr/lib/systemd/user}" "${LICENSEDIR:=/usr/share/licenses/quiver}"
: "${EXEC_START:=$BINDIR/quiverd}"

if [[ "$(realpath -m "$BINDIR/../lib/quiver")" != "$(realpath -m "$LIBDIR")" ]]; then
    echo "layout.sh: LIBDIR ($LIBDIR) must be \$BINDIR/../lib/quiver, where quiverd looks for the bridge" >&2
    exit 1
fi

for bin in quiver quiverd quiver-app ubi; do
    install -Dm755 "$target/release/$bin" "$root$BINDIR/$bin"
done
install -Dm644 "$target/x86_64-pc-windows-gnu/release/ubi-bridge.exe" "$root$LIBDIR/ubi-bridge.exe"

# The Sight Vulkan layer (crates/ubi-sight-layer), VK_LAYER_QUIVER_sight: an explicit layer,
# so it loads only into launches that name it in VK_INSTANCE_LAYERS. Its manifests are the
# template with the installed path filled in; `library_arch` lets the 64-bit and 32-bit ones
# share the layer name. The 32-bit one only with SIGHT_LAYER_I686=1 (built for
# i686-unknown-linux-gnu, for 32-bit games).
sight_layer() { # <built .so> <arch>
    local lib=$LIBDIR/sight/$2/libquiver_sight_layer.so bits=64
    [[ $2 == i686 ]] && bits=32
    install -Dm755 "$1" "$root$lib"
    install -d "$root$DATADIR/vulkan/explicit_layer.d"
    sed -e "s|@LIBRARY_PATH@|$lib|" -e "s|@LIBRARY_ARCH@|$bits|" \
        "$src/crates/ubi-sight-layer/manifest/VK_LAYER_QUIVER_sight.json.in" \
        >"$root$DATADIR/vulkan/explicit_layer.d/VK_LAYER_QUIVER_sight.$2.json"
    chmod 644 "$root$DATADIR/vulkan/explicit_layer.d/VK_LAYER_QUIVER_sight.$2.json"
}
sight_layer "$target/release/libquiver_sight_layer.so" x86_64
if [[ ${SIGHT_LAYER_I686:-0} == 1 ]]; then
    sight_layer "$target/i686-unknown-linux-gnu/release/libquiver_sight_layer.so" i686
fi

install -d "$root$DATADIR/applications"
sed -e "s|^Exec=.*|Exec=$BINDIR/quiver-app|" -e "s|^Icon=.*|Icon=quiver|" \
    "$src/contrib/quiver.desktop" >"$root$DATADIR/applications/quiver.desktop"
chmod 644 "$root$DATADIR/applications/quiver.desktop"
install -Dm644 "$src/contrib/quiver.svg" "$root$DATADIR/icons/hicolor/scalable/apps/quiver.svg"

install -d "$root$UNITDIR"
sed -e "s|^ExecStart=.*|ExecStart=$EXEC_START|" \
    "$src/contrib/quiverd.service" >"$root$UNITDIR/quiverd.service"
chmod 644 "$root$UNITDIR/quiverd.service"

lic=$root$LICENSEDIR
install -Dm644 "$src/LICENSE" "$lic/LICENSE"
install -Dm644 "$src/crates/ubi-ui/assets/fonts/Geist-OFL.txt" "$lic/fonts/Geist-OFL.txt"
install -Dm644 "$src/crates/ubi-ui/assets/fonts/Archivo-OFL.txt" "$lic/fonts/Archivo-OFL.txt"
install -Dm644 "$src/crates/ubi-ui/assets/icons/LICENSE" "$lic/icons/Lucide-LICENSE"
# GPUI (vendored and patched under third_party/zed) is Apache-2.0; the same text covers
# text_input.rs, adapted from GPUI's examples/input.rs.
install -Dm644 "$src/third_party/zed/gpui/LICENSE-APACHE" "$lic/gpui/LICENSE-APACHE"
