// Push constants shared by both pipelines (32 bytes; src/compositor.rs `Push`).
layout(push_constant) uniform Push {
    vec2 extent;   // swapchain extent, pixels
    vec2 origin;   // quad: top-left of the HUD image, swapchain pixels
    vec2 size;     // quad: image size, pixels
    float opacity; // descriptor opacity, applied to premultiplied colour
    uint flags;    // FLAG_SRGB_DECODE: the target is an _SRGB format; decode vertex colours
} pc;

const uint FLAG_SRGB_DECODE = 1u;
