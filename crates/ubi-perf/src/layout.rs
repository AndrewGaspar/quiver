//! Exact shared-memory layouts (SIGHT-HUD §3.8), mirrored by `include/quiver_perf.h`.
//!
//! These structs document the byte layout and give `offset_of!` values; the protocol code
//! never takes a `&` to them in shared memory. It reads and writes the fields that change
//! through atomics at these offsets, and copies the rest word by word, because the other side
//! of every mapping is another process (and, for rings, an untrusted one).
//!
//! Little-endian x86-64 only, like the rest of the design.

#[cfg(target_endian = "big")]
compile_error!("ubi-perf's shared-memory formats are little-endian");

/// `"QUQVRING"` read as a little-endian u64.
pub const RING_MAGIC: u64 = 0x474E_4952_5651_5551;
pub const RING_VERSION: u16 = 1;
/// The ring header is one 4 KiB page; slots start right after it.
pub const RING_HEADER_BYTES: usize = 4096;
pub const SLOT_BYTES: usize = 128;
pub const RING_MIN_SLOTS: u32 = 16;
/// 8192 × 128 B = 1 MiB of slots, the ring budget the bridge enforces per mod connection.
pub const RING_MAX_SLOTS: u32 = 8192;
/// The layer's default: 4096 slots = 512 KiB, ~13 s of three records per frame at 100 fps.
pub const RING_DEFAULT_SLOTS: u32 = 4096;
pub const RING_STATUS_BYTES: usize = 2048;
/// Bytes of payload in the status block after its 16-byte seqlock header.
pub const RING_STATUS_DATA_BYTES: usize = RING_STATUS_BYTES - 16;
/// Frame fields per record.
pub const RECORD_FIELDS: usize = 12;
/// Bytes of an event label carried in a record (`f[1..7]`).
pub const EVENT_LABEL_BYTES: usize = 48;

/// `"QUQVHUDI"` read as a little-endian u64.
pub const HUD_MAGIC: u64 = 0x4944_5548_5651_5551;
pub const HUD_VERSION: u16 = 1;
pub const HUD_HEADER_BYTES: usize = 4096;
/// Where the pixels start inside each buffer.
pub const HUD_PIXELS_OFFSET: usize = 256;
/// Capacity of each buffer, descriptor included (SIGHT-HUD §3.8, open question 1).
pub const HUD_MAX_BUFFER_BYTES: u32 = 4 << 20;
pub const HUD_MAX_DIM: u32 = 2048;
pub const HUD_MAX_GRAPHS: usize = 2;
pub const HUD_FORMAT_RGBA8_PREMUL_SRGB: u32 = 1;

/// `qv_ring_header.clock`.
pub const CLOCK_QPC: u16 = 1;
pub const CLOCK_MONO_RAW_NS: u16 = 2;
/// `qv_ring_header.producer_kind`.
pub const PRODUCER_LAYER: u32 = 1;
pub const PRODUCER_MOD: u32 = 2;

/// Record flags (`qv_record.flags`). Frame records: one of REAL, GENERATED, NOFG (REAL|NOFG
/// for "FG off"). Event records: HAS_LABEL, HAS_DURATION. Layer present records: PRESENT_*.
pub const FLAG_REAL: u8 = 1 << 0;
pub const FLAG_GENERATED: u8 = 1 << 1;
pub const FLAG_NOFG: u8 = 1 << 2;
pub const FLAG_HAS_LABEL: u8 = 1 << 3;
pub const FLAG_HAS_DURATION: u8 = 1 << 4;
/// The app chained `VkPresentIdKHR` (DXVK's frame id); `key` is that id, else the layer's own
/// per-swapchain present count.
pub const FLAG_PRESENT_ID: u8 = 1 << 5;
/// The layer composited the HUD into this present.
pub const FLAG_PRESENT_HUD: u8 = 1 << 6;
pub const FLAGS_KNOWN: u8 = 0x7f;

