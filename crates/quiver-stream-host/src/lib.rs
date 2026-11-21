//! The server side of a Quiver stream (design/REMOTE.md §2): one process per stream.
//!
//! It makes the virtual display (headless gamescope at the client's size and rate), captures
//! what is shown there, encodes it with NVENC, captures the stream's own audio sink and
//! encodes it as Opus, injects the client's input into that display only, and sends rumble and
//! the cursor back. It speaks only [`quiver_stream_proto`] types: the transport
//! (`quiver-stream-net`) and quiverd wire it up.

pub mod cuda;
pub mod nvenc;
pub mod pattern;
pub mod recovery;
pub mod stats;
pub mod video;
