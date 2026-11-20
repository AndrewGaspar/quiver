//! Lab limits (design/LAB.md §15): how much of the machine lab instances may use, set by the
//! person (`quiver lab limits`, `config.json` → `lab.limits`) and enforced quietly by each
//! instance's supervisor. Only lab instances are limited; the person's own games are never
//! stopped, slowed or touched.
//!
//! Two profiles: `always`, and `while_playing`, which applies (on top of `always`, the
//! stricter of the two per field) while one of the person's own games runs, launched by
//! Quiver or through Steam (`quiver steam-run`), as quiverd tracks them (`Request::Sessions`
//! holders `launcher` or `steam`). Lab runs (`lab`) and other bridge sessions (`bridge`:
//! containers such as a mod harness) aren't the person playing.
//!
//! Each profile has CPU cores (whole cores, both SMT threads, the slowest cores first),
//! a CPU quota, memory high/max, and a GPU share. How each is enforced:
//!
//! - **CPU quota, memory** go into the container's own cgroup (`cpu.max`, `memory.high`,
//!   `memory.max`), which systemd delegates to the person (controllers `cpu memory pids`).
//!   `podman update` can set these but can't lift them again (`--cpus 0` and `--memory 0`
//!   leave the old values), so the supervisor writes the same files itself.
//! - **CPU cores**: `cpuset` isn't delegated, so `podman run/update --cpuset-cpus` fail
//!   (runc: `openat2 …/cpuset.cpus: no such file or directory`). The cores are enforced as
//!   CPU affinity of every thread in the container, re-applied every couple of seconds (runc
//!   resets an exec'd process's affinity to all CPUs, and a game may set its own), with a
//!   quota of the same number of CPUs in `cpu.max` as the hard ceiling in between.
//! - **GPU share**: no cgroup controller covers NVIDIA graphics. The Sight layer in the lab
//!   game creates its devices at low queue priority and holds presents back by the interval
//!   the supervisor's governor sets from the instance's measured GPU use (NVML per-process
//!   utilization), so it stays under the share ([`Governor`], [`quiver_perf::labgpu`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// For tests and experiments: a JSON file with the limits (a [`LabLimits`]) used instead of
/// `config.json`'s, read and written by `quiver lab limits` and every supervisor.
pub const ENV_FILE: &str = "QUIVER_LAB_LIMITS";
/// Where a supervisor says what it enforces (`<instance>/limits.json`).
pub const STATUS_FILE: &str = "limits.json";

/// `config.json` → `lab`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LabSettings {
    #[serde(default)]
    pub limits: LabLimits,
}

impl LabSettings {
    pub fn is_default(&self) -> bool {
        *self == LabSettings::default()
    }
}

/// A size in bytes, written `16G`, `512M`, `1048576`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

impl Bytes {
    pub fn parse(s: &str) -> Result<Bytes> {
        let t = s.trim();
        let (num, mul) = match t.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
            Some((i, _)) => {
                let unit = t[i..].to_ascii_lowercase();
                let mul: u64 = match unit.trim_end_matches("ib").trim_end_matches('b') {
                    "" => 1,
                    "k" => 1 << 10,
                    "m" => 1 << 20,
                    "g" => 1 << 30,
                    "t" => 1 << 40,
                    _ => bail!("{s:?}: a size like 16G, 512M or 1048576"),
                };
                (&t[..i], mul)
            }
            None => (t, 1),
        };
        let v: f64 = num.trim().parse().with_context(|| format!("{s:?}: a size like 16G, 512M or 1048576"))?;
        if !(v > 0.0) || !v.is_finite() {
            bail!("{s:?}: a size above zero");
        }
        Ok(Bytes((v * mul as f64) as u64))
    }
}

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.0;
        for (u, m) in [("T", 1u64 << 40), ("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
            if v >= m && v % m == 0 {
                return write!(f, "{}{u}", v / m);
            }
        }
        for (u, m) in [("T", 1u64 << 40), ("G", 1 << 30), ("M", 1 << 20)] {
            if v >= m {
                return write!(f, "{:.1}{u}", v as f64 / m as f64);
            }
        }
        write!(f, "{v}")
    }
}

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Bytes, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(u64),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(Bytes(n)),
            Raw::S(s) => Bytes::parse(&s).map_err(serde::de::Error::custom),
        }
    }
}

/// One profile's limits; `None` is no limit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Whole physical cores (both SMT threads of each), the slowest cores first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// CPU time, in CPUs (hardware threads): `cpu.max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<f64>,
    /// Above this the container's memory is reclaimed hard (`memory.high`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high: Option<Bytes>,
    /// The container's processes are OOM-killed above this (`memory.max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<Bytes>,
    /// The lab's share of the GPU, percent (1–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_percent: Option<u32>,
}

