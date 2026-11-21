#version 450
// The overlay: quads in window pixels.
layout(location = 0) in vec2 pos;
layout(location = 1) in vec2 uv;    // atlas texel (glyphs, logo) or offset from the rect's centre (shapes)
layout(location = 2) in vec4 color;
layout(location = 3) in vec4 shape; // half size (xy), corner radius, kind (0 image, 1 rounded rect)
layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;
layout(location = 2) flat out vec4 v_shape;
layout(push_constant) uniform P {
    vec2 screen;
    int mode;
    float ui_nits;
} p;
void main() {
    v_uv = uv;
    v_color = color;
    v_shape = shape;
    gl_Position = vec4(pos / p.screen * 2.0 - 1.0, 0.0, 1.0);
}
