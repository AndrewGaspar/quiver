#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"

layout(set = 0, binding = 0) uniform sampler2D hud;
layout(location = 0) in vec2 v_texel;
layout(location = 0) out vec4 color;

bool inside(vec2 p, vec4 r) {
    return p.x >= r.x && p.y >= r.y && p.x < r.z && p.y < r.w;
}

// Texel fetches: the image is laid out at the target's pixel scale, so each fragment is one
// texel (its centre interpolates to texel + 0.5). Into a squeezed stereo eye (hsbs / htab)
// each fragment covers two texels along that axis (its centre lands on their shared edge):
// their average, a 2:1 box filter. The texture is premultiplied; an _SRGB view decodes it for
// sRGB targets, so the average is in linear light there.
//
// Lifts (design/STEREO-UI.md): parts of the image at their own depth, back to front, each
// moved `lift_dx` further than the image in this eye. A fragment shows the front-most lift
// whose moved rectangle covers it, else the image as it lies. Where that would show pixels of
// a lift in front, the lift has moved off them: they're filled from the column just beside
// the lift on the side it left, which a writer pads with the background. Every fragment is
// written once, so nothing is blended twice; shifts are whole pixels, so text stays crisp.
void main() {
    vec2 t = v_texel;
    int n = int(min(pc.nlifts, 4u));
    // Push constants are indexed only by the (uniform) loop counters: a fragment's own layer
    // differs across a wave, and indexing by it is what some compilers get wrong.
    int layer = -1;
    float own = 0.0;
    vec2 q = t;
    // Back to front: the last (front-most) match wins.
    for (int i = 0; i < n; ++i) {
        vec2 c = t - vec2(pc.lift_dx[i], 0.0);
        if (inside(c, pc.lift[i])) {
            layer = i;
            own = pc.lift_dx[i];
            q = c;
        }
    }
    vec2 d = vec2((pc.flags & FLAG_SQUEEZE_X) != 0u ? 0.5 : 0.0, (pc.flags & FLAG_SQUEEZE_Y) != 0u ? 0.5 : 0.0);
    for (int j = 0; j < n; ++j) {
        vec4 r = pc.lift[j];
        if (j > layer && inside(q, r)) {
            q.x = pc.lift_dx[j] > own ? r.x - 0.5 - d.x : r.z + 0.5 + d.x;
        }
    }
    ivec2 size = ivec2(pc.size);
    ivec2 a = clamp(ivec2(floor(q - d)), ivec2(0), size - 1);
    ivec2 b = clamp(ivec2(floor(q + d)), ivec2(0), size - 1);
    color = 0.5 * (texelFetch(hud, a, 0) + texelFetch(hud, b, 0)) * pc.opacity;
}