fn min_opt<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y < x { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

impl Profile {
    pub fn is_empty(&self) -> bool {
        *self == Profile::default()
    }

    /// Per field, the stricter of the two.
    pub fn stricter(&self, o: &Profile) -> Profile {
        Profile {
            cpu_cores: min_opt(self.cpu_cores, o.cpu_cores),
            cpu_quota: min_opt(self.cpu_quota, o.cpu_quota),
            memory_high: min_opt(self.memory_high, o.memory_high),
            memory_max: min_opt(self.memory_max, o.memory_max),
            gpu_percent: min_opt(self.gpu_percent, o.gpu_percent),
        }
    }

    /// `cpu 6 cores, gpu ≤20%`, or `none`.
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        if let Some(c) = self.cpu_cores {
            v.push(format!("cpu {c} core{}", if c == 1 { "" } else { "s" }));
        }
        if let Some(q) = self.cpu_quota {
            v.push(format!("cpu quota {} CPUs", fmt_num(q)));
        }
        if let Some(m) = self.memory_high {
            v.push(format!("memory high {m}"));
        }
        if let Some(m) = self.memory_max {
            v.push(format!("memory max {m}"));
        }
        if let Some(g) = self.gpu_percent {
            v.push(format!("gpu ≤{g}%"));
        }
        if v.is_empty() { "none".into() } else { v.join(", ") }
    }

    /// One `key=value` change (`cpu=6`, `cpu_quota=4.5`, `memory_high=16G`, `memory_max=off`,
    /// `gpu=20`, `gpu=20%`; `off`/`none` lifts it).
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        let off = matches!(v, "off" | "none" | "" | "-");
        match key {
            "cpu" | "cpu_cores" | "cores" => {
                self.cpu_cores = if off {
                    None
                } else {
                    let n: u32 = v.trim_end_matches("cores").trim_end_matches("core").trim().parse().with_context(|| format!("{key}: a number of cores"))?;
                    if n == 0 {
                        bail!("{key}: at least 1 core (or off)");
                    }
                    Some(n)
                }
            }
            "cpu_quota" | "cpus" | "quota" => {
                self.cpu_quota = if off {
                    None
                } else {
                    let q: f64 = v.parse().with_context(|| format!("{key}: CPUs, e.g. 4 or 2.5"))?;
                    if !(q > 0.0) || !q.is_finite() {
                        bail!("{key}: above zero (or off)");
                    }
                    Some(q)
                }
            }
            "memory_high" | "mem_high" => self.memory_high = if off { None } else { Some(Bytes::parse(v)?) },
            "memory_max" | "mem_max" | "memory" => self.memory_max = if off { None } else { Some(Bytes::parse(v)?) },
            "gpu" | "gpu_percent" => {
                self.gpu_percent = if off {
                    None
                } else {
                    let g: u32 = v.trim_end_matches('%').trim().parse().with_context(|| format!("{key}: a percentage, e.g. 20"))?;
                    if !(1..=100).contains(&g) {
                        bail!("{key}: 1 to 100 percent (or off)");
                    }
                    (g < 100).then_some(g)
                }
            }
            _ => bail!("unknown limit '{key}' (cpu, cpu_quota, memory_high, memory_max, gpu)"),
        }
        Ok(())
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v}") }
}

/// The two profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabLimits {
    #[serde(default)]
    pub always: Profile,
    #[serde(default = "default_while_playing")]
    pub while_playing: Profile,
}

/// While the person plays: a fifth of the GPU and 6 cores.
pub fn default_while_playing() -> Profile {
    Profile { cpu_cores: Some(6), gpu_percent: Some(20), ..Default::default() }
}

impl Default for LabLimits {
    fn default() -> Self {
        LabLimits { always: Profile::default(), while_playing: default_while_playing() }
    }
}

/// Which profile is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Which {
    None,
    Always,
    WhilePlaying,
}

impl Which {
    pub fn label(self) -> &'static str {
        match self {
            Which::None => "no limits",
            Which::Always => "always",
            Which::WhilePlaying => "while playing",
        }
    }
}

impl LabLimits {
    /// What applies now, and which profile it comes from.
    pub fn effective(&self, playing: bool) -> (Which, Profile) {
        let p = if playing { self.always.stricter(&self.while_playing) } else { self.always.clone() };
        let w = match (p.is_empty(), playing && !self.while_playing.is_empty()) {
            (true, _) => Which::None,
            (false, true) => Which::WhilePlaying,
            (false, false) => Which::Always,
        };
        (w, p)
    }

    /// Either profile has a GPU share: lab games then get the layer's GPU page at every launch
    /// (low priority from the start; the governor paces only while a share applies).
    pub fn any_gpu(&self) -> bool {
        self.always.gpu_percent.is_some() || self.while_playing.gpu_percent.is_some()
    }

    pub fn profile_mut(&mut self, name: &str) -> Result<&mut Profile> {
        match name {
            "always" => Ok(&mut self.always),
            "while_playing" | "while-playing" | "playing" => Ok(&mut self.while_playing),
            _ => bail!("no profile '{name}' (always, while_playing)"),
        }
    }

