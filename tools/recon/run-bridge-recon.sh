#!/usr/bin/env bash
# Run ubi-bridge + the UPLAY harness together in the recon prefix, with no network.
#   run-bridge-recon.sh <recon-dir> <bridge.exe> [uplay_id] [seconds]
set -euo pipefail
R=$1 BRIDGE=$2 ID=${3:-3539} SECS=${4:-8}
export WINEPREFIX=$R/prefix WINEDEBUG=-all
cp "$BRIDGE" "$WINEPREFIX/drive_c/game/ubi-bridge.exe"
exec unshare -r -n bash -c '
  wine "C:\\game\\ubi-bridge.exe" > "$0/bridge.log" 2>&1 &
  sleep 3
  WINEDEBUG=${HARNESS_DEBUG:--all} wine "C:\\game\\harness.exe" "C:\\game\\uplay_r1_loader64.dll" "$1" "$2"
  sleep 1
  wineserver -k
' "$R" "$ID" "$SECS"
