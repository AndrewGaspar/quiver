#!/bin/bash
# Render the widget in every state with canned queue data, check what it shows,
# and (with OUT_DIR) save screenshots: <state>-bar.png, <state>-vbar.png and
# <state>-panel.png, plus states.png, a contact sheet when ImageMagick is there.
#
#   tests/render.sh [OUT_DIR]
#
# Runs Quickshell offscreen (no compositor, no display), against the shell's
# Ui/Commons from $OMARCHY_SHELL_DIR, else $OMARCHY_PATH/shell, else
# /usr/share/omarchy/shell. `quiver` is tests/harness/fake-quiver: nothing
# talks to quiverd, launches the app or sends notifications.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
plugin=$(dirname "$here")
out=${1:-}

shell_dir=${OMARCHY_SHELL_DIR:-}
if [[ -z $shell_dir ]]; then
    for d in "${OMARCHY_PATH:+$OMARCHY_PATH/shell}" /usr/share/omarchy/shell; do
        if [[ -n $d && -f $d/Ui/qmldir ]]; then
            shell_dir=$d
            break
        fi
    done
fi
[[ -n $shell_dir ]] || { echo "render.sh: no Omarchy shell found (set OMARCHY_SHELL_DIR)" >&2; exit 1; }
command -v quickshell >/dev/null || { echo "render.sh: quickshell isn't installed" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# The shell's Ui, except KeyboardPanel (a layer-shell window) is a stand-in.
mkdir "$tmp/Ui"
for f in "$shell_dir"/Ui/*; do ln -s "$f" "$tmp/Ui/"; done
ln -sfn "$here/harness/KeyboardPanel.qml" "$tmp/Ui/KeyboardPanel.qml"
ln -s "$shell_dir/Commons" "$tmp/Commons"
ln -s "$plugin" "$tmp/plugin"
cp "$here/harness/shell.qml" "$tmp/shell.qml"
log=$tmp/actions.log
: >"$log"
# Belt and braces: whatever the widget runs by name is a stand-in here.
mkdir "$tmp/bin"
ln -s "$here/harness/fake-quiver" "$tmp/bin/quiver"
for cmd in notify-send omarchy-launch-or-focus uwsm-app; do
    printf '#!/bin/sh\necho "%s $*" >>"%s"\n' "$cmd" "$tmp/stray.log" >"$tmp/bin/$cmd"
    chmod +x "$tmp/bin/$cmd"
done
if [[ -n $out ]]; then
    mkdir -p "$out"
    out=$(cd "$out" && pwd)
fi

status=0
env -u WAYLAND_DISPLAY -u DISPLAY -u QT_QPA_PLATFORMTHEME -u HYPRLAND_INSTANCE_SIGNATURE \
    QT_QPA_PLATFORM=${HARNESS_QPA:-offscreen} DISPLAY=${HARNESS_DISPLAY:-} HARNESS_OUT="$out" HARNESS_FAKE_QUIVER="$here/harness/fake-quiver" \
    PATH="$tmp/bin:$PATH" FAKE_QUIVER_LOG="$log" timeout 90 quickshell -p "$tmp" --no-color >"$tmp/qs.log" 2>&1 || status=$?

fail() {
    echo "not ok - $1" >&2
    sed 's/^/    /' "$tmp/qs.log" >&2
    exit 1
}
((status == 0)) || fail "quickshell exited with $status"
# QML errors in the plugin's own files.
if grep -E "plugin/[A-Za-z]+\.(qml|js)" "$tmp/qs.log" | grep -qiE "error|warn|TypeError|ReferenceError|Unable to assign"; then
    fail "QML warnings from the plugin"
fi
grep -q "RESULT pass" "$tmp/qs.log" || fail "widget states"
echo "ok - $(sed -n 's/.*CHECKS \([0-9]*\).*/\1/p' "$tmp/qs.log") widget state checks"

# The actions the panel ran, as argv: resume for the failing item; pause-all
# pauses the waiting items before the active one; the malformed ref never runs.
tab=$'\t'
expected="resume${tab}99999
pause${tab}5059
pause${tab}13504
pause${tab}epic:dc9d2e595d0e4650b35d659f90d41059"
if [[ $(cat "$log") != "$expected" ]]; then
    echo "expected:" >&2
    echo "$expected" >&2
    echo "got:" >&2
    cat "$log" >&2
    fail "quiver actions"
fi
echo "ok - quiver actions run with checked refs, in order"
# "Open Quiver" (run once by the harness): Hyprland focuses an open window, and
# quiver-app turns it to Downloads (or starts there), each argument on its own.
# Nothing else: no notification, no other launch.
expected_open="omarchy-launch-or-focus ^quiver$ true
uwsm-app -- $here/harness/quiver-app --page downloads"
if [[ $(sort "$tmp/stray.log" 2>/dev/null) != "$(sort <<<"$expected_open")" ]]; then
    echo "expected:" >&2
    echo "$expected_open" >&2
    echo "got:" >&2
    cat "$tmp/stray.log" >&2
    fail "Open Quiver, and nothing else launched or notified"
fi
echo "ok - Open Quiver focuses the window and asks quiver-app for Downloads"

if [[ -n $out ]] && command -v magick >/dev/null; then
    rows=()
    for f in "$out"/*-panel.png; do
        name=$(basename "$f" -panel.png)
        # Bar slices on the left (horizontal over vertical), the panel on the right.
        magick -background '#111111' \
            \( "$out/$name-bar.png" "$out/$name-vbar.png" -gravity northwest -splice 0x12 -append \) \
            "$f" -gravity west -splice 12x0 -gravity northwest -background "#111111" +append "$tmp/row-$name.png"
        rows+=("$tmp/row-$name.png")
    done
    magick montage -label '%t' "${rows[@]}" -tile 3x -geometry +16+16 -background '#111111' \
        -fill '#bbbbbb' "$out/states.png"
    echo "ok - screenshots in $out (states.png)"
fi