    /// The limits in force for the lab, and where they came from: `$QUIVER_LAB_LIMITS`, else
    /// `config.json` → `lab.limits` (missing: the defaults). A broken file gives the defaults
    /// and says why.
    pub fn load() -> (LabLimits, String, Option<String>) {
        if let Some(p) = std::env::var_os(ENV_FILE).filter(|p| !p.is_empty()) {
            let p = PathBuf::from(p);
            return match std::fs::read_to_string(&p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (LabLimits::default(), p.display().to_string(), None),
                Err(e) => (LabLimits::default(), p.display().to_string(), Some(format!("{}: {e}", p.display()))),
                Ok(t) => match serde_json::from_str(&t) {
                    Ok(l) => (l, p.display().to_string(), None),
                    Err(e) => (LabLimits::default(), p.display().to_string(), Some(format!("{}: {e} (using the defaults)", p.display()))),
                },
            };
        }
        let path = crate::launcher::Config::path();
        let src = format!("{} (lab.limits)", path.display());
        let Ok(text) = std::fs::read_to_string(&path) else { return (LabLimits::default(), src, None) };
        match from_config_json(&text) {
            Ok(l) => (l, src, None),
            Err(e) => (LabLimits::default(), src, Some(format!("{e:#} (using the defaults)"))),
        }
    }

    /// Writes them where [`LabLimits::load`] reads them. Returns that place.
    pub fn save(&self) -> Result<String> {
        if let Some(p) = std::env::var_os(ENV_FILE).filter(|p| !p.is_empty()) {
            let p = PathBuf::from(p);
            super::write_atomic(&p, &serde_json::to_vec_pretty(self)?)?;
            return Ok(p.display().to_string());
        }
        let mut cfg = crate::launcher::Config::load()?;
        cfg.lab.limits = self.clone();
        cfg.save()?;
        Ok(format!("{} (lab.limits)", crate::launcher::Config::path().display()))
    }
}

/// `lab.limits` from `config.json`'s text (missing: the defaults).
pub fn from_config_json(text: &str) -> Result<LabLimits> {
    let v: serde_json::Value = serde_json::from_str(text).context("config.json")?;
    match v.get("lab").and_then(|l| l.get("limits")) {
        None => Ok(LabLimits::default()),
        Some(l) => serde_json::from_value(l.clone()).context("config.json lab.limits"),
    }
}

// ---- The person playing.

/// The person's own games among quiverd's sessions: held by Quiver's launcher or by Steam
/// (`quiver steam-run`). Lab runs and other bridge sessions (containers) aren't.
pub fn playing_games(sessions: &[quiver_api::ActiveSession]) -> Vec<quiver_api::GameRef> {
    let mut out = Vec::new();
    for s in sessions {
        if s.holders.iter().any(|h| h == crate::playsession::LAUNCHER_HOLDER || h == crate::playsession::STEAM_HOLDER) {
            let g = s.game_ref();
            if !out.contains(&g) {
                out.push(g);
            }
        }
    }
    out
}

/// What the supervisor knows about the person playing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Playing {
    /// quiverd couldn't be asked: `while_playing` doesn't apply.
    Unknown { why: String },
    No,
    Yes { games: Vec<String> },
}

impl Playing {
    pub fn is_yes(&self) -> bool {
        matches!(self, Playing::Yes { .. })
    }
    /// `Alan Wake Remastered is running`.
    pub fn describe(&self) -> String {
        match self {
            Playing::Unknown { why } => format!("whether you're playing is unknown ({why})"),
            Playing::No => "none of your games is running".into(),
            Playing::Yes { games } => match games.as_slice() {
                [] => "one of your games is running".into(),
                [g] => format!("{g} is running"),
                gs => format!("{} are running", gs.join(", ")),
            },
        }
    }
}

// ---- CPUs.

/// One physical core: its hardware threads and how fast it is (ACPI CPPC's highest perf, else
/// the max frequency; higher is faster).
#[derive(Debug, Clone, PartialEq)]
pub struct Core {
    pub threads: Vec<u32>,
    pub perf: u64,
}

/// The online CPUs' cores from sysfs (`/sys/devices/system/cpu`).
pub fn topology(sys_cpu: &Path) -> Vec<Core> {
    let online = std::fs::read_to_string(sys_cpu.join("online")).ok().map(|s| parse_cpulist(&s)).unwrap_or_default();
    let mut seen: BTreeSet<Vec<u32>> = BTreeSet::new();
    let mut out = Vec::new();
    for cpu in online {
        let d = sys_cpu.join(format!("cpu{cpu}"));
        let mut threads = std::fs::read_to_string(d.join("topology/thread_siblings_list")).map(|s| parse_cpulist(&s)).unwrap_or_else(|_| vec![cpu]);
        threads.sort_unstable();
        if !seen.insert(threads.clone()) {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(d.join(f)).ok().and_then(|s| s.trim().parse::<u64>().ok());
        let perf = read("acpi_cppc/highest_perf").or_else(|| read("cpufreq/cpuinfo_max_freq")).unwrap_or(0);
        out.push(Core { threads, perf });
    }
    out
}

/// `0-3,8,10-11` → [0,1,2,3,8,10,11].
pub fn parse_cpulist(s: &str) -> Vec<u32> {
    let mut v = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    v.extend(a..=b.min(a + 4096));
                }
            }
            None => {
                if let Ok(a) = part.trim().parse() {
                    v.push(a);
                }
            }
        }
    }
    v
}