/// Metric index 0 is never valid, so a zeroed slot can't pass for a record. Mods get indices
/// 1..=64 from `metrics_ok`; the layer uses its built-ins.
pub const METRIC_NONE: u16 = 0;
/// The layer's per-present record (`present.frame`, [`crate::record::LayerPresent`]).
pub const LAYER_METRIC_PRESENT: u16 = 1;

/// `qv_ring_header`: one 4 KiB page at offset 0 of every ring file.
#[repr(C, align(64))]
pub struct RingHeader {
    /// Written last at creation (release).
    pub magic: u64,
    pub version: u16,
    pub header_bytes: u16,
    /// 128.
    pub slot_bytes: u16,
    /// [`CLOCK_QPC`] or [`CLOCK_MONO_RAW_NS`].
    pub clock: u16,
    /// Power of two, [`RING_MIN_SLOTS`]..=[`RING_MAX_SLOTS`].
    pub slot_count: u32,
    /// [`PRODUCER_LAYER`] or [`PRODUCER_MOD`].
    pub producer_kind: u32,
    /// Ticks per second (QPC: 10,000,000 under Wine 10; RAW ns: 1e9).
    pub clock_freq: u64,
    /// Written by the creator, reported out of band (the bridge's `ring` message).
    pub nonce: [u8; 16],
    /// Linux pid for the layer; Windows pid for mods.
    pub writer_pid: u32,
    /// Bumps when the producer's metric indices change.
    pub schema_rev: u32,
    pub _pad0: [u8; 8],
    /// Next slot to reserve (writers: `fetch_add`). Own cache line.
    pub reserve: u64,
    pub _pad1: [u8; 56],
    /// Writer's clock, updated ≥ 1/s even when idle. Own cache line with `dropped`.
    pub heartbeat: u64,
    /// Records the writer discarded (ring disabled, nobody consuming, overflow).
    pub dropped: u64,
    pub _pad2: [u8; 48],
    /// Producer status block, seqlocked.
    pub status: StatusBlock,
    pub _pad3: [u8; RING_HEADER_BYTES - 192 - RING_STATUS_BYTES],
}

/// `qv_status_block`: the ring header's 2 KiB status area. `seq` is odd while the writer
/// updates it; `format` says what `data` holds ([`STATUS_FORMAT_LAYER`] for the layer).
#[repr(C, align(64))]
pub struct StatusBlock {
    pub seq: u64,
    /// Valid bytes in `data`.
    pub len: u32,
    pub format: u32,
    pub data: [u8; RING_STATUS_DATA_BYTES],
}

pub const STATUS_FORMAT_NONE: u32 = 0;
/// `data` holds a [`LayerStatus`].
pub const STATUS_FORMAT_LAYER: u32 = 1;

/// `qv_record`: one 128-byte ring slot.
///
/// SIGHT-HUD §3.8 sketched `metric`, `flags`, `swapchain` and `nfields` as four u32s, which
/// with twelve fields is 136 bytes; this keeps twelve fields and 128 bytes by narrowing
/// `metric` (≤ 64 per producer) to u16 and `flags`/`nfields` to u8.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Slot index + 1, stored last with release ordering. 0 while being written.
    pub commit: u64,
    /// Index from `metrics_ok` (or [`LAYER_METRIC_PRESENT`]); 0 is invalid.
    pub metric: u16,
    /// `FLAG_*`.
    pub flags: u8,
    /// Valid entries in `f`, ≤ 12.
    pub nfields: u8,
    /// Writer-chosen id; changes when the swapchain is recreated.
    pub swapchain: u32,
    /// Correlation key (present seq, frame id, present id).
    pub key: u64,
    /// The record's own time, in the ring's clock.
    pub t: i64,
    /// Frame fields; series value as f64 bits in `f[0]`; event duration in `f[0]` and the
    /// label's bytes in `f[1..7]`.
    pub f: [i64; RECORD_FIELDS],
}

