//! The server side of a Quiver stream (design/REMOTE.md §2): one process per stream.
//!
//! It makes the virtual display (headless gamescope at the client's size and rate), captures
//! what is shown there, encodes it with NVENC, captures the stream's own audio sink and
//! encodes it as Opus, injects the client's input into that display only, and sends rumble and
//! the cursor back. It speaks only [`quiver_stream_proto`] types: the transport
//! (`quiver-stream-net`) and quiverd wire it up through [`Host`].
//!
//! | Piece | Module |
//! |---|---|
//! | the async API | [`host`] |
//! | behind the transport, for quiverd (`serve`) | [`serve`] |
//! | the test pattern (and its tone) | [`pattern`], [`tone`] |
//! | NVENC, CUDA | [`nvenc`], [`cuda`], [`recovery`] |
//! | the encode thread | [`video`] |
//! | headless gamescope | [`display`] |
//! | PipeWire capture; capture in the Sight layer | [`capture`], [`layer_capture`] |
//! | the stream's sink, Opus | [`audio`], [`opus`], [`ogg`] |
//! | keyboard, mouse, cursor; pads | [`input`] |
//! | who is seated; the pad namespace and rule | [`seat`], [`sandbox`] |
//!
//! ## Measured (RTX 5070 Laptop, driver 620.70, 2026-11-21)
//!
//! **Encode**, NVENC API directly, preset P1, ultra-low-latency, test pattern with the whole
//! picture moving (`quiver-stream-host --target test-pattern --seconds 3`), from
//! `nvEncEncodePicture` to the bitstream in host memory, p50 / p99:
//!
//! | | H.264 | HEVC | AV1 |
//! |---|---|---|---|
//! | 2560×1440 @ 120 | 1.61 / 2.19 ms | 1.51 / 2.29 ms | 1.57 / 2.31 ms |
//! | 3840×2160 @ 60 | 3.79 / 4.43 ms | 3.31 / 4.32 ms | 3.54 / 4.13 ms |
//!
//! The API rather than libav because a stream needs reference invalidation and bitrate
//! changes without an IDR, which libav's NVENC wrapper doesn't offer; the hardware time is the
//! same either way. Reference invalidation recovers bit-exactly without a keyframe on all three
//! codecs (`tests/live_encode.rs`).
//!
//! **Capture** of a program in headless gamescope over PipeWire (shared-memory BGRx, page-locked,
//! copied into CUDA), with the test window stamping the moment it drew each frame
//! (`tests/live_command.rs`), p50:
//!
//! | | program draws → dequeued | dequeued → encode start | encode |
//! |---|---|---|---|
//! | 1280×720 @ 60 | 28.9 ms | 0.37 ms | 0.52 ms |
//! | 2560×1440 @ 120 | 13.1 ms | 1.11 ms | 1.47 ms |
//!
//! ## Capturing in the Sight layer
//!
//! gamescope's screencast costs one to two refreshes between the program presenting and the
//! frame reaching capture (its composite, then the PipeWire buffer on a later paint), plus a
//! GPU-to-system-memory copy in gamescope and the upload here, and it is 8-bit SDR. So a
//! command target captures in the game instead when the Sight layer is in it
//! ([`layer_capture`]; the layer's side is `quiver-sight-layer`'s `stream` module): at the
//! game's `vkQueuePresentKHR` one compute pass converts the presented image straight to the
//! encoder's input (NV12 or P010, YUV444 when asked) in GPU memory shared with CUDA, and the
//! host waits on the slot's timeline value and copies it into the encoder's input. HDR10 and
//! scRGB games stream as HDR10 (P010, PQ, BT.2020, with the game's `vkSetHdrMetadataEXT`
//! metadata). Programs without the layer, and the time before the layer's first frame, use the
//! screencast. The costs: Vulkan games only (DXVK and vkd3d-proton games are; OpenGL ones
//! aren't), no gamescope overlays in the picture (the cursor is sent separately anyway), and
//! the layer must be in the streamed game.
//!
//! Measured with a Vulkan program in headless gamescope (`tests/live_layer.rs`: `MAILBOX` at
//! the display's rate, stamping the moment it drew each frame; HEVC, preset P1), against the
//! same program over PipeWire, p50 / p99, with another GPU workload on the machine:
//!
//! | | program draws → capture | capture → encode start | encode | program draws → encoded |
//! |---|---|---|---|---|
//! | 1440p120 SDR, PipeWire | 4.6 / 97 ms | 1.14 / 4.0 ms | 1.47 ms | 8.0 / 103 ms |
//! | 1440p120 SDR, layer | 0.18 / 0.28 ms | 0.28 / 2.4 ms | 1.47 ms | 2.0 / 5.0 ms |
//! | 1440p120 HDR, PipeWire (SDR) | 6.3 / 60 ms | 1.23 / 2.5 ms | 1.49 ms | 9.1 / 64 ms |
//! | 1440p120 HDR, layer (HDR10) | 0.25 / 0.55 ms | 0.33 / 1.9 ms | 1.66 ms | 2.3 / 4.5 ms |
//! | 2160p60 SDR, PipeWire | 25.8 / 30 ms | 2.65 / 3.8 ms | 3.44 ms | 32.1 / 38.7 ms |
//! | 2160p60 SDR, layer | 0.35 / 0.44 ms | 0.46 / 2.6 ms | 3.37 ms | 4.2 / 7.0 ms |
//! | 2160p60 HDR, PipeWire (SDR) | 15.1 / 22 ms | 2.90 / 6.7 ms | 3.43 ms | 21.9 / 32.4 ms |
//! | 2160p60 HDR, layer (HDR10) | 0.27 / 0.44 ms | 0.60 / 2.7 ms | 3.33 ms | 4.3 / 7.3 ms |
//!
//! "Capture" is the game's present for the layer (so its "capture → encode start" includes the
//! game's own GPU work, the conversion and the wait here) and the dequeue for PipeWire. Its
//! p95 of about 2 ms is that GPU work finishing late, not the conversion (below) or CUDA's
//! wait (queueing the wait later changed nothing): most likely time-slicing against the other
//! GPU workload that was running (about 2 ms is NVIDIA's timeslice). The layer
//! itself adds, per captured present, 35–45 µs of CPU (p50; the first present of a ring makes
//! it, a few ms once) and 55 µs (1440p NV12), 65 µs (1440p P010), 110 µs (2160p NV12) or
//! 145 µs (2160p P010) of GPU. With no stream it adds nothing measurable: one relaxed load per
//! present.

pub mod audio;
pub mod capture;
pub mod cuda;
pub mod display;
pub mod host;
pub mod input;
pub mod layer_capture;
pub mod nvenc;
pub mod ogg;
pub mod opus;
pub mod pattern;
pub mod recovery;
pub mod sandbox;
pub mod seat;
pub mod serve;
pub mod stats;
pub mod testwin;
pub mod tone;
pub mod video;

pub use host::{Host, HostOptions, Target};
