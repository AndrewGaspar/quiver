//! Built-in metric descriptors: the canonical metrics quiverd derives from roles (§3.4),
//! the system and display producers' metrics, and quiverd's own (session time, clock).
//!
//! Canonical ids have no namespace (`fps`, `frametime`), so they never collide with a
//! producer's (which have at least two segments).

use quiver_perf::descriptor::{Better, Display, Format, Level, MetricDescriptor, MetricKind, SampleRate, StateValue, Threshold, Unit};

pub const PRODUCER_QUIVERD: &str = "quiverd";
pub const PRODUCER_LAYER: &str = "layer";
pub const PRODUCER_SYSTEM: &str = "system";
pub const PRODUCER_DISPLAY: &str = "display";
/// Quiver Tweaks (design/TWEAKS.md §6): what each active tweak does this session.
pub const PRODUCER_TWEAKS: &str = "tweaks";

/// Generic roles (§3.4, §3.17): filled only by implementation-agnostic producers (the Sight
/// layer's Vulkan interception, NGX's and dxvk-nvapi's logs, the game's DLLs: the `nvidia`
/// producer), never by a mod. A mod's claim on one is recorded in the catalog as a conflict
/// and not bound; its metric stays under the mod's own namespace.
///
/// A counter of application-rendered frames, whose rate is `fps.base` (mean only), and a
/// counter of the further (generated) presents.
pub const ROLE_BASE_COUNT: &str = "frame.base.count";
pub const ROLE_GENERATED_COUNT: &str = "frame.generated.count";
/// A gauge of mean game-to-render latency in ms (simulation start → GPU render end).
pub const ROLE_LATENCY_RENDER: &str = "latency.render";
/// A gauge of mean PC latency to present in ms (simulation start → present).
pub const ROLE_LATENCY_PC: &str = "latency.pc";
pub const ROLE_FG_STATE: &str = "fg.state";
pub const ROLE_UPSCALER: &str = "upscaler";
pub const ROLE_RENDER_RES: &str = "render.resolution";
pub const ROLE_OUTPUT_RES: &str = "output.resolution";
/// The ring roles of §3.4 (`frame.base`, `frame.generated`, `latency.frame`) are generic too.
pub const GENERIC_ROLES: [&str; 11] = [
    ROLE_BASE_COUNT,
    ROLE_GENERATED_COUNT,
    ROLE_LATENCY_RENDER,
    ROLE_LATENCY_PC,
    ROLE_FG_STATE,
    ROLE_UPSCALER,
    ROLE_RENDER_RES,
    ROLE_OUTPUT_RES,
    "frame.base",
    "frame.generated",
    "latency.frame",
];

/// The latency split's stages: (role and canonical id, label, short label, help). They add up
/// to the game-to-render latency (`latency.render`): the `nvidia` producer fills them from the
/// driver's Reflex timing reports (the layer's opt-in `vkGetLatencyTimingsNV` reads), a mod
/// with Reflex markers of its own may too. Scan-out isn't in any report, so the last stage is
/// the GPU's (it was `latency.split.out`, "present and scan-out", before anything filled it).
pub const LATENCY_SPLIT: [(&str, &str, &str, &str); 4] = [
    ("latency.split.sim", "Latency: simulation", "SIM", "Simulation start to simulation end."),
    ("latency.split.render", "Latency: render submit", "RENDER", "Simulation end to the end of the frame's render submission on the CPU."),
    ("latency.split.queue", "Latency: queue", "QUEUE", "Render submission's end to the GPU starting the frame: the driver's and the OS's queues."),
    ("latency.split.gpu", "Latency: GPU", "GPU", "The GPU starting the frame to its last GPU work. Scan-out isn't measured."),
];

/// Whether `producer` may fill generic roles: built-in producers only, not mods.
pub fn agnostic(producer: &str) -> bool {
    !producer.starts_with("mod:")
}

