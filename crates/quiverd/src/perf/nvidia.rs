//! The `nvidia` producer (SIGHT-HUD §3.17): NVIDIA metrics observed without any mod's help.
//!
//! | Metric | Source |
//! |---|---|
//! | `nv.reflex.mode`, `nv.reflex.cap` | the Sight layer's `vkSetLatencySleepModeNV` hook (status block) |
//! | `nv.frames.app`, `nv.frames.generated` → `fps.base`, `fg.ratio`, `fg.active` | the layer's frame counting (sleeps or present markers) |
//! | `nv.latency.pc` → `latency.pc` | the layer's markers and its own present timing |
//! | `nv.latency.render` → `latency.render` | opt-in: the layer's `vkGetLatencyTimingsNV` reads |
//! | `nv.fg.driver` | the launch's environment: NVIDIA Smooth Motion is on (it sits above the layer) |
//! | `dlss.version`, `dlss.fg.version`, `dlss.rr.version` | the DLLs mapped in the game process (`/proc/<pid>/maps`) and their version resources |
//! | `dlss.sr`, `dlss.sr.preset`, `dlss.sr.model`, `dlss.sr.quality`, `dlss.sr.render`, `dlss.sr.output` → `upscaler`, `render.resolution` | opt-in: NGX's own log in the session dir (`ngx_log`) |
//! | `dlss.sr.requested` | the launch's dxvk-nvapi DRS override (Quiver Tweaks or the person's env): requested, not measured |
//!
//! Mods never fill these roles (§3.4): what a mod reports stays under its own namespace.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use quiver_perf::descriptor::{Better, Display, Level, MetricDescriptor, MetricKind, RoleClaim, SampleRate, StateValue, Threshold, Unit};
use quiver_perf::layout::{LayerStatus, REFLEX_BOOST, REFLEX_COUNTING_MARKERS, REFLEX_COUNTING_SLEEP, REFLEX_LOW_LATENCY, REFLEX_MODE_SET};

use super::catalog;
use super::registry::Store;

pub const PRODUCER: &str = "nvidia";
/// The NGX log dir inside the session dir (`ngx_log`).
pub const NGX_DIR: &str = "ngx";

fn d(id: &str, kind: MetricKind, unit: Unit, label: &str, short: Option<&str>, help: &str, group: &str) -> MetricDescriptor {
    let mut d = MetricDescriptor::new(id, kind);
    d.unit = unit;
    d.label = Some(label.into());
    d.short = short.map(Into::into);
    d.help = Some(help.into());
    d.group = Some(group.into());
    d
}

fn role(d: &mut MetricDescriptor, r: &str) {
    d.roles = vec![RoleClaim { role: r.into(), flags: vec![], on: None }];
}

