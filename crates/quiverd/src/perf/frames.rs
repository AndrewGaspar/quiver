//! The Sight panel's Performance section (design/SIGHT.md §14, SightPerformance mock-up): a
//! running game's last 30 s of frame times for its graph ([`frame_times`]), and the same
//! window saved as a performance log ([`mangohud_csv`]) in MangoHud's CSV format, so tools that
//! read MangoHud logs (FlightlessMango's log viewer, spreadsheets) read it too (SIGHT-HUD §1.7).
//!
//! The registry keeps the last [`KEEP_NS`] of presents and sensor samples; logs go to the
//! game's data folder (`<data>/games/<game>/perf-logs/`), not the session dir, which goes
//! away with the session.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quiver_api::perf::{FrameTimes, Stat};

use super::format::Raw;
use super::registry::{Store, WINDOW_1S};
use super::stats::{KEEP_NS, percentile_sorted};

/// The longest span the graph and the log cover (a little under what the registry keeps).
pub const MAX_SPAN_MS: u32 = 30_000;

/// The worst frame time per slot, oldest first: `slots` slots over `(now - span, now]`.
fn bucket(frames: &[(i64, f64)], now: i64, span_ns: i64, slots: usize) -> Vec<Option<f32>> {
    let mut out = vec![None; slots];
    let start = now - span_ns;
    for (t, ms) in frames {
        if *t <= start || *t > now {
            continue;
        }
        let i = (((t - start) as i128 * slots as i128) / span_ns as i128).clamp(0, slots as i128 - 1) as usize;
        let v = *ms as f32;
        out[i] = Some(out[i].map_or(v, |o: f32| o.max(v)));
    }
    out
}

/// Frames over four times the median, per minute: the session summary's hitch rule over a
/// window. `None` with under five seconds of frames.
pub fn hitches_per_min(frames: &[f64]) -> Option<f32> {
    let covered: f64 = frames.iter().sum();
    if covered < 5_000.0 {
        return None;
    }
    let mut v = frames.to_vec();
    v.sort_by(f64::total_cmp);
    let p50 = percentile_sorted(&v, 50.0);
    let n = frames.iter().filter(|ms| **ms > 4.0 * p50).count();
    Some((n as f64 * 60_000.0 / covered) as f32)
}

/// The panel's graph: the last `span_ms` (≤ 30 s) in `buckets` slots (30–1200).
pub fn frame_times(session: &str, store: &Store, now: i64, span_ms: u32, buckets: u32) -> FrameTimes {
    let span_ms = span_ms.clamp(1000, MAX_SPAN_MS);
    let span_ns = span_ms as i64 * 1_000_000;
    let slots = buckets.clamp(30, 1200) as usize;
    let (presented, rendered) = store.frames.timed(now - span_ns);
    let ms: Vec<f64> = presented.iter().map(|(_, m)| *m).collect();
    FrameTimes {
        session: session.to_owned(),
        span_ms,
        presented: bucket(&presented, now, span_ns, slots),
        rendered: if rendered.is_empty() { Vec::new() } else { bucket(&rendered, now, span_ns, slots) },
        frames: presented.len() as u32,
        hitches_per_min: hitches_per_min(&ms),
    }
}

/// The machine, for the log's header.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub os: String,
    pub cpu: String,
    pub kernel: String,
    pub driver: String,
}

impl Machine {
    /// Read from `/etc/os-release`, `/proc/cpuinfo`, the kernel's release and NVIDIA's driver
    /// version (when there is one).
    pub fn read() -> Machine {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        let os = read("/etc/os-release").lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_owned()).unwrap_or_else(|| "Linux".into());
        let cpu = read("/proc/cpuinfo").lines().find_map(|l| l.strip_prefix("model name")).and_then(|v| v.split_once(':')).map(|(_, v)| v.trim().to_owned()).unwrap_or_default();
        let kernel = read("/proc/sys/kernel/osrelease").trim().to_owned();
        // "NVRM version: NVIDIA UNIX x86_64 Kernel Module  620.70.06  …"
        let driver = read("/proc/driver/nvidia/version").lines().next().and_then(|l| l.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))).map(|v| format!("NVIDIA {v}")).unwrap_or_default();
        Machine { os, cpu, kernel, driver }
    }
}