/// [0,1,2,3,8,10,11] → `0-3,8,10-11`.
pub fn format_cpulist(cpus: &[u32]) -> String {
    let mut c = cpus.to_vec();
    c.sort_unstable();
    c.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let start = c[i];
        let mut end = start;
        while i + 1 < c.len() && c[i + 1] == end + 1 {
            i += 1;
            end = c[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    parts.join(",")
}

/// `n` whole cores for the lab: the slowest first (on a hybrid CPU, the compact cores, leaving
/// the fast ones to the person's game), the highest-numbered among equals (cpu 0 and its
/// neighbours carry the system's interrupts). Both SMT threads of each core, never one. `None`
/// when `n` covers every core (nothing to restrict).
pub fn pick_cores(cores: &[Core], n: u32) -> Option<Vec<u32>> {
    if n as usize >= cores.len() {
        return None;
    }
    let mut order: Vec<&Core> = cores.iter().collect();
    order.sort_by(|a, b| a.perf.cmp(&b.perf).then(b.threads.first().cmp(&a.threads.first())));
    let mut threads: Vec<u32> = order.iter().take(n as usize).flat_map(|c| c.threads.iter().copied()).collect();
    threads.sort_unstable();
    Some(threads)
}

/// What a profile becomes on this machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The CPUs the container's threads may run on (affinity), when restricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<Vec<u32>>,
    /// `cpu.max` in CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high: Option<Bytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<Bytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_percent: Option<u32>,
}

impl Plan {
    pub fn new(p: &Profile, cores: &[Core]) -> Plan {
        let cpus = p.cpu_cores.and_then(|n| pick_cores(cores, n));
        // The affinity is re-applied every few seconds; between, a quota of as many CPUs as it
        // allows is the hard ceiling.
        let implied = cpus.as_ref().map(|c| c.len() as f64);
        Plan { cpus, cpu_max: min_opt(p.cpu_quota, implied), memory_high: p.memory_high, memory_max: p.memory_max, gpu_percent: p.gpu_percent }
    }

    /// `cpu.max`'s line: `<quota> <period>` or `max <period>`.
    pub fn cpu_max_line(&self) -> String {
        const PERIOD: u64 = 100_000;
        match self.cpu_max {
            Some(c) => format!("{} {PERIOD}", ((c * PERIOD as f64).round() as u64).max(1000)),
            None => format!("max {PERIOD}"),
        }
    }
}

// ---- The container's cgroup.

/// A container's cgroup folder: `/sys/fs/cgroup` + podman's `State.CgroupPath`.
pub fn cgroup_dir(cgroup_path: &str) -> PathBuf {
    Path::new("/sys/fs/cgroup").join(cgroup_path.trim().trim_start_matches('/'))
}

/// Writes the plan's CPU quota and memory limits into the container's cgroup, where they
/// differ. Returns what changed (`cpu.max 1200000 100000`).
pub fn apply_cgroup(dir: &Path, plan: &Plan) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    let mut put = |file: &str, want: String| -> Result<()> {
        let p = dir.join(file);
        let now = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        if cgroup_value_differs(file, now.trim(), &want) {
            std::fs::write(&p, &want).with_context(|| format!("write {want:?} to {}", p.display()))?;
            changed.push(format!("{file} {want}"));
        }
        Ok(())
    };
    put("cpu.max", plan.cpu_max_line())?;
    put("memory.high", plan.memory_high.map_or("max".into(), |b| b.0.to_string()))?;
    put("memory.max", plan.memory_max.map_or("max".into(), |b| b.0.to_string()))?;
    Ok(changed)
}

/// Memory values are rounded to pages by the kernel: compare in pages.
fn cgroup_value_differs(file: &str, now: &str, want: &str) -> bool {
    if file.starts_with("memory.")
        && let (Ok(a), Ok(b)) = (now.parse::<u64>(), want.parse::<u64>())
    {
        return a / 4096 != b / 4096;
    }
    now != want
}

/// The threads of the processes in a cgroup (`cgroup.procs`, then `/proc/<pid>/task`).
pub fn cgroup_threads(dir: &Path) -> Vec<u32> {
    let mut out = Vec::new();
    for pid in std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
        match std::fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(rd) => out.extend(rd.flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())),
            Err(_) => out.push(pid),
        }
    }
    out
}

/// The processes in a cgroup.
pub fn cgroup_pids(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Puts every thread on `cpus` (`None`: every online CPU), where its affinity differs.
/// Returns (threads changed, threads that refused; a thread that exited meanwhile is neither).
pub fn apply_affinity(threads: &[u32], cpus: Option<&[u32]>, online: &[u32]) -> (usize, usize) {
    let want: &[u32] = cpus.unwrap_or(online);
    if want.is_empty() {
        return (0, 0);
    }
    // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET with ids below CPU_SETSIZE.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &c in want {
        if (c as usize) < libc::CPU_SETSIZE as usize {
            unsafe { libc::CPU_SET(c as usize, &mut set) };
        }
    }
    let (mut changed, mut failed) = (0, 0);
    for &t in threads {
        // SAFETY: as above; sched_getaffinity/sched_setaffinity on a tid with a set we own.
        let mut cur: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let got = unsafe { libc::sched_getaffinity(t as libc::pid_t, std::mem::size_of::<libc::cpu_set_t>(), &mut cur) };
        if got != 0 {
            continue; // gone
        }
        if unsafe { libc::CPU_EQUAL(&cur, &set) } {
            continue;
        }
        if unsafe { libc::sched_setaffinity(t as libc::pid_t, std::mem::size_of::<libc::cpu_set_t>(), &set) } == 0 {
            changed += 1;
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            failed += 1;
        }
    }
    (changed, failed)
}