pub fn descriptors() -> Vec<MetricDescriptor> {
    let mut mode = d(
        "nv.reflex.mode",
        MetricKind::State,
        Unit::None,
        "Reflex",
        Some("RFX"),
        "The Reflex sleep mode the game set (vkSetLatencySleepModeNV through the Sight layer): off, on, or on with boost.",
        "Latency",
    );
    mode.values = vec![
        StateValue { value: "off".into(), label: Some("off".into()), level: Some(Level::Dim) },
        StateValue { value: "on".into(), label: Some("on".into()), level: None },
        StateValue { value: "boost".into(), label: Some("boost".into()), level: Some(Level::Good) },
    ];
    mode.display = Some(Display::Badge);
    let mut cap = d("nv.reflex.cap", MetricKind::Gauge, Unit::Fps, "Reflex cap", Some("CAP"), "Reflex's frame cap: 1 ÷ the minimum present interval the game set.", "Latency");
    cap.format = Some(quiver_perf::descriptor::Format { precision: Some(0), ..Default::default() });
    let mut app = d(
        "nv.frames.app",
        MetricKind::Counter,
        Unit::Count,
        "Game frames (Reflex)",
        None,
        "Application-rendered frames the Sight layer counted: one per vkLatencySleepNV, else one per present marker (the spec's frame attribution). Fills fps.base.",
        "Frame generation",
    );
    role(&mut app, catalog::ROLE_BASE_COUNT);
    let mut gen_ = d(
        "nv.frames.generated",
        MetricKind::Counter,
        Unit::Count,
        "Generated presents",
        None,
        "Presents beyond the first of each game frame, as the layer sees them. Smooth Motion and lsfg-vk sit above the layer: their frames aren't counted.",
        "Frame generation",
    );
    role(&mut gen_, catalog::ROLE_GENERATED_COUNT);
    let mut pc = d(
        "nv.latency.pc",
        MetricKind::Gauge,
        Unit::Ms,
        "PC latency (to present)",
        Some("PC"),
        "Simulation start (the input sample when the game marks it) to the frame's present returning at the Sight layer, from Reflex markers. Excludes the GPU work still queued and scan-out.",
        "Latency",
    );
    role(&mut pc, catalog::ROLE_LATENCY_PC);
    let mut render = d(
        "nv.latency.render",
        MetricKind::Gauge,
        Unit::Ms,
        "Game to render",
        Some("LAT"),
        "Simulation start to the frame's last GPU work, from the driver's Reflex reports (vkGetLatencyTimingsNV; opt-in: quiver hud set GAME reflex_timings=on).",
        "Latency",
    );
    role(&mut render, catalog::ROLE_LATENCY_RENDER);
    for x in [&mut pc, &mut render] {
        x.better = Some(Better::Lower);
        x.thresholds = vec![Threshold { above: Some(30.0), below: None, level: Level::Warn }, Threshold { above: Some(50.0), below: None, level: Level::Bad }];
    }
    let driver = d(
        "nv.fg.driver",
        MetricKind::Text,
        Unit::None,
        "Driver frame generation",
        Some("FG"),
        "NVIDIA Smooth Motion is on for this launch. It generates frames above the Sight layer, so the layer can't count them; FG ratio isn't shown.",
        "Frame generation",
    );
    let mut sr = d("dlss.sr", MetricKind::Text, Unit::None, "DLSS", None, "DLSS Super Resolution and the preset NGX used at the latest feature creation (NGX log).", "Upscaling");
    role(&mut sr, catalog::ROLE_UPSCALER);
    let preset = d(
        "dlss.sr.preset",
        MetricKind::Text,
        Unit::None,
        "DLSS preset",
        Some("SR"),
        "The SR preset NGX logged at feature creation, with * when it came from a driver-settings override (ngx_log).",
        "Upscaling",
    );
    let model = d("dlss.sr.model", MetricKind::Text, Unit::None, "DLSS model", None, "The preset's model family: CNN (A–F) or Transformer (J–M).", "Upscaling");
    let quality = d("dlss.sr.quality", MetricKind::Text, Unit::None, "DLSS quality", Some("Q"), "The SR quality mode at feature creation: DLAA, Quality, Balanced, Perf, Ultra Perf (NGX log).", "Upscaling");
    let mut res = d("dlss.sr.render", MetricKind::Text, Unit::None, "DLSS render resolution", None, "SR's input size at feature creation (NGX log).", "Upscaling");
    role(&mut res, catalog::ROLE_RENDER_RES);
    let mut out = d("dlss.sr.output", MetricKind::Text, Unit::None, "DLSS output resolution", None, "SR's output size at feature creation (NGX log).", "Upscaling");
    role(&mut out, catalog::ROLE_OUTPUT_RES);
    let version = d(
        "dlss.version",
        MetricKind::Text,
        Unit::None,
        "DLSS SR DLL",
        Some("v"),
        "The version of the nvngx_dlss.dll (or NGX updater model) loaded in the game process. Loaded isn't necessarily evaluating.",
        "Upscaling",
    );
    let fg_version = d("dlss.fg.version", MetricKind::Text, Unit::None, "DLSS FG DLL", Some("FG"), "The version of the nvngx_dlssg.dll loaded in the game process.", "Frame generation");
    let rr_version = d("dlss.rr.version", MetricKind::Text, Unit::None, "DLSS RR DLL", Some("RR"), "The version of the nvngx_dlssd.dll (Ray Reconstruction) loaded in the game process.", "Upscaling");
    let requested = d(
        "dlss.sr.requested",
        MetricKind::Text,
        Unit::None,
        "DLSS preset requested",
        Some("REQ"),
        "The SR preset and mode this launch asks the driver for (dxvk-nvapi DRS settings, from Quiver Tweaks or the environment). Requested, not measured: DLSS only uses presets its DLL has.",
        "Upscaling",
    );
    let mut v = vec![mode, cap, app, gen_, pc, render, driver, sr, preset, model, quality, res, out, version, fg_version, rr_version, requested];
    for x in &mut v {
        if x.sample.is_none() && matches!(x.kind, MetricKind::Gauge | MetricKind::Counter | MetricKind::State) {
            x.sample = Some(SampleRate::Hz(4.0));
        }
    }
    v
}

