#!/bin/bash
# The end-to-end test's driver: the scratch GameStream server and the real Moonlight client
# (inside a headless gamescope), together in a private namespace.
#
#   GS_SERVER=path/to/quiver-gamestream-test GS_STATE=dir [GS_SERVER_ARGS=...] [GS_MTU=1280] \
#       moonlight-e2e.sh
#
# Steps: pair (PIN 4321), list, reach the server over IPv6, stream at --packet-size 1392 (what
# Moonlight uses for a host it thinks is local: too big for a 1280 MTU, so the server must
# cut it), stream at Moonlight's own choice for 127.0.0.1 (it counts loopback as remote:
# 1024, which fits), quit the app. Everything is logged under GS_STATE; each
# step prints a `STEP` line, and screenshots land in GS_STATE/client/shots.
#
# Isolation: user, mount, network and PID namespaces (so `pgrep moonlight` sees only ours).
# The network namespace has only loopback, its MTU set to GS_MTU (Tailscale's 1280 by default), so nothing reaches the real network
# and the default GameStream ports are free. A private /tmp holds gamescope's X display (ten
# fake lock files push it to :10 or above), its sockets and its screenshots. /dev/input is
# emptied and /dev/null covers every /dev/hidraw* (no real pads or keyboards; SDL's HIDAPI
# is off too). HOME and the XDG dirs point into GS_STATE, so Moonlight's config stays there.
# No session bus (no screensaver inhibit) and SDL's dummy audio driver (nothing plays).
set -u
: "${GS_SERVER:?}" "${GS_STATE:?}"
GS_MTU=${GS_MTU:-1280}
GS_STREAM_SECS=${GS_STREAM_SECS:-10}
if [ -z "${GS_E2E_NS:-}" ]; then
    exec env GS_E2E_NS=1 unshare --user --map-root-user --mount --net --pid --fork --mount-proc --propagation private bash "$0" "$@"
fi

ip link set lo mtu "$GS_MTU" up || exit 90
mount -t tmpfs none /tmp || exit 90
mount -t tmpfs none /dev/input 2>/dev/null || true
for f in /dev/hidraw*; do [ -e "$f" ] && mount --bind /dev/null "$f"; done
mkdir -p /tmp/.X11-unix
for n in $(seq 0 9); do printf '%10d\n' $$ > /tmp/.X$n-lock; done

CLIENT=$GS_STATE/client
export HOME=$CLIENT/home
export XDG_CONFIG_HOME=$HOME/.config XDG_CACHE_HOME=$HOME/.cache XDG_DATA_HOME=$HOME/.local/share
export XDG_RUNTIME_DIR=/tmp/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$CLIENT/shots"
chmod 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS PULSE_SERVER PIPEWIRE_REMOTE
export SDL_AUDIODRIVER=dummy QT_QPA_PLATFORM=xcb SDL_VIDEODRIVER=x11 SDL_JOYSTICK_HIDAPI=0
# (/sys still shows the outer namespace's devices; ask ip.)
echo "STEP namespace lo mtu $(ip -o link show lo | sed -n 's/.* mtu \([0-9]*\).*/\1/p')"

SLOG=$GS_STATE/server.log
# shellcheck disable=SC2086
QUIVER_SHM_ROOT=$GS_STATE/shm RUST_LOG=${RUST_LOG:-info} "$GS_SERVER" --state "$GS_STATE/server" --bind 127.0.0.1 --bind ::1 \
    --pin 4321 --exit-after 400 ${GS_SERVER_ARGS:-} > "$SLOG" 2>&1 &
SPID=$!
for _ in $(seq 300); do grep -q '^READY' "$SLOG" && break; kill -0 $SPID 2>/dev/null || break; sleep 0.1; done
grep -q '^READY' "$SLOG" || { echo "STEP server failed"; tail -20 "$SLOG"; exit 92; }
echo "STEP server ready"

# gamescope can crash while tearing down after its child exits. A core limit of exactly 1
# byte tells the kernel not to hand the dump to systemd-coredump (no crash notification on
# the desktop), and gamescope is killed outright once Moonlight is done.
GAMESCOPE=(prlimit --core=1 gamescope --backend headless -W 1280 -H 720 --)

