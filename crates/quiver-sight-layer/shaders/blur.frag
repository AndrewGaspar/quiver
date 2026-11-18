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
    uint mode;     // MODE_*
} pc;

const uint MODE_PREFILTER = 0u; // 4x4 box: four bilinear taps one source texel off centre
const uint MODE_DOWN = 1u;      // Kawase down: centre x4 and four diagonal taps
const uint MODE_UP = 2u;        // Kawase up: four axial and four diagonal (x2) taps
const uint MODE_COMPOSITE = 3u; // up, then dim, written at alpha

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;

vec3 tap(vec2 d) {
    return texture(src, uv + d * pc.texel).rgb;
}

vec3 up() {
    float o = pc.offset;
    vec3 s = tap(vec2(-o, 0.0)) + tap(vec2(o, 0.0)) + tap(vec2(0.0, -o)) + tap(vec2(0.0, o));
    float h = 0.5 * o;
    s += 2.0 * (tap(vec2(-h, -h)) + tap(vec2(h, -h)) + tap(vec2(-h, h)) + tap(vec2(h, h)));
    return s / 12.0;
}

void main() {
    vec3 c;
    if (pc.mode == MODE_PREFILTER) {
        c = 0.25 * (tap(vec2(-1.0, -1.0)) + tap(vec2(1.0, -1.0)) + tap(vec2(-1.0, 1.0)) + tap(vec2(1.0, 1.0)));
    } else if (pc.mode == MODE_DOWN) {
        float o = pc.offset;
        c = (4.0 * tap(vec2(0.0)) + tap(vec2(-o, -o)) + tap(vec2(o, -o)) + tap(vec2(-o, o)) + tap(vec2(o, o))) / 8.0;
    } else {
        c = up();
    }
    if (pc.mode == MODE_COMPOSITE) {
        color = vec4(c * (1.0 - pc.dim), pc.alpha);
    } else {
        color = vec4(c, 1.0);
    }
}
