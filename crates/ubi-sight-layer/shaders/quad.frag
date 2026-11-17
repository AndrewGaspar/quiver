#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

layout(set = 0, binding = 0) uniform sampler2D hud;
layout(location = 0) out vec4 color;

// 1:1 texel fetch: the image is laid out at the swapchain's pixel scale. The texture is
// premultiplied; an _SRGB view decodes it for sRGB targets.
void main() {
    ivec2 size = ivec2(pc.size);
    ivec2 t = clamp(ivec2(floor(gl_FragCoord.xy - pc.origin)), ivec2(0), size - 1);
    color = texelFetch(hud, t, 0) * pc.opacity;
}
