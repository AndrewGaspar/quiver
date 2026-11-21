#!/usr/bin/env bash
# Lay out a built Quiver under a staging root. Shared by the Arch package (packaging/arch/
# PKGBUILD) and the per-user install (tools/install-user.sh), so both ship the same files.
#
#   packaging/layout.sh <cargo target dir> <staging root>
#
# Final locations come from the environment (absolute paths, as installed; files land at
# <staging root><path>):
#   BINDIR      quiver, quiverd, quiver-app, quiver-sight, quiver-webview, quiver-stream-host,
#               quiver-stream, ubi (the CLI's old name)  (/usr/bin)
#   LIBDIR      quiver-bridge.exe, ubi-bridge.exe (a link to it, the old name),
#               sight/<arch>/libquiver_sight_layer.so,
#               sight/<arch>/libquiver_sight_input.so         (/usr/lib/quiver)
#               skills/quiver/SKILL.md (the agent skill: how agents use Quiver)
#               omarchy/quiver.downloads/ (the Omarchy bar widget; `quiver omarchy install-plugin`)
#   DATADIR     applications/quiver.desktop, icons/hicolor/,  (/usr/share)
#               vulkan/implicit_layer.d/VK_LAYER_QUIVER_sight.<arch>.json
#   SIGHT_LAYER_I686=1  also the 32-bit Sight layer and input preload (built for i686-unknown-linux-gnu)
#   UNITDIR     quiverd.service (systemd user unit), quiver.slice  (/usr/lib/systemd/user)
#   LICENSEDIR  LICENSE and third-party licenses             (/usr/share/licenses/quiver)
#   EXEC_START  the unit's ExecStart                         ($BINDIR/quiverd)
#   UDEVDIR     72-quiver-lab-pad.rules (system packages only: hides lab pads from the host)
#   SYSUSERSDIR quiver-lab-pad.conf (the group those pads' nodes get)  (unset: neither)
#
# quiverd finds the bridge at <its dir>/../lib/quiver/quiver-bridge.exe, so LIBDIR must be
# $BINDIR/../lib/quiver (crates/quiver-core/src/launcher.rs, bridge_candidates).
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

# quiver-sight (Sight's HUD renderer and toasts) must sit next to quiverd, which starts it, as
# must quiver-stream-host (a stream's host, and Moonlight's display for a game). quiver-stream
# (the stream window) sits next to quiver and quiver-app, which run it.
for bin in quiver quiverd quiver-app quiver-sight quiver-webview quiver-stream-host quiver-stream ubi; do
    install -Dm755 "$target/release/$bin" "$root$BINDIR/$bin"
done
install -Dm644 "$target/x86_64-pc-windows-gnu/release/quiver-bridge.exe" "$root$LIBDIR/quiver-bridge.exe"
# The bridge's name before the rename, for scripts and launch options that still name it.
# install-user.sh installs it as a copy (its manifest tracks files).
ln -sfn quiver-bridge.exe "$root$LIBDIR/ubi-bridge.exe"

# The Sight Vulkan layer (crates/quiver-sight-layer), VK_LAYER_QUIVER_sight: an implicit layer
# the loader enables only with QUIVER_SIGHT=1 (or when VK_INSTANCE_LAYERS names it), which
# Quiver sets for launches with the HUD on. Its manifests are the
# template with the installed path filled in; `library_arch` lets the 64-bit and 32-bit ones
# share the layer name. The 32-bit one only with SIGHT_LAYER_I686=1 (built for
# i686-unknown-linux-gnu, for 32-bit games).
sight_layer() { # <built .so> <arch>
    local lib=$LIBDIR/sight/$2/libquiver_sight_layer.so bits=64
    [[ $2 == i686 ]] && bits=32
    install -Dm755 "$1" "$root$lib"
    install -d "$root$DATADIR/vulkan/implicit_layer.d"
    sed -e "s|@LIBRARY_PATH@|$lib|" -e "s|@LIBRARY_ARCH@|$bits|" \
        "$src/crates/quiver-sight-layer/manifest/VK_LAYER_QUIVER_sight.json.in" \
        >"$root$DATADIR/vulkan/implicit_layer.d/VK_LAYER_QUIVER_sight.$2.json"
    chmod 644 "$root$DATADIR/vulkan/implicit_layer.d/VK_LAYER_QUIVER_sight.$2.json"
}
sight_layer "$target/release/libquiver_sight_layer.so" x86_64
if [[ ${SIGHT_LAYER_I686:-0} == 1 ]]; then
    sight_layer "$target/i686-unknown-linux-gnu/release/libquiver_sight_layer.so" i686
