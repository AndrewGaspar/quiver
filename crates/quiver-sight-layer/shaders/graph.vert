#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

layout(location = 0) in vec2 pos;    // target pixels (pc.extent)
layout(location = 1) in vec4 rgba;   // premultiplied, sRGB-encoded (R8G8B8A8_UNORM)
layout(location = 0) out vec4 v_color;

void main() {
    // Moved with what it belongs to on a stereo frame (the HUD's graph, the panel's cursor):
    // this eye's share of its disparity.
    gl_Position = vec4((pos + pc.shift) / pc.extent * 2.0 - 1.0, 0.0, 1.0);
    v_color = rgba;
}
