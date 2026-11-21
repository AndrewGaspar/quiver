//! The test pattern's sound: silence, with a 1 kHz tone burst for the first 50 ms of every
//! second of the host's clock. A client that knows the clock offset can line the burst's onset
//! up with the frames whose stamped capture time crosses the same second, and so measure
//! audio/video sync.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use quiver_stream_proto::{AudioPacket, Micros, now_micros};
use tokio::sync::mpsc;

use crate::opus;

/// Whether the 5 ms frame starting at `t` (µs) is in a burst.
pub fn in_burst(t: Micros) -> bool {
    t % 1_000_000 < 50_000
}

/// One frame of samples starting at `t`, interleaved for `channels`.
pub fn frame_at(t: Micros, frame: usize, channels: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frame * channels];
    if in_burst(t) {
        for i in 0..frame {
            let s = (t as f64 / 1e6 + i as f64 / opus::RATE as f64) * 1000.0 * std::f64::consts::TAU;
            let v = (s.sin() * 0.3) as f32;
            for c in 0..channels {
                out[i * channels + c] = v;
            }
        }
    }
    out
}

pub fn start(channels: u8, packets: mpsc::Sender<AudioPacket>, stop: Arc<AtomicBool>, _refresh_mhz: u32) -> Result<std::thread::JoinHandle<()>> {
    let mut enc = opus::Encoder::new(channels, 50, if channels > 2 { 256_000 } else { 128_000 })?;
    Ok(std::thread::Builder::new().name("tone".into()).spawn(move || {
        let frame_us = (enc.frame as u64 * 1_000_000) / opus::RATE as u64;
        // Frames start on 5 ms boundaries of the clock, so bursts start exactly on the second.
        let mut t = now_micros() / frame_us * frame_us + frame_us;
        let mut seq = 0;
        while !stop.load(Ordering::Relaxed) {
            let now = now_micros();
            if t + frame_us > now {
                std::thread::sleep(std::time::Duration::from_micros(t + frame_us - now));
            }
            let pcm = frame_at(t, enc.frame, enc.channels as usize);
            if let Ok(data) = enc.encode(&pcm)
                && packets.try_send(AudioPacket { seq, captured: t, mic: false, data }).is_err()
                && packets.is_closed()
            {
                return;
            }
            seq += 1;
            t += frame_us;
        }
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_on_the_second() {
        assert!(in_burst(5_000_000) && in_burst(5_045_000));
        assert!(!in_burst(5_050_000) && !in_burst(4_999_995));
        assert!(frame_at(7_000_000, 240, 2).iter().any(|s| s.abs() > 0.1));
        assert!(frame_at(7_100_000, 240, 2).iter().all(|s| *s == 0.0));
    }
}