fi
# The Sight panel's input preload (crates/quiver-sight-input), libquiver_sight_input.so, next
# to the layer: quiverd puts $LIBDIR/sight/$PLATFORM/libquiver_sight_input.so in LD_PRELOAD for
# launches with the layer (ld.so's $PLATFORM is x86_64 or i686, these directories' names).
install -Dm755 "$target/release/libquiver_sight_input.so" "$root$LIBDIR/sight/x86_64/libquiver_sight_input.so"
if [[ ${SIGHT_LAYER_I686:-0} == 1 ]]; then
    install -Dm755 "$target/i686-unknown-linux-gnu/release/libquiver_sight_input.so" "$root$LIBDIR/sight/i686/libquiver_sight_input.so"
fi

install -d "$root$DATADIR/applications"
sed -e "s|^Exec=.*|Exec=$BINDIR/quiver-app|" -e "s|^Icon=.*|Icon=quiver|" \
    "$src/contrib/quiver.desktop" >"$root$DATADIR/applications/quiver.desktop"
chmod 644 "$root$DATADIR/applications/quiver.desktop"
install -Dm644 "$src/contrib/quiver.svg" "$root$DATADIR/icons/hicolor/scalable/apps/quiver.svg"
# PNG renders of it (tools/render-icons.sh), for consumers that only read raster icons.
for png in "$src"/contrib/icons/hicolor/*/apps/quiver.png; do
    size=$(basename "$(dirname "$(dirname "$png")")")
    install -Dm644 "$png" "$root$DATADIR/icons/hicolor/$size/apps/quiver.png"
done

# The agent skill (Claude Code / Codex / ~/.agents): matches this build's CLI. A package can
# link $LIBDIR/skills/quiver into a skills directory; tools/install-user.sh does it per user.
install -Dm644 "$src/contrib/skills/quiver/SKILL.md" "$root$LIBDIR/skills/quiver/SKILL.md"

# The Omarchy bar widget for the download queue (contrib/omarchy/quiver.downloads, tests
# left out). Installed only as files: `quiver omarchy install-plugin` links it into
# ~/.config/omarchy/plugins and puts it in the bar, when someone asks for it.
omarchy_plugin=$src/contrib/omarchy/quiver.downloads
for f in manifest.json Widget.qml Body.qml Ring.qml BrandRing.qml Service.qml Model.js "$omarchy_plugin"/i18n/*.json; do
    f=${f#"$omarchy_plugin/"}
    install -Dm644 "$omarchy_plugin/$f" "$root$LIBDIR/omarchy/quiver.downloads/$f"
done

# The daemon's systemd user unit and its slice (design/SERVICE.md). Installed, never enabled
# here: `quiver remote enable --at-boot` enables it (clients start it on demand otherwise).
install -d "$root$UNITDIR"
sed -e "s|^ExecStart=.*|ExecStart=$EXEC_START|" \
    "$src/contrib/quiverd.service" >"$root$UNITDIR/quiverd.service"
chmod 644 "$root$UNITDIR/quiverd.service"
install -Dm644 "$src/contrib/quiver.slice" "$root$UNITDIR/quiver.slice"

# quiver lab's virtual game controllers (design/LAB.md §16): the udev rule that keeps every lab
# pad away from the host, and the group its nodes get. Root's to install, so only a system
# package does (UDEVDIR, SYSUSERSDIR); a per-user install prints the commands
# (`quiver lab doctor`). pacman's hooks run systemd-sysusers and reload udev.
if [[ -n ${SYSUSERSDIR:-} ]]; then
    install -Dm644 "$src/contrib/lab/quiver-lab-pad.sysusers" "$root$SYSUSERSDIR/quiver-lab-pad.conf"
fi
if [[ -n ${UDEVDIR:-} ]]; then
    install -Dm644 "$src/contrib/lab/72-quiver-lab-pad.rules" "$root$UDEVDIR/72-quiver-lab-pad.rules"
fi

lic=$root$LICENSEDIR
install -Dm644 "$src/LICENSE" "$lic/LICENSE"
install -Dm644 "$src/crates/quiver-ui/assets/fonts/Geist-OFL.txt" "$lic/fonts/Geist-OFL.txt"
install -Dm644 "$src/crates/quiver-ui/assets/fonts/Archivo-OFL.txt" "$lic/fonts/Archivo-OFL.txt"
install -Dm644 "$src/crates/quiver-ui/assets/icons/LICENSE" "$lic/icons/Lucide-LICENSE"
# GPUI (vendored and patched under third_party/zed) is Apache-2.0; the same text covers
# text_input.rs, adapted from GPUI's examples/input.rs.
install -Dm644 "$src/third_party/zed/gpui/LICENSE-APACHE" "$lic/gpui/LICENSE-APACHE"
