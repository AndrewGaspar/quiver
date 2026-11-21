#!/bin/sh
# Quiver: the one root step (design/INSTALL.md), written by `quiver system setup` on 2026-11-21 12:00
# for ajg. It installs only what `quiver system setup --check` found missing or outdated:
#   Lab controllers: missing (game controllers in lab instances (quiver lab … --pads))
#   Stream controllers: missing (game controllers in streams from this machine)
#   Controllers with nobody seated: missing (stream controllers while nobody is signed in at this machine (a headless server))
#   Firewall profiles: missing (firewall rules by name ("Quiver Moonlight", "Quiver Remote"), and a checkbox in a firewall GUI)
#   Start at boot: missing (quiverd at boot, before anyone signs in (a headless server))
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

# 1. Groups (contrib/sysusers/quiver.conf). Nobody is ever a member: each gid is in your subordinate
#    range, so only a lab container or a stream's own namespace is given one.
put sysusers/quiver.conf /etc/sysusers.d/quiver.conf 2fa40fb00642511dd6e9114a390a4618b4525c52cf2c0da169df1d00236b6a68
new=""
for g in quiver-lab-pad quiver-stream-pad quiver-uinput; do
    getent group "$g" >/dev/null || new="$new $g"
done
if [ -n "$new" ]; then
    systemd-sysusers "$ROOT/etc/sysusers.d/quiver.conf"
    for g in $new; do
        getent group "$g" >/dev/null && note "group $g" && echo "created group $g"
    done
fi

# 2. udev rules (each carries its marker line, so Quiver recognises its version).
# Lab controllers: 72-quiver-lab-pad.rules
put lab/72-quiver-lab-pad.rules /etc/udev/rules.d/72-quiver-lab-pad.rules f731b1eaf9faffa667b53c90fda9580b4e05b4ff8260b529fc977241c9fbba0c
# Stream controllers: 72-quiver-stream-pad.rules
put stream/72-quiver-stream-pad.rules /etc/udev/rules.d/72-quiver-stream-pad.rules 7851b64f2283c74d006736ba5f62532e2c4ea26dc4e69cbaeeac42f03059e1e0
# Controllers with nobody seated: 72-quiver-uinput.rules
put udev/72-quiver-uinput.rules /etc/udev/rules.d/72-quiver-uinput.rules 062725397b9c4ed40456236f3a15552529f441c7a64cb63546367d40f6364314
if [ "$udev_changed" = 1 ]; then
    udevadm control --reload
fi
# Apply the uinput rule to the /dev/uinput that exists now (new pads need nothing).
if [ -e "$ROOT/sys/devices/virtual/misc/uinput" ]; then
    udevadm trigger --action=change /sys/devices/virtual/misc/uinput
fi

# 3. Firewall (ufw): Quiver's profiles, then rules from your LAN and tailnet only, never from
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
# Moonlight from 10.0.0.0/24 (enp3s0): can't read the rules without root, so this asks ufw first.
ufw_has 47984,47989,48010/tcp 10.0.0.0/24 && ufw_has 47998:48000/udp 10.0.0.0/24 && ufw_has 5353/udp 10.0.0.0/24 || ufw allow from 10.0.0.0/24 to any app 'Quiver Moonlight' comment quiver
# Moonlight from the tailnet (tailscale0): can't read the rules without root, so this asks ufw first.
ufw_has 47984,47989,48010/tcp 'on tailscale0' && ufw_has 47998:48000/udp 'on tailscale0' || ufw allow in on tailscale0 to any app 'Quiver Moonlight' comment quiver
# Quiver remote play from 10.0.0.0/24 (enp3s0): can't read the rules without root, so this asks ufw first.
ufw_has 47990/tcp 10.0.0.0/24 && ufw_has 47998:48061/udp 10.0.0.0/24 || ufw allow from 10.0.0.0/24 to any app 'Quiver Remote' comment quiver
# Quiver remote play from the tailnet (tailscale0): can't read the rules without root, so this asks ufw first.
ufw_has 47990/tcp 'on tailscale0' && ufw_has 47998:48061/udp 'on tailscale0' || ufw allow in on tailscale0 to any app 'Quiver Remote' comment quiver

# 4. Lingering: your user manager (and quiverd.service, once enabled) starts at boot, before anyone
#    signs in. polkit doesn't let you turn it on yourself here.
if [ ! -e "$ROOT/var/lib/systemd/linger/ajg" ]; then
    loginctl enable-linger ajg
    note "linger ajg"
    echo "lingering on for ajg"
fi

echo "done: check with quiver system setup --check"