start_moonlight() { # LOG ARGS...
    local log=$1
    shift
    "${GAMESCOPE[@]}" moonlight "$@" > "$log" 2>&1 &
    GPID=$!
    DISP=
    for _ in $(seq 100); do
        s=$(ls /tmp/.X11-unix 2>/dev/null | sed -n 's/^X\([0-9]*\)$/\1/p' | head -1)
        if [ -n "$s" ]; then DISP=:$s; break; fi
        sleep 0.1
    done
    case "$DISP" in "" | :[0-9]) echo "STEP refusing display '$DISP'"; kill -KILL $GPID; exit 91 ;; esac
    for _ in $(seq 100); do pgrep -x moonlight >/dev/null && break; sleep 0.1; done
}

stop_moonlight() {
    local ml
    ml=$(pgrep -x moonlight) && kill -TERM $ml
    for _ in $(seq 30); do pgrep -x moonlight >/dev/null || break; sleep 0.1; done
    kill -KILL "$GPID" 2>/dev/null
    wait "$GPID" 2>/dev/null
    rm -f /tmp/.X11-unix/X* /tmp/.X1[0-9]-lock
}

# Until Moonlight exits, CHECK succeeds, or SECS pass.
wait_for() { # SECS CHECK...
    local secs=$1
    shift
    for _ in $(seq $((secs * 10))); do
        "$@" && return 0
        pgrep -x moonlight >/dev/null || { sleep 0.5; "$@"; return; }
        sleep 0.1
    done
    return 1
}

shot() { # NAME
    rm -f /tmp/gamescope.png
    DISPLAY=$DISP xprop -root -f GAMESCOPECTRL_REQUEST_SCREENSHOT 32c -set GAMESCOPECTRL_REQUEST_SCREENSHOT 1
    for _ in $(seq 60); do
        if [ -s /tmp/gamescope.png ]; then
            s1=$(stat -c %s /tmp/gamescope.png); sleep 0.1
            [ "$s1" = "$(stat -c %s /tmp/gamescope.png)" ] && break
        fi
        sleep 0.05
    done
    [ -s /tmp/gamescope.png ] && cp /tmp/gamescope.png "$CLIENT/shots/$1.png" && echo "STEP shot $CLIENT/shots/$1.png"
}

stream() { # NAME HOST ARGS...
    local name=$1 host=$2
    shift 2
    start_moonlight "$GS_STATE/$name.log" stream "$host" "Quiver Test Pattern" --resolution 1280x720 --fps 60 --bitrate 8000 --display-mode windowed "$@"
    local t=0
    while [ $t -lt "$GS_STREAM_SECS" ]; do
        sleep 2.5
        t=$((t + 3))
        shot "$name-$t"
    done
    stop_moonlight
    sleep 1
    echo "STEP streamed $name"
}

paired() { grep -q '^PAIRED' "$SLOG"; }
start_moonlight "$GS_STATE/pair.log" pair 127.0.0.1 --pin 4321
wait_for 60 paired && echo "STEP paired"
stop_moonlight

start_moonlight "$GS_STATE/list.log" list 127.0.0.1
listed() { grep -q 'Second Title' "$GS_STATE/list.log"; }
wait_for 30 listed && echo "STEP listed"
stop_moonlight

# Moonlight learns the server's IPv6 address; the first stream then goes over IPv6 and the
# resumed one after it over 127.0.0.1.
start_moonlight "$GS_STATE/list-v6.log" list "[::1]"
# (`moonlight list` then says it failed to connect: its CLI matches hosts by bare address,
# "::1", which is what the stream step uses.)
wait_for 30 false; echo "STEP reached over IPv6"
stop_moonlight

stream stream-1392 ::1 --packet-size 1392
stream stream-auto 127.0.0.1

start_moonlight "$GS_STATE/quit.log" quit 127.0.0.1
quitted() { grep -q "Moonlight quit the app" "$SLOG"; }
wait_for 30 quitted && echo "STEP quit"
stop_moonlight

kill -TERM $SPID
wait $SPID
echo "STEP done"
