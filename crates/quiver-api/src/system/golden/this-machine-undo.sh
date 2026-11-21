#!/bin/sh
# Quiver: undo the root step (design/INSTALL.md), written by `quiver system setup --undo` on 2026-11-21 12:00
# for ajg. It removes only what `quiver system setup` installed (/var/lib/quiver/system-setup.list) and the
# firewall rules tagged "quiver". Everything else stays, the rules you installed by hand included:
#   /etc/udev/rules.d/72-quiver-lab-pad.rules (not installed by setup)
#   /etc/udev/rules.d/72-quiver-stream-pad.rules (not installed by setup)
#   ufw rules without the "quiver" comment
set -eu

# Every path below is under $ROOT: empty for the real system; tests point it at a fake one.
ROOT=${QUIVER_SETUP_ROOT:-}
if [ -z "$ROOT" ] && [ "$(id -u)" != 0 ]; then
    echo "This is Quiver's root step: run it with sudo (sudo sh $0)." >&2
    exit 1
fi
LIST="$ROOT/var/lib/quiver/system-setup.list"
mkdir -p "$(dirname "$LIST")"
touch "$LIST"
note() { grep -qxF "$1" "$LIST" || echo "$1" >>"$LIST"; }
udev_changed=0

# rm_ours PATH MARKER: remove PATH if it still is Quiver's file.
rm_ours() {
    [ -f "$ROOT$1" ] || return 0
    if ! grep -qF "$2" "$ROOT$1"; then
        echo "left alone: $1 (it isn't Quiver's any more)"
        return 0
    fi
    rm -f "$ROOT$1"
    echo "removed $1"
    case $1 in */udev/rules.d/*) udev_changed=1 ;; esac
}

# ufw rules tagged "quiver" (yours, without the tag, stay), last first so the numbers hold.
i=0
while n=$(ufw status numbered | sed -n 's/^\[ *\([0-9][0-9]*\)\].*# quiver$/\1/p' | tail -n 1) && [ -n "$n" ]; do
    i=$((i + 1))
    [ "$i" -le 200 ] || { echo "setup: ufw rule $n won't go away" >&2; exit 1; }
    ufw --force delete "$n"
done

rm -f "$LIST"
echo "done: check with quiver system setup --check"
