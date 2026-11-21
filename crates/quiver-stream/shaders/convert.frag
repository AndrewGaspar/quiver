#version 450
// Stage A: a decoded Y'CbCr picture to R'G'B' (still in the stream's transfer function).
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 o;
layout(set = 0, binding = 0) uniform sampler2D tex_y;
layout(set = 0, binding = 1) uniform sampler2D tex_u; // CbCr (2 planes) or Cb (3 planes)
layout(set = 0, binding = 2) uniform sampler2D tex_v; // Cr (3 planes only)
layout(push_constant) uniform P {
    mat4 m;      // R'G'B' = m * (Y, Cb, Cr, 1), the samples as stored
    vec2 uv_max; // the picture's part of the image (decoders pad to the coded size)
    int planes;
} p;
void main() {
    vec2 t = uv * p.uv_max;
    float y = texture(tex_y, t).r;
    vec2 c = p.planes == 2 ? texture(tex_u, t).rg : vec2(texture(tex_u, t).r, texture(tex_v, t).r);
    vec3 rgb = (p.m * vec4(y, c, 1.0)).rgb;
    o = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
