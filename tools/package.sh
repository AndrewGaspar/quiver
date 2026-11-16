#!/usr/bin/env bash
# Build the quiver-git Arch package from this checkout's HEAD, for a system-wide install
# (the per-user tools/install-user.sh needs no root and is the usual way).
#
# Usage: tools/package.sh [--allow-dirty]
#
#   --allow-dirty  build even with uncommitted changes (they are NOT in the package: it is
#                  built from the committed HEAD)
#
# makepkg runs in QUIVER_PKG_DIR (default ~/.cache/quiver-pkg) and clones this checkout from
# there, so nothing is written into the repo. The cargo target dir in it is kept between
# builds, so rebuilds are incremental. Prints the package and the command to install it.
set -euo pipefail

allow_dirty=0
for arg in "$@"; do
    case $arg in
        --allow-dirty) allow_dirty=1 ;;
        -h | --help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"; exit 0 ;;
        *) echo "package.sh: unknown option '$arg' (see --help)" >&2; exit 2 ;;
    esac
done

repo=$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." rev-parse --show-toplevel)
dirty=$(git -C "$repo" status --porcelain)
if [[ -n $dirty ]]; then
    if ((!allow_dirty)); then
        echo "package.sh: $repo has uncommitted changes; commit them, or pass --allow-dirty to package HEAD without them:" >&2
        echo "$dirty" | sed 's/^/  /' >&2
        exit 1
    fi
    echo "package.sh: warning: uncommitted changes are not packaged (building HEAD)" >&2
fi
commit=$(git -C "$repo" rev-parse HEAD)

dir=${QUIVER_PKG_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/quiver-pkg}
mkdir -p "$dir/pkg"
cp "$repo/packaging/arch/PKGBUILD" "$repo/packaging/arch/quiver.install" "$dir/"
cd "$dir"

export QUIVER_REPO=$repo QUIVER_COMMIT=$commit QUIVER_CARGO_TARGET_DIR=$dir/target
export PKGDEST=$dir/pkg SRCDEST=$dir BUILDDIR=$dir
makepkg -f --noconfirm

pkg=$(makepkg --packagelist | grep -v -- '-debug-' | head -n1)
[[ -f $pkg ]] || { echo "package.sh: makepkg finished but $pkg isn't there" >&2; exit 1; }
# Keep the newest three packages (something to roll back to with pacman -U).
ls -1t "$dir"/pkg/quiver-git-*.pkg.tar.zst 2>/dev/null | tail -n +4 | xargs -r rm -f

echo
echo "Package: $pkg"
echo "Install: sudo pacman -U $pkg"
