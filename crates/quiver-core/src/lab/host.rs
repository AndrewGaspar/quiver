//! The host's state, logged with every launch and measurement: free memory, load, the GPU
//! and who else is using it. A number measured while something else was rendering isn't
//! comparable, and the log has to say so.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::procs::{self, Proc};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    pub util_percent: Option<f64>,
    pub mem_used_mib: Option<f64>,
    pub mem_total_mib: Option<f64>,
    pub temp_c: Option<f64>,
    pub sm_clock_mhz: Option<f64>,
    pub power_w: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuUser {
    pub pid: u32,
    pub program: String,
    /// `host`, `lab:<instance>` or `container:<name>`.
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostState {
    pub at: DateTime<Utc>,
    pub mem_total_mib: u64,
    pub mem_available_mib: u64,
    pub swap_used_mib: u64,
    pub load: [f64; 3],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
    /// Windows programs and other processes holding the GPU, with their owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_users: Vec<GpuUser>,
}

impl HostState {
    /// One line for logs.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "mem {}/{} MiB free, load {:.2} {:.2} {:.2}",
            self.mem_available_mib, self.mem_total_mib, self.load[0], self.load[1], self.load[2]
        );
        for g in &self.gpus {
            s.push_str(&format!(
                "; gpu {} {}% {}/{} MiB {}°C",
                g.name,
                g.util_percent.map_or("?".into(), |v| format!("{v:.0}")),
                g.mem_used_mib.map_or("?".into(), |v| format!("{v:.0}")),
                g.mem_total_mib.map_or("?".into(), |v| format!("{v:.0}")),
                g.temp_c.map_or("?".into(), |v| format!("{v:.0}")),
            ));
        }
        let others: Vec<String> = self.gpu_users.iter().map(|u| format!("{} ({})", u.program, u.owner)).collect();
        if !others.is_empty() {
            s.push_str(&format!("; gpu users: {}", others.join(", ")));
        }
        s
    }

    /// Other lab instances or containers using the GPU, besides `me` (`lab:<name>`).
    pub fn contenders(&self, me: &str) -> Vec<&GpuUser> {
        self.gpu_users.iter().filter(|u| u.owner != me && u.owner != "host").collect()
    }
}

/// `/proc/meminfo` in MiB: (total, available, swap used).
pub fn parse_meminfo(text: &str) -> (u64, u64, u64) {
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix(':').map(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().unwrap_or(0)))
            .unwrap_or(0)
            / 1024
    };
    (get("MemTotal"), get("MemAvailable"), get("SwapTotal").saturating_sub(get("SwapFree")))
}

pub fn parse_loadavg(text: &str) -> [f64; 3] {
    let mut it = text.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
}

/// The query `nvidia-smi` is run with.
pub const NVIDIA_SMI_QUERY: &str = "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,clocks.sm,power.draw";

/// `nvidia-smi --query-gpu=… --format=csv,noheader,nounits` lines.
pub fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            Gpu {
                name: f.first().copied().unwrap_or_default().to_owned(),
                util_percent: num(1),
                mem_used_mib: num(2),
                mem_total_mib: num(3),
                temp_c: num(4),
                sm_clock_mhz: num(5),
                power_w: num(6),
            }
        })
        .collect()
}

/// Take a snapshot. `containers` maps libpod ids to names (`podman ps`), for owners.
pub fn snapshot(procs: &[Proc], containers: &BTreeMap<String, String>) -> HostState {
    let (mem_total_mib, mem_available_mib, swap_used_mib) = parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
    let load = parse_loadavg(&std::fs::read_to_string("/proc/loadavg").unwrap_or_default());
    let gpus = std::process::Command::new("nvidia-smi")
        .args([NVIDIA_SMI_QUERY, "--format=csv,noheader,nounits"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_nvidia_smi(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    // Only programs worth naming: Windows programs and gamescope, not every desktop app.
    let candidates: Vec<&Proc> = procs.iter().filter(|p| p.is_windows() && !p.is_wine_service() || p.comm == "gamescope").collect();
    let pids: Vec<u32> = candidates.iter().map(|p| p.pid).collect();
    let on_gpu = procs::gpu_pids_in(Path::new("/proc"), &pids);
    let gpu_users = candidates
        .iter()
        .filter(|p| on_gpu.contains(&p.pid))
        .map(|p| GpuUser { pid: p.pid, program: p.program().to_owned(), owner: procs::owner(p, containers) })
        .collect();
    HostState { at: Utc::now(), mem_total_mib, mem_available_mib, swap_used_mib, load, gpus, gpu_users }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let mi = "MemTotal:       65536000 kB\nMemFree:  1 kB\nMemAvailable:   32768000 kB\nSwapTotal: 2048 kB\nSwapFree: 1024 kB\n";
        assert_eq!(parse_meminfo(mi), (64000, 32000, 1));
        assert_eq!(parse_loadavg("1.50 0.75 0.25 2/1000 1234"), [1.5, 0.75, 0.25]);
        let g = parse_nvidia_smi("NVIDIA GeForce RTX 4090, 37, 10240, 24564, 61, 2520, 250.13\n");
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].name.as_str(), g[0].util_percent, g[0].power_w), ("NVIDIA GeForce RTX 4090", Some(37.0), Some(250.13)));
        assert_eq!(parse_nvidia_smi("X, [N/A], 1, 2, 3, 4, [N/A]")[0].util_percent, None);
    }

    #[test]
    fn summaries_name_contention() {
        let h = HostState {
            at: Utc::now(),
            mem_total_mib: 64000,
            mem_available_mib: 30000,
            swap_used_mib: 0,
            load: [1.0, 1.0, 1.0],
            gpus: vec![],
            gpu_users: vec![
                GpuUser { pid: 1, program: "ACOrigins.exe".into(), owner: "lab:t1".into() },
                GpuUser { pid: 2, program: "ACOrigins.exe".into(), owner: "lab:t2".into() },
                GpuUser { pid: 3, program: "gamescope".into(), owner: "host".into() },
            ],
        };
        assert_eq!(h.contenders("lab:t1").len(), 1);
        let s = h.summary();
        assert!(s.contains("ACOrigins.exe (lab:t2)") && s.contains("gamescope (host)"), "{s}");
    }
}
