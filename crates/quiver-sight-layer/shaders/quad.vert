#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

// A triangle strip of four vertices covering the image at its position. Positions are in
// `pc.extent` pixels, which the viewport maps onto the target: the whole swapchain, or one
// eye of a packed stereo frame (laid out in that eye's logical pixels; src/compositor.rs),
// moved by this eye's share of the image's disparity (`pc.shift`, zero for mono).
layout(location = 0) out vec2 v_texel;

void main() {
    vec2 uv = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 p = pc.origin + pc.shift + uv * pc.size;
    v_texel = uv * pc.size;
    gl_Position = vec4(p / pc.extent * 2.0 - 1.0, 0.0, 1.0);
}
