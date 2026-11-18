//! Ring records: constructors for each kind and checked accessors for readers.

use crate::layout::*;
pub use crate::layout::{LayerStatus, Record};

/// The layer's present record fields (`present.frame`, in this order).
pub const PRESENT_F_ENTRY: usize = 0;
/// CPU time when the layer's `vkQueuePresentKHR` returned (after the driver).
pub const PRESENT_F_RETURN: usize = 1;
pub const PRESENT_F_IMAGE_INDEX: usize = 2;
/// `VkPresentModeKHR`.
pub const PRESENT_F_PRESENT_MODE: usize = 3;
/// `VkResult` of the present for this swapchain.
pub const PRESENT_F_RESULT: usize = 4;
/// `VkPresentIdKHR` id the app chained (DXVK's frame id), 0 when none.
pub const PRESENT_F_PRESENT_ID: usize = 5;
/// The layer's own per-swapchain present count (1, 2, 3, …).
pub const PRESENT_F_SEQ: usize = 6;
/// CPU ns the layer spent on the HUD in this present (upload + composite recording).
pub const PRESENT_F_HUD_NS: usize = 7;
pub const PRESENT_NFIELDS: usize = 8;
/// Field names in the order above, as the built-in descriptor lists them.
pub const PRESENT_FIELD_NAMES: [&str; PRESENT_NFIELDS] =
    ["entry", "return", "image_index", "present_mode", "result", "present_id", "seq", "hud_ns"];

/// Why a record (built or read) isn't acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// Metric 0, or above the producer's declared count.
    Metric(u16),
    /// More than 12 fields.
    Fields(u8),
    /// Unknown flag bits, or REAL and GENERATED together.
    Flags(u8),
}

impl core::fmt::Display for RecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordError::Metric(m) => write!(f, "metric index {m} out of range"),
            RecordError::Fields(n) => write!(f, "{n} fields (max {RECORD_FIELDS})"),
            RecordError::Flags(b) => write!(f, "invalid flags {b:#04x}"),
        }
    }
}

impl Record {
    pub const ZERO: Record =
        Record { commit: 0, metric: 0, flags: 0, nfields: 0, swapchain: 0, key: 0, t: 0, f: [0; RECORD_FIELDS] };

    /// A `frame` record: one per frame, `fields` in the descriptor's `fields` order.
    pub fn frame(metric: u16, key: u64, flags: u8, swapchain: u32, t: i64, fields: &[i64]) -> Result<Record, RecordError> {
        if fields.len() > RECORD_FIELDS {
            return Err(RecordError::Fields(fields.len().min(255) as u8));
        }
        let mut r = Record { metric, key, flags, swapchain, t, nfields: fields.len() as u8, ..Record::ZERO };
        r.f[..fields.len()].copy_from_slice(fields);
        Ok(r)
    }

    /// A `series` record: one value at a time, `f[0]` = the f64's bits.
    pub fn series(metric: u16, t: i64, value: f64) -> Record {
        let mut r = Record { metric, t, nfields: 1, ..Record::ZERO };
        r.f[0] = value.to_bits() as i64;
        r
    }

    /// An `event` record: `f[0]` = duration (with [`FLAG_HAS_DURATION`]), `f[1..7]` = up to
    /// 48 bytes of UTF-8 label (with [`FLAG_HAS_LABEL`]), cut at a char boundary.
    pub fn event(metric: u16, t: i64, duration: Option<i64>, label: Option<&str>) -> Record {
        let mut r = Record { metric, t, nfields: 1, ..Record::ZERO };
        if let Some(d) = duration {
            r.f[0] = d;
            r.flags |= FLAG_HAS_DURATION;
        }
        if let Some(label) = label {
            let mut n = label.len().min(EVENT_LABEL_BYTES);
            while !label.is_char_boundary(n) {
                n -= 1;
            }
            let mut bytes = [0u8; EVENT_LABEL_BYTES];
            bytes[..n].copy_from_slice(&label.as_bytes()[..n]);
            for (i, chunk) in bytes.chunks_exact(8).enumerate() {
                r.f[1 + i] = i64::from_le_bytes(chunk.try_into().unwrap());
            }
            r.flags |= FLAG_HAS_LABEL;
            r.nfields = 7;
        }
        r
    }