/// The online CPUs.
pub fn online_cpus() -> Vec<u32> {
    std::fs::read_to_string("/sys/devices/system/cpu/online").map(|s| parse_cpulist(&s)).unwrap_or_default()
}

// ---- The GPU governor.

/// The governor's view of one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuSample {
    /// This instance's processes' share of the GPU over the last tick, percent (NVML).
    pub own: f64,
    /// Other lab instances' share, percent.
    pub others: f64,
    /// Lab instances using the GPU, this one included.
    pub instances: u32,
    /// This instance's presents per second over the tick (the layer's count).
    pub fps: f64,
}

/// Holds an instance's GPU use under its part of the lab's share by capping its present rate.
///
/// Its part is `max(share / instances, share - others)`: an even split, and whatever the other
/// instances leave. Each tick it compares its smoothed use with that: over, the cap comes down
/// to the rate that would have used exactly that (use scales with frames); well under, the cap
/// goes up by at most 30% a tick, and goes away when the game no longer reaches it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Governor {
    pub share: f64,
    /// Presents per second the game is held to (`None`: free).
    pub cap: Option<f64>,
    /// Smoothed own use, percent.
    pub own: Option<f64>,
    pub target: f64,
}

/// The slowest the governor ever holds a game to.
pub const MIN_FPS: f64 = 2.0;

impl Governor {
    pub fn new(share: f64) -> Governor {
        Governor { share, target: share, ..Default::default() }
    }

    /// One tick (about a second); returns the present interval for the page, ns (0: none).
    pub fn step(&mut self, s: GpuSample) -> u64 {
        let n = s.instances.max(1) as f64;
        self.target = (self.share / n).max(self.share - s.others).clamp(0.5, self.share);
        // NVML's per-process samples are noisy (each over 0.2-0.8 s): smooth them.
        let own = match self.own {
            Some(o) => 0.6 * o + 0.4 * s.own,
            None => s.own,
        };
        self.own = Some(own);
        let t = self.target;
        if s.fps < 0.5 {
            return self.interval();
        }
        self.cap = match self.cap {
            // Free and over: the rate that would have used exactly `t` (use scales with frames).
            None if own > t * 1.05 => {
                self.own = Some(t);
                Some((s.fps * t / own).max(MIN_FPS))
            }
            None => None,
            Some(c) => {
                // Proportional in log terms, at most -25%/+25% a tick, so one noisy sample
                // doesn't throw the rate around and the mean settles on the target.
                let next = c * (t / own.max(0.05)).sqrt().clamp(0.75, 1.25);
                if (own < t * 0.5 && s.fps < c * 0.6) || next > 2000.0 {
                    None // the game no longer reaches the cap: something else holds it back
                } else {
                    Some(next.max(MIN_FPS))
                }
            }
        };
        self.interval()
    }

    fn interval(&self) -> u64 {
        self.cap.map_or(0, |c| (1e9 / c) as u64)
    }
}

// ---- What a supervisor enforces.

/// The GPU part of [`Status`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuStatus {
    pub percent: u32,
    /// This instance's part of it now.
    pub target: f64,
    /// `priority low`, `priority: the driver has no global priority`, … (the layer's word).
    pub priority: String,
    /// Measured share of this instance, percent (smoothed), while a run is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    /// The cap on presents per second the governor holds, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_fps: Option<f64>,
    /// Presents per second over the last tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    /// Why the GPU part isn't working, if it isn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `<instance>/limits.json`, rewritten by the supervisor on every change and every few seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub at: DateTime<Utc>,
    pub source: String,
    pub limits: LabLimits,
    pub playing: Playing,
    pub which: Which,
    pub effective: Profile,
    pub plan: Plan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuStatus>,
    /// Problems enforcing (a cgroup write refused, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Older than this, a status file's supervisor is gone.
pub const STALE_SECS: i64 = 15;