/// `qv_layer_status`: what the Sight layer keeps in its ring's status block
/// (format [`STATUS_FORMAT_LAYER`]). quiverd reads it for the presenter choice (§2.6), the
/// HUD model's extent, `quiver hud status` and recording metadata. Strings are NUL-padded
/// UTF-8 straight from the game's Vulkan calls: untrusted.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerStatus {
    /// `size_of::<LayerStatus>()` as the writer knew it (forward compatibility).
    pub struct_bytes: u32,
    /// `LAYER_*` flags.
    pub flags: u32,
    /// The extent of the swapchain the HUD draws on (the largest one).
    pub extent_w: u32,
    pub extent_h: u32,
    /// `VkFormat`, `VkColorSpaceKHR`, `VkPresentModeKHR` of that swapchain.
    pub vk_format: u32,
    pub color_space: u32,
    pub present_mode: u32,
    pub image_count: u32,
    /// The layer's id for that swapchain (as in its present records).
    pub swapchain: u32,
    /// Why the layer can't draw (`DRAW_ERR_*`), 0 when it can.
    pub draw_error: u32,
    /// Presents the HUD was composited into: the "drawn" counter of §2.6.
    pub drawn: u64,
    /// All presents seen.
    pub presents: u64,
    /// The `hud.image` publish counter the layer last uploaded.
    pub hud_published: u64,
    pub vendor_id: u32,
    pub device_id: u32,
    pub driver_version: u32,
    pub api_version: u32,
    /// `VkApplicationInfo::engineVersion`.
    pub engine_version: u32,
    pub swapchain_count: u32,
    /// `VkApplicationInfo::pEngineName` ("DXVK", "vkd3d").
    pub engine_name: [u8; 32],
    pub app_name: [u8; 64],
    pub device_name: [u8; 64],
    pub _reserved: [u8; 8],
}

pub const LAYER_DRAWING: u32 = 1 << 0;
pub const LAYER_LOW_LATENCY2: u32 = 1 << 1;
pub const LAYER_PRESENT_ID: u32 = 1 << 2;
pub const LAYER_PRESENT_WAIT: u32 = 1 << 3;
/// Drawing disabled for the session after a panic in the layer.
pub const LAYER_PANICKED: u32 = 1 << 4;
/// The HUD is toggled off (pass-through).
pub const LAYER_HUD_OFF: u32 = 1 << 5;

pub const DRAW_ERR_NONE: u32 = 0;
/// Swapchain format/colour space the compositor can't handle (HDR before phase 2, exotic).
pub const DRAW_ERR_FORMAT: u32 = 1;
/// No `COLOR_ATTACHMENT` usage and it couldn't be added at creation.
pub const DRAW_ERR_USAGE: u32 = 2;
/// The present queue family has no graphics.
pub const DRAW_ERR_QUEUE: u32 = 3;
pub const DRAW_ERR_DEVICE_LOST: u32 = 4;
/// No valid image: missing, stale, laid out for another extent, or failed validation.
pub const DRAW_ERR_NO_IMAGE: u32 = 5;
pub const DRAW_ERR_INTERNAL: u32 = 6;

/// `qv_hud_header`: offset 0 of `hud.image`, one 4 KiB page.
#[repr(C, align(64))]
pub struct HudHeader {
    /// Written last at creation (release).
    pub magic: u64,
    pub version: u16,
    pub header_bytes: u16,
    /// Capacity of each buffer including its descriptor; multiple of 4096, ≤ 4 MiB.
    pub buffer_bytes: u32,
    pub _pad0: [u8; 48],
    /// Publish counter; buffer `published % 2` is current. Own cache line.
    pub published: u64,
    /// quiver-sight's CLOCK_MONOTONIC_RAW ns, ≥ 1/s.
    pub heartbeat: u64,
    pub writer_pid: u32,
    pub _pad1: [u8; HUD_HEADER_BYTES - 84],
}

