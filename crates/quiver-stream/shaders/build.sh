#!/usr/bin/env bash
# Rebuilds the committed SPIR-V from the GLSL here. Needs glslc (shaderc) and spirv-val.
# The crate embeds the .spv files (src/gpu/shaders.rs), so it builds without them.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
for s in fullscreen.vert convert.frag blit.frag ui.vert ui.frag; do
    glslc --target-env=vulkan1.3 -O -o "$s.spv" "$s"
    spirv-val --target-env vulkan1.3 "$s.spv"
done
echo "shaders ok"
