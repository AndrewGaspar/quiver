#!/bin/sh
# Quiver: the one root step (design/INSTALL.md), written by `quiver system setup` on 2026-11-21 12:00
# for ajg. It installs only what `quiver system setup --check` found missing or outdated:
#   Firewall profiles: missing (firewall rules by name ("Quiver Moonlight", "Quiver Remote"), and a checkbox in a firewall GUI)
#   Firewall: remote play: missing (your other Quiver machines reaching this one (remote play, streams))
# Running it again changes nothing. `quiver system setup --undo` removes what it installed.
# Files are copied byte for byte from the installed Quiver, never written here.
SRC=/home/ajg/.local/lib/quiver/contrib
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

# put REL DEST SHA256: install $SRC/REL as DEST unless DEST already is that file. The
# source must be the exact file this Quiver ships.
put() {
    if ! printf '%s  %s\n' "$3" "$SRC/$1" | sha256sum -c --status; then
        echo "setup: $SRC/$1 isn't the file this Quiver ships (sha256 $3); reinstall Quiver and run quiver system setup again" >&2
        exit 1
    fi
    if [ -f "$ROOT$2" ] && cmp -s "$SRC/$1" "$ROOT$2"; then
        return 0
    fi
    install -Dm644 "$SRC/$1" "$ROOT$2"
    note "file $2"
    echo "installed $2"
    case $2 in */udev/rules.d/*) udev_changed=1 ;; esac
}

# 1. Firewall (ufw): Quiver's profiles, then rules from your LAN and tailnet only, never from
#    anywhere, for the servers you have on. Each rule is tagged "quiver" so --undo finds it.
put ufw/quiver /etc/ufw/applications.d/quiver d1b93ddb930c12870daf1328cf2c1ce80c881baa06f2056c755b198a4a3c11bd

# ufw_has SPEC WHERE: `ufw status` already allows SPEC (47984,47989,48010/tcp) from WHERE (a
# subnet, or "on IFACE"), as a rule of your own may.
ufw_has() {
    ufw status | awk -v spec="$1" -v where="$2" '
        where ~ /^on / { split(where, w, " "); if ($1 == spec && $2 == "on" && $3 == w[2] && $4 == "ALLOW") found = 1; next }
        $1 == spec && $2 == "ALLOW" && ($3 == where || ($3 == "IN" && $4 == where)) { found = 1 }
        END { exit !found }'
}
# Quiver remote play from 192.168.50.0/24 (wlp191s0): TCP 47990; UDP 48001–48061 not allowed yet.
ufw_has 47990/tcp 192.168.50.0/24 && ufw_has 47998:48061/udp 192.168.50.0/24 || ufw allow from 192.168.50.0/24 to any app 'Quiver Remote' comment quiver
# Quiver remote play from the tailnet (tailscale0): TCP 47990; UDP 48001–48061 not allowed yet.
ufw_has 47990/tcp 'on tailscale0' && ufw_has 47998:48061/udp 'on tailscale0' || ufw allow in on tailscale0 to any app 'Quiver Remote' comment quiver

echo "done: check with quiver system setup --check"