/// `qv_hud_graph`: a rectangle the layer fills with the live frame-time graph.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HudGraph {
    /// In image pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 1000–10000.
    pub span_ms: u32,
    /// Vertical range; `max_ms == 0`: auto.
    pub min_ms: f32,
    pub max_ms: f32,
    /// Premultiplied RGBA8 (byte order R, G, B, A in memory), resolved from theme levels.
    pub line: u32,
    pub fill: u32,
    pub muted: u32,
    /// 0 = no threshold.
    pub warn_above_ms: u32,
    pub bad_above_ms: u32,
    pub warn: u32,
    pub bad: u32,
    /// `GRAPH_SERIES_*` bits.
    pub series: u32,
}

pub const GRAPH_SERIES_PRESENTED: u32 = 1;
pub const GRAPH_SERIES_BASE: u32 = 2;
pub const GRAPH_SERIES_GENERATED_TINT: u32 = 4;

/// `qv_hud_buffer`: the descriptor at the start of each buffer; pixels follow at
/// [`HUD_PIXELS_OFFSET`]. Buffer `i` is at `header_bytes + i * buffer_bytes`.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudBufferHeader {
    /// Odd while quiver-sight writes this buffer.
    pub seq: u64,
    /// Pixels, pixels, bytes per row (multiple of 4, ≥ width × 4).
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// [`HUD_FORMAT_RGBA8_PREMUL_SRGB`].
    pub format: u32,
    /// Image pixels per logical px.
    pub scale: f32,
    /// The swapchain extent it was laid out for.
    pub extent_w: u32,
    pub extent_h: u32,
    /// Where it goes, in swapchain pixels (anchor resolved).
    pub x: i32,
    pub y: i32,
    /// Applied by the layer, so fades need no re-render.
    pub opacity: f32,
    /// ≤ 2.
    pub ngraphs: u32,
    pub graph: [HudGraph; HUD_MAX_GRAPHS],
    pub _pad0: u32,
    /// The HUD model revision this image shows.
    pub model_rev: u64,
    pub _reserved: [u8; HUD_PIXELS_OFFSET - 184],
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<RingHeader>() == RING_HEADER_BYTES);
    assert!(offset_of!(RingHeader, reserve) == 64);
    assert!(offset_of!(RingHeader, heartbeat) == 128);
    assert!(offset_of!(RingHeader, status) == 192);
    assert!(size_of::<StatusBlock>() == RING_STATUS_BYTES);
    assert!(size_of::<Record>() == SLOT_BYTES);
    assert!(align_of::<Record>() == 8);
    assert!(size_of::<LayerStatus>() == 256);
    assert!(size_of::<LayerStatus>() <= RING_STATUS_DATA_BYTES);
    assert!(size_of::<HudHeader>() == HUD_HEADER_BYTES);
    assert!(offset_of!(HudHeader, published) == 64);
    assert!(size_of::<HudGraph>() == 60);
    assert!(size_of::<HudBufferHeader>() == HUD_PIXELS_OFFSET);
    assert!(offset_of!(HudBufferHeader, model_rev) == 176);
    assert!(RING_MAGIC.to_le_bytes()[0] == b'Q' && RING_MAGIC.to_le_bytes()[7] == b'G');
    assert!(HUD_MAGIC.to_le_bytes()[4] == b'H' && HUD_MAGIC.to_le_bytes()[7] == b'I');
};

/// What the C header must agree with: every struct size, every field offset and every
/// constant. `tests/header.rs` checks `include/quiver_perf.h`'s `_Static_assert`s and
/// `#define`s against this table, and compiles the header so the C compiler checks the
/// asserts against the header's own structs.
pub struct CLayout {
    /// (C struct name, `sizeof`).
    pub sizes: &'static [(&'static str, usize)],
    /// (C struct name, field, `offsetof`).
    pub offsets: &'static [(&'static str, &'static str, usize)],
    /// (macro name, value).
    pub constants: &'static [(&'static str, u64)],
}

macro_rules! o {
    ($s:literal, $t:ty, $f:ident) => {
        ($s, stringify!($f), core::mem::offset_of!($t, $f))
    };
}

