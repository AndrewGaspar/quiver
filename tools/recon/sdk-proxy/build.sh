#!/usr/bin/env bash
# Build a logging proxy for an SDK loader DLL:  build.sh <real-loader.dll> <out-dir>
# Install: rename the game's loader to <name>_real.dll and put the proxy in its place.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$2"
python3 -I "$here/gen.py" "$1" "$2"
name=$(basename "$1" .dll)
x86_64-w64-mingw32-gcc -O1 -shared -o "$2/$name.dll" "$2/proxy.c" "$2/proxy.def" -static-libgcc
echo "built $2/$name.dll"