/// Declares the producer and its metrics once.
pub fn ensure(store: &mut Store) -> bool {
    if store.producer_mut(PRODUCER).is_some() {
        return false;
    }
    store.producer(PRODUCER, "NVIDIA (Reflex, DLSS)", true);
    for d in descriptors() {
        store.declare(PRODUCER, d);
    }
    true
}

/// The layer status counters at the last drain, for deltas.
#[derive(Debug, Default, Clone, Copy)]
pub struct Last {
    pc: (u64, u64),
    render: (u64, u64),
}

/// What the layer's status block says, into the store. Returns whether the producer was
/// declared now (a catalog change).
pub fn apply_layer(store: &mut Store, s: &LayerStatus, last: &mut Last, now: i64) -> bool {
    let active = s.reflex_flags & (REFLEX_MODE_SET | REFLEX_COUNTING_SLEEP | REFLEX_COUNTING_MARKERS) != 0 || s.pc_latency_frames > 0 || s.render_latency_frames > 0;
    if !active && store.producer_mut(PRODUCER).is_none() {
        return false;
    }
    let fresh = ensure(store);
    if s.reflex_flags & REFLEX_MODE_SET != 0 {
        let mode = match (s.reflex_flags & REFLEX_LOW_LATENCY != 0, s.reflex_flags & REFLEX_BOOST != 0) {
            (false, _) => "off",
            (true, false) => "on",
            (true, true) => "boost",
        };
        store.push_state_value("nv.reflex.mode", now, mode);
        if s.reflex_min_interval_us > 0 {
            store.push_num("nv.reflex.cap", now, 1e6 / s.reflex_min_interval_us as f64);
        } else {
            store.clear("nv.reflex.cap");
        }
    }
    if s.reflex_flags & (REFLEX_COUNTING_SLEEP | REFLEX_COUNTING_MARKERS) != 0 {
        store.push_counter("nv.frames.app", now, s.app_frames);
        store.push_counter("nv.frames.generated", now, s.app_presents.saturating_sub(s.app_frames));
    }
    let mut delta = |id: &str, prev: &mut (u64, u64), frames: u64, sum: u64| {
        if frames < prev.0 || sum < prev.1 {
            // A new swapchain started its counters again.
            *prev = (0, 0);
        }
        let (n, us) = (frames - prev.0, sum - prev.1);
        if n > 0 {
            store.push_num(id, now, us as f64 / n as f64 / 1000.0);
            *prev = (frames, sum);
        }
    };
    delta("nv.latency.pc", &mut last.pc, s.pc_latency_frames, s.pc_latency_sum_us);
    delta("nv.latency.render", &mut last.render, s.render_latency_frames, s.render_latency_sum_us);
    fresh
}

// ---- the launch's environment

/// What a launch's environment asks of NVIDIA's stack.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LaunchNv {
    /// The DRS SR preset override (`K`, `latest`) and mode (`Perf`, `DLAA`).
    pub preset: Option<String>,
    pub mode: Option<String>,
    /// `NVPRESENT_ENABLE_SMOOTH_MOTION` on, and not vetoed.
    pub smooth_motion: bool,
}

/// A DRS setting from `DXVK_NVAPI_DRS_<KEY>` or `DXVK_NVAPI_DRS_SETTINGS` (the dedicated
/// variable wins, as in dxvk-nvapi).
fn drs<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    if let Some(v) = env.get(&format!("DXVK_NVAPI_DRS_{key}")) {
        return Some(v.trim());
    }
    env.get("DXVK_NVAPI_DRS_SETTINGS")?.split(',').filter_map(|kv| kv.split_once('=')).find(|(k, _)| k.trim().eq_ignore_ascii_case(key)).map(|(_, v)| v.trim())
}

/// `render_preset_k` / `11` / `render_preset_latest` → `K` / `latest`; off → `None`.
pub fn preset_name(v: &str) -> Option<String> {
    let v = v.to_ascii_lowercase();
    if let Some(l) = v.strip_prefix("render_preset_") {
        return match l {
            "latest" => Some("latest".into()),
            "default" => None,
            l if l.len() == 1 => Some(l.to_ascii_uppercase()),
            _ => None,
        };
    }
    let n = v.strip_prefix("0x").map_or_else(|| v.parse::<u32>().ok(), |h| u32::from_str_radix(h, 16).ok())?;
    match n {
        0 => None,
        0x00FF_FFFF => Some("latest".into()),
        1..=26 => Some(((b'A' + n as u8 - 1) as char).to_string()),
        _ => None,
    }
}