impl Status {
    pub fn read(inst_dir: &Path) -> Option<Status> {
        serde_json::from_slice(&std::fs::read(inst_dir.join(STATUS_FILE)).ok()?).ok()
    }
    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        (now - self.at).num_seconds() < STALE_SECS
    }
    /// Anything is limited now.
    pub fn active(&self) -> bool {
        self.which != Which::None
    }

    /// What is enforced right now, with how: `cpu 6 cores (cpus 6-11,18-23), gpu ≤20%
    /// (priority low, governor holding 18%)`.
    pub fn enforced(&self) -> String {
        let p = &self.effective;
        let mut v = Vec::new();
        if let Some(c) = p.cpu_cores {
            match &self.plan.cpus {
                Some(cpus) => v.push(format!("cpu {c} cores (cpus {})", format_cpulist(cpus))),
                None => v.push(format!("cpu {c} cores (all of them: nothing to restrict)")),
            }
        }
        if let Some(q) = self.plan.cpu_max {
            if p.cpu_quota.is_some() || p.cpu_cores.is_none() {
                v.push(format!("cpu quota {} CPUs", fmt_num(q)));
            }
        }
        if let Some(m) = p.memory_high {
            v.push(format!("memory high {m}"));
        }
        if let Some(m) = p.memory_max {
            v.push(format!("memory max {m}"));
        }
        if let Some(g) = p.gpu_percent {
            let how = match &self.gpu {
                None => "governor not started".to_owned(),
                Some(gs) => {
                    let mut h = vec![gs.priority.clone()];
                    match (gs.measured, gs.cap_fps) {
                        (Some(m), Some(c)) => h.push(format!("governor holding {m:.0}% at ≤{c:.0} fps")),
                        (Some(m), None) => h.push(format!("using {m:.0}%, not held back")),
                        (None, _) => h.push("no run on the GPU".into()),
                    }
                    if let Some(n) = &gs.note {
                        h.push(n.clone());
                    }
                    if (gs.target - g as f64).abs() > 0.5 {
                        h.push(format!("this instance's part {:.0}%", gs.target));
                    }
                    h.join(", ")
                }
            };
            v.push(format!("gpu ≤{g}% ({how})"));
        }
        if v.is_empty() { "nothing".into() } else { v.join(", ") }
    }

    /// The one line: `while playing: Alan Wake Remastered is running → cpu 6 cores …`.
    pub fn line(&self) -> String {
        match self.which {
            Which::None => format!("lab limits: none in force ({})", self.playing.describe()),
            Which::Always if !self.limits.while_playing.is_empty() => format!("lab limits: always → {} ({})", self.enforced(), self.playing.describe()),
            Which::Always => format!("lab limits: always → {}", self.enforced()),
            Which::WhilePlaying => format!("lab limits: while playing: {} → {}", self.playing.describe(), self.enforced()),
        }
    }
}

/// What agents are told when a limit is in force (design/LAB.md §15): wall-clock performance
/// is throttled, fixed-step virtual-clock results aren't. `virtual_clock`: the run has it on.
pub fn throttle_note(st: &Status, virtual_clock: bool) -> Option<String> {
    if !st.active() {
        return None;
    }
    Some(if virtual_clock {
        format!(
            "THROTTLED: lab limits in force ({}). Wall-clock speed is held back; this run's fixed-step virtual-clock results (frame k at k/HZ s) are unaffected.",
            st.effective.describe()
        )
    } else {
        format!(
            "THROTTLED: lab limits in force ({}). Wall-clock frame rates and timings here measure the limit, not the game. Fixed-step virtual-clock results are unaffected: use clock=virtual:HZ (quiver lab launch N --clock virtual:62.5) for numbers that don't depend on the machine.",
            st.effective.describe()
        )
    })
}

