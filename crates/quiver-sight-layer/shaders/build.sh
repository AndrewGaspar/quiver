#!/usr/bin/env bash
# Rebuilds the committed SPIR-V from the GLSL here. Needs glslc (shaderc) and spirv-val.
# The layer embeds the .spv files (src/pipelines.rs), so the crate builds without them.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
for s in quad.vert quad.frag graph.vert graph.frag blur.vert blur.frag; do
    glslc --target-env=vulkan1.0 -O -o "$s.spv" "$s"
    spirv-val --target-env vulkan1.0 "$s.spv"
done
echo "shaders ok"