/// A CSV field: no commas or line breaks inside (MangoHud's readers split on commas).
fn field(s: &str) -> String {
    s.replace([',', '\n', '\r'], " ")
}

/// The value a sampled metric had at `t` (its latest sample at or before it, else its first).
fn at(samples: &[(i64, f64)], t: i64) -> Option<f64> {
    let i = samples.partition_point(|(st, _)| *st <= t);
    samples.get(i.saturating_sub(1)).map(|(_, v)| *v)
}

/// The last `seconds` (≤ 30) of frames as a MangoHud log: its system header, then one row per
/// presented frame (fps, frame time ms, the sensors at that moment, elapsed ns since the
/// first). Returns the text, the frames written and the seconds they cover.
pub fn mangohud_csv(store: &Store, now: i64, seconds: u32, m: &Machine) -> (String, u32, f32) {
    let span_ns = seconds.clamp(1, MAX_SPAN_MS / 1000) as i64 * 1_000_000_000;
    let since = now - span_ns.min(KEEP_NS);
    let (frames, _) = store.frames.timed(since);
    let text = |id: &str| match store.value(id, Stat::Last, WINDOW_1S, now) {
        Some(Raw::Text(s)) => s,
        _ => String::new(),
    };
    let gb = |id: &str| match store.value(id, Stat::Last, WINDOW_1S, now) {
        Some(Raw::Num(v)) if v > 0.0 => format!("{:.0}", v / 1e9),
        _ => String::new(),
    };
    let mut out = String::new();
    out.push_str("os,cpu,gpu,ram,kernel,driver,cpuscheduler\n");
    let _ = writeln!(out, "{},{},{},{},{},{},", field(&m.os), field(&m.cpu), field(&text("gpu.name")), gb("mem.total"), field(&m.kernel), field(&m.driver));
    out.push_str("--------------------FRAME METRICS--------------------\n");
    out.push_str("fps,frametime,cpu_load,gpu_load,cpu_temp,gpu_temp,gpu_core_clock,gpu_mem_clock,gpu_vram_used,gpu_power,ram_used,elapsed\n");
    // MangoHud logs memory in GB and power in W.
    let cols: [(&str, f64); 9] = [
        ("cpu.load", 1.0),
        ("gpu.load", 1.0),
        ("cpu.temp", 1.0),
        ("gpu.temp", 1.0),
        ("gpu.clock", 1.0),
        ("gpu.mem_clock", 1.0),
        ("gpu.vram.used", 1e-9),
        ("gpu.power", 1.0),
        ("mem.used", 1e-9),
    ];
    let samples: Vec<Vec<(i64, f64)>> = cols.iter().map(|(id, _)| store.samples_since(id, since - 2_000_000_000)).collect();
    let t0 = frames.first().map(|(t, _)| *t).unwrap_or(now);
    for (t, ms) in &frames {
        let _ = write!(out, "{:.1},{:.3}", if *ms > 0.0 { 1000.0 / ms } else { 0.0 }, ms);
        for ((_, scale), s) in cols.iter().zip(&samples) {
            match at(s, *t) {
                Some(v) if *scale == 1.0 => {
                    let _ = write!(out, ",{v:.0}");
                }
                Some(v) => {
                    let _ = write!(out, ",{:.3}", v * scale);
                }
                None => out.push_str(",0"),
            }
        }
        let _ = writeln!(out, ",{}", t - t0);
    }
    let covered = frames.iter().map(|(_, ms)| ms).sum::<f64>() as f32 / 1000.0;
    (out, frames.len() as u32, covered)
}