pub const C_LAYOUT: CLayout = CLayout { sizes: SIZES, offsets: OFFSETS, constants: CONSTANTS };

const OFFSETS: &[(&str, &str, usize)] = &[
    o!("qv_ring_header", RingHeader, magic),
    o!("qv_ring_header", RingHeader, version),
    o!("qv_ring_header", RingHeader, header_bytes),
    o!("qv_ring_header", RingHeader, slot_bytes),
    o!("qv_ring_header", RingHeader, clock),
    o!("qv_ring_header", RingHeader, slot_count),
    o!("qv_ring_header", RingHeader, producer_kind),
    o!("qv_ring_header", RingHeader, clock_freq),
    o!("qv_ring_header", RingHeader, nonce),
    o!("qv_ring_header", RingHeader, writer_pid),
    o!("qv_ring_header", RingHeader, schema_rev),
    o!("qv_ring_header", RingHeader, reserve),
    o!("qv_ring_header", RingHeader, heartbeat),
    o!("qv_ring_header", RingHeader, dropped),
    o!("qv_ring_header", RingHeader, status),
    o!("qv_status_block", StatusBlock, seq),
    o!("qv_status_block", StatusBlock, len),
    o!("qv_status_block", StatusBlock, format),
    o!("qv_status_block", StatusBlock, data),
    o!("qv_record", Record, commit),
    o!("qv_record", Record, metric),
    o!("qv_record", Record, flags),
    o!("qv_record", Record, nfields),
    o!("qv_record", Record, swapchain),
    o!("qv_record", Record, key),
    o!("qv_record", Record, t),
    o!("qv_record", Record, f),
    o!("qv_layer_status", LayerStatus, struct_bytes),
    o!("qv_layer_status", LayerStatus, flags),
    o!("qv_layer_status", LayerStatus, extent_w),
    o!("qv_layer_status", LayerStatus, extent_h),
    o!("qv_layer_status", LayerStatus, vk_format),
    o!("qv_layer_status", LayerStatus, color_space),
    o!("qv_layer_status", LayerStatus, present_mode),
    o!("qv_layer_status", LayerStatus, image_count),
    o!("qv_layer_status", LayerStatus, swapchain),
    o!("qv_layer_status", LayerStatus, draw_error),
    o!("qv_layer_status", LayerStatus, drawn),
    o!("qv_layer_status", LayerStatus, presents),
    o!("qv_layer_status", LayerStatus, hud_published),
    o!("qv_layer_status", LayerStatus, vendor_id),
    o!("qv_layer_status", LayerStatus, device_id),
    o!("qv_layer_status", LayerStatus, driver_version),
    o!("qv_layer_status", LayerStatus, api_version),
    o!("qv_layer_status", LayerStatus, engine_version),
    o!("qv_layer_status", LayerStatus, swapchain_count),
    o!("qv_layer_status", LayerStatus, engine_name),
    o!("qv_layer_status", LayerStatus, app_name),
    o!("qv_layer_status", LayerStatus, device_name),
    o!("qv_hud_header", HudHeader, magic),
    o!("qv_hud_header", HudHeader, version),
    o!("qv_hud_header", HudHeader, header_bytes),
    o!("qv_hud_header", HudHeader, buffer_bytes),
    o!("qv_hud_header", HudHeader, published),
    o!("qv_hud_header", HudHeader, heartbeat),
    o!("qv_hud_header", HudHeader, writer_pid),
    o!("qv_hud_graph", HudGraph, x),
    o!("qv_hud_graph", HudGraph, y),
    o!("qv_hud_graph", HudGraph, w),
    o!("qv_hud_graph", HudGraph, h),
    o!("qv_hud_graph", HudGraph, span_ms),
    o!("qv_hud_graph", HudGraph, min_ms),
    o!("qv_hud_graph", HudGraph, max_ms),
    o!("qv_hud_graph", HudGraph, line),
    o!("qv_hud_graph", HudGraph, fill),
    o!("qv_hud_graph", HudGraph, muted),
    o!("qv_hud_graph", HudGraph, warn_above_ms),
    o!("qv_hud_graph", HudGraph, bad_above_ms),
    o!("qv_hud_graph", HudGraph, warn),
    o!("qv_hud_graph", HudGraph, bad),
    o!("qv_hud_graph", HudGraph, series),
    o!("qv_hud_buffer", HudBufferHeader, seq),
    o!("qv_hud_buffer", HudBufferHeader, width),
    o!("qv_hud_buffer", HudBufferHeader, height),
    o!("qv_hud_buffer", HudBufferHeader, stride),
    o!("qv_hud_buffer", HudBufferHeader, format),
    o!("qv_hud_buffer", HudBufferHeader, scale),
    o!("qv_hud_buffer", HudBufferHeader, extent_w),
    o!("qv_hud_buffer", HudBufferHeader, extent_h),
    o!("qv_hud_buffer", HudBufferHeader, x),
    o!("qv_hud_buffer", HudBufferHeader, y),
    o!("qv_hud_buffer", HudBufferHeader, opacity),
    o!("qv_hud_buffer", HudBufferHeader, ngraphs),
    o!("qv_hud_buffer", HudBufferHeader, graph),
    o!("qv_hud_buffer", HudBufferHeader, model_rev),
            ];

