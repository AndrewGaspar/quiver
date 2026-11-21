//! quiver-stream: the client side of Quiver remote play (design/REMOTE.md), the stream window
//! quiver-app launches.
//!
//! The boundary with the transport is [`link::StreamLink`] (video, audio and control in;
//! input and control out); [`net`] fills one from `quiver-stream-net` (`quiver-stream
//! connect`, the ticket on stdin), and [`source::FileSource`] stands in for a host during
//! development. [`caps`] says what this machine can do (`quiver-stream caps --json`).
//! The pieces:
//!
//! - **Decode** ([`decode`], [`av`], csrc/qs_av.c): libav, Vulkan Video first, on the
//!   presenter's own Vulkan device ([`gpu::Gpu`]), so decoded frames are sampled where the
//!   decoder wrote them; NVDEC, VA-API and software (slice threads) after it. Low delay: no
//!   reordering, a frame out per frame in. Lost frames: [`gap`].
//! - **Stage A** ([`gpu::convert`], decode thread): Y'CbCr → R'G'B' ([`color`]) into one of
//!   four slots, at once, so the decoder's image goes back to it after a fraction of a ms.
//! - **Stage B** ([`gpu::present`], present thread): the newest slot (late latching: acquire
//!   first, then take the picture) scaled with its aspect kept, plus the overlay ([`ui`]);
//!   IMMEDIATE/MAILBOX for lowest latency, FIFO for smooth; HDR10 swapchain with metadata, or
//!   tone-mapping.
//! - **Input** ([`input`], [`keymap`], [`app`]): SDL3 keyboard grab, relative mouse, pads.
//! - **Audio** ([`audio`], [`jitter`], [`opus`]): adaptive jitter buffer, FEC/PLC, SDL3 audio.
//!
//! Measured on an RTX 5070 Laptop (driver 620.70), release build, frames fed at 120 fps
//! (2026-11-21). Decode, packet in → converted picture done on the GPU (`quiver-stream bench`,
//! median / p90):
//!
//! | stream | Vulkan Video (zero-copy) | NVDEC (copy down and up) | software |
//! |---|---|---|---|
//! | HEVC 2560×1440 | 1.2 / 1.9 ms | 4.0 / 4.8 ms | 6.8 ms (debug build) |
//! | AV1 2560×1440 | 1.8 / 2.1 ms | 3.9 / 4.3 ms | 8.2 ms (debug build) |
//! | HEVC 3840×2160 | 2.1 / 2.8 ms | 6.5 / 8.3 ms | 13.4 ms (debug build) |
//! | AV1 3840×2160 | 2.8 / 3.5 ms | 6.8 / 7.7 ms | 17.3 ms (debug build) |
//!
//! End to end in a headless gamescope at 120 Hz (`play --measure`, stamps read back from
//! every frame, present time from `VK_KHR_present_wait`), median / p90:
//!
//! | stream, mode | decode + readback | present | capture → present |
//! |---|---|---|---|
//! | HEVC 1440p, IMMEDIATE | 1.1 / 1.2 ms | 0.5 / 0.7 ms | 1.5 / 2.0 ms |
//! | HEVC 1440p, FIFO | 1.0 / 1.2 ms | 0.6 / 4.4 ms | 1.7 / 5.4 ms |
//! | AV1 2160p, IMMEDIATE | 2.3 / 3.7 ms | 0.6 / 0.7 ms | 3.0 / 4.4 ms |
//! | AV1 2160p, FIFO | 3.0 / 3.9 ms | 3.0 / 4.4 ms | 6.5 / 6.8 ms |
//!
//! gamescope's WSI offers IMMEDIATE and FIFO (MAILBOX with its own WSI layer), and its
//! headless "display" is a timer, so the FIFO numbers say little about a real panel.

pub mod app;
pub mod audio;
pub mod av;
pub mod bench;
pub mod caps;
pub mod color;
pub mod decode;
pub mod gap;
pub mod gpu;
pub mod input;
pub mod jitter;
pub mod keymap;
pub mod link;
pub mod net;
pub mod opus;
pub mod source;
pub mod stats;
pub mod ui;
pub mod video;