    /// The valid fields (`nfields` clamped to 12, so a hostile count can't overrun).
    pub fn fields(&self) -> &[i64] {
        &self.f[..(self.nfields as usize).min(RECORD_FIELDS)]
    }

    /// `f[0]` as an f64, if finite (a series record's value).
    pub fn series_value(&self) -> Option<f64> {
        let v = f64::from_bits(self.f[0] as u64);
        (self.nfields >= 1 && v.is_finite()).then_some(v)
    }

    pub fn event_duration(&self) -> Option<i64> {
        (self.flags & FLAG_HAS_DURATION != 0).then_some(self.f[0])
    }

    /// The label's raw bytes up to the first NUL. Untrusted: sanitize before display.
    pub fn event_label_bytes(&self) -> Option<&[u8]> {
        if self.flags & FLAG_HAS_LABEL == 0 {
            return None;
        }
        // SAFETY: f[1..7] is 48 initialized bytes inside `self`; u8 has no alignment needs.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(self.f[1..].as_ptr().cast::<u8>(), EVENT_LABEL_BYTES) };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(EVENT_LABEL_BYTES);
        Some(&bytes[..end])
    }

    /// The label if it is valid UTF-8.
    pub fn event_label(&self) -> Option<&str> {
        self.event_label_bytes().and_then(|b| core::str::from_utf8(b).ok())
    }

    pub fn is_real(&self) -> bool {
        self.flags & FLAG_REAL != 0
    }
    pub fn is_generated(&self) -> bool {
        self.flags & FLAG_GENERATED != 0
    }
    pub fn is_nofg(&self) -> bool {
        self.flags & FLAG_NOFG != 0
    }

    /// Structural checks a reader applies to every slot (SIGHT-HUD §3.16): metric index in
    /// `1..=max_metric`, ≤ 12 fields, known flags, not both real and generated.
    pub fn check(&self, max_metric: u16) -> Result<(), RecordError> {
        if self.metric == METRIC_NONE || self.metric > max_metric {
            return Err(RecordError::Metric(self.metric));
        }
        if self.nfields as usize > RECORD_FIELDS {
            return Err(RecordError::Fields(self.nfields));
        }
        if self.flags & !FLAGS_KNOWN != 0 || (self.is_real() && self.is_generated()) {
            return Err(RecordError::Flags(self.flags));
        }
        Ok(())
    }

    pub(crate) fn to_words(self) -> [u64; 16] {
        // SAFETY: Record is repr(C), 128 bytes, align 8, integers only, no padding.
        unsafe { core::mem::transmute::<Record, [u64; 16]>(self) }
    }

    pub(crate) fn from_words(w: [u64; 16]) -> Record {
        // SAFETY: every bit pattern is a valid Record (integers only, no padding).
        unsafe { core::mem::transmute::<[u64; 16], Record>(w) }
    }
}

/// One `vkQueuePresentKHR` for one swapchain, as the Sight layer records it
/// (metric [`LAYER_METRIC_PRESENT`], clock CLOCK_MONOTONIC_RAW ns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerPresent {
    /// The layer's swapchain id; changes on `vkCreateSwapchainKHR`.
    pub swapchain: u32,
    /// `VkPresentIdKHR` (DXVK's per-swapchain frame id) when the app chained one.
    pub present_id: Option<u64>,
    /// The layer's own present count on this swapchain.
    pub seq: u64,
    pub entry_ns: i64,
    pub return_ns: i64,
    pub image_index: u32,
    pub present_mode: u32,
    pub result: i32,
    /// The HUD was composited into this present.
    pub hud_drawn: bool,
    pub hud_ns: i64,
    /// While the layer counts Reflex frames: `Some(true)` for the first present of an
    /// application-rendered frame ([`FLAG_REAL`]), `Some(false)` for a further one
    /// ([`FLAG_GENERATED`]: a frame generator's).
    pub app_frame: Option<bool>,
}

