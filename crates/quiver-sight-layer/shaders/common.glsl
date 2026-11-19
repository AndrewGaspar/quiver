// Push constants shared by both pipelines (128 bytes; src/pipelines.rs `Push`).
layout(push_constant) uniform Push {
    vec2 extent;   // the target's extent in pixels: the swapchain, or one eye's logical size
    vec2 origin;   // quad: top-left of the image, in those pixels
    vec2 size;     // quad: image size, pixels
    float opacity; // descriptor opacity, applied to premultiplied colour
    uint flags;    // FLAG_*
    // Depth on a stereo frame (design/STEREO-UI.md): this eye's shift for the whole draw
    // (quad, graph, cursor), in `extent` pixels, ...
    vec2 shift;
    uint nlifts;   // quad: lifts in use, <= 4
    uint pad0;
    // ... and the image's lifts, back to front: a rectangle in image texels (x0, y0, x1, y1)
    // drawn `lift_dx` further along x than the rest of the image.
    vec4 lift[4];
    vec4 lift_dx;
} pc;

// The target is an _SRGB format: decode vertex colours.
const uint FLAG_SRGB_DECODE = 1u;
// Drawn into a stereo eye squeezed 2:1 along x (hsbs) or y (htab): average texel pairs.
const uint FLAG_SQUEEZE_X = 2u;
const uint FLAG_SQUEEZE_Y = 4u;