/// Where a session's log goes: `<game dir>/perf-logs/<local time>-<session>.csv`.
pub fn log_path(game_dir: &Path, session: &str, at: chrono::DateTime<chrono::Local>) -> PathBuf {
    let safe: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    game_dir.join("perf-logs").join(format!("{}-{safe}.csv", at.format("%Y%m%d-%H%M%S")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::catalog;
    use chrono::Utc;
    use quiver_perf::layout::LayerStatus;

    const MS: i64 = 1_000_000;

    /// 30 s at ~120 fps with a hitch every 10 s, frame generation counting every other present,
    /// GPU load sampled once a second.
    fn store() -> (Store, i64) {
        let mut s = Store::new(0, Utc::now());
        s.producer("layer", "Sight layer", true);
        s.producer("system", "System", true);
        for d in catalog::system() {
            s.declare("system", d);
        }
        s.set_layer_status(LayerStatus { swapchain: 7, ..LayerStatus::ZERO });
        let mut t = 1_000 * MS;
        let mut k = 0;
        while t < 31_000 * MS {
            t += if k % 1200 == 600 { 40 * MS } else { 8_333_333 };
            s.present(7, t, t);
            if k % 2 == 0 {
                s.base_frame(7, t);
            }
            if k % 120 == 0 {
                s.push_num("gpu.load", t, 50.0 + (k / 120) as f64);
                s.push_num("mem.used", t, 12e9);
            }
            k += 1;
        }
        s.push_text("gpu.name", t, "NVIDIA GeForce RTX 5070, Laptop".into());
        (s, t)
    }

    #[test]
    fn the_graph_has_both_series_and_the_hitches() {
        let (s, now) = store();
        let f = frame_times("s-1", &s, now, 30_000, 300);
        assert_eq!((f.span_ms, f.presented.len(), f.rendered.len()), (30_000, 300, 300));
        assert!(f.frames > 3500, "{}", f.frames);
        // Slots hold the worst frame: three hitches of 40 ms.
        let spikes = f.presented.iter().filter(|v| v.is_some_and(|ms| ms > 30.0)).count();
        assert_eq!(spikes, 3);
        // Rendered frames are two presents long.
        let typical = f.rendered.iter().flatten().filter(|ms| (**ms - 16.67).abs() < 0.1).count();
        assert!(typical > 250, "{typical}");
        // Three hitches in ~30 s.
        let h = f.hitches_per_min.unwrap();
        assert!((5.5..=6.5).contains(&h), "{h}");
        // The span is clamped; without rendered frames that series is empty.
        assert_eq!(frame_times("s-1", &s, now, 120_000, 10).presented.len(), 30);
        assert!(hitches_per_min(&[8.3; 100]).is_none(), "under five seconds");
    }

    #[test]
    fn the_log_reads_like_mangohuds() {
        let (s, now) = store();
        let m = Machine { os: "Arch Linux".into(), cpu: "AMD Ryzen AI 9 HX 370".into(), kernel: "7.2.15".into(), driver: "NVIDIA 620.70.06".into() };
        let (csv, frames, secs) = mangohud_csv(&s, now, 30, &m);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "os,cpu,gpu,ram,kernel,driver,cpuscheduler");
        assert_eq!(lines[1], "Arch Linux,AMD Ryzen AI 9 HX 370,NVIDIA GeForce RTX 5070  Laptop,,7.2.15,NVIDIA 620.70.06,");
        assert!(lines[2].contains("FRAME METRICS"));
        assert!(lines[3].starts_with("fps,frametime,") && lines[3].ends_with(",elapsed"));
        assert_eq!(lines.len() - 4, frames as usize);
        assert!((29.0..=30.5).contains(&secs), "{secs}");
        // A row: fps, frame time, the sensors then (GPU load sampled each second), elapsed.
        let row: Vec<&str> = lines[4 + 200].split(',').collect();
        assert_eq!(row.len(), 12);
        assert_eq!(row[1], "8.333");
        assert_eq!(row[0], "120.0");
        assert!(row[3].parse::<f64>().unwrap() >= 50.0, "{row:?}");
        assert_eq!(row[10], "12.000", "RAM in GB");
        let first: Vec<&str> = lines[4].split(',').collect();
        assert_eq!(first[11], "0", "elapsed from the first frame");
        // A shorter log.
        let (_, ten, _) = mangohud_csv(&s, now, 10, &m);
        assert!(ten < frames / 2);
    }

    #[test]
    fn logs_go_to_the_games_folder() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-19T21:18:05+00:00").unwrap().with_timezone(&chrono::Local);
        let p = log_path(Path::new("/data/games/epic-x"), "s-1/../x", at);
        assert_eq!(p.parent().unwrap(), Path::new("/data/games/epic-x/perf-logs"));
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with("-s-1x.csv") && !name.contains('/'), "{name}");
    }
}
