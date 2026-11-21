#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
// Stage B: the converted picture, scaled into its rectangle on the swapchain.
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 o;
layout(set = 0, binding = 0) uniform sampler2D pic;
layout(push_constant) uniform P {
    int mode;
    float sdr_nits;
    float peak_nits;
} p;
void main() {
    o = vec4(encode_out(texture(pic, uv).rgb, p.mode, p.sdr_nits, p.peak_nits), 1.0);
}
