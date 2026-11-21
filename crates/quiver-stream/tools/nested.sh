#!/usr/bin/env bash
# Run a command (quiver-stream) inside a private, headless gamescope, so window tests never
# touch the desktop the person is using:
#   - bubblewrap gives it its own /tmp (with X lock files for :0-:9, so Xwayland takes :10
#     or later), its own network namespace (so the abstract X sockets are its own too), a
#     /dev without input devices or hidraw (no real pads or keyboards), and an empty
#     XDG_RUNTIME_DIR (no Wayland, PipeWire or D-Bus of the session).
#   - SDL audio goes to the dummy driver.
#
#   OUT=dir ACTIONS='sleep 3; shot first; key ctrl+alt+shift+s; sleep 1; shot stats' \
#     nested.sh [-W 1920] [-H 1080] [-r 60] [--hdr] [--share-net] -- quiver-stream play file.h265 ...
#
# --share-net keeps the caller's network namespace, so the command can reach a stream host on
# loopback (or, under quiver-netlab, the lab's client side). Abstract X sockets are then
# shared with every other X server in that namespace, so every display with a lock file in
# the real /tmp is locked inside too (as are :0-:9), and the display the command gets is
# checked against the real /tmp's sockets before anything runs.
# STDIN_FILE=path feeds that file to the command's stdin (a stream ticket for `connect`); - passes
# this script's own stdin through.
#
# ACTIONS is a ';'-separated script run once the command has started:
#   sleep S | shot NAME (gamescope screenshot to $OUT/NAME.png) | key A+B+C (X keysyms or
#   keycodes, tapped) | down K | up K | click N | move DX DY | xkb OPTION (setxkbmap -option)
# Input goes in with XTest (target/debug/examples/xinject). The command's stderr is in
# $OUT/stderr.log, stdout in $OUT/stdout.log.
set -euo pipefail
W=1920 H=1080 R=60 HDR=() SHARE_NET=0
while [ $# -gt 0 ]; do
    case "$1" in
        -W) W=$2; shift 2 ;;
        -H) H=$2; shift 2 ;;
        -r) R=$2; shift 2 ;;
        --hdr) HDR=(--hdr-enabled --hdr-debug-force-output); shift ;;
        --share-net) SHARE_NET=1; shift ;;
        --) shift; break ;;
        *) break ;;
    esac
done
[ $# -gt 0 ] || { echo "usage: nested.sh [-W w] [-H h] [-r hz] [--hdr] -- command..." >&2; exit 2; }
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../../.." && pwd)
OUT=${OUT:-$root/target/stream-test/nested}
mkdir -p "$OUT"
XINJECT=${XINJECT:-$root/target/debug/examples/xinject}
ACTIONS=${ACTIONS:-}
uid=$(id -u)

devs=()
for d in /dev/dri /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm /dev/nvidia-uvm-tools /dev/nvidia-caps; do
    [ -e "$d" ] && devs+=(--dev-bind "$d" "$d")
done

# Displays the rest of this network namespace has, which a shared-net run must not take.
taken=""
if [ "$SHARE_NET" = 1 ]; then
    for l in /tmp/.X*-lock; do
        [ -e "$l" ] || continue
        n=${l#/tmp/.X}; n=${n%-lock}
        taken="$taken $n"
    done
fi
STDIN_FILE=${STDIN_FILE:-/dev/null}

net=(--unshare-net)
[ "$SHARE_NET" = 1 ] && net=()
# The stdin file and the stats file live outside the private /tmp.
binds=()
for f in "$STDIN_FILE" "${QUIVER_NETLAB_STATS:-}"; do
    [ -n "$f" ] && [ "$f" != /dev/null ] && [ "$f" != - ] && [ -e "$f" ] && binds+=(--bind "$f" "$f")
done

inner=$(cat <<'EOS'
set -u
for d in 0 1 2 3 4 5 6 7 8 9 $TAKEN; do printf "%10d\n" $$ > /tmp/.X$d-lock; done
mkdir -p /tmp/xdg && chmod 700 /tmp/xdg
exec gamescope --backend headless -W "$NW" -H "$NH" -w "$NW" -h "$NH" -r "$NR" $NHDR -- bash -c '
  case "$DISPLAY" in :[0-9]|:[0-9].*) echo "refusing display $DISPLAY" >&2; exit 1;; esac
  for t in $TAKEN; do [ "$DISPLAY" = ":$t" ] && { echo "refusing display $DISPLAY: the host has it" >&2; exit 1; }; done
  echo "display $DISPLAY" >&2
  if [ "$STDIN_FILE" = - ]; then "$@" > "$OUT/stdout.log" 2> "$OUT/stderr.log" &
  else "$@" < "$STDIN_FILE" > "$OUT/stdout.log" 2> "$OUT/stderr.log" &
  fi
  pid=$!
  shot() {
    rm -f /tmp/gamescope.png
    xprop -root -f GAMESCOPECTRL_REQUEST_SCREENSHOT 32c -set GAMESCOPECTRL_REQUEST_SCREENSHOT 1
    for i in $(seq 100); do
      if [ -s /tmp/gamescope.png ]; then s1=$(stat -c %s /tmp/gamescope.png); sleep 0.1
        [ "$s1" = "$(stat -c %s /tmp/gamescope.png)" ] && { cp /tmp/gamescope.png "$OUT/$1.png"; echo "shot $1" >&2; return; }; fi
      sleep 0.05
    done
    echo "no screenshot for $1" >&2
  }
  IFS=";" read -ra acts <<< "$ACTIONS"
  for a in "${acts[@]}"; do
    set -- $a
    [ $# -gt 0 ] || continue
    kill -0 $pid 2>/dev/null || break
    case "$1" in
      sleep) sleep "$2" ;;
      shot) shot "$2" ;;
      xkb) setxkbmap -option "$2" ;;
      key|down|up|click|move) "$XINJECT" "$@" ;;
    esac
  done
  wait $pid
  echo "exit $?" >&2
' -- "$@"
EOS
)

exec env -i HOME="$HOME" PATH="$PATH" USER="${USER:-}" LANG="${LANG:-C.UTF-8}" \
    QUIVER_STREAM_LOG="${QUIVER_STREAM_LOG:-info}" QUIVER_SHM_ROOT="$OUT/shm" \
    NW="$W" NH="$H" NR="$R" NHDR="${HDR[*]:-}" OUT="$OUT" ACTIONS="$ACTIONS" XINJECT="$XINJECT" \
    TAKEN="$taken" STDIN_FILE="$STDIN_FILE" ${QUIVER_NETLAB_STATS:+QUIVER_NETLAB_STATS="$QUIVER_NETLAB_STATS"} \
    SDL_AUDIO_DRIVER=dummy XDG_RUNTIME_DIR=/tmp/xdg ENABLE_GAMESCOPE_WSI="${ENABLE_GAMESCOPE_WSI:-}" \
    bwrap --dev-bind / / --tmpfs /tmp --dev /dev "${devs[@]}" --tmpfs "/run/user/$uid" \
        --bind "$OUT" "$OUT" "${binds[@]}" "${net[@]}" --unshare-ipc --die-with-parent \
        -- bash -c "$inner" bash "$@"
