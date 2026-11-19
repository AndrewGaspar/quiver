#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

layout(set = 0, binding = 0) uniform sampler2D hud;
layout(location = 0) in vec2 v_texel;
layout(location = 0) out vec4 color;

// Texel fetches: the image is laid out at the target's pixel scale, so each fragment is one
// texel (its centre interpolates to texel + 0.5). Into a squeezed stereo eye (hsbs / htab)
// each fragment covers two texels along that axis (its centre lands on their shared edge):
// their average, a 2:1 box filter. The texture is premultiplied; an _SRGB view decodes it for
// sRGB targets, so the average is in linear light there.
void main() {
    ivec2 size = ivec2(pc.size);
    vec2 d = vec2((pc.flags & FLAG_SQUEEZE_X) != 0u ? 0.5 : 0.0, (pc.flags & FLAG_SQUEEZE_Y) != 0u ? 0.5 : 0.0);
    ivec2 a = clamp(ivec2(floor(v_texel - d)), ivec2(0), size - 1);
    ivec2 b = clamp(ivec2(floor(v_texel + d)), ivec2(0), size - 1);
    color = 0.5 * (texelFetch(hud, a, 0) + texelFetch(hud, b, 0)) * pc.opacity;
}