fn mode_name(v: &str) -> Option<&'static str> {
    Some(match v.to_ascii_lowercase().as_str() {
        "performance" | "0" => "Perf",
        "balanced" | "1" => "Balanced",
        "quality" | "2" => "Quality",
        "dlaa" | "4" => "DLAA",
        "ultra_performance" | "5" => "Ultra Perf",
        "custom" | "6" => "Custom",
        _ => return None,
    })
}

impl LaunchNv {
    pub fn from_env(env: &BTreeMap<String, String>) -> LaunchNv {
        let on = |k: &str| env.get(k).is_some_and(|v| !v.is_empty() && v != "0");
        let override_on = drs(env, "NGX_DLSS_SR_OVERRIDE").is_none_or(|v| !v.eq_ignore_ascii_case("off") && v != "0");
        LaunchNv {
            preset: drs(env, "NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION").filter(|_| override_on).and_then(preset_name),
            mode: drs(env, "NGX_DLSS_SR_MODE").and_then(mode_name).map(str::to_owned),
            smooth_motion: on("NVPRESENT_ENABLE_SMOOTH_MOTION") && !env.contains_key("DISABLE_LAYER_NV_PRESENT_1"),
        }
    }

    /// `K · Perf`, `latest`.
    pub fn requested(&self) -> Option<String> {
        match (&self.preset, &self.mode) {
            (None, None) => None,
            (Some(p), None) => Some(p.clone()),
            (None, Some(m)) => Some(m.clone()),
            (Some(p), Some(m)) => Some(format!("{p} · {m}")),
        }
    }
}

/// The launch's requests into the store.
pub fn apply_launch(store: &mut Store, l: &LaunchNv, now: i64) -> bool {
    store.smooth_motion = l.smooth_motion;
    let req = l.requested();
    if !l.smooth_motion && req.is_none() {
        return false;
    }
    let fresh = ensure(store);
    if l.smooth_motion {
        store.push_text("nv.fg.driver", now, "Smooth Motion (driver)".into());
    }
    if let Some(r) = req {
        store.push_text("dlss.sr.requested", now, r);
    }
    fresh
}

// ---- DLSS: the DLLs in the game process, and NGX's log

/// Which NGX snippet a mapped file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Snippet {
    Sr,
    Fg,
    Rr,
}

impl Snippet {
    fn metric(self) -> &'static str {
        match self {
            Snippet::Sr => "dlss.version",
            Snippet::Fg => "dlss.fg.version",
            Snippet::Rr => "dlss.rr.version",
        }
    }
}

/// A `/proc/<pid>/maps` path that is an NGX snippet: the game's DLL, or the NGX updater's
/// model (`…/NGX/models/dlss/versions/<N>/files/<x>.bin`, N = major << 16 | minor << 8 | patch).
pub fn snippet_of(path: &str) -> Option<(Snippet, Option<String>)> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next()?;
    let by_name = |n: &str| match n {
        "nvngx_dlss.dll" => Some(Snippet::Sr),
        "nvngx_dlssg.dll" => Some(Snippet::Fg),
        "nvngx_dlssd.dll" => Some(Snippet::Rr),
        _ => None,
    };
    if let Some(s) = by_name(name) {
        return Some((s, None));
    }
    let (_, rest) = lower.split_once("/ngx/models/")?;
    let mut it = rest.split('/');
    let kind = match it.next()? {
        "dlss" => Snippet::Sr,
        "dlssg" => Snippet::Fg,
        "dlssd" => Snippet::Rr,
        _ => return None,
    };
    (it.next()? == "versions").then_some(())?;
    let n: u32 = it.next()?.parse().ok()?;
    name.ends_with(".bin").then(|| (kind, Some(format!("{}.{}.{}", n >> 16, (n >> 8) & 0xff, n & 0xff))))
}

