//! Headless measurements: decode latency per frame (no window), and the test-pattern
//! generator that makes streams for `--measure`.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_stream_proto::pattern;

use crate::av::{AVCOL_TRC_SMPTE2084, Backend, Demuxer};
use crate::decode::VideoDecoder;
use crate::gpu::Gpu;
use crate::gpu::convert::Converter;
use crate::stats::Samples;

#[derive(Debug, serde::Serialize)]
pub struct DecodeBench {
    pub file: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub backend: Backend,
    pub decoder: String,
    pub frames: usize,
    /// Packet in to picture out, on the CPU (for Vulkan Video, the decode is submitted then).
    pub cpu_us: Summary,
    /// Packet in to the converted picture done on the GPU: what the presenter waits for.
    pub ready_us: Summary,
    /// Frames whose test pattern read back with the right seq.
    pub pattern_ok: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
pub struct Summary {
    pub median: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

impl From<&Samples> for Summary {
    fn from(s: &Samples) -> Self {
        Self { median: s.percentile(50), p90: s.percentile(90), p99: s.percentile(99), max: s.percentile(100) }
    }
}

/// Decode `file` at `fps` (frames fed on a clock, as a stream would), timing each frame.
pub fn decode(file: &Path, fps: f64, backend: Option<Backend>, max_frames: usize, check_pattern: bool) -> Result<DecodeBench> {
    let (entry, instance, exts) = Gpu::instance(&[])?;
    let gpu = Arc::new(Gpu::new(entry, instance, exts, None)?);
    let mut conv = Converter::new(gpu.clone())?;
    conv.readback = check_pattern;
    let mut vd = VideoDecoder::new(gpu.clone(), backend);
    let mut dmx = Demuxer::open(file)?;
    vd.open(dmx.codec)?;
    let period = Duration::from_secs_f64(1.0 / fps);
    let (mut cpu, mut ready) = (Samples::default(), Samples::default());
    let mut pattern_ok = 0;
    let start = Instant::now();
    let mut n = 0usize;
    // The first frames include the decoder's set-up; leave them out of the numbers.
    const WARMUP: usize = 10;
    while n < max_frames {
        let Some((data, _key)) = dmx.read()? else { break };
        let due = start + period * n as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let t0 = Instant::now();
        let pics = vd.decode(&data, n as i64).map_err(|e| anyhow::anyhow!("decode: {}", crate::av::strerror(e)))?;
        let t1 = Instant::now();
        for mut pic in pics {
            let hdr = pic.color().trc == AVCOL_TRC_SMPTE2084;
            let c = conv.convert(&mut pic, hdr, false)?;
            drop(pic);
            if !conv.wait_done(&c, 1_000_000_000)? {
                bail!("a frame took over a second");
            }
            let t2 = Instant::now();
            if check_pattern && conv.read_pattern(&c)?.is_some_and(|s| s.seq == n as u32) {
                pattern_ok += 1;
            }
            if n >= WARMUP {
                cpu.push(t1.duration_since(t0).as_micros() as u64);
                ready.push(t2.duration_since(t0).as_micros() as u64);
            }
        }
        n += 1;
    }
    if ready.is_empty() {
        bail!("no frames decoded from {}", file.display());
    }
    Ok(DecodeBench {
        file: file.display().to_string(),
        codec: format!("{:?}", dmx.codec),
        width: dmx.width,
        height: dmx.height,
        backend: vd.backend().unwrap_or(Backend::Software),
        decoder: String::new(),
        frames: n,
        cpu_us: (&cpu).into(),
        ready_us: (&ready).into(),
        pattern_ok: check_pattern.then_some(pattern_ok),
    })
}

/// Raw NV12 frames with the test pattern (seq = frame number, captured = 0) over moving
/// content, to stdout, for piping into an encoder:
/// `quiver-stream pattern 2560x1440 600 | ffmpeg -f rawvideo -pix_fmt nv12 -s 2560x1440 -r 120 -i - -c:v hevc_nvenc -bf 0 out.h265`.
pub fn pattern_frames(width: u32, height: u32, frames: u32) -> Result<()> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!("NV12 needs an even size");
    }
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![0u8; w * h * 3 / 2];
    let mut out = std::io::stdout().lock();
    for n in 0..frames {
        let (y, uv) = buf.split_at_mut(w * h);
        // A moving gradient with a sweeping bar, so the encoder has motion to code.
        for row in 0..h {
            for col in 0..w {
                let v = ((col + row + n as usize * 4) % 256) as u8;
                let bar = (col + n as usize * 8) % w < w / 16;
                y[row * w + col] = if bar { 200 } else { 16 + v / 2 };
            }
        }
        for row in 0..h / 2 {
            for col in 0..w / 2 {
                uv[row * w + col * 2] = (128 + ((col + n as usize) % 64) as i32 - 32) as u8;
                uv[row * w + col * 2 + 1] = (128 + ((row + n as usize) % 64) as i32 - 32) as u8;
            }
        }
        pattern::draw_luma8(y, w, width, height, false, pattern::Stamp { seq: n, captured: 0 });
        // Neutral chroma under the pattern, so it stays grey.
        let (rw, rh) = pattern::region(width, height);
        for row in 0..(rh as usize / 2).min(h / 2) {
            for col in 0..(rw as usize / 2).min(w / 2) {
                uv[row * w + col * 2] = 128;
                uv[row * w + col * 2 + 1] = 128;
            }
        }
        out.write_all(&buf).context("writing frames")?;
    }
    Ok(())
}
