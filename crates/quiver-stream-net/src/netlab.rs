//! The stats lines `quiver-netlab run` reads (crates/quiver-netlab, `stats` module), written
//! by the real host and client when they run under it.
//!
//! The runner names a file in `QUIVER_NETLAB_STATS`; each workload appends one JSON object a
//! line, about once a second:
//!
//! ```json
//! {"t_us": 81234567890, "client": { ...ClientStats... }, "extra": {"frame_gaps": 1, "recv_kbps": 19500}}
//! {"t_us": 81234568001, "host": { ...HostStats... }, "extra": {"target_kbps": 40000}}
//! ```
//!
//! `t_us` is [`now_micros`]. The `extra` keys the report knows are `frame_gaps`,
//! `max_gap_ms`, `recv_kbps`, `latency_p99_us` and `target_kbps`. Outside the netlab (no
//! variable) nothing is written.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use quiver_stream_proto::{ClientStats, HostStats, Micros, now_micros};
use serde::Serialize;

/// The environment variable naming the stats file.
pub const ENV_FILE: &str = "QUIVER_NETLAB_STATS";

#[derive(Serialize)]
struct Line<'a> {
    t_us: Micros,
    #[serde(skip_serializing_if = "Option::is_none")]
    client: Option<&'a ClientStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<&'a HostStats>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extra: &'a BTreeMap<String, f64>,
}

/// Appends stats lines to the netlab's file, or nowhere.
pub struct StatsFile {
    out: Option<std::fs::File>,
}

impl StatsFile {
    /// The file `QUIVER_NETLAB_STATS` names, if it is set (and opens).
    pub fn from_env() -> StatsFile {
        let out = std::env::var_os(ENV_FILE).and_then(|p| Self::open(Path::new(&p)).ok());
        StatsFile { out }
    }

    /// Appends to `path`.
    pub fn at(path: &Path) -> std::io::Result<StatsFile> {
        Ok(StatsFile { out: Some(Self::open(path)?) })
    }

    fn open(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().create(true).append(true).open(path)
    }

    /// Whether lines go anywhere.
    pub fn enabled(&self) -> bool {
        self.out.is_some()
    }

    pub fn client(&mut self, stats: &ClientStats, extra: &BTreeMap<String, f64>) {
        self.write(&Line { t_us: now_micros(), client: Some(stats), host: None, extra });
    }

    pub fn host(&mut self, stats: &HostStats, extra: &BTreeMap<String, f64>) {
        self.write(&Line { t_us: now_micros(), client: None, host: Some(stats), extra });
    }

    fn write(&mut self, line: &Line) {
        let Some(f) = &mut self.out else { return };
        let Ok(mut s) = serde_json::to_string(line) else { return };
        s.push('\n');
        // One write per line: the file is opened for append, so lines from the two sides
        // never interleave mid-line.
        let _ = f.write_all(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_the_netlab_shape() {
        let dir = std::env::temp_dir().join(format!("qsn-netlab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.jsonl");
        let mut f = StatsFile::at(&path).unwrap();
        f.client(&ClientStats { frames_received: 120, capture_to_present_us: 4000, ..Default::default() }, &[("frame_gaps".to_string(), 2.0)].into());
        f.host(&HostStats { fps: 120.0, ..Default::default() }, &BTreeMap::new());
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["client"]["frames_received"], 120);
        assert_eq!(lines[0]["extra"]["frame_gaps"], 2.0);
        assert!(lines[0]["t_us"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["host"]["fps"], 120.0);
        assert!(lines[1].get("extra").is_none() && lines[1].get("client").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!StatsFile { out: None }.enabled());
    }
}
