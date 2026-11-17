#!/usr/bin/env bash
# Lay out a built Quiver under a staging root. Shared by the Arch package (packaging/arch/
# PKGBUILD) and the per-user install (tools/install-user.sh), so both ship the same files.
#
#   packaging/layout.sh <cargo target dir> <staging root>
#
# Final locations come from the environment (absolute paths, as installed; files land at
# <staging root><path>):
#   BINDIR      quiver, quiverd, quiver-app, ubi             (/usr/bin)
#   LIBDIR      ubi-bridge.exe                               (/usr/lib/quiver)
#   DATADIR     applications/quiver.desktop, icons/hicolor/  (/usr/share)
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
