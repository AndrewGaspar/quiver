#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 2) flat in vec4 v_shape;
layout(location = 0) out vec4 o;
layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(push_constant) uniform P {
    vec2 screen;
    int mode;
    float ui_nits;
} p;
void main() {
    vec4 c = v_color;
    if (v_shape.w < 0.5) {
        c *= texture(atlas, v_uv / vec2(textureSize(atlas, 0)));
    } else {
        // Rounded rectangle: v_uv is the pixel's offset from the centre.
        vec2 q = abs(v_uv) - v_shape.xy + v_shape.z;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - v_shape.z;
        c.a *= clamp(0.5 - d, 0.0, 1.0);
    }
    o = vec4(encode_out(c.rgb, p.mode, p.ui_nits, p.ui_nits), c.a);
}
