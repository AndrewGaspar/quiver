#!/usr/bin/env bash
# Build the 32-bit secur32.dll TLS tap:  build.sh <out-dir> [real secur32.dll]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$1
real=${2:-$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64/files/lib/wine/i386-windows/secur32.dll}
mkdir -p "$out"
python3 -I "$here/gen.py" "$real" "$out"
i686-w64-mingw32-gcc -O1 -shared -o "$out/secur32.dll" "$here/tap.c" "$out/exports.c" "$out/secur32.def" -static-libgcc
echo "built $out/secur32.dll"
