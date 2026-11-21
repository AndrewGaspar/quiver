//! The stats lines workloads write for the report.
//!
//! A workload under `quiver-netlab run` finds a file in `QUIVER_NETLAB_STATS` (also open on
//! fd `QUIVER_NETLAB_STATS_FD`, 3) and appends one JSON object per line, about once a second
//! or more often:
//!
//! ```json
//! {"t_us": 81234567890, "client": { ...quiver_stream_proto::ClientStats... }, "extra": {"frame_gaps": 1}}
//! {"t_us": 81234568001, "host": { ...quiver_stream_proto::HostStats... }}
//! ```
//!
//! `t_us` is `CLOCK_MONOTONIC` in microseconds ([`quiver_stream_proto::now_micros`]); the
//! namespaces share the clock, so the netlab lines these up with its own timeline. `client`
//! and `host` are the proto crate's types, with the meanings documented there (counts are
//! since the previous line). `extra` takes any further numbers; the report knows these:
//!
//! | key | meaning |
//! |---|---|
//! | `frame_gaps` | presentation gaps in the period: frames shown more than 2.5 frame intervals apart |
//! | `max_gap_ms` | the longest such gap |
//! | `recv_kbps` | the stream's received bitrate |
//! | `latency_p99_us` | the period's p99 capture → present (the median is `capture_to_present_us`) |
//! | `target_kbps` | (host) the rate the controller is aiming at |

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use quiver_stream_proto::{ClientStats, HostStats, Micros, now_micros};
use serde::{Deserialize, Serialize};

/// The environment variable naming the stats file.
pub const ENV_FILE: &str = "QUIVER_NETLAB_STATS";
/// The environment variable naming the fd the stats file is also open on.
pub const ENV_FD: &str = "QUIVER_NETLAB_STATS_FD";
/// The fd the runner opens the stats file on.
pub const STATS_FD: i32 = 3;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsLine {
    pub t_us: Micros,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStats>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, f64>,
}

impl StatsLine {
    pub fn extra(&self, key: &str) -> Option<f64> {
        self.extra.get(key).copied()
    }
}

/// Appends stats lines to the file a run gave this process, or nowhere when run outside
/// the netlab.
pub struct StatsWriter {
    out: Option<std::fs::File>,
}

impl StatsWriter {
    /// The writer for `QUIVER_NETLAB_STATS`, or `path` when given.
    pub fn from_env(path: Option<&Path>) -> std::io::Result<StatsWriter> {
        let path = path.map(Path::to_path_buf).or_else(|| std::env::var_os(ENV_FILE).map(Into::into));
        let out = match path {
            Some(p) => Some(std::fs::OpenOptions::new().create(true).append(true).open(p)?),
            None => None,
        };
        Ok(StatsWriter { out })
    }

    pub fn write(&mut self, line: &StatsLine) {
        if let Some(f) = &mut self.out {
            let mut s = serde_json::to_string(line).unwrap_or_default();
            s.push('\n');
            let _ = f.write_all(s.as_bytes());
        }
    }

    pub fn client(&mut self, stats: ClientStats, extra: BTreeMap<String, f64>) {
        self.write(&StatsLine { t_us: now_micros(), client: Some(stats), host: None, extra });
    }

    pub fn host(&mut self, stats: HostStats, extra: BTreeMap<String, f64>) {
        self.write(&StatsLine { t_us: now_micros(), client: None, host: Some(stats), extra });
    }
}

/// Reads a stats file, skipping lines that don't parse. Returns the lines and the number
/// skipped.
pub fn read(path: &Path) -> (Vec<StatsLine>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else { return (vec![], 0) };
    let mut bad = 0;
    let mut lines = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<StatsLine>(l) {
            Ok(s) => lines.push(s),
            Err(_) => bad += 1,
        }
    }
    lines.sort_by_key(|l| l.t_us);
    (lines, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut w = StatsWriter::from_env(Some(&path)).unwrap();
        w.client(ClientStats { frames_received: 60, ..Default::default() }, [("frame_gaps".to_string(), 2.0)].into());
        w.host(HostStats { fps: 60.0, ..Default::default() }, BTreeMap::new());
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"not json\n").unwrap();
        let (lines, bad) = read(&path);
        assert_eq!(bad, 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].client.as_ref().unwrap().frames_received, 60);
        assert_eq!(lines[0].extra("frame_gaps"), Some(2.0));
        assert_eq!(lines[1].host.as_ref().unwrap().fps, 60.0);
    }
}
