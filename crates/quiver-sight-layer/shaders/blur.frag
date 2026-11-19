#version 450

// The backdrop blur behind the Sight panel (src/blur.rs): a dual-filter (Kawase) blur. The
// swapchain image is box-filtered to a quarter of its size, halved a few times and doubled
// back up, and the last step writes the result over the swapchain image at `alpha` (the
// fade), darkened by `dim`. Values are whatever the swapchain holds (sRGB-encoded for UNORM,
// linear through _SRGB views and for scRGB, PQ for HDR10): the blur goes back into the same
// encoding it came from.
layout(set = 0, binding = 0) uniform sampler2D src;

// src/blur.rs `BlurPush` (24 bytes).
layout(push_constant) uniform Push {
    vec2 texel;    // one texel of `src`, in uv
    float offset;  // filter spread, in source texels
    float alpha;   // composite: the fade
    float dim;     // composite: darkening, 0..1
    uint mode;     // MODE_*, and SPLIT_* << 8
} pc;

const uint MODE_PREFILTER = 0u; // 4x4 box: four bilinear taps one source texel off centre
const uint MODE_DOWN = 1u;      // Kawase down: centre x4 and four diagonal taps
const uint MODE_UP = 2u;        // Kawase up: four axial and four diagonal (x2) taps
const uint MODE_COMPOSITE = 3u; // up, then dim, written at alpha
// A packed stereo frame (design/SIGHT.md §15): every tap stays inside the eye (half) the
// fragment is in, so neither eye's blur takes colour from the other across the seam. Levels
// have even sizes, so the seam (uv 0.5) is a texel edge at every level.
const uint SPLIT_X = 1u;        // left | right (sbs, hsbs)
const uint SPLIT_Y = 2u;        // top / bottom (tab, htab)

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;

vec2 lo;
vec2 hi;

vec3 tap(vec2 d) {
    return texture(src, clamp(uv + d * pc.texel, lo, hi)).rgb;
}

vec3 up() {
    float o = pc.offset;
    vec3 s = tap(vec2(-o, 0.0)) + tap(vec2(o, 0.0)) + tap(vec2(0.0, -o)) + tap(vec2(0.0, o));
    float h = 0.5 * o;
    s += 2.0 * (tap(vec2(-h, -h)) + tap(vec2(h, -h)) + tap(vec2(-h, h)) + tap(vec2(h, h)));
    return s / 12.0;
}

void main() {
    uint mode = pc.mode & 0xffu;
    uint split = pc.mode >> 8;
    lo = vec2(0.0);
    hi = vec2(1.0);
    if (split == SPLIT_X) {
        if (uv.x < 0.5) hi.x = 0.5; else lo.x = 0.5;
    } else if (split == SPLIT_Y) {
        if (uv.y < 0.5) hi.y = 0.5; else lo.y = 0.5;
    }
    // Half a source texel in: bilinear taps never reach past the edge (as clamp-to-edge).
    lo += 0.5 * pc.texel;
    hi -= 0.5 * pc.texel;
    vec3 c;
    if (mode == MODE_PREFILTER) {
        c = 0.25 * (tap(vec2(-1.0, -1.0)) + tap(vec2(1.0, -1.0)) + tap(vec2(-1.0, 1.0)) + tap(vec2(1.0, 1.0)));
    } else if (mode == MODE_DOWN) {
        float o = pc.offset;
        c = (4.0 * tap(vec2(0.0)) + tap(vec2(-o, -o)) + tap(vec2(o, -o)) + tap(vec2(-o, o)) + tap(vec2(o, o))) / 8.0;
    } else {
        c = up();
    }
    if (mode == MODE_COMPOSITE) {
        color = vec4(c * (1.0 - pc.dim), pc.alpha);
    } else {
        color = vec4(c, 1.0);
    }
}