/// A PE file's `VS_FIXEDFILEINFO` file version (`310.9.16`, a trailing `.0` dropped).
pub fn pe_version(bytes: &[u8]) -> Option<String> {
    let tag: Vec<u8> = "VS_VERSION_INFO".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let start = bytes.windows(tag.len()).position(|w| w == tag.as_slice())?;
    let sig = 0xFEEF_04BDu32.to_le_bytes();
    let rel = bytes[start..].windows(4).position(|w| w == sig)?;
    let at = start + rel;
    let word = |o: usize| bytes.get(at + o..at + o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let (ms, ls) = (word(8)?, word(12)?);
    let parts = [ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff];
    let v = if parts[3] == 0 { format!("{}.{}.{}", parts[0], parts[1], parts[2]) } else { format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]) };
    Some(v)
}

/// What NGX's log says about the latest feature creations.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NgxFacts {
    pub preset: Option<String>,
    /// `override`, `app` or `default`.
    pub via: Option<&'static str>,
    pub quality: Option<String>,
    pub render: Option<(u32, u32)>,
    pub output: Option<(u32, u32)>,
    pub version: Option<String>,
}

fn num_after(line: &str, key: &str) -> Option<u32> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.trim_start_matches([' ', ':', '=']);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `K`, `Preset_K`, `NVSDK_NGX_DLSS_Hint_Render_Preset_K`, `11` → `K`.
fn preset_token(t: &str) -> Option<String> {
    let t = t.trim().trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
    let last = t.rsplit('_').next().unwrap_or(t);
    if last.len() == 1 && last.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(last.to_ascii_uppercase());
    }
    if last.eq_ignore_ascii_case("default") {
        return Some("default".into());
    }
    last.parse::<u32>().ok().and_then(|n| preset_name(&n.to_string()))
}

fn quality_label(s: &str) -> Option<&'static str> {
    let l = s.to_ascii_lowercase();
    Some(match () {
        _ if l.contains("ultraperf") => "Ultra Perf",
        _ if l.contains("ultraquality") => "Ultra Quality",
        _ if l.contains("maxperf") || l == "perf" || l == "performance" => "Perf",
        _ if l.contains("maxquality") || l == "quality" => "Quality",
        _ if l.contains("balanced") => "Balanced",
        _ if l.contains("dlaa") => "DLAA",
        _ => return None,
    })
}

/// Reads NGX's SR lines (format strings of nvngx_dlss.dll 310.9.16; SIGHT-HUD §3.17). The
/// last feature creation wins.
pub fn parse_ngx(text: &str, f: &mut NgxFacts) {
    for line in text.lines() {
        if let Some(v) = num_after(line, "InCreationTimeWidth") {
            f.render = Some((v, f.render.map_or(0, |r| r.1)));
        }
        if let Some(v) = num_after(line, "InCreationTimeHeight") {
            f.render = Some((f.render.map_or(0, |r| r.0), v));
        }
        if let Some(v) = num_after(line, "OutWidth") {
            f.output = Some((v, f.output.map_or(0, |r| r.1)));
        }
        if let Some(v) = num_after(line, "OutHeight") {
            f.output = Some((f.output.map_or(0, |r| r.0), v));
        }
        if let Some(i) = line.find("PerfQualityValue") {
            let rest = line[i + "PerfQualityValue".len()..].trim();
            if let Some(q) = rest.split_whitespace().next().and_then(quality_label) {
                f.quality = Some(q.into());
            }
        }
        for (needle, via) in [("Using DRS Overridden Preset", "override"), ("Using App hint Preset", "app"), ("using title default Preset", "default")] {
            if let Some(i) = line.find(needle) {
                if let Some(p) = line[i + needle.len()..].split_whitespace().next().and_then(preset_token) {
                    f.preset = Some(p);
                    f.via = Some(via);
                }
                // `(Quality) Using …`: the mode label.
                if let (Some(a), Some(b)) = (line.find('('), line.find(')'))
                    && a < b
                    && b < i
                    && let Some(q) = quality_label(&line[a + 1..b])
                {
                    f.quality = Some(q.into());
                }
            }
        }
        // The snippet's `Build v310.9.16 CL …`, or the core's `… snippet: …/… version: 310.9.16`.
        for key in ["Build v", "version: "] {
            if (line.contains("snippet") || key == "Build v")
                && let Some(i) = line.find(key)
            {
                let v: String = line[i + key.len()..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                if v.split('.').count() >= 3 {
                    f.version = Some(v.trim_end_matches('.').into());
                }
            }
        }
    }
}

/// The preset's model family.
pub fn model_of(preset: &str) -> Option<&'static str> {
    match preset {
        "A" | "B" | "C" | "D" | "E" | "F" => Some("CNN"),
        "J" | "K" | "L" | "M" => Some("Transformer"),
        _ => None,
    }
}