/// The label perf numbers carry under a limit without the virtual clock.
pub const THROTTLED_LABEL: &str = "throttled (lab limits in force, wall clock)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(Bytes::parse("16G").unwrap(), Bytes(16 << 30));
        assert_eq!(Bytes::parse("512MiB").unwrap(), Bytes(512 << 20));
        assert_eq!(Bytes::parse("1.5g").unwrap(), Bytes(3 << 29));
        assert_eq!(Bytes::parse("4096").unwrap(), Bytes(4096));
        assert!(Bytes::parse("lots").is_err() && Bytes::parse("0").is_err() && Bytes::parse("-1G").is_err());
        assert_eq!(Bytes(16 << 30).to_string(), "16G");
        assert_eq!(Bytes(3 << 29).to_string(), "1536M");
        let j: Profile = serde_json::from_str(r#"{"memory_max": "8G", "memory_high": 1048576}"#).unwrap();
        assert_eq!((j.memory_max, j.memory_high), (Some(Bytes(8 << 30)), Some(Bytes(1 << 20))));
        assert_eq!(serde_json::to_value(&j).unwrap()["memory_max"], "8G");
    }

    #[test]
    fn defaults_and_profiles() {
        let l = LabLimits::default();
        assert!(l.always.is_empty());
        assert_eq!(l.while_playing, Profile { cpu_cores: Some(6), gpu_percent: Some(20), ..Default::default() });
        assert_eq!(l.effective(false), (Which::None, Profile::default()));
        assert_eq!(l.effective(true).0, Which::WhilePlaying);
        assert!(l.any_gpu());
        // `always` and `while_playing` together: the stricter per field.
        let mut l = l;
        l.always.set("gpu", "50%").unwrap();
        l.always.set("cpu", "8").unwrap();
        l.always.set("memory_max", "24G").unwrap();
        let (w, p) = l.effective(true);
        assert_eq!(w, Which::WhilePlaying);
        assert_eq!((p.cpu_cores, p.gpu_percent, p.memory_max), (Some(6), Some(20), Some(Bytes(24 << 30))));
        assert_eq!(l.effective(false), (Which::Always, l.always.clone()));
        // Off lifts; 100% is no limit; bad values refused.
        l.always.set("gpu", "off").unwrap();
        assert_eq!(l.always.gpu_percent, None);
        l.always.set("gpu", "100").unwrap();
        assert_eq!(l.always.gpu_percent, None);
        assert!(l.always.set("gpu", "0").is_err() && l.always.set("cpu", "0").is_err() && l.always.set("disk", "1").is_err());
        assert!(l.profile_mut("sometimes").is_err());
        assert_eq!(default_while_playing().describe(), "cpu 6 cores, gpu ≤20%");
        // An empty while_playing: never "while playing".
        let l = LabLimits { always: Profile { cpu_quota: Some(4.0), ..Default::default() }, while_playing: Profile::default() };
        assert_eq!(l.effective(true).0, Which::Always);
    }

    #[test]
    fn config_json() {
        assert_eq!(from_config_json(r#"{"library": "/x"}"#).unwrap(), LabLimits::default());
        let l = from_config_json(r#"{"lab": {"limits": {"always": {"cpu_quota": 8}, "while_playing": {"gpu_percent": 10}}}}"#).unwrap();
        assert_eq!((l.always.cpu_quota, l.while_playing.gpu_percent, l.while_playing.cpu_cores), (Some(8.0), Some(10), None));
        // An explicit empty while_playing stays empty (not the defaults).
        let l = from_config_json(r#"{"lab": {"limits": {"while_playing": {}}}}"#).unwrap();
        assert!(l.while_playing.is_empty());
        assert!(from_config_json(r#"{"lab": {"limits": {"always": {"disk": 1}}}}"#).is_err());
        // In Config: written only when set.
        let s = LabSettings::default();
        assert!(s.is_default());
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpulist("0-3,8,10-11\n"), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(format_cpulist(&[11, 10, 8, 0, 1, 2, 3]), "0-3,8,10-11");
        assert_eq!(format_cpulist(&[]), "");
    }

    /// The Ryzen AI 9 HX 370: 4 Zen 5 cores (cpus 0-3 + 12-15, CPPC 196-208) and 8 Zen 5c
    /// (4-11 + 16-23, CPPC 125).
    fn hx370() -> Vec<Core> {
        let mut v = Vec::new();
        for c in 0..12u32 {
            let perf = if c < 4 { [196, 202, 208, 208][c as usize] } else { 125 };
            v.push(Core { threads: vec![c, c + 12], perf });
        }
        v
    }

    #[test]
    fn picks_whole_slow_cores() {
        let cores = hx370();
        let six = pick_cores(&cores, 6).unwrap();
        assert_eq!(format_cpulist(&six), "6-11,18-23", "the six highest-numbered compact cores, both threads");
        let ten = pick_cores(&cores, 10).unwrap();
        assert_eq!(format_cpulist(&ten), "0-1,4-13,16-23", "every compact core, then the slowest fast ones (CPPC 196, 202)");
        assert_eq!(pick_cores(&cores, 12), None);
    }

    #[test]
    fn plans() {
        let cores = hx370();
        let p = Plan::new(&default_while_playing(), &cores);
        assert_eq!(p.cpus.as_ref().map(|c| format_cpulist(c)).as_deref(), Some("6-11,18-23"));
        assert_eq!(p.cpu_max, Some(12.0), "a quota of the threads allowed, the ceiling between affinity passes");
        assert_eq!(p.cpu_max_line(), "1200000 100000");
        assert_eq!(Plan::new(&Profile { cpu_cores: Some(6), cpu_quota: Some(4.0), ..Default::default() }, &cores).cpu_max, Some(4.0));
        assert_eq!(Plan::new(&Profile { cpu_cores: Some(12), ..Default::default() }, &cores).cpus, None, "all cores: no affinity");
        assert_eq!(Plan::default().cpu_max_line(), "max 100000");
    }

    #[test]
    fn cgroup_files() {
        let d = tempfile::tempdir().unwrap();
        for (f, v) in [("cpu.max", "max 100000\n"), ("memory.high", "max\n"), ("memory.max", "max\n")] {
            std::fs::write(d.path().join(f), v).unwrap();
        }
        let plan = Plan { cpu_max: Some(12.0), memory_high: Some(Bytes(8 << 30)), ..Default::default() };
        let ch = apply_cgroup(d.path(), &plan).unwrap();
        assert_eq!(ch, vec!["cpu.max 1200000 100000".to_owned(), format!("memory.high {}", 8u64 << 30)]);
        assert_eq!(std::fs::read_to_string(d.path().join("cpu.max")).unwrap(), "1200000 100000");
        // The same again: nothing to write. Lifting writes max.
        std::fs::write(d.path().join("memory.high"), format!("{}\n", 8u64 << 30)).unwrap();
        assert!(apply_cgroup(d.path(), &plan).unwrap().is_empty());
        let ch = apply_cgroup(d.path(), &Plan::default()).unwrap();
        assert_eq!(ch, vec!["cpu.max max 100000".to_owned(), "memory.high max".to_owned()]);
        assert!(!cgroup_value_differs("memory.max", "8589934592", "8589934593"), "pages");
    }

    #[test]
    fn affinity_on_own_thread() {
        let online = online_cpus();
        if online.len() < 2 {
            return;
        }
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        let one = [online[online.len() - 1]];
        assert_eq!(apply_affinity(&[tid], Some(&one), &online), (1, 0));
        assert_eq!(apply_affinity(&[tid], Some(&one), &online), (0, 0), "already there");
        assert_eq!(apply_affinity(&[tid], None, &online), (1, 0), "lifted: every online CPU");
        assert_eq!(apply_affinity(&[u32::MAX - 7], None, &online), (0, 0), "a thread that's gone");
    }

    fn session(holders: &[&str], game: Option<quiver_api::GameRef>) -> quiver_api::ActiveSession {
        quiver_api::ActiveSession { product_id: 3539, started: Utc::now(), holders: holders.iter().map(|h| h.to_string()).collect(), game, ..Default::default() }
    }

    #[test]
    fn who_is_playing() {
        let epic: quiver_api::GameRef = "steam:620".parse().unwrap();
        let s = vec![session(&["lab"], None), session(&["bridge"], None)];
        assert!(playing_games(&s).is_empty(), "lab runs and container bridges aren't the person playing");
        let s = vec![session(&["launcher", "bridge"], None), session(&["steam"], Some(epic)), session(&["launcher"], None)];
        assert_eq!(playing_games(&s), vec![quiver_api::GameRef::ubisoft(3539), epic]);
        assert_eq!(Playing::Yes { games: vec!["Alan Wake Remastered".into()] }.describe(), "Alan Wake Remastered is running");
    }

    #[test]
    fn governor_holds_the_share() {
        // A game whose GPU use is proportional to its frame rate: 0.5% per fps uncapped at 160
        // fps (80%). The governor brings it to 20%.
        let mut g = Governor::new(20.0);
        let uncapped = 160.0;
        let mut fps = uncapped;
        let mut use_ = 0.0;
        for _ in 0..30 {
            use_ = fps * 0.5;
            let i = g.step(GpuSample { own: use_, others: 0.0, instances: 1, fps });
            fps = if i == 0 { uncapped } else { (1e9 / i as f64).min(uncapped) };
        }
        assert!((17.0..=21.5).contains(&use_), "held at {use_}% ({fps} fps, cap {:?})", g.cap);
        // The game gets cheaper (a menu): 0.1% per fps; the cap rises and lets go.
        let mut last = 0.0;
        for _ in 0..40 {
            last = fps * 0.1;
            let i = g.step(GpuSample { own: last, others: 0.0, instances: 1, fps });
            fps = if i == 0 { uncapped } else { (1e9 / i as f64).min(uncapped) };
        }
        assert!(last <= 20.5 && fps > 150.0, "{last}% at {fps} fps, cap {:?}", g.cap);
        // Noisy samples (±60%, as NVML's are): the mean still settles near the target.
        let mut g = Governor::new(20.0);
        let mut fps = uncapped;
        let mut seed = 7u64;
        let mut uses = Vec::new();
        for k in 0..400 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = 0.4 + 1.2 * ((seed >> 33) as f64 / (1u64 << 31) as f64);
            let u = fps * 0.5 * noise;
            if k >= 100 {
                uses.push(fps * 0.5);
            }
            let i = g.step(GpuSample { own: u, others: 0.0, instances: 1, fps });
            fps = if i == 0 { uncapped } else { (1e9 / i as f64).min(uncapped) };
        }
        let mean = uses.iter().sum::<f64>() / uses.len() as f64;
        assert!((15.0..=23.0).contains(&mean), "noisy: mean {mean}%");
        // Two instances: an even split at least, the rest of what the other leaves.
        let mut g = Governor::new(20.0);
        g.step(GpuSample { own: 30.0, others: 15.0, instances: 2, fps: 60.0 });
        assert_eq!(g.target, 10.0);
        g.step(GpuSample { own: 30.0, others: 2.0, instances: 2, fps: 60.0 });
        assert_eq!(g.target, 18.0);
        // A frame rate never goes under MIN_FPS.
        let mut g = Governor::new(1.0);
        for _ in 0..20 {
            g.step(GpuSample { own: 90.0, others: 0.0, instances: 1, fps: 3.0 });
        }
        assert_eq!(g.cap, Some(MIN_FPS));
    }

    #[test]
    fn status_lines() {
        let limits = LabLimits::default();
        let (which, effective) = limits.effective(true);
        let cores = hx370();
        let mut st = Status {
            at: Utc::now(),
            source: "x".into(),
            limits: limits.clone(),
            playing: Playing::Yes { games: vec!["Alan Wake Remastered".into()] },
            which,
            plan: Plan::new(&effective, &cores),
            effective,
            gpu: Some(GpuStatus { percent: 20, target: 20.0, priority: "priority low".into(), measured: Some(18.2), cap_fps: Some(41.0), fps: Some(41.0), note: None }),
            errors: vec![],
        };
        assert_eq!(
            st.line(),
            "lab limits: while playing: Alan Wake Remastered is running → cpu 6 cores (cpus 6-11,18-23), gpu ≤20% (priority low, governor holding 18% at ≤41 fps)"
        );
        assert!(throttle_note(&st, false).unwrap().contains("Fixed-step virtual-clock results are unaffected"));
        assert!(throttle_note(&st, true).unwrap().contains("unaffected"));
        let (which, effective) = limits.effective(false);
        st.which = which;
        st.effective = effective;
        st.playing = Playing::No;
        assert!(!st.active() && throttle_note(&st, false).is_none());
        assert_eq!(st.line(), "lab limits: none in force (none of your games is running)");
        let back: Status = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }
}
