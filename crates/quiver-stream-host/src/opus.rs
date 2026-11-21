//! Opus encoding (libopus, linked), in low-delay mode with 5 ms frames.
//!
//! Stereo is plain Opus. 5.1 and 7.1 use libopus's surround encoder with mapping family 1
//! (Vorbis channel order): for 5.1 that is 4 streams, 2 of them coupled, channel mapping
//! `0 4 1 2 3 5`; for 7.1, 5 streams, 3 coupled, `0 6 1 2 3 4 5 7`. A multistream packet with
//! one stream (stereo) is an ordinary Opus packet. Input is interleaved f32 in Vorbis order
//! (FL FC FR RL RR LFE for 5.1); [`vorbis_positions`] gives the PipeWire positions to ask for.

use std::ffi::{c_int, c_uchar};

use anyhow::{Result, bail};

#[repr(C)]
struct OpusMSEncoder {
    _p: [u8; 0],
}

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_multistream_surround_encoder_create(fs: i32, channels: c_int, family: c_int, streams: *mut c_int, coupled: *mut c_int, mapping: *mut c_uchar, application: c_int, error: *mut c_int) -> *mut OpusMSEncoder;
    fn opus_multistream_encode_float(st: *mut OpusMSEncoder, pcm: *const f32, frame_size: c_int, data: *mut c_uchar, max: i32) -> i32;
    fn opus_multistream_encoder_ctl(st: *mut OpusMSEncoder, request: c_int, ...) -> c_int;
    fn opus_multistream_encoder_destroy(st: *mut OpusMSEncoder);
    fn opus_strerror(error: c_int) -> *const std::ffi::c_char;
}

const APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const SET_BITRATE: c_int = 4002;
const SET_COMPLEXITY: c_int = 4010;

pub const RATE: u32 = 48_000;

/// PipeWire's channel positions in the order the encoder wants them.
pub fn vorbis_positions(channels: u8) -> &'static [&'static str] {
    match channels {
        1 => &["MONO"],
        6 => &["FL", "FC", "FR", "RL", "RR", "LFE"],
        8 => &["FL", "FC", "FR", "SL", "SR", "RL", "RR", "LFE"],
        _ => &["FL", "FR"],
    }
}

/// The sink's positions, in PipeWire's usual order (what a game sees as its output).
pub fn sink_positions(channels: u8) -> &'static str {
    match channels {
        6 => "FL,FR,FC,LFE,RL,RR",
        8 => "FL,FR,FC,LFE,RL,RR,SL,SR",
        _ => "FL,FR",
    }
}

pub struct Encoder {
    st: *mut OpusMSEncoder,
    pub channels: u8,
    pub streams: u8,
    pub coupled: u8,
    pub mapping: Vec<u8>,
    /// Samples per channel in one frame.
    pub frame: usize,
    out: Vec<u8>,
}

// SAFETY: the encoder state is only touched through &mut self.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `frame_ms` is 2.5, 5, 10 or 20 (as tenths: 25, 50, 100, 200).
    pub fn new(channels: u8, frame_ms_x10: u32, bitrate: i32) -> Result<Encoder> {
        let channels = match channels {
            1 | 2 | 6 | 8 => channels,
            _ => 2,
        };
        let family = if channels <= 2 { 0 } else { 1 };
        let (mut streams, mut coupled, mut err) = (0, 0, 0);
        let mut mapping = vec![0u8; channels as usize];
        let st = unsafe { opus_multistream_surround_encoder_create(RATE as i32, channels as c_int, family, &mut streams, &mut coupled, mapping.as_mut_ptr(), APPLICATION_RESTRICTED_LOWDELAY, &mut err) };
        if st.is_null() || err != 0 {
            let msg = unsafe { std::ffi::CStr::from_ptr(opus_strerror(err)) }.to_string_lossy().into_owned();
            bail!("opus encoder: {msg}");
        }
        unsafe {
            opus_multistream_encoder_ctl(st, SET_BITRATE, bitrate as c_int);
            opus_multistream_encoder_ctl(st, SET_COMPLEXITY, 8 as c_int);
        }
        let frame = (RATE * frame_ms_x10 / 10_000) as usize;
        Ok(Encoder { st, channels, streams: streams as u8, coupled: coupled as u8, mapping, frame, out: vec![0; 4000] })
    }

    /// Encode one frame of interleaved samples (`frame * channels` of them).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        assert_eq!(pcm.len(), self.frame * self.channels as usize);
        let n = unsafe { opus_multistream_encode_float(self.st, pcm.as_ptr(), self.frame as c_int, self.out.as_mut_ptr(), self.out.len() as i32) };
        if n < 0 {
            bail!("opus encode: error {n}");
        }
        Ok(self.out[..n as usize].to_vec())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_multistream_encoder_destroy(self.st) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_and_surround_layouts() {
        let e = Encoder::new(2, 50, 128_000).unwrap();
        assert_eq!((e.streams, e.coupled, e.frame), (1, 1, 240));
        let e = Encoder::new(6, 50, 256_000).unwrap();
        assert_eq!((e.streams, e.coupled, e.mapping.as_slice()), (4, 2, &[0u8, 4, 1, 2, 3, 5][..]));
        let e = Encoder::new(8, 50, 384_000).unwrap();
        assert_eq!((e.streams, e.coupled), (5, 3));
        assert_eq!(vorbis_positions(6).len(), 6);
    }

    #[test]
    fn encodes_5ms_frames() {
        let mut e = Encoder::new(2, 50, 128_000).unwrap();
        let pcm: Vec<f32> = (0..480).map(|i| ((i / 2) as f32 * 0.05).sin() * 0.5).collect();
        let mut sizes = Vec::new();
        for _ in 0..10 {
            sizes.push(e.encode(&pcm).unwrap().len());
        }
        assert!(sizes.iter().all(|s| *s > 2 && *s < 400), "{sizes:?}");
        // TOC byte: CELT-only (restricted low delay), 5 ms → config 29 (FB, 5 ms) → 0xe8 or 0xec (stereo).
        let p = e.encode(&pcm).unwrap();
        assert_eq!(p[0] >> 3, 29, "toc {:#x}", p[0]);
    }
}
