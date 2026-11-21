#!/bin/bash
# The end-to-end test's driver: the scratch GameStream server and the real Moonlight client
# (inside a headless gamescope), together in a private namespace.
#
#   GS_SERVER=path/to/quiver-gamestream-test GS_STATE=dir [GS_SERVER_ARGS=...] [GS_STREAM_ARGS=...] [GS_MTU=1280] \
#       moonlight-e2e.sh
#
# With GS_QUIVERD=path/to/quiverd and GS_CLI=path/to/quiver instead, the server is a scratch
# quiverd (its own HOME and XDG dirs under GS_STATE/daemon): the CLI turns Moonlight on and
# enters the PIN, and the app streamed is quiverd's "Test pattern", or GS_APP (a game: with
# GS_DAEMON_ENV="QUIVER_TEST_GAMES=… QUIVER_STREAM_HOST_BIN=…", a scratch test program
# launched into the stream's display as quiverd launches games).
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
APP="Quiver Test Pattern"
LISTED="Second Title"
if [ -n "${GS_QUIVERD:-}" ]; then
    D=$GS_STATE/daemon
    # The runtime dir (its socket) on the private /tmp: socket paths are short.
    DRUN=/tmp/qd
    mkdir -p "$D/data/quiver/remote" "$DRUN"
    chmod 700 "$DRUN"
    echo '{"accounts":[]}' > "$D/data/quiver/accounts.json"
    printf '{"library":"%s/lib","prefixes":"%s/pfx","proton":"%s","sdk_source":"%s/sdk","save_backups":"%s/backups","report_playtime":false,"steam_offers":false}' "$D" "$D" "${GS_PROTON:-$D/proton}" "$D" "$D" > "$D/data/quiver/config.json"
    echo '{"name":"tower"}' > "$D/data/quiver/remote/machine.json"
    daemon_env() {
        env -i PATH=/usr/bin:/bin HOME="$D/home" XDG_DATA_HOME="$D/data" XDG_CONFIG_HOME="$D/config" XDG_STATE_HOME="$D/state" \
            XDG_CACHE_HOME="$D/cache" XDG_RUNTIME_DIR="$DRUN" QUIVER_SHM_ROOT="$GS_STATE/shm" QUIVER_BROKER=127.0.0.1:0 \
            QUIVER_AUTO_RESTART=0 QUIVER_REMOTE_MDNS=0 QUIVER_MOONLIGHT_TEST_PATTERN=1 RUST_LOG=${RUST_LOG:-info} ${GS_DAEMON_ENV:-} "$@"
    }
    daemon_env "$GS_QUIVERD" > "$SLOG" 2>&1 &
    SPID=$!
    for _ in $(seq 300); do [ -S "$DRUN/quiver/quiverd.sock" ] && break; sleep 0.1; done
    daemon_env "$GS_CLI" remote moonlight enable --bind 127.0.0.1 --bind ::1 > "$GS_STATE/enable.log" 2>&1 || { echo "STEP enable failed"; cat "$GS_STATE/enable.log"; exit 92; }
    grep -q 'listening on' "$GS_STATE/enable.log" || { echo "STEP server failed"; cat "$GS_STATE/enable.log"; tail -20 "$SLOG"; exit 92; }
    APP=${GS_APP:-Test pattern}
    LISTED=${GS_APP:-Test pattern}
else
    # shellcheck disable=SC2086
    QUIVER_SHM_ROOT=$GS_STATE/shm RUST_LOG=${RUST_LOG:-info} "$GS_SERVER" --state "$GS_STATE/server" --bind 127.0.0.1 --bind ::1 \
        --pin 4321 --exit-after 400 ${GS_SERVER_ARGS:-} > "$SLOG" 2>&1 &
    SPID=$!
    for _ in $(seq 300); do grep -q '^READY' "$SLOG" && break; kill -0 $SPID 2>/dev/null || break; sleep 0.1; done
    grep -q '^READY' "$SLOG" || { echo "STEP server failed"; tail -20 "$SLOG"; exit 92; }
fi
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
    start_moonlight "$GS_STATE/$name.log" stream "$host" "$APP" --resolution 1280x720 --fps 60 --bitrate 8000 --display-mode windowed ${GS_STREAM_ARGS:-} "$@"
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

