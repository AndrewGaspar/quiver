//! Shared formats for Sight's performance HUD and pluggable metrics (`design/SIGHT-HUD.md`):
//!
//! - [`layout`]: the exact shared-memory layouts (§3.8), mirrored by `include/quiver_perf.h`.
//! - [`ring`]: per-frame / per-event rings, one writer process, lock-free readers.
//! - [`record`]: ring records (layer presents, mod frames, series, events).
//! - [`hud`]: `hud.image`, the double-buffered, seqlocked HUD image.
//! - [`panel`]: `panel.input`, the Sight panel drawn in the game: who captures input, the
//!   forwarded events, the layer's word (with `panel.image`, the panel's pixels).
//! - [`stereo`]: packed stereo frames (sbs/hsbs/tab/htab): eyes, logical sizes, the pointer.
//! - [`session`]: the session dir (`/dev/shm/quiver-<uid>/<session>/`, `QUIVER_PERF_DIR`).
//! - [`clock`]: CLOCK_MONOTONIC_RAW, Wine QPC conversion, offset checks and fits.
//! - [`descriptor`], [`control`]: metric descriptors and the JSON control-plane payloads.
//!
//! Without the `std` feature the crate is `no_std`: the layouts and both protocols work over
//! memory the caller maps ([`shm::RawRegion`]).

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "descriptor")]
extern crate alloc;

pub mod clock;
mod error;
pub mod hud;
pub mod labgpu;
pub mod layout;
pub mod panel;
pub mod record;
pub mod ring;
pub mod shm;
pub mod stereo;

#[cfg(feature = "std")]
pub mod session;

#[cfg(feature = "descriptor")]
pub mod control;
#[cfg(feature = "descriptor")]
pub mod descriptor;

pub use error::Error;
pub use layout::{HudBufferHeader, HudGraph, LayerStatus, Record};

/// The C header, for tools that want to install or vendor it.
pub const C_HEADER: &str = include_str!("../include/quiver_perf.h");

/// A random 64-bit value from the standard library's per-process hash keys, the time and a
/// counter. Good enough for nonces and temp names (identity, not secrecy).
#[cfg(feature = "std")]
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish()
}

/// A fresh ring nonce (`qv_ring_header.nonce`).
#[cfg(feature = "std")]
pub fn random_nonce() -> [u8; 16] {
    let mut n = [0u8; 16];
    n[..8].copy_from_slice(&random_u64().to_le_bytes());
    n[8..].copy_from_slice(&random_u64().to_le_bytes());
    n
}
