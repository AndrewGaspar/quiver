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
//! | the test pattern (and its tone) | [`pattern`], [`tone`] |
//! | NVENC, CUDA | [`nvenc`], [`cuda`], [`recovery`] |
//! | the encode thread | [`video`] |
//! | headless gamescope | [`display`] |
//! | PipeWire capture | [`capture`] |
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
//! ## Capturing in the Sight layer instead
//!
//! Not built; evaluated. gamescope's screencast costs about 1.6 refreshes between the program
//! presenting and the frame reaching capture (its composite, then the PipeWire buffer on a
//! later paint), plus a GPU-to-system-memory copy in gamescope and the upload here. The Sight
//! layer (`crates/quiver-sight-layer`) already copies the presented swapchain image in the
//! game's own `vkQueuePresentKHR` (LAB.md §13's captures). Copying instead into memory exported
//! as an opaque fd (one `vkCmdCopyImage` in the present's submit, about 0.1 ms at 1440p),
//! signalling an exported semaphore, and handing both to the host once, lets CUDA import it
//! (`cuImportExternalMemory`) with no system-memory round trip, at present time rather than
//! gamescope's next paint. That would cut the capture stage from ~13 ms + 1.1 ms to well under
//! a millisecond at 120 Hz, and it sees the game's HDR10 or scRGB swapchain (gamescope's
//! screencast is 8-bit SDR only). Its costs: Vulkan games only (DXVK and vkd3d-proton games
//! are; OpenGL ones aren't), no gamescope overlays in the picture (the cursor is sent
//! separately anyway), and the layer must be in every streamed game. Worth building next.

pub mod audio;
pub mod capture;
pub mod cuda;
pub mod display;
pub mod host;
pub mod input;
pub mod nvenc;
pub mod ogg;
pub mod opus;
pub mod pattern;
pub mod recovery;
pub mod sandbox;
pub mod seat;
pub mod stats;
pub mod testwin;
pub mod tone;
pub mod video;

pub use host::{Host, HostOptions, Target};