/// One session's DLSS scan state (the sensors thread's).
#[derive(Default)]
pub struct DlssScan {
    next_maps: Option<Instant>,
    versions: BTreeMap<Snippet, String>,
    /// path → version, so a mapped file is read once.
    seen: HashMap<String, Option<String>>,
    ngx_mtime: Option<SystemTime>,
    ngx: NgxFacts,
}

/// What a scan found, to push.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DlssFound {
    pub versions: BTreeMap<Snippet, String>,
    pub ngx: NgxFacts,
}

impl DlssScan {
    /// Reads the game processes' maps (every 3 s until the SR DLL shows, then every 20 s)
    /// and the NGX log dir (when it changed). `None` when nothing changed.
    pub fn scan(&mut self, pids: &[u32], ngx_dir: Option<&Path>, now: Instant) -> Option<DlssFound> {
        let mut changed = false;
        if self.next_maps.is_none_or(|t| now >= t) {
            let every = if self.versions.contains_key(&Snippet::Sr) { 20 } else { 3 };
            self.next_maps = Some(now + Duration::from_secs(every));
            for pid in pids {
                let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else { continue };
                for path in maps.lines().filter_map(|l| l.split_once('/').map(|(_, p)| format!("/{p}"))) {
                    let Some((kind, from_name)) = snippet_of(&path) else { continue };
                    let v = self.seen.entry(path.clone()).or_insert_with(|| from_name.or_else(|| read_pe_version(Path::new(&path)))).clone();
                    if let Some(v) = v
                        && self.versions.get(&kind) != Some(&v)
                    {
                        self.versions.insert(kind, v);
                        changed = true;
                    }
                }
            }
        }
        if let Some(dir) = ngx_dir
            && let Some((mtime, facts)) = read_ngx(dir, self.ngx_mtime)
        {
            self.ngx_mtime = Some(mtime);
            if facts != self.ngx {
                self.ngx = facts;
                changed = true;
            }
        }
        changed.then(|| DlssFound { versions: self.versions.clone(), ngx: self.ngx.clone() })
    }
}

fn read_pe_version(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() || md.len() > 256 << 20 {
        return None;
    }
    pe_version(&std::fs::read(p).ok()?)
}

/// The NGX logs in `dir` (newest change `> since`), parsed oldest file first.
fn read_ngx(dir: &Path, since: Option<SystemTime>) -> Option<(SystemTime, NgxFacts)> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if md.is_file() && name.starts_with("nvngx") && name.ends_with(".log") { Some((md.modified().ok()?, e.path())) } else { None }
        })
        .collect();
    let newest = files.iter().map(|f| f.0).max()?;
    if since.is_some_and(|s| newest <= s) {
        return None;
    }
    files.sort();
    let mut facts = NgxFacts::default();
    for (_, p) in files {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        // The creation lines come early; a verbose log can grow.
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(8 << 20)]);
        parse_ngx(&text, &mut facts);
    }
    Some((newest, facts))
}

/// A scan's findings into the store.
pub fn apply_dlss(store: &mut Store, f: &DlssFound, now: i64) -> bool {
    if f.versions.is_empty() && f.ngx == NgxFacts::default() {
        return false;
    }
    let fresh = ensure(store);
    for (k, v) in &f.versions {
        store.push_text(k.metric(), now, v.clone());
    }
    let n = &f.ngx;
    let res = |r: (u32, u32)| format!("{}×{}", r.0, r.1);
    if let Some(p) = &n.preset {
        let star = if n.via == Some("override") { "*" } else { "" };
        store.push_text("dlss.sr.preset", now, format!("{p}{star}"));
        store.push_text("dlss.sr", now, if p == "default" { "DLSS".into() } else { format!("DLSS {p}") });
        if let Some(m) = model_of(p) {
            store.push_text("dlss.sr.model", now, m.into());
        }
    }
    if let Some(q) = &n.quality {
        store.push_text("dlss.sr.quality", now, q.clone());
    }
    if let Some(r) = n.render.filter(|r| r.0 > 0 && r.1 > 0) {
        store.push_text("dlss.sr.render", now, res(r));
    }
    if let Some(r) = n.output.filter(|r| r.0 > 0 && r.1 > 0) {
        store.push_text("dlss.sr.output", now, res(r));
    }
    if !f.versions.contains_key(&Snippet::Sr)
        && let Some(v) = &n.version
    {
        store.push_text("dlss.version", now, v.clone());
    }
    fresh
}

