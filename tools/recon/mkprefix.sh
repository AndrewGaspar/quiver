#!/usr/bin/env bash
# Build a throwaway Wine prefix holding Connect's program files (reflinked) but
# none of the user's Connect data (no AppData, so no credentials or settings).
#   mkprefix.sh <dest> <source-prefix>
set -euo pipefail
DEST=$1 SRC=$2
UPC_REL="drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher"
[[ -e $DEST ]] && { echo "$DEST exists" >&2; exit 1; }
export WINEPREFIX=$DEST/prefix WINEDEBUG=-all
mkdir -p "$DEST"
wineboot -i >/dev/null 2>&1
wineserver -w
mkdir -p "$WINEPREFIX/$UPC_REL"
for f in "$SRC/$UPC_REL"/*; do
  case $(basename "$f") in games|cache|logs|savegames|crashes) continue;; esac
  cp -a --reflink=always "$f" "$WINEPREFIX/$UPC_REL/"
done
wine reg add 'HKLM\Software\Wow6432Node\Ubisoft\Launcher' /v InstallDir /d 'C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\' /f >/dev/null
wine reg add 'HKLM\Software\Wow6432Node\Ubisoft\Launcher' /v Version /d "$(cat "$SRC/$UPC_REL/version.txt")" /f >/dev/null
wineserver -w
echo "prefix ready: $WINEPREFIX"
