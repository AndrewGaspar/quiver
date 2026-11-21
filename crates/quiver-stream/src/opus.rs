//! libopus: the decoder (with PLC and in-band FEC), and an encoder for the loopback source
//! and the tests.
//!
//! Stereo is a plain Opus stream. 5.1 and 7.1 are multistream with channel mapping family 1
//! (Vorbis order), laid out as libopus's surround encoder makes them: 4 streams with 2
//! coupled for 5.1, 5 with 3 for 7.1.

use std::ffi::c_int;
use std::ptr::NonNull;

use anyhow::{Result, bail};

#[allow(non_camel_case_types)]
enum OpusMSDecoder {}
#[allow(non_camel_case_types)]
enum OpusEncoder {}

unsafe extern "C" {
    fn opus_multistream_decoder_create(
        fs: i32,
        channels: c_int,
        streams: c_int,
        coupled: c_int,
        mapping: *const u8,
        error: *mut c_int,
    ) -> *mut OpusMSDecoder;
    fn opus_multistream_decode_float(
        st: *mut OpusMSDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_multistream_decoder_destroy(st: *mut OpusMSDecoder);
    fn opus_encoder_create(fs: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut OpusEncoder;
    fn opus_encode_float(st: *mut OpusEncoder, pcm: *const f32, frame_size: c_int, data: *mut u8, max: i32) -> i32;
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
}

pub const RATE: u32 = 48_000;
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;

fn layout(channels: u8) -> Result<(c_int, c_int, &'static [u8])> {
    Ok(match channels {
        1 => (1, 0, &[0]),
        2 => (1, 1, &[0, 1]),
        6 => (4, 2, &[0, 4, 1, 2, 3, 5]),
        8 => (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]),
        n => bail!("{n} audio channels: Opus streams here are 1, 2, 6 or 8"),
    })
}

pub struct Decoder {
    raw: NonNull<OpusMSDecoder>,
    pub channels: u8,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(channels: u8) -> Result<Self> {
        let (streams, coupled, mapping) = layout(channels)?;
        let mut err = 0;
        let raw = unsafe {
            opus_multistream_decoder_create(RATE as i32, channels as c_int, streams, coupled, mapping.as_ptr(), &mut err)
        };
        match NonNull::new(raw) {
            Some(raw) if err == 0 => Ok(Self { raw, channels }),
            _ => bail!("opus decoder: error {err}"),
        }
    }

    /// Decode a packet into `pcm` (interleaved; its length sets the frame size). `None`
    /// conceals a lost frame; `fec` decodes the previous frame from this packet's FEC data.
    /// Returns samples per channel.
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool, pcm: &mut [f32]) -> Result<usize> {
        let frame = pcm.len() / self.channels as usize;
        let (ptr, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        let n = unsafe {
            opus_multistream_decode_float(self.raw.as_ptr(), ptr, len, pcm.as_mut_ptr(), frame as c_int, fec as c_int)
        };
        if n < 0 {
            bail!("opus decode: error {n}");
        }
        Ok(n as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_multistream_decoder_destroy(self.raw.as_ptr()) }
    }
}

/// A stereo or mono encoder (low-delay mode, FEC on).
pub struct Encoder {
    raw: NonNull<OpusEncoder>,
    channels: u8,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(channels: u8, bitrate: i32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            bail!("the test encoder is mono or stereo");
        }
        let mut err = 0;
        let raw = unsafe { opus_encoder_create(RATE as i32, channels as c_int, OPUS_APPLICATION_RESTRICTED_LOWDELAY, &mut err) };
        let raw = match NonNull::new(raw) {
            Some(r) if err == 0 => r,
            _ => bail!("opus encoder: error {err}"),
        };
        unsafe {
            opus_encoder_ctl(raw.as_ptr(), OPUS_SET_BITRATE_REQUEST, bitrate as c_int);
            opus_encoder_ctl(raw.as_ptr(), OPUS_SET_INBAND_FEC_REQUEST, 1 as c_int);
            opus_encoder_ctl(raw.as_ptr(), OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5 as c_int);
        }
        Ok(Self { raw, channels })
    }

    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        let frame = pcm.len() / self.channels as usize;
        let mut out = vec![0u8; 1500];
        let n = unsafe { opus_encode_float(self.raw.as_ptr(), pcm.as_ptr(), frame as c_int, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            bail!("opus encode: error {n}");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.raw.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frame: usize, n: usize, hz: f32) -> Vec<f32> {
        (0..frame).flat_map(|i| {
            let t = (n * frame + i) as f32 / RATE as f32;
            let v = (t * hz * std::f32::consts::TAU).sin() * 0.5;
            [v, v]
        }).collect()
    }

    #[test]
    fn round_trip_conceal_and_fec() {
        let frame = 240; // 5 ms
        let mut enc = Encoder::new(2, 128_000).unwrap();
        let mut dec = Decoder::new(2).unwrap();
        let packets: Vec<Vec<u8>> = (0..40).map(|n| enc.encode(&tone(frame, n, 440.0)).unwrap()).collect();
        let mut pcm = vec![0f32; frame * 2];
        let mut energy = 0.0;
        for p in &packets[..20] {
            assert_eq!(dec.decode(Some(p), false, &mut pcm).unwrap(), frame);
            energy += pcm.iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy > 10.0, "decoded a tone: {energy}");
        // Packet 20 lost: conceal, or recover it from 21's FEC.
        assert_eq!(dec.decode(None, false, &mut pcm).unwrap(), frame);
        assert_eq!(dec.decode(Some(&packets[21]), true, &mut pcm).unwrap(), frame);
        assert_eq!(dec.decode(Some(&packets[21]), false, &mut pcm).unwrap(), frame);
    }

    #[test]
    fn surround_layouts_open() {
        for ch in [1u8, 2, 6, 8] {
            Decoder::new(ch).unwrap();
        }
        assert!(Decoder::new(3).is_err());
    }
}