/// The NGX logging environment for a launch whose session dir is `perf_dir` (a Linux path;
/// NGX gets the Wine `Z:` form). Keys the launch already sets are left alone.
pub fn ngx_env(perf_dir: &Path, env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let dir = perf_dir.join(NGX_DIR);
    let win = format!("Z:{}\\", dir.display().to_string().replace('/', "\\"));
    [("__NGX_LOG_LEVEL", "1".to_owned()), ("__NGX_ENABLE_OVERRIDE_LOG_PATH", "1".to_owned()), ("__NGX_LOG_PATH_OVERRIDE", win)]
        .into_iter()
        .filter(|(k, _)| !env.contains_key(*k))
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use quiver_api::perf::Stat;
    use quiver_perf::layout::{REFLEX_MARKERS, REFLEX_SLEEPING};

    const MS: i64 = 1_000_000;

    #[test]
    fn descriptors_validate() {
        for d in descriptors() {
            d.validate(None).unwrap_or_else(|e| panic!("{}: {e}", d.id));
            assert!(d.short.as_ref().is_none_or(|s| s.chars().count() <= 6), "{}", d.id);
        }
    }

    #[test]
    fn layer_status_to_metrics() {
        let mut s = Store::new(0, Utc::now());
        let mut last = Last::default();
        let mut st = LayerStatus::ZERO;
        // Nothing Reflex: no producer at all.
        assert!(!apply_layer(&mut s, &st, &mut last, 1000 * MS));
        assert!(s.producer_mut(PRODUCER).is_none());
        st.reflex_flags = REFLEX_MODE_SET | REFLEX_LOW_LATENCY | REFLEX_BOOST | REFLEX_SLEEPING | REFLEX_MARKERS | REFLEX_COUNTING_SLEEP;
        st.reflex_min_interval_us = 8333;
        st.app_frames = 100;
        st.app_presents = 200;
        st.pc_latency_frames = 100;
        st.pc_latency_sum_us = 1_650_000;
        assert!(apply_layer(&mut s, &st, &mut last, 1000 * MS));
        let txt = |s: &Store, id: &str| s.formatted(id, Stat::Last, super::super::registry::WINDOW_1S, 1000 * MS).map(|v| v.text);
        assert_eq!(txt(&s, "nv.reflex.mode").as_deref(), Some("boost"));
        assert_eq!(txt(&s, "nv.reflex.cap").as_deref(), Some("120"));
        assert_eq!(txt(&s, "latency.pc").as_deref(), Some("16.5"));
        // Off with no cap: the cap goes (hidden), the mode says off.
        st.reflex_flags = REFLEX_MODE_SET;
        st.reflex_min_interval_us = 0;
        apply_layer(&mut s, &st, &mut last, 1100 * MS);
        assert_eq!(txt(&s, "nv.reflex.mode").as_deref(), Some("off"));
        assert_eq!(s.state("nv.reflex.cap", 1100 * MS), quiver_api::perf::MetricState::Declared);
    }

    #[test]
    fn launch_env_requests() {
        let env = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let l = LaunchNv::from_env(&env(&[("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE", "on"), ("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION", "render_preset_k")]));
        assert_eq!(l.requested().as_deref(), Some("K"));
        let l = LaunchNv::from_env(&env(&[("DXVK_NVAPI_DRS_SETTINGS", "NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION=render_preset_latest,NGX_DLSS_SR_MODE=performance")]));
        assert_eq!(l.requested().as_deref(), Some("latest · Perf"));
        // The dedicated variable wins over the list (dxvk-nvapi's rule); an explicit off.
        let l = LaunchNv::from_env(&env(&[("DXVK_NVAPI_DRS_SETTINGS", "NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION=11"), ("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION", "13")]));
        assert_eq!(l.preset.as_deref(), Some("M"));
        let l = LaunchNv::from_env(&env(&[("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE", "off"), ("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION", "render_preset_k")]));
        assert_eq!(l.preset, None);
        assert!(LaunchNv::from_env(&env(&[("NVPRESENT_ENABLE_SMOOTH_MOTION", "1")])).smooth_motion);
        assert!(!LaunchNv::from_env(&env(&[("NVPRESENT_ENABLE_SMOOTH_MOTION", "1"), ("DISABLE_LAYER_NV_PRESENT_1", "1")])).smooth_motion);
        assert_eq!(LaunchNv::from_env(&BTreeMap::new()), LaunchNv::default());
    }

    #[test]
    fn snippets_in_maps() {
        assert_eq!(snippet_of("/home/x/Games/ACO/nvngx_dlss.dll"), Some((Snippet::Sr, None)));
        assert_eq!(snippet_of("/p/drive_c/windows/system32/NVNGX_DLSSG.DLL"), Some((Snippet::Fg, None)));
        assert_eq!(snippet_of("/p/drive_c/ProgramData/NVIDIA/NGX/models/dlss/versions/20318480/files/160_E658703.bin"), Some((Snippet::Sr, Some("310.9.16".into()))));
        assert_eq!(snippet_of("/usr/lib/nvidia/wine/_nvngx.dll"), None);
        assert_eq!(snippet_of("/x/d3d11.dll"), None);
    }

    #[test]
    fn pe_version_skips_false_signatures() {
        let mut b = vec![0u8; 64];
        // A stray signature before the version resource (seen in nvngx_dlss.dll).
        b.extend(0xFEEF_04BDu32.to_le_bytes());
        b.extend([0xff; 12]);
        b.extend("VS_VERSION_INFO".encode_utf16().flat_map(|u| u.to_le_bytes()));
        b.extend([0; 6]);
        b.extend(0xFEEF_04BDu32.to_le_bytes());
        b.extend(0x0001_0000u32.to_le_bytes());
        b.extend(((310u32 << 16) | 9).to_le_bytes());
        b.extend((16u32 << 16).to_le_bytes());
        assert_eq!(pe_version(&b).as_deref(), Some("310.9.16"));
        assert_eq!(pe_version(b"MZ nothing"), None);
    }

    #[test]
    fn ngx_log_lines() {
        let log = "\
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:120] Build v310.9.16 CL 36522741
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:131] PerfQualityValue NVSDK_NGX_PerfQuality_Value_MaxPerf (0)
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:140] InCreationTimeWidth 1280
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:141] InCreationTimeHeight 800
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:142] OutWidth 2560
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:143] OutHeight 1600
[2026-11-17 21:04:12] [tid:31028][NgxDltss::Create:150] Info: (Perf) App hint Preset Unspecified or Overridden, using title default Preset M
";
        let mut f = NgxFacts::default();
        parse_ngx(log, &mut f);
        assert_eq!(
            f,
            NgxFacts { preset: Some("M".into()), via: Some("default"), quality: Some("Perf".into()), render: Some((1280, 800)), output: Some((2560, 1600)), version: Some("310.9.16".into()) }
        );
        // A later creation with a driver override (the letter may be decorated).
        parse_ngx("[t] [tid:1][x:1] Info: (Quality) Using DRS Overridden Preset Preset_K\n[t] InCreationTimeWidth 1707\n", &mut f);
        assert_eq!((f.preset.as_deref(), f.via, f.quality.as_deref(), f.render), (Some("K"), Some("override"), Some("Quality"), Some((1707, 800))));
        let mut s = Store::new(0, Utc::now());
        apply_dlss(&mut s, &DlssFound { versions: BTreeMap::from([(Snippet::Sr, "310.9.16".into())]), ngx: f }, 10);
        let t = |id: &str| s.value(id, Stat::Last, 1_000_000_000, 10);
        assert_eq!(t("upscaler"), Some(super::super::format::Raw::Text("DLSS K".into())));
        assert_eq!(t("dlss.sr.preset"), Some(super::super::format::Raw::Text("K*".into())));
        assert_eq!(t("dlss.sr.model"), Some(super::super::format::Raw::Text("Transformer".into())));
        assert_eq!(t("render.resolution"), Some(super::super::format::Raw::Text("1707×800".into())));
    }

    #[test]
    fn ngx_env_uses_the_session_dir() {
        let e = ngx_env(Path::new("/dev/shm/quiver-1000/s-abc"), &BTreeMap::new());
        assert!(e.contains(&("__NGX_LOG_PATH_OVERRIDE".into(), "Z:\\dev\\shm\\quiver-1000\\s-abc\\ngx\\".into())));
        assert_eq!(e.len(), 3);
        let mine = BTreeMap::from([("__NGX_LOG_LEVEL".to_string(), "2".to_string())]);
        assert_eq!(ngx_env(Path::new("/x"), &mine).len(), 2);
    }
}
