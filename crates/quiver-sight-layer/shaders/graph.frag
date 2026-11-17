#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

layout(location = 0) in vec4 v_color;
layout(location = 0) out vec4 color;

vec3 srgb_to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), greaterThan(c, vec3(0.04045)));
}

void main() {
    vec4 c = v_color;
    if ((pc.flags & FLAG_SRGB_DECODE) != 0u) {
        c.rgb = srgb_to_linear(c.rgb);
    }
    color = c * pc.opacity;
}
