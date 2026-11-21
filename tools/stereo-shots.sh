#!/usr/bin/env bash
# Renders every major screen of the demo app off-screen, mono and packed side by side for two
# eyes, and measures each element's disparity (design/STEREO-UI.md, "Evidence").
#
#   tools/stereo-shots.sh OUT_DIR [BINARY] [SCENE…]
#
# BINARY defaults to target/release/quiver-app. With SCENE names, only those. Nothing here
# touches a compositor, the daemon or the person's settings: demo mode, the off-screen
# renderer, no Wayland, X or Hyprland variables.
set -euo pipefail
out=${1:?usage: stereo-shots.sh OUT_DIR [BINARY] [SCENE…]}
bin=${2:-target/release/quiver-app}
shift $(( $# >= 2 ? 2 : 1 ))
mkdir -p "$out"
here=$(cd "$(dirname "$0")" && pwd)

# name | QUIVER_DEMO_GLOBAL | QUIVER_DEMO_INPUT | hover x,y | extra env (K=V ...)
scenes=$(cat <<'EOF'
library|||
library-focus||key down down right|
library-hover|||330,700
library-menu||key down down y|
palette|palette||
game-overview|game=3539||
game-actions|game=3539|key down|
game-launch|game=3539,tab=launch||
game-profiles|game=3539,tab=profiles||
game-profiles-dialog|game=3539,tab=profiles,dialog=new-profile||
game-tweaks|game=3539,tab=tweaks||
game-performance|game=3539,tab=performance||
game-saves|game=3539,tab=saves||
game-addons|game=3539,tab=add-ons||
game-files|game=3539,tab=files||
game-options|options=options||
game-options-unsaved|options=options,options-unsaved||
game-options-keys|options=keys||
game-options-changes|options=changes||
game-logs|game=3539,tab=logs||
game-steam|game=66088||
downloads|downloads||
downloads-focus|downloads,dl-focus=1||
install-dialog|install=5059||
activity|activity||
settings-accounts|settings=accounts||
settings-library|settings=library||
settings-compat|settings=compat||
settings-performance|settings=performance||
settings-tweaks|settings=tweaks||
settings-steam|settings=steam||
settings-appearance|settings=appearance||
settings-display|settings=display,focus-body||
settings-automation|settings=automation||
settings-about|settings=about||
settings-machines|settings=machines||
settings-machines-add|settings=machines,add-machine||
settings-machines-pair|settings=machines,remote-on,pair-code||
machines-menu|machines-menu||
machines-tower|machine=tower||
machines-tower-game|machine=tower,machine-game=3539||
machines-game-both|game=3539|key y|
machines-game-remote|game=6100,stream=6100||
machines-downloads|downloads||
confirm|settings=accounts,confirm-signout||
welcome|welcome||
welcome-steam|welcome=steam||
signin|signed-out,add-account||
signin-2fa|2fa||
now-playing|playing,now-playing||
toast|toast=Refreshing the library…||
tooltip|||1795,25
library-200|||330,700|QUIVER_STEREO_SHOT_STRENGTHS=200
library-scale150||key down down right||QUIVER_STEREO_SHOT_SCALE=1.5 QUIVER_STEREO_SHOT_SIZE=1280x720
library-narrow||key down down||QUIVER_STEREO_SHOT_SIZE=560x900
game-narrow|game=3539|||QUIVER_STEREO_SHOT_SIZE=760x900
EOF
)

want=("$@")
pids=()
run() {
  local name=$1 global=$2 input=$3 hover=$4 extra=$5
  # shellcheck disable=SC2086
  env -u WAYLAND_DISPLAY -u DISPLAY -u HYPRLAND_INSTANCE_SIGNATURE $extra \
    QUIVER_DEMO=1 QUIVER_STEREO_SHOT="$out" QUIVER_STEREO_SHOT_NAME="$name" \
    QUIVER_DEMO_GLOBAL="$global" QUIVER_DEMO_INPUT="$input" QUIVER_STEREO_SHOT_HOVER="$hover" \
    QUIVER_STEREO_SHOT_WAIT="${QUIVER_STEREO_SHOT_WAIT:-6000}" \
    "$bin" >"$out/$name.log" 2>&1 || echo "$name: failed (see $out/$name.log)"
}
while IFS='|' read -r name global input hover extra; do
  [ -z "$name" ] && continue
  if [ ${#want[@]} -gt 0 ] && [[ ! " ${want[*]} " == *" $name "* ]]; then continue; fi
  run "$name" "$global" "$input" "$hover" "$extra" &
  pids+=($!)
  # A few at a time: each is a GPU process.
  if [ ${#pids[@]} -ge 4 ]; then wait "${pids[0]}"; pids=("${pids[@]:1}"); fi
done <<<"$scenes"
wait
names=$(ls "$out" | sed -n 's/-scopes\.json$//p' | sort)
python3 -I "$here/stereo-evidence.py" "$out" $names
