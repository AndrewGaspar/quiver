#!/usr/bin/env bash
# instance.sh — run a Ubisoft game against ubi-bridge (no Connect) in a headless container.
#
#   instance.sh create <name> [src-prefix]   reflink-clone a prefix; strip Connect's login data
#                                            and disable Connect's executables
#   instance.sh run <name> <uplay-id> <exe>  write a fresh launch context, start Xvfb, ubi-bridge,
#                                            then the game (<exe> is a Windows path). Extra game
#                                            environment: UBI_GAME_ENV="K=V K2=V2"
#   instance.sh shot <name> [file]           screenshot the container's display
#   instance.sh keys <name> <xdotool args>   send input
#   instance.sh logs <name>                  tail bridge and game logs
#   instance.sh stop <name>
#
# Live requests from the game (Denuvo tokens) go from ubi-bridge to `ubi broker` on the host,
# which holds the Ubisoft session; the container shares the host network to reach it.
#
# The clone keeps the source prefix's path inside the container so registry paths stay valid.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
INST_ROOT=${UBI_INST_ROOT:-$HOME/Games/ubi-instances}
DEFAULT_SRC=${UBI_SRC_PREFIX:-$HOME/Games/ubisoft-connect}
PROTON=${UBI_PROTON:-$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64}
IMAGE=${UBI_IMAGE:-localhost/aco-headless:latest}
RES=${UBI_RES:-1920x1080}
BROKER=${UBI_BROKER:-127.0.0.1:47810}
UPC_REL="drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher"
CONNECT_APPDATA="drive_c/users/steamuser/AppData/Local/Ubisoft Game Launcher"
CONNECT_EXES=(upc.exe UbisoftConnect.exe UbisoftGameLauncher.exe UbisoftGameLauncher64.exe UplayService.exe UplayWebCore.exe UbisoftExtension.exe)

die() { echo "instance: $*" >&2; exit 1; }
ctr() { echo "ubi-$1"; }
dir() { echo "$INST_ROOT/$1"; }

cmd_create() {
  local name=$1 src=${2:-$DEFAULT_SRC} dst; dst=$(dir "$name")
  [[ -e $dst ]] && die "$name exists"
  pgrep -f "$src" >/dev/null && die "something is running from $src; quit it first"
  mkdir -p "$dst/out"
  cp -a --reflink=always "$src" "$dst/prefix"
  echo "$src" > "$dst/src"
  rm -f "$dst/prefix/pfx.lock"
  rm -rf "$dst/prefix/$CONNECT_APPDATA"
  local exe
  for exe in "${CONNECT_EXES[@]}"; do
    [[ -e "$dst/prefix/$UPC_REL/$exe" ]] && mv "$dst/prefix/$UPC_REL/$exe" "$dst/prefix/$UPC_REL/$exe.off"
  done
  echo "created $dst (Connect login data removed, Connect executables disabled)"
}

cmd_run() {
  local name=$1 id=$2 exe=$3 dst src; dst=$(dir "$name")
  [[ -d $dst/prefix ]] || die "no instance $name"
  src=$(cat "$dst/src")
  podman rm -f "$(ctr "$name")" >/dev/null 2>&1 || true
  cargo build -q --manifest-path "$ROOT/Cargo.toml" -p ubi-cli
  cargo build -q --manifest-path "$ROOT/Cargo.toml" -p ubi-bridge --target x86_64-pc-windows-gnu
  mkdir -p "$dst/prefix/drive_c/ubi" "$dst/out"
  cp "$ROOT/target/x86_64-pc-windows-gnu/debug/ubi-bridge.exe" "$dst/prefix/drive_c/ubi/"
  # One broker on the host serves every instance; start it if nothing is listening.
  if ! (exec 3<>"/dev/tcp/${BROKER%:*}/${BROKER#*:}") 2>/dev/null; then
    RUST_LOG=${RUST_LOG:-ubi_core=info} nohup "$ROOT/target/debug/ubi" broker --listen "$BROKER" >>"$INST_ROOT/broker.log" 2>&1 &
    echo "started broker on $BROKER (log: $INST_ROOT/broker.log)"
  fi
  local user_id
  ( umask 077; "$ROOT/target/debug/ubi" launch-context "$id" --broker "$BROKER" -o "$dst/out/context.json" )
  user_id=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["account_id"])' "$dst/out/context.json")
  # Same layout Connect uses: savegames\<user id>\<uplay id>
  python3 -I - "$dst/out/context.json" "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\savegames\\$user_id\\$id" <<'EOF'
