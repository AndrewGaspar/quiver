//! Built-in metric descriptors: the canonical metrics quiverd derives from roles (§3.4),
//! the system and display producers' metrics, and quiverd's own (session time, clock).
//!
//! Canonical ids have no namespace (`fps`, `frametime`), so they never collide with a
//! producer's (which have at least two segments).

use ubi_perf::descriptor::{Better, Display, Format, Level, MetricDescriptor, MetricKind, SampleRate, StateValue, Threshold, Unit};

pub const PRODUCER_QUIVERD: &str = "quiverd";
pub const PRODUCER_LAYER: &str = "layer";
pub const PRODUCER_SYSTEM: &str = "system";
pub const PRODUCER_DISPLAY: &str = "display";

/// Roles a phase-1 mod fills over JSON, beside ubi-perf's ring roles (`frame.base`, …):
/// a counter of real frames, whose rate is `fps.base` (mean only), and a counter of
/// generated frames.
pub const ROLE_BASE_COUNT: &str = "frame.base.count";
pub const ROLE_GENERATED_COUNT: &str = "frame.generated.count";
/// A gauge of mean render latency in ms the mod computes from Reflex reports.
pub const ROLE_LATENCY_RENDER: &str = "latency.render";
pub const ROLE_FG_STATE: &str = "fg.state";
pub const ROLE_UPSCALER: &str = "upscaler";
pub const ROLE_RENDER_RES: &str = "render.resolution";
pub const ROLE_OUTPUT_RES: &str = "output.resolution";