impl LayerPresent {
    /// `key` is the present id when chained, else `seq`; `t` is the entry time.
    pub fn to_record(&self) -> Record {
        let mut flags = 0;
        if self.present_id.is_some() {
            flags |= FLAG_PRESENT_ID;
        }
        if self.hud_drawn {
            flags |= FLAG_PRESENT_HUD;
        }
        match self.app_frame {
            Some(true) => flags |= FLAG_REAL,
            Some(false) => flags |= FLAG_GENERATED,
            None => {}
        }
        let mut f = [0i64; PRESENT_NFIELDS];
        f[PRESENT_F_ENTRY] = self.entry_ns;
        f[PRESENT_F_RETURN] = self.return_ns;
        f[PRESENT_F_IMAGE_INDEX] = self.image_index as i64;
        f[PRESENT_F_PRESENT_MODE] = self.present_mode as i64;
        f[PRESENT_F_RESULT] = self.result as i64;
        f[PRESENT_F_PRESENT_ID] = self.present_id.unwrap_or(0) as i64;
        f[PRESENT_F_SEQ] = self.seq as i64;
        f[PRESENT_F_HUD_NS] = self.hud_ns;
        let key = self.present_id.unwrap_or(self.seq);
        Record::frame(LAYER_METRIC_PRESENT, key, flags, self.swapchain, self.entry_ns, &f).unwrap()
    }

    /// Decodes a layer present record; `None` if it isn't one.
    pub fn from_record(r: &Record) -> Option<LayerPresent> {
        if r.metric != LAYER_METRIC_PRESENT || (r.nfields as usize) < PRESENT_NFIELDS {
            return None;
        }
        let f = &r.f;
        Some(LayerPresent {
            swapchain: r.swapchain,
            present_id: (r.flags & FLAG_PRESENT_ID != 0).then_some(f[PRESENT_F_PRESENT_ID] as u64),
            seq: f[PRESENT_F_SEQ] as u64,
            entry_ns: f[PRESENT_F_ENTRY],
            return_ns: f[PRESENT_F_RETURN],
            image_index: f[PRESENT_F_IMAGE_INDEX] as u32,
            present_mode: f[PRESENT_F_PRESENT_MODE] as u32,
            result: f[PRESENT_F_RESULT] as i32,
            hud_drawn: r.flags & FLAG_PRESENT_HUD != 0,
            hud_ns: f[PRESENT_F_HUD_NS],
            app_frame: if r.is_real() {
                Some(true)
            } else if r.is_generated() {
                Some(false)
            } else {
                None
            },
        })
    }
}

impl LayerStatus {
    pub const ZERO: LayerStatus = LayerStatus {
        struct_bytes: core::mem::size_of::<LayerStatus>() as u32,
        flags: 0,
        extent_w: 0,
        extent_h: 0,
        vk_format: 0,
        color_space: 0,
        present_mode: 0,
        image_count: 0,
        swapchain: 0,
        draw_error: 0,
        drawn: 0,
        presents: 0,
        hud_published: 0,
        vendor_id: 0,
        device_id: 0,
        driver_version: 0,
        api_version: 0,
        engine_version: 0,
        swapchain_count: 0,
        engine_name: [0; 32],
        app_name: [0; 64],
        device_name: [0; 64],
        keys: 0,
        key_toggles: 0,
        key_nexts: 0,
        reflex_flags: 0,
        reflex_min_interval_us: 0,
        reflex_markers: 0,
        reflex_sleeps: 0,
        app_frames: 0,
        app_presents: 0,
        pc_latency_frames: 0,
        pc_latency_sum_us: 0,
        render_latency_frames: 0,
        render_latency_sum_us: 0,
        key_panels: 0,
        backdrop_error: 0,
        backdrop_gpu_ns: 0,
    };

    /// The X11 keymap polling's state (`KEYS_*`).
    pub fn keys_x11(&self) -> u32 {
        self.keys & 0xff
    }

    /// The Wayland keyboard's state (`KEYS_WL_*`).
    pub fn keys_wayland(&self) -> u32 {
        (self.keys >> crate::layout::KEYS_WL_SHIFT) & 0xff
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: repr(C), integers and byte arrays only, no padding (size asserted 336).
        unsafe { core::slice::from_raw_parts((self as *const LayerStatus).cast::<u8>(), core::mem::size_of::<LayerStatus>()) }
    }