const SIZES: &[(&str, usize)] = &[
    ("qv_ring_header", core::mem::size_of::<RingHeader>()),
    ("qv_status_block", core::mem::size_of::<StatusBlock>()),
    ("qv_record", core::mem::size_of::<Record>()),
    ("qv_layer_status", core::mem::size_of::<LayerStatus>()),
    ("qv_hud_header", core::mem::size_of::<HudHeader>()),
    ("qv_hud_graph", core::mem::size_of::<HudGraph>()),
    ("qv_hud_buffer", core::mem::size_of::<HudBufferHeader>()),
];

/// `#define`s in the C header and the Rust value each must equal.
const CONSTANTS: &[(&str, u64)] = &[
    ("QV_RING_MAGIC", RING_MAGIC),
    ("QV_RING_VERSION", RING_VERSION as u64),
    ("QV_RING_HEADER_BYTES", RING_HEADER_BYTES as u64),
    ("QV_SLOT_BYTES", SLOT_BYTES as u64),
    ("QV_RING_MIN_SLOTS", RING_MIN_SLOTS as u64),
    ("QV_RING_MAX_SLOTS", RING_MAX_SLOTS as u64),
    ("QV_RING_DEFAULT_SLOTS", RING_DEFAULT_SLOTS as u64),
    ("QV_RING_STATUS_BYTES", RING_STATUS_BYTES as u64),
    ("QV_RING_STATUS_DATA_BYTES", RING_STATUS_DATA_BYTES as u64),
    ("QV_RECORD_FIELDS", RECORD_FIELDS as u64),
    ("QV_EVENT_LABEL_BYTES", EVENT_LABEL_BYTES as u64),
    ("QV_HUD_MAGIC", HUD_MAGIC),
    ("QV_HUD_VERSION", HUD_VERSION as u64),
    ("QV_HUD_HEADER_BYTES", HUD_HEADER_BYTES as u64),
    ("QV_HUD_PIXELS_OFFSET", HUD_PIXELS_OFFSET as u64),
    ("QV_HUD_MAX_BUFFER_BYTES", HUD_MAX_BUFFER_BYTES as u64),
    ("QV_HUD_MAX_DIM", HUD_MAX_DIM as u64),
    ("QV_HUD_MAX_GRAPHS", HUD_MAX_GRAPHS as u64),
    ("QV_HUD_FORMAT_RGBA8_PREMUL_SRGB", HUD_FORMAT_RGBA8_PREMUL_SRGB as u64),
    ("QV_CLOCK_QPC", CLOCK_QPC as u64),
    ("QV_CLOCK_MONO_RAW_NS", CLOCK_MONO_RAW_NS as u64),
    ("QV_PRODUCER_LAYER", PRODUCER_LAYER as u64),
    ("QV_PRODUCER_MOD", PRODUCER_MOD as u64),
    ("QV_FLAG_REAL", FLAG_REAL as u64),
    ("QV_FLAG_GENERATED", FLAG_GENERATED as u64),
    ("QV_FLAG_NOFG", FLAG_NOFG as u64),
    ("QV_FLAG_HAS_LABEL", FLAG_HAS_LABEL as u64),
    ("QV_FLAG_HAS_DURATION", FLAG_HAS_DURATION as u64),
    ("QV_FLAG_PRESENT_ID", FLAG_PRESENT_ID as u64),
    ("QV_FLAG_PRESENT_HUD", FLAG_PRESENT_HUD as u64),
    ("QV_METRIC_NONE", METRIC_NONE as u64),
    ("QV_LAYER_METRIC_PRESENT", LAYER_METRIC_PRESENT as u64),
    ("QV_STATUS_FORMAT_NONE", STATUS_FORMAT_NONE as u64),
    ("QV_STATUS_FORMAT_LAYER", STATUS_FORMAT_LAYER as u64),
    ("QV_LAYER_DRAWING", LAYER_DRAWING as u64),
    ("QV_LAYER_LOW_LATENCY2", LAYER_LOW_LATENCY2 as u64),
    ("QV_LAYER_PRESENT_ID", LAYER_PRESENT_ID as u64),
    ("QV_LAYER_PRESENT_WAIT", LAYER_PRESENT_WAIT as u64),
    ("QV_LAYER_PANICKED", LAYER_PANICKED as u64),
    ("QV_LAYER_HUD_OFF", LAYER_HUD_OFF as u64),
    ("QV_DRAW_ERR_NONE", DRAW_ERR_NONE as u64),
    ("QV_DRAW_ERR_FORMAT", DRAW_ERR_FORMAT as u64),
    ("QV_DRAW_ERR_USAGE", DRAW_ERR_USAGE as u64),
    ("QV_DRAW_ERR_QUEUE", DRAW_ERR_QUEUE as u64),
    ("QV_DRAW_ERR_DEVICE_LOST", DRAW_ERR_DEVICE_LOST as u64),
    ("QV_DRAW_ERR_NO_IMAGE", DRAW_ERR_NO_IMAGE as u64),
    ("QV_DRAW_ERR_INTERNAL", DRAW_ERR_INTERNAL as u64),
    ("QV_GRAPH_SERIES_PRESENTED", GRAPH_SERIES_PRESENTED as u64),
    ("QV_GRAPH_SERIES_BASE", GRAPH_SERIES_BASE as u64),
    ("QV_GRAPH_SERIES_GENERATED_TINT", GRAPH_SERIES_GENERATED_TINT as u64),
    ("QV_PRESENT_F_ENTRY", crate::record::PRESENT_F_ENTRY as u64),
    ("QV_PRESENT_F_RETURN", crate::record::PRESENT_F_RETURN as u64),
    ("QV_PRESENT_F_IMAGE_INDEX", crate::record::PRESENT_F_IMAGE_INDEX as u64),
    ("QV_PRESENT_F_PRESENT_MODE", crate::record::PRESENT_F_PRESENT_MODE as u64),
    ("QV_PRESENT_F_RESULT", crate::record::PRESENT_F_RESULT as u64),
    ("QV_PRESENT_F_PRESENT_ID", crate::record::PRESENT_F_PRESENT_ID as u64),
    ("QV_PRESENT_F_SEQ", crate::record::PRESENT_F_SEQ as u64),
    ("QV_PRESENT_F_HUD_NS", crate::record::PRESENT_F_HUD_NS as u64),
    ("QV_PRESENT_NFIELDS", crate::record::PRESENT_NFIELDS as u64),
    ("QV_COMMIT_BUSY", crate::ring::COMMIT_BUSY),
];
