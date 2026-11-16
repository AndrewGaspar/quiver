#!/usr/bin/env bash
# Install or remove the SDK logging proxy in a game directory.
#   install.sh install <game-dir> <proxy.dll>   original -> uplay_r1_loader64_real.dll, proxy in its place
#   install.sh remove  <game-dir>               restore the original loader (log is kept)
set -euo pipefail
cmd=$1 dir=$2 name=uplay_r1_loader64
case $cmd in
  install)
    [[ -f $dir/${name}_real.dll ]] && { echo "already installed" >&2; exit 1; }
    mv "$dir/$name.dll" "$dir/${name}_real.dll"
    cp "$3" "$dir/$name.dll"
    echo "installed; log will be $dir/sdk-calls.log";;
  remove)
    [[ -f $dir/${name}_real.dll ]] || { echo "not installed" >&2; exit 1; }
    mv -f "$dir/${name}_real.dll" "$dir/$name.dll"
    echo "restored original $name.dll";;
  *) echo "usage: install.sh install|remove <game-dir> [proxy.dll]" >&2; exit 2;;
esac