    /// From a status block payload. Short payloads (an older layer) are zero-extended.
    pub fn from_bytes(b: &[u8]) -> LayerStatus {
        let mut s = LayerStatus::ZERO;
        let n = b.len().min(core::mem::size_of::<LayerStatus>());
        // SAFETY: as in as_bytes; every bit pattern is valid.
        unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), (&mut s as *mut LayerStatus).cast::<u8>(), n) };
        s
    }

    /// Copies `s` NUL-padded into a fixed string field, cut at a char boundary.
    pub fn set_str(field: &mut [u8], s: &str) {
        field.fill(0);
        let mut n = s.len().min(field.len().saturating_sub(1));
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        field[..n].copy_from_slice(&s.as_bytes()[..n]);
    }

    /// A fixed string field up to its first NUL, if valid UTF-8. Untrusted.
    pub fn get_str(field: &[u8]) -> Option<&str> {
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        core::str::from_utf8(&field[..end]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_roundtrip() {
        let p = LayerPresent {
            swapchain: 3,
            present_id: Some(1234),
            seq: 99,
            entry_ns: 1_000_000,
            return_ns: 1_050_000,
            image_index: 2,
            present_mode: 2,
            result: -1000001004,
            hud_drawn: true,
            hud_ns: 4100,
            app_frame: Some(true),
        };
        let r = p.to_record();
        assert_eq!(r.key, 1234);
        assert_eq!(r.t, 1_000_000);
        assert!(r.check(LAYER_METRIC_PRESENT).is_ok());
        assert_eq!(LayerPresent::from_record(&r), Some(p));
        assert!(r.is_real() && r.check(LAYER_METRIC_PRESENT).is_ok());
        let g = LayerPresent { app_frame: Some(false), ..p };
        assert!(g.to_record().is_generated());
        assert_eq!(LayerPresent::from_record(&g.to_record()), Some(g));
        let q = LayerPresent { present_id: None, app_frame: None, ..p };
        assert_eq!(q.to_record().key, 99);
        assert_eq!(LayerPresent::from_record(&q.to_record()), Some(q));
        assert_eq!(Record::from_words(r.to_words()), r);
    }

    #[test]
    fn event_label_cut_at_char_boundary() {
        let long = "é".repeat(40); // 80 bytes
        let r = Record::event(2, 5, Some(7), Some(&long));
        let l = r.event_label().unwrap();
        assert_eq!(l.len(), 48);
        assert!(long.starts_with(l));
        assert_eq!(r.event_duration(), Some(7));
        let r = Record::event(2, 5, None, None);
        assert_eq!(r.event_label(), None);
        assert_eq!(r.event_duration(), None);
        let r = Record::event(2, 5, None, Some("begin"));
        assert_eq!(r.event_label(), Some("begin"));
    }

    #[test]
    fn checks() {
        assert_eq!(Record::ZERO.check(64), Err(RecordError::Metric(0)));
        assert_eq!(Record::series(65, 0, 1.0).check(64), Err(RecordError::Metric(65)));
        assert_eq!(Record::series(3, 0, 1.5).series_value(), Some(1.5));
        assert_eq!(Record::series(3, 0, f64::NAN).series_value(), None);
        let mut r = Record::series(3, 0, 1.0);
        r.nfields = 200;
        assert_eq!(r.check(64), Err(RecordError::Fields(200)));
        assert_eq!(r.fields().len(), 12);
        r.nfields = 1;
        r.flags = FLAG_REAL | FLAG_GENERATED;
        assert_eq!(r.check(64), Err(RecordError::Flags(3)));
        r.flags = 0x80;
        assert!(r.check(64).is_err());
        assert!(Record::frame(1, 0, 0, 0, 0, &[0; 13]).is_err());
    }

    #[test]
    fn layer_status_bytes() {
        let mut s = LayerStatus::ZERO;
        s.extent_w = 2560;
        LayerStatus::set_str(&mut s.engine_name, "DXVK");
        let b = s.as_bytes();
        assert_eq!(b.len(), 336);
        // A layer from before the panel key left these 8 bytes reserved (zero).
        assert_eq!(LayerStatus::from_bytes(b).key_panels, 0);
        let t = LayerStatus::from_bytes(b);
        assert_eq!(t, s);
        assert_eq!(LayerStatus::get_str(&t.engine_name), Some("DXVK"));
        assert_eq!(LayerStatus::from_bytes(&b[..8]).extent_w, 0);
        // A layer from before the Reflex fields (256 bytes): they read as zero.
        let mut r = s;
        r.app_frames = 9;
        assert_eq!(LayerStatus::from_bytes(&r.as_bytes()[..256]).app_frames, 0);
    }
}
