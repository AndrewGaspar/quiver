#!/usr/bin/env bash
# Install or remove the secur32 TLS tap in the Lutris Ubisoft Connect prefix.
#   install.sh install <secur32.dll>   copy the tap next to upc.exe and add secur32=n,b to the
#                                      Lutris game config (backed up first)
#   install.sh remove [log-dest]       restore the config, delete the tap, move tls-tap.log out
set -euo pipefail
connect="$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher"
yml=${LUTRIS_YML:-$HOME/.local/share/lutris/games/assassins-creed-origins-1794793554.yml}
backup="$yml.tls-tap-backup"
case $1 in
  install)
    [[ -e $backup || -e $connect/secur32.dll ]] && { echo "already installed" >&2; exit 1; }
    cp -p "$yml" "$backup"
    cp "$2" "$connect/secur32.dll"
    grep -q '^  overrides:$' "$yml" || { echo "no wine overrides block in $yml" >&2; rm "$connect/secur32.dll" "$backup"; exit 1; }
    sed -i 's/^  overrides:$/  overrides:\n    secur32: n,b/' "$yml"
    echo "installed; log: $connect/tls-tap.log"
    grep -A3 '^  overrides:' "$yml";;
  remove)
    [[ -e $backup ]] && mv -f "$backup" "$yml"
    rm -f "$connect/secur32.dll"
    if [[ -e $connect/tls-tap.log ]]; then
      dest=${2:-.}; mv "$connect/tls-tap.log" "$dest/"; echo "log moved to $dest/tls-tap.log"
    fi
    echo "removed";;
  *) echo "usage: install.sh install <secur32.dll> | remove [log-dest]" >&2; exit 2;;
esac
