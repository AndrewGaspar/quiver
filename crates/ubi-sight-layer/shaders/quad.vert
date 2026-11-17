#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

// A triangle strip of four vertices covering the HUD image at its swapchain position.
void main() {
    vec2 uv = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 p = pc.origin + uv * pc.size;
    gl_Position = vec4(p / pc.extent * 2.0 - 1.0, 0.0, 1.0);
}
