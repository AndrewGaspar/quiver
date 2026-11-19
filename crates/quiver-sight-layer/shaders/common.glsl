// Push constants shared by both pipelines (32 bytes; src/compositor.rs `Push`).
layout(push_constant) uniform Push {
    vec2 extent;   // the target's extent in pixels: the swapchain, or one eye's logical size
    vec2 origin;   // quad: top-left of the image, in those pixels
    vec2 size;     // quad: image size, pixels
    float opacity; // descriptor opacity, applied to premultiplied colour
    uint flags;    // FLAG_*
} pc;

// The target is an _SRGB format: decode vertex colours.
const uint FLAG_SRGB_DECODE = 1u;
// Drawn into a stereo eye squeezed 2:1 along x (hsbs) or y (htab): average texel pairs.
const uint FLAG_SQUEEZE_X = 2u;
const uint FLAG_SQUEEZE_Y = 4u;
