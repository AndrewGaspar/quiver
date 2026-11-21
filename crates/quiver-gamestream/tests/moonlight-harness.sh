#!/bin/bash
# Runs the real Moonlight client inside a headless gamescope, isolated from the person's
# session, for the end-to-end test.
#
#   moonlight-harness.sh STATE run SECS ARGS...           moonlight ARGS, until it exits (at most SECS)
#   moonlight-harness.sh STATE stream SECS SHOTS ARGS...  moonlight ARGS for SECS seconds,
#                                                         with a screenshot every SHOTS seconds
#
# Isolation: a user and mount namespace with a private /tmp (so gamescope's X display, its
# lock files and its screenshot are ours; ten fake lock files push the display to :10 or
# above), an empty /dev/input and /dev/null over every /dev/hidraw* (no real pads or
# keyboards; SDL's HIDAPI is off too). HOME and the XDG dirs point into STATE, so
# Moonlight's config (its paired hosts) stays there. No session bus (no screensaver inhibit
# on the desktop) and SDL's dummy audio driver (nothing plays aloud).
set -u
STATE=$1
shift
if [ -z "${GS_HARNESS_NS:-}" ]; then
    exec env GS_HARNESS_NS=1 unshare --user --map-root-user --mount --propagation private bash "$0" "$STATE" "$@"
fi

mount -t tmpfs none /tmp || exit 90
mount -t tmpfs none /dev/input 2>/dev/null || true
# SDL also opens pads through hidraw (and may switch a Steam Controller's mode): hide them.
for f in /dev/hidraw*; do [ -e "$f" ] && mount --bind /dev/null "$f"; done
mkdir -p /tmp/.X11-unix
for n in $(seq 0 9); do printf '%10d\n' $$ > /tmp/.X$n-lock; done

export HOME=$STATE/home
export XDG_CONFIG_HOME=$HOME/.config XDG_CACHE_HOME=$HOME/.cache XDG_DATA_HOME=$HOME/.local/share
# The private /tmp keeps the runtime dir short (socket paths) and ours.
export XDG_RUNTIME_DIR=/tmp/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$STATE/shots"
chmod 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS PULSE_SERVER PIPEWIRE_REMOTE
export SDL_AUDIODRIVER=dummy QT_QPA_PLATFORM=xcb SDL_VIDEODRIVER=x11 SDL_JOYSTICK_HIDAPI=0

# gamescope can crash while tearing down after its child exits. A core limit of exactly 1
# byte tells the kernel not to hand the dump to systemd-coredump (no crash notification on
# the desktop), and gamescope is killed outright once Moonlight is gone.
GAMESCOPE=(prlimit --core=1 gamescope --backend headless -W 1280 -H 720 --)

# Waits for gamescope's Moonlight child to appear and then exit (at most SECS seconds).
wait_moonlight() {
    local secs=$1 ml= i
    for i in $(seq 100); do
        ml=$(pgrep -P "$GPID" -x moonlight) && break
        kill -0 "$GPID" 2>/dev/null || return
        sleep 0.1
    done
    for i in $(seq $((secs * 10))); do
        kill -0 "$ml" 2>/dev/null || return
        sleep 0.1
    done
}

stop_all() {
    local ml
    ml=$(pgrep -P "$GPID" -x moonlight) && kill -TERM $ml
    for _ in $(seq 30); do
        pgrep -P "$GPID" -x moonlight >/dev/null || break
        sleep 0.1
    done
    kill -KILL "$GPID" 2>/dev/null
    wait "$GPID" 2>/dev/null
}

find_display() {
    DISP=
    for _ in $(seq 100); do
        s=$(ls /tmp/.X11-unix 2>/dev/null | sed -n 's/^X\([0-9]*\)$/\1/p' | head -1)
        if [ -n "$s" ]; then DISP=:$s; break; fi
        sleep 0.1
    done
    echo "HARNESS display $DISP"
    case "$DISP" in :[0-9]) echo "HARNESS refusing low display $DISP"; kill -KILL "$GPID"; exit 91 ;; esac
}

MODE=$1
shift
case "$MODE" in
run)
    SECS=$1
    shift
    "${GAMESCOPE[@]}" moonlight "$@" &
    GPID=$!
    find_display
    wait_moonlight "$SECS"
    stop_all
    echo "HARNESS done"
    ;;
stream)
    SECS=$1 SHOTS=$2
    shift 2
    "${GAMESCOPE[@]}" moonlight "$@" &
    GPID=$!
    find_display
    t=0 n=0
    while [ "$t" -lt "$SECS" ]; do
        sleep "$SHOTS"
        t=$((t + SHOTS))
        kill -0 $GPID 2>/dev/null || { echo "HARNESS gamescope exited early"; break; }
        rm -f /tmp/gamescope.png
        DISPLAY=$DISP xprop -root -f GAMESCOPECTRL_REQUEST_SCREENSHOT 32c -set GAMESCOPECTRL_REQUEST_SCREENSHOT 1
        for _ in $(seq 60); do
            if [ -s /tmp/gamescope.png ]; then
                s1=$(stat -c %s /tmp/gamescope.png); sleep 0.1
                [ "$s1" = "$(stat -c %s /tmp/gamescope.png)" ] && break
            fi
            sleep 0.05
        done
        n=$((n + 1))
        if [ -s /tmp/gamescope.png ]; then
            cp /tmp/gamescope.png "$STATE/shots/shot-$n.png" && echo "HARNESS shot $STATE/shots/shot-$n.png"
        else
            echo "HARNESS no screenshot at ${t}s"
        fi
    done
    stop_all
    echo "HARNESS done"
    ;;
*)
    echo "unknown mode $MODE" >&2
    exit 2
    ;;
esac