# Moonlight 6.2 makes its pairing key from the salted PIN's hash with a C-string copy, so
# when the hash has a zero byte in its first 16 (about 1 salt in 17) its key is wrong and it
# reports "Incorrect PIN" (fixed in later moonlight-qt). A retry draws a new salt.
paired() { grep -q '^PAIRED' "$SLOG"; }
settled() { paired || [ "$(grep -c '^PAIR FAILED' "$SLOG")" -ge "$attempt" ]; }
# With quiverd, the PIN goes in through the CLI once the request shows in its status.
pending() { daemon_env "$GS_CLI" remote moonlight status --json 2>/dev/null | grep -q '"pending"'; }
for attempt in 1 2 3; do
    start_moonlight "$GS_STATE/pair-$attempt.log" pair 127.0.0.1 --pin 4321
    if [ -n "${GS_QUIVERD:-}" ]; then
        wait_for 30 pending
        if daemon_env "$GS_CLI" remote moonlight pin 4321 > "$GS_STATE/pin-$attempt.log" 2>&1; then
            echo "PAIRED $(cat "$GS_STATE/pin-$attempt.log")" >> "$SLOG"
        else
            echo "PAIR FAILED $(cat "$GS_STATE/pin-$attempt.log")" >> "$SLOG"
        fi
    else
        wait_for 60 settled
    fi
    # Moonlight finishes on its side (the HTTPS check, saving the host) after the server pins it.
    paired && wait_for 5 false
    stop_moonlight
    paired && break
done
paired && echo "STEP paired"

start_moonlight "$GS_STATE/list.log" list 127.0.0.1
listed() { grep -q "$LISTED" "$GS_STATE/list.log"; }
wait_for 30 listed && echo "STEP listed"
stop_moonlight

# GS_RESUME=1 (with GS_QUIVERD and a GS_APP game): the resume round instead of the two
# streams. Stream the game, kill the Moonlight process outright (as iOS does to a backgrounded
# app), see quiverd keep the game, stream again (Moonlight resumes the running app), quit.
if [ -n "${GS_RESUME:-}" ]; then
    first_video() { grep -q "Received first video packet" "$1"; }
    start_moonlight "$GS_STATE/resume-1.log" stream 127.0.0.1 "$APP" --resolution 1280x720 --fps 60 --bitrate 8000 --display-mode windowed
    wait_for 90 first_video "$GS_STATE/resume-1.log" && echo "STEP first stream video"
    sleep 2
    shot resume-1
    kill -KILL "$(pgrep -x moonlight)" 2>/dev/null
    kill -KILL "$GPID" 2>/dev/null
    wait "$GPID" 2>/dev/null
    rm -f /tmp/.X11-unix/X* /tmp/.X1[0-9]-lock
    echo "STEP killed client"
    kept() { daemon_env "$GS_CLI" stream --list > "$GS_STATE/streams-kept.txt" 2>&1; grep -q "resume from Moonlight" "$GS_STATE/streams-kept.txt"; }
    for _ in $(seq 300); do kept && break; sleep 0.1; done
    kept && echo "STEP kept"
    # What quiverd's restart waits on: the kept stream.
    daemon_env "$GS_CLI" daemon status > "$GS_STATE/daemon-kept.txt" 2>&1
    start_moonlight "$GS_STATE/resume-2.log" stream 127.0.0.1 "$APP" --resolution 1280x720 --fps 60 --bitrate 8000 --display-mode windowed
    wait_for 60 first_video "$GS_STATE/resume-2.log" && echo "STEP resumed video"
    sleep 2
    shot resume-2
    stop_moonlight
    sleep 1
    start_moonlight "$GS_STATE/quit.log" quit 127.0.0.1
    quitted() { grep -q "Moonlight quit the app" "$SLOG"; }
    wait_for 30 quitted && echo "STEP quit"
    stop_moonlight
    sleep 3
    daemon_env "$GS_CLI" stream --list > "$GS_STATE/streams-after.txt" 2>&1
    kill -TERM $SPID
    wait $SPID
    echo "STEP done"
    exit 0
fi

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