import json, sys
p, path = sys.argv[1], sys.argv[2]
c = json.load(open(p)); c["savegame_path"] = path
json.dump(c, open(p, "w"), indent=1)
EOF
  local lib32=() f
  for f in /usr/lib32/libGLX_nvidia.so* /usr/lib32/libnvidia-*.so* /usr/lib32/libEGL_nvidia.so* /usr/lib32/libcuda.so*; do
    lib32+=(-v "$f:$f:ro")
  done
  podman run -d --rm --name "$(ctr "$name")" \
    --hostname "$(cat /etc/hostname)" --network host \
    --device nvidia.com/gpu=all --userns=keep-id --user 1000:1000 --shm-size=2g \
    --security-opt label=disable \
    -v "$dst/prefix:$src" \
    -v "$PROTON:$PROTON:ro" \
    -v /usr/lib/nvidia/wine:/usr/lib/nvidia/wine:ro \
    -v "$dst/out:/out" \
    "${lib32[@]}" \
    -v /etc/machine-id:/etc/machine-id:ro \
    -e HOME=/home/steamuser -e USER=steamuser \
    -e DISPLAY=:0 -e XDG_RUNTIME_DIR=/tmp/xdg \
    -e WINEPREFIX="$src" -e STEAM_COMPAT_DATA_PATH="$src" \
    -e STEAM_COMPAT_CLIENT_INSTALL_PATH=/tmp/steam \
    -e STEAM_COMPAT_APP_ID=0 -e SteamAppId=0 -e SteamGameId=0 \
    -e PROTON_ENABLE_NVAPI=1 -e DXVK_ENABLE_NVAPI=1 \
    -e WINEDEBUG="${UBI_WINEDEBUG:--all}" \
    -e PROTON_LOG=1 -e PROTON_LOG_DIR=/out \
    -e UBI_BRIDGE_LOG='Z:\out\bridge-ipc.log' \
    -e UBI_GAME_ENV="${UBI_GAME_ENV:-}" -e UBI_UPLAY_ID="$id" \
    -e UBI_PROTON="$PROTON" -e UBI_RES="$RES" -e UBI_EXE="$exe" \
    "$IMAGE" bash -c '
      mkdir -p /tmp/xdg /tmp/steam; chmod 700 /tmp/xdg
      Xvfb :0 -screen 0 ${UBI_RES}x24 -nolisten tcp >/out/xvfb.log 2>&1 &
      for i in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done
      echo "boot $(date -Is)" >> /out/run.log
      python3 "$UBI_PROTON/proton" run "C:\\ubi\\ubi-bridge.exe" "Z:\\out\\context.json" >/out/bridge.log 2>&1 &
      for i in $(seq 100); do grep -q "listening on .*uplay_ipc_pipe" /out/bridge-ipc.log 2>/dev/null && break; sleep 0.2; done
      echo "game $(date -Is)" >> /out/run.log
      # `run`, not `waitforexitandrun`: the latter waits for the bridge'"'"'s wineserver to exit.
      # What Connect hands a game it starts: arguments for uplay_aux_r164.dll, paths base64-encoded.
      b64() { printf %s "$1" | base64 -w0; }
      export UPLAY_ARGUMENTS="-upc_uplay_id $UBI_UPLAY_ID -upc_game_version 1 -upc_exe_path $(b64 "$UBI_EXE") -upc_working_directory $(b64 "${UBI_EXE%\\*}")"
      echo "UPLAY_ARGUMENTS=$UPLAY_ARGUMENTS" >> /out/run.log
      echo "game env: $UBI_GAME_ENV" >> /out/run.log
      env $UBI_GAME_ENV python3 "$UBI_PROTON/proton" run "$UBI_EXE" >>/out/run.log 2>&1
      echo "game exited $(date -Is) status $?" >> /out/run.log' >/dev/null
  echo "started $(ctr "$name"); logs in $dst/out"
}

cmd_shot() {
  local name=$1 out=${2:-$(dir "$1")/out/shot.png}
  podman exec "$(ctr "$name")" import -window root /out/.shot.png
  mv "$(dir "$name")/out/.shot.png" "$out"
  echo "$out"
}

cmd_keys() { local name=$1; shift; podman exec "$(ctr "$name")" xdotool "$@"; }
cmd_logs() { local d; d=$(dir "$1")/out; tail -60 "$d/bridge-ipc.log"; echo "--- run.log"; tail -20 "$d/run.log"; }
cmd_stop() { podman rm -f -t 5 "$(ctr "$1")" >/dev/null; }

cmd=${1:-}; shift || true
case $cmd in
  create|run|shot|keys|logs|stop) "cmd_$cmd" "$@";;
  *) sed -n '2,13p' "$0"; exit 2;;
esac