/// What feeds a canonical metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Presented frames from the layer.
    Frames,
    /// The rate of the counter that fills a role.
    CounterRate(&'static str),
    /// `fps` ÷ `fps.base`.
    FgRatio,
    /// Whether frame generation is on (a base provider, and `fg.state` not off).
    FgActive,
    /// The value of the metric that fills a role (a state, text or gauge).
    Alias(&'static str),
    /// `output.resolution`: a mod's role, else the layer's swapchain extent.
    OutputResolution,
    /// Seconds since the session started.
    SessionTime,
    /// Local wall-clock time.
    Clock,
}

/// A canonical metric and its source.
pub struct Canonical {
    pub desc: MetricDescriptor,
    pub source: Source,
    /// Which window statistic of presented frame times it is (`Frames` only).
    pub frame_stat: FrameStat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStat {
    None,
    /// 1000 ÷ mean frame time.
    Fps,
    /// Mean frame time.
    FrameTime,
    /// 1000 ÷ p99 frame time.
    P1Low,
    /// 1000 ÷ p99.9 frame time.
    P01Low,
    /// 1000 ÷ the mean of the slowest 1%.
    P1LowAvg,
    /// p99 frame time.
    FrameTimeP99,
}

fn d(id: &str, kind: MetricKind, unit: Unit, label: &str, short: Option<&str>, help: &str) -> MetricDescriptor {
    let mut d = MetricDescriptor::new(id, kind);
    d.unit = unit;
    d.label = Some(label.into());
    d.short = short.map(Into::into);
    d.help = Some(help.into());
    d
}

fn above(v: f64, level: Level) -> Threshold {
    Threshold { above: Some(v), below: None, level }
}

fn on_off() -> Vec<StateValue> {
    vec![
        StateValue { value: "off".into(), label: Some("off".into()), level: Some(Level::Dim) },
        StateValue { value: "on".into(), label: Some("on".into()), level: None },
    ]
}

pub fn canonical() -> Vec<Canonical> {
    let frames = |id: &str, unit, label: &str, short, help: &str, stat, better| {
        let mut desc = d(id, MetricKind::Gauge, unit, label, short, help);
        desc.better = Some(better);
        desc.group = Some("Frames".into());
        desc.sample = Some(SampleRate::PerFrame);
        Canonical { desc, source: Source::Frames, frame_stat: stat }
    };
    let mut out = vec![
        frames("fps", Unit::Fps, "FPS", Some("FPS"), "Presented frames per second (1000 ÷ the mean frame time), from the Sight layer.", FrameStat::Fps, Better::Higher),
        frames("frametime", Unit::Ms, "Frame time", Some("FT"), "Mean time between presented frames.", FrameStat::FrameTime, Better::Lower),
        frames(
            "fps.p1low",
            Unit::Fps,
            "1% low",
            Some("1%↓"),
            "1000 ÷ the 99th-percentile frame time in the window (CapFrameX P1).",
            FrameStat::P1Low,
            Better::Higher,
        ),
        frames("fps.p01low", Unit::Fps, "0.1% low", Some("0.1%↓"), "1000 ÷ the 99.9th-percentile frame time in the window.", FrameStat::P01Low, Better::Higher),
        frames(
            "fps.p1low.avg",
            Unit::Fps,
            "1% low (average)",
            None,
            "1000 ÷ the mean of the slowest 1% of frame times (MangoHud-style).",
            FrameStat::P1LowAvg,
            Better::Higher,
        ),
        frames("frametime.p99", Unit::Ms, "Frame time p99", Some("p99"), "99th-percentile frame time in the window.", FrameStat::FrameTimeP99, Better::Lower),
    ];
    let mut alias = |id: &str, kind, unit, label: &str, short: Option<&str>, help: &str, source| {
        let mut desc = d(id, kind, unit, label, short, help);
        desc.group = Some("Frames".into());
        out.push(Canonical { desc, source, frame_stat: FrameStat::None });
    };
    alias(
        "fps.base",
        MetricKind::Gauge,
        Unit::Fps,
        "Base FPS",
        Some("FPS"),
        "Frames the game rendered per second (a frame-generation mod's real frames; else presented FPS). Mean only in this version.",
        Source::CounterRate(ROLE_BASE_COUNT),
    );
    alias("fps.generated", MetricKind::Gauge, Unit::Fps, "Generated FPS", Some("GEN"), "Generated frames per second, from a frame-generation mod.", Source::CounterRate(ROLE_GENERATED_COUNT));
    alias("fg.ratio", MetricKind::Gauge, Unit::Ratio, "FG ratio", Some("FG"), "Presented ÷ base FPS.", Source::FgRatio);
    alias("fg.active", MetricKind::State, Unit::None, "FG active", None, "Frame generation is on: a mod provides base frames and its FG state isn't off.", Source::FgActive);
    alias("fg.state", MetricKind::State, Unit::None, "Frame generation", Some("FG"), "The frame-generation state a mod reports.", Source::Alias(ROLE_FG_STATE));
    alias(
        "latency.render",
        MetricKind::Gauge,
        Unit::Ms,
        "Render latency",
        Some("LAT"),
        "Simulation start to GPU end, from a mod's Reflex reports.",
        Source::Alias(ROLE_LATENCY_RENDER),
    );
    alias("upscaler", MetricKind::Text, Unit::None, "Upscaler", None, "The upscaler a mod reports (\"DLSS K\", \"TAA\").", Source::Alias(ROLE_UPSCALER));
    alias("render.resolution", MetricKind::Text, Unit::None, "Render resolution", None, "The game's internal resolution, from a mod.", Source::Alias(ROLE_RENDER_RES));
    alias("output.resolution", MetricKind::Text, Unit::None, "Output resolution", None, "The swapchain's size, from the Sight layer (or a mod).", Source::OutputResolution);
    alias("session.time", MetricKind::Gauge, Unit::S, "Session time", Some("TIME"), "Time since the game started.", Source::SessionTime);
    alias("clock.time", MetricKind::Text, Unit::None, "Clock", None, "Local time.", Source::Clock);
    for c in &mut out {
        if c.desc.id == "fg.active" {
            c.desc.values = on_off();
        }
        if c.desc.id == "frametime" || c.desc.id == "frametime.p99" {
            c.desc.thresholds = vec![above(33.4, Level::Warn), above(50.0, Level::Bad)];
        }
        if c.desc.id == "latency.render" {
            c.desc.thresholds = vec![above(30.0, Level::Warn), above(50.0, Level::Bad)];
            c.desc.better = Some(Better::Lower);
        }
        if c.desc.id == "fg.ratio" {
            c.desc.display = Some(Display::Badge);
        }
    }
    out
}

/// The system producer's metrics (`gpu.*`, `cpu.*`, `mem.*`, `bat.*`, `proc.*`).
pub fn system() -> Vec<MetricDescriptor> {
    let g = |id: &str, unit, label: &str, short, help: &str, group: &str| {
        let mut x = d(id, MetricKind::Gauge, unit, label, short, help);
        x.group = Some(group.into());
        x.sample = Some(SampleRate::Hz(1.0));
        x
    };
    let mut v = vec![
        g("gpu.load", Unit::Percent, "GPU load", Some("GPU"), "Utilization of the game's GPU.", "GPU"),
        g("gpu.temp", Unit::Celsius, "GPU temperature", None, "GPU core temperature.", "GPU"),
        g("gpu.power", Unit::Watt, "GPU power", None, "GPU board power draw.", "GPU"),
        g("gpu.clock", Unit::MHz, "GPU clock", None, "GPU graphics clock.", "GPU"),
        g("gpu.vram.used", Unit::Bytes, "VRAM used", Some("VRAM"), "Video memory in use on the game's GPU.", "GPU"),
        g("gpu.vram.total", Unit::Bytes, "VRAM total", None, "Video memory on the game's GPU.", "GPU"),
        g("cpu.load", Unit::Percent, "CPU load", Some("CPU"), "All cores, from /proc/stat.", "CPU"),
        g("cpu.temp", Unit::Celsius, "CPU temperature", None, "Package temperature (hwmon).", "CPU"),
        g("cpu.clock", Unit::MHz, "CPU clock", None, "Mean core clock.", "CPU"),
        g("mem.used", Unit::Bytes, "RAM used", Some("RAM"), "System memory in use (total − available).", "Memory"),
        g("mem.total", Unit::Bytes, "RAM total", None, "System memory.", "Memory"),
        g("proc.rss", Unit::Bytes, "Game RAM", None, "The game process's resident memory.", "Memory"),
        g("proc.cpu", Unit::Percent, "Game CPU", None, "The game process's CPU time, percent of one core.", "CPU"),
        g("bat.level", Unit::Percent, "Battery", Some("BAT"), "Battery charge.", "Power"),
        g("bat.power", Unit::Watt, "Battery power", None, "Power drawn from (or into) the battery.", "Power"),
    ];
    let mut name = d("gpu.name", MetricKind::Text, Unit::None, "GPU", None, "The GPU the metrics are from.", );
    name.group = Some("GPU".into());
    v.push(name);
    let mut bat = d("bat.status", MetricKind::State, Unit::None, "Power source", None, "Battery or AC.", );
    bat.group = Some("Power".into());
    bat.values = vec![
        StateValue { value: "ac".into(), label: Some("AC".into()), level: Some(Level::Dim) },
        StateValue { value: "charging".into(), label: Some("charging".into()), level: Some(Level::Good) },
        StateValue { value: "discharging".into(), label: Some("battery".into()), level: Some(Level::Warn) },
        StateValue { value: "full".into(), label: Some("full".into()), level: Some(Level::Dim) },
    ];
    v.push(bat);
    for x in &mut v {
        match x.id.as_str() {
            "gpu.temp" | "cpu.temp" => x.thresholds = vec![above(85.0, Level::Warn), above(95.0, Level::Bad)],
            "gpu.load" | "cpu.load" => {
                x.range = Some(ubi_perf::descriptor::Range::Fixed { min: 0.0, max: 100.0 });
                x.display = Some(Display::Number);
            }
            "bat.level" => x.thresholds = vec![Threshold { above: None, below: Some(20.0), level: Level::Warn }, Threshold { above: None, below: Some(10.0), level: Level::Bad }],
            "gpu.vram.used" | "mem.used" | "proc.rss" | "gpu.vram.total" | "mem.total" => x.format = Some(Format { si: true, ..Default::default() }),
            _ => {}
        }
    }
    v
}

/// The display producer's metrics, from Hyprland's monitor state.
pub fn display() -> Vec<MetricDescriptor> {
    let mut mode = d("display.mode", MetricKind::Text, Unit::None, "Display mode", Some("DISP"), "The game's monitor: size and refresh rate.");
    let mut refresh = d("display.refresh", MetricKind::Gauge, Unit::Hz, "Refresh rate", None, "The game's monitor refresh rate.");
    let mut vrr = d("display.vrr", MetricKind::State, Unit::None, "VRR", Some("VRR"), "Variable refresh is active on the game's monitor.");
    vrr.values = on_off();
    let mut tearing = d("display.tearing", MetricKind::State, Unit::None, "Tearing", None, "The game's monitor is tearing (immediate presents).");
    tearing.values = on_off();
    let mut path = d(
        "display.path",
        MetricKind::Text,
        Unit::None,
        "Display path",
        None,
        "Direct scan-out or composited, and what blocks scan-out (Hyprland's directScanoutBlockedBy).",
    );
    for x in [&mut mode, &mut refresh, &mut vrr, &mut tearing, &mut path] {
        x.group = Some("Display".into());
        x.sample = Some(SampleRate::Hz(1.0));
    }
    vec![mode, refresh, vrr, tearing, path]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_descriptors_validate_and_canonical_ids_are_unique() {
        for d in system().into_iter().chain(display()) {
            d.validate(None).unwrap_or_else(|e| panic!("{}: {e}", d.id));
        }
        let c = canonical();
        let mut ids: Vec<&str> = c.iter().map(|c| c.desc.id.as_str()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
        assert!(c.iter().any(|c| c.desc.id == "fps.p1low" && c.desc.help.as_deref().unwrap().contains("99th")));
    }
}