/// What feeds a canonical metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Presented frames from the layer.
    Frames,
    /// The rate of the counter that fills a role.
    CounterRate(&'static str),
    /// Presents per application frame: (base + generated) ÷ base from the same counters,
    /// else `fps` ÷ `fps.base`. Not shown while a driver frame generator (Smooth Motion) the
    /// layer can't see is on.
    FgRatio,
    /// Whether frame generation is on (a base provider, and `fg.state` not off).
    FgActive,
    /// The value of the metric that fills a role (a state, text or gauge).
    Alias(&'static str),
    /// `output.resolution`: a mod's role, else the layer's swapchain extent.
    OutputResolution,
    /// `frametime.base`: 1000 ÷ `fps.base`.
    BaseFrameTime,
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
        "Frames the game rendered per second: application frames the Sight layer counts from Reflex (sleeps or present markers); else presented FPS. Mean only in this version.",
        Source::CounterRate(ROLE_BASE_COUNT),
    );
    alias(
        "frametime.base",
        MetricKind::Gauge,
        Unit::Ms,
        "Base frame time",
        Some("FT"),
        "Mean time between the frames the game rendered (1000 ÷ base FPS). The frame-time graph draws them as its second line while frame generation is on.",
        Source::BaseFrameTime,
    );
    alias("fps.generated", MetricKind::Gauge, Unit::Fps, "Generated FPS", Some("GEN"), "Presents beyond the first of each game frame, per second (the Sight layer's Reflex counting).", Source::CounterRate(ROLE_GENERATED_COUNT));
    alias(
        "fg.ratio",
        MetricKind::Gauge,
        Unit::Ratio,
        "FG ratio",
        Some("FG"),
        "Presents per game frame the Sight layer sees (needs Reflex sleeps or markers). Smooth Motion and lsfg-vk sit above the layer and aren't counted.",
        Source::FgRatio,
    );
    alias("fg.active", MetricKind::State, Unit::None, "FG active", None, "Frame generation is on: the layer counts game frames and presents clearly outrun them.", Source::FgActive);
    alias("fg.state", MetricKind::State, Unit::None, "Frame generation", Some("FG"), "A frame-generation state from an agnostic source (none yet).", Source::Alias(ROLE_FG_STATE));
    alias(
        "latency.render",
        MetricKind::Gauge,
        Unit::Ms,
        "Render latency",
        Some("LAT"),
        "Simulation start to GPU end, from the driver's Reflex reports (the Sight layer's opt-in vkGetLatencyTimingsNV reads).",
        Source::Alias(ROLE_LATENCY_RENDER),
    );
    alias(
        "latency.pc",
        MetricKind::Gauge,
        Unit::Ms,
        "PC latency (to present)",
        Some("PC"),
        "Simulation start (or the marked input sample) to the frame's present returning at the Sight layer, from Reflex markers. Excludes queued GPU work and scan-out.",
        Source::Alias(ROLE_LATENCY_PC),
    );
    // The frame's latency split into stages (SIGHT-HUD §1.1's stacked bar), each a gauge in
    // ms. Not generic roles: the `nvidia` producer fills them from the layer's opt-in timing
    // reports, and a mod with Reflex markers of its own may too.
    for (role, label, short, help) in LATENCY_SPLIT {
        alias(role, MetricKind::Gauge, Unit::Ms, label, Some(short), help, Source::Alias(role));
    }
    alias("upscaler", MetricKind::Text, Unit::None, "Upscaler", None, "The upscaler and preset NGX logged (\"DLSS K\"; opt-in NGX log).", Source::Alias(ROLE_UPSCALER));
    alias("render.resolution", MetricKind::Text, Unit::None, "Render resolution", None, "DLSS's input size at feature creation, from NGX's log.", Source::Alias(ROLE_RENDER_RES));
    alias("output.resolution", MetricKind::Text, Unit::None, "Output resolution", None, "DLSS's output size from NGX's log, else the swapchain's size from the Sight layer.", Source::OutputResolution);
    alias("session.time", MetricKind::Gauge, Unit::S, "Session time", Some("TIME"), "Time since the game started.", Source::SessionTime);
    alias("clock.time", MetricKind::Text, Unit::None, "Clock", None, "Local time.", Source::Clock);
    for c in &mut out {
        if c.desc.id == "fg.active" {
            c.desc.values = on_off();
        }
        if c.desc.id == "frametime" || c.desc.id == "frametime.p99" {
            c.desc.thresholds = vec![above(33.4, Level::Warn), above(50.0, Level::Bad)];
        }
        if c.desc.id == "latency.render" || c.desc.id == "latency.pc" {
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
    // Where the GPU numbers come from, for the Sight panel's tiles ("GPU · nvml").
    let mut source = d("gpu.source", MetricKind::Text, Unit::None, "GPU sensors", None, "Where the GPU metrics are read: nvml (NVIDIA's library) or sysfs (the kernel driver's files).", );
    source.group = Some("GPU".into());
    source.visibility = Some(quiver_perf::descriptor::Visibility::Advanced);
    v.push(source);
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
                x.range = Some(quiver_perf::descriptor::Range::Fixed { min: 0.0, max: 100.0 });
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
    let mut tearing = d("display.tearing", MetricKind::State, Unit::None, "Tearing", Some("TEAR"), "The game's monitor is tearing (immediate presents).");
    tearing.values = on_off();
    let mut path = d("display.path", MetricKind::Text, Unit::None, "Display path", None, "`direct` (scan-out) or `composited`.");
    let mut blocked = d(
        "display.blocked",
        MetricKind::Text,
        Unit::None,
        "Scan-out blocked by",
        None,
        "What keeps Hyprland from scanning the game out directly, in words (its directScanoutBlockedBy), or none.",
    );
    for x in [&mut mode, &mut refresh, &mut vrr, &mut tearing, &mut path, &mut blocked] {
        x.group = Some("Display".into());
        x.sample = Some(SampleRate::Hz(1.0));
    }
    vec![mode, refresh, vrr, tearing, path, blocked]
}

/// `tweaks.summary`: the Full preset's chip row (`CAP 160 · DLSS K · LL on`).
pub fn tweaks_summary() -> MetricDescriptor {
    let mut s = d("tweaks.summary", MetricKind::Text, Unit::None, "Quiver Tweaks", Some("TWK"), "The tweaks this session applies; a ? marks one nothing has confirmed yet.");
    s.group = Some("Tweaks".into());
    s
}

/// `tweaks.<id>` (what it does, in words) and `tweaks.<id>.effect` (its effect).
pub fn tweak_metrics(id: &str, label: &str) -> Vec<MetricDescriptor> {
    let mut text = d(&format!("tweaks.{id}"), MetricKind::Text, Unit::None, label, None, "The tweak's value and how it reaches the game.");
    let mut state = d(&format!("tweaks.{id}.effect"), MetricKind::State, Unit::None, &format!("{label}: effect"), None, "Whether the game used it: confirmed, unconfirmed, provided, and so on.");
    state.values = ["applied", "confirmed", "measured", "unconfirmed", "provided", "superseded", "unavailable", "failed"]
        .iter()
        .map(|v| StateValue {
            value: (*v).into(),
            label: Some((*v).into()),
            level: Some(match *v {
                "confirmed" | "measured" | "provided" => Level::Good,
                "unavailable" | "failed" => Level::Warn,
                _ => Level::Dim,
            }),
        })
        .collect();
    for x in [&mut text, &mut state] {
        x.group = Some("Tweaks".into());
    }
    vec![text, state]
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
