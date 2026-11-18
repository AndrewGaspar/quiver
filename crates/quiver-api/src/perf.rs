//! Performance metrics and the pinned HUD (design/SIGHT-HUD.md §3.11).
//!
//! quiverd is the hub: per running session it keeps a registry of metrics (descriptors from
//! the Sight layer, its own sensors, the display and mods), aggregates them (1 s, 10 s and
//! session windows) and formats every value, so the CLI, agents, the app and the HUD show the
//! same strings (decision 3).
//!
//! Requests go in [`Request::Perf`](crate::Request::Perf). Events that only some connections
//! get (a subscription's updates, an attached HUD renderer's models) and the perf broadcasts
//! are [`Event`](crate::Event)s on the wire (`Event::PerfUpdate`, `HudModel`, …; daemons from
//! before that sent [`Reply::Perf`](crate::Reply::Perf), still read);
//! [`Client::subscribe_perf`](crate::Client::subscribe_perf) receives them as [`PerfEvent`]s.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::GameRef;
pub use quiver_perf::descriptor::{Level, MetricDescriptor, MetricKind, Unit, Visibility};

// ---------------------------------------------------------------------------------------
// Requests

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PerfRequest {
    /// Running sessions with perf state (`Vec<PerfSessionInfo>`).
    Sessions,
    /// The metric catalog (`Vec<MetricInfo>`) of a running session, picked by session id
    /// or product id. With a product id and no running session: the static catalog
    /// (built-in metrics; mods' static descriptors once MOD-CONFIG ships them).
    Catalog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The game: a Ubisoft product goes out as `product_id` (as always), another
        /// store's game as `game` (`steam:620`).
        #[serde(flatten, with = "crate::game::persisted::option")]
        game: Option<GameRef>,
    },
    /// Receive [`PerfEvent::Update`] on this connection every `every_ms` (≥ 100) for the
    /// metrics `select` names (ids, roles, or globs such as `gpu.*`; empty: the headline
    /// set). Returns the subscription id (`u64`). ≤ 8 per connection; dropped on disconnect.
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The game: a Ubisoft product goes out as `product_id` (as always), another
        /// store's game as `game` (`steam:620`).
        #[serde(flatten, with = "crate::game::persisted::option")]
        game: Option<GameRef>,
        #[serde(default)]
        select: Vec<String>,
        #[serde(default = "default_every_ms")]
        every_ms: u32,
        /// The window the stats are over: 1000 (the HUD's), 10000, or 0 for the session.
        #[serde(default = "default_window_ms")]
        window_ms: u32,
        /// Empty: each metric's default (`mean` for per-frame metrics, `last` otherwise).
        #[serde(default)]
        stats: Vec<Stat>,
    },
    Unsubscribe { sub: u64 },
    /// HUD settings at a scope, and what they resolve to there (`HudSettingsView`).
    HudGet {
        #[serde(default)]
        scope: HudScopeRef,
    },
    /// Change HUD settings at a scope: fields set in `set` are stored, keys in `unset`
    /// (`layout`, `on`, `enabled`, …) removed. Returns `HudSettingsView`.
    HudSet {
        #[serde(default)]
        scope: HudScopeRef,
        #[serde(default)]
        set: HudScope,
        #[serde(default)]
        unset: Vec<String>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Show or hide the HUD of a running game (`HudStatus`). The state is remembered as
    /// the game's last one (decision 12). No session named: the focused game, else the
    /// only running one.
    HudToggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The game: a Ubisoft product goes out as `product_id` (as always), another
        /// store's game as `game` (`steam:620`).
        #[serde(flatten, with = "crate::game::persisted::option")]
        game: Option<GameRef>,
    },
    /// Next preset for a running game (`HudStatus`); shows the HUD if it was hidden.
    HudNext {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The game: a Ubisoft product goes out as `product_id` (as always), another
        /// store's game as `game` (`steam:620`).
        #[serde(flatten, with = "crate::game::persisted::option")]
        game: Option<GameRef>,
    },
    /// `quiver-sight` only: this connection receives [`PerfEvent::HudModel`] for the
    /// session from now on. Returns the current `HudModel`.
    HudAttach { session: String },
    /// Presenter in use and why, image age, layer draw counter, last error (`HudStatus`).
    HudStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The game: a Ubisoft product goes out as `product_id` (as always), another
        /// store's game as `game` (`steam:620`).
        #[serde(flatten, with = "crate::game::persisted::option")]
        game: Option<GameRef>,
    },
    /// Built-in presets and the user's layout files (`Vec<HudLayoutInfo>`).
    HudLayouts,
    /// A layout resolved to its rows (`HudLayout`): a built-in id or a user layout's id.
    HudLayout { id: String },
    /// The HUD's keys (`HudKeybinds`): read in the game by the Sight layer, each running
    /// game's state, and the Hyprland binds fallback as data and config text (never applied).
    HudKeybinds,
    /// What a launch of the game would get from the HUD (`LaunchPerfPlan`): environment,
    /// layer order, MangoHud remap. Read-only; for launchers (`quiver steam-run`) and tests.
    LaunchEnv {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// A game's play sessions with their performance summaries, newest first
    /// (`Vec<SessionSummary>`, at most `limit`).
    Summaries {
        #[serde(flatten)]
        game: crate::game::GameField,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    /// Development: start (or with `stop`, end) a perf session without a game, with a
    /// session dir, so a test writer can play the Sight layer. Returns `PerfSessionInfo`.
    DevSession {
        product_id: u32,
        #[serde(default)]
        stop: bool,
        /// Also start `quiver-sight` (as a launch would), so the HUD renders and toasts
        /// show for the dev session. Off by default: tests' dev sessions draw nothing.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sight: bool,
    },
}

fn default_limit() -> usize {
    20
}

fn default_every_ms() -> u32 {
    1000
}

fn default_window_ms() -> u32 {
    1000
}

/// A statistic over a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stat {
    /// The latest value.
    Last,
    Mean,
    Min,
    Max,
    P50,
    P95,
    P99,
    /// 1000 ÷ the 99th-percentile frame time (CapFrameX P1; decision 11). For `fps`-like
    /// metrics only.
    P1low,
    /// 1000 ÷ the 99.9th-percentile frame time.
    P01low,
    /// Counters: per second.
    Rate,
    /// Counters: the total.
    Total,
}

impl Stat {
    pub fn as_str(self) -> &'static str {
        match self {
            Stat::Last => "last",
            Stat::Mean => "mean",
            Stat::Min => "min",
            Stat::Max => "max",
            Stat::P50 => "p50",
            Stat::P95 => "p95",
            Stat::P99 => "p99",
            Stat::P1low => "p1low",
            Stat::P01low => "p01low",
            Stat::Rate => "rate",
            Stat::Total => "total",
        }
    }

    pub fn parse(s: &str) -> Option<Stat> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
    }
}

// ---------------------------------------------------------------------------------------
// Events

/// Perf events, on the wire as `{"perf": {...}}` ([`Reply::Perf`](crate::Reply::Perf)).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PerfEvent {
    /// One subscription's values (only to the connection that subscribed).
    Update(PerfUpdate),
    /// The HUD model for an attached session (only to the `HudAttach` connection).
    HudModel(HudModel),
    /// A session's catalog changed (a producer came or went, a mod declared metrics).
    CatalogChanged { session: String, product_id: u32 },
    /// HUD settings or a running HUD's state changed.
    HudChanged {
        /// The Ubisoft product whose settings changed (`None`: the global scope, or another
        /// store's game).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// A perf session started or ended (with its summary when it ended).
    Session {
        session: String,
        /// 0 for another store's game (then `game` names it).
        product_id: u32,
        ended: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<Box<PerfSummary>>,
        /// Another store's game (appended; Ubisoft sessions leave it out).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        game: Option<GameRef>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerfUpdate {
    pub sub: u64,
    pub session: String,
    pub product_id: u32,
    pub at: DateTime<Utc>,
    /// metric id → stat → value.
    pub values: BTreeMap<String, BTreeMap<Stat, PerfValue>>,
}

/// One value, already formatted by quiverd.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PerfValue {
    /// The number, for numeric metrics (in the descriptor's unit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// As shown: "158", "12.7", "6.1 GB", "on". `—` when there is no value.
    pub text: String,
    /// The unit as shown after the text ("ms", "%", "°", "W"); empty when none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    #[serde(default, skip_serializing_if = "is_normal")]
    pub level: Level,
    /// The metric is stale (no data within twice its expected period).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

fn is_normal(l: &Level) -> bool {
    *l == Level::Normal
}

impl PerfValue {
    /// "12.7 ms", "97%", "71°".
    pub fn display(&self) -> String {
        match self.unit.as_str() {
            "" => self.text.clone(),
            u if u.starts_with(['%', '°', '×']) => format!("{}{u}", self.text),
            u => format!("{} {u}", self.text),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Sessions and the catalog

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSessionInfo {
    pub session: String,
    /// 0 for another store's game (then `game` names it).
    pub product_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub started: DateTime<Utc>,
    /// `/dev/shm/quiver-<uid>/<session>` when the HUD is enabled for the launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    pub hud_enabled: bool,
    pub producers: Vec<ProducerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presenter: Option<Presenter>,
    /// Started with `DevSession` (no game).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dev: bool,
    /// Another store's game (appended; Ubisoft sessions leave it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
}

impl PerfSessionInfo {
    pub fn game_ref(&self) -> GameRef {
        self.game.unwrap_or(GameRef::ubisoft(self.product_id))
    }
}

/// A producer of a session's metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProducerInfo {
    /// `layer`, `system`, `display`, `quiverd`, `mod:<surface>`, `bridge`.
    pub id: String,
    /// "Sight layer", "System", "DLSS mod".
    pub label: String,
    pub connected: bool,
    /// `none`, `ok`, or `invalid: <reason>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ring: String,
    /// `ok` or `suspect` (a clock that doesn't line up, §3.9).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub clock: String,
    pub metrics: usize,
    /// Ring records read, lost (overrun or stuck) and rejected (failed checks).
    #[serde(default)]
    pub records: u64,
    #[serde(default)]
    pub lost: u64,
    #[serde(default)]
    pub rejected: u64,
    /// One line: pid, device, why it's off.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricState {
    /// In the catalog, no data yet.
    Declared,
    /// Data within twice its expected period.
    Live,
    /// No recent data: the HUD shows `—`.
    Stale,
    /// Its producer disconnected.
    Gone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricInfo {
    pub descriptor: MetricDescriptor,
    /// The producer id ([`ProducerInfo::id`]); `quiverd` for canonical metrics.
    pub producer: String,
    pub producer_label: String,
    /// A canonical metric (no namespace, §3.4): derived by quiverd from a role.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub canonical: bool,
    /// For canonical metrics: the metric (or role) it comes from right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Roles this metric fills (bound), and roles it claimed but lost to another producer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    pub state: MetricState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<PerfValue>,
}

/// A play session's performance summary (`PlaySession.perf`, decision 13): ~1 KB.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PerfSummary {
    /// Seconds covered by frame data (layer presents), and by sensor data.
    #[serde(default)]
    pub frame_seconds: f64,
    #[serde(default)]
    pub sensor_seconds: f64,
    #[serde(default)]
    pub frames: u64,
    /// Presented FPS: mean, 1% and 0.1% lows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<FpsStats>,
    /// Base (real-frame) FPS when a mod provides it; mean only in this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps_base: Option<FpsStats>,
    /// Presented frame time, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frametime_ms: Option<Percentiles>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg_ratio: Option<f64>,
    /// Render latency, ms (a mod's gauge in this version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<MeanMax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_load: Option<MeanMax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_temp: Option<MeanMax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_power: Option<MeanMax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_load: Option<MeanMax>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_temp: Option<MeanMax>,
    /// Peak VRAM used, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_peak: Option<u64>,
    /// Frames slower than 4× the median frame time.
    #[serde(default)]
    pub hitches: u64,
    /// The GPU the numbers are from ("NVIDIA GeForce RTX 5070 Laptop GPU").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    /// Who drew the HUD for most of the session (a desktop HUD costs scan-out and VRR
    /// cadence, so those numbers are flagged); `None`: the HUD was off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presenter: Option<Presenter>,
}

/// A recorded play session and its summary (`Summaries`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub started: DateTime<Utc>,
    pub seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub perf: PerfSummary,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct FpsStats {
    pub mean: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p1low: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p01low: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct MeanMax {
    pub mean: f64,
    pub max: f64,
}

// ---------------------------------------------------------------------------------------
// HUD settings

/// Where a HUD setting lives: global (no product), per game, or per profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct HudScopeRef {
    /// The game (none: the global scope). A Ubisoft product is written `product_id`, as
    /// before; another store's game `game` (`steam:620`).
    #[serde(flatten, with = "crate::game::persisted::option")]
    pub game: Option<GameRef>,
    /// A profile id (the daemon resolves names). Needs a game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl HudScopeRef {
    pub fn global() -> Self {
        Self::default()
    }

    /// A Ubisoft product's scope.
    pub fn game(product_id: u32) -> Self {
        Self::of(GameRef::ubisoft(product_id))
    }

    /// A game's scope, any store.
    pub fn of(game: GameRef) -> Self {
        Self { game: Some(game), profile: None }
    }

    /// The Ubisoft product, for a Ubisoft game's scope.
    pub fn product_id(&self) -> Option<u32> {
        self.game.and_then(|g| g.as_ubisoft())
    }

    pub fn label(&self) -> String {
        let name = |g: &GameRef| g.as_ubisoft().map(|p| p.to_string()).unwrap_or_else(|| g.to_string());
        match (&self.game, &self.profile) {
            (None, _) => "global".into(),
            (Some(g), None) => format!("game {}", name(g)),
            (Some(g), Some(prof)) => format!("game {} profile {prof}", name(g)),
        }
    }
}

/// HUD settings at one scope; unset fields come from the next scope out (profile > game >
/// global > defaults, §1.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HudScope {
    /// Quiver's HUD is set up for the game's launches: the session dir, the Sight layer and
    /// `hud.image`. Off by default for now; nothing about a launch changes while it's off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Shown at launch (the toggle chord changes it for the session and remembers it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    /// A preset (`fps`, `compact`, `standard`, `full`, `clock`) or a user layout id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presenter: Option<PresenterSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captures: Option<Captures>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// Logical px from the anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<[f32; 2]>,
    /// 0.75–2.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    /// Background opacity 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<f32>,
    /// Text opacity 0.5–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_opacity: Option<f32>,
    /// Colour values by their threshold level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_colors: Option<bool>,
    /// Frame-time graph span, ms (1000–10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_span_ms: Option<u32>,
    /// Extra margin, percent of the frame (TVs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_area: Option<f32>,
    /// Number updates per second: 1, 2, 4 or 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<u32>,
}

/// HUD settings resolved for a game and profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HudConfig {
    pub enabled: bool,
    pub on: bool,
    pub layout: String,
    pub presenter: PresenterSetting,
    pub captures: Captures,
    pub anchor: Anchor,
    pub offset: [f32; 2],
    pub scale: f32,
    pub background: f32,
    pub text_opacity: f32,
    pub level_colors: bool,
    pub graph_span_ms: u32,
    pub safe_area: f32,
    pub rate_hz: u32,
}

impl Default for HudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on: false,
            layout: "compact".into(),
            presenter: PresenterSetting::Auto,
            captures: Captures::Shown,
            anchor: Anchor::TopLeft,
            offset: [12.0, 12.0],
            scale: 1.0,
            background: 0.45,
            text_opacity: 1.0,
            level_colors: true,
            graph_span_ms: 3000,
            safe_area: 0.0,
            rate_hz: 2,
        }
    }
}

impl HudConfig {
    /// `self` with `s`'s set fields on top, clamped to their ranges.
    pub fn with(mut self, s: &HudScope) -> Self {
        if let Some(v) = s.enabled {
            self.enabled = v;
        }
        if let Some(v) = s.on {
            self.on = v;
        }
        if let Some(v) = &s.layout {
            self.layout = v.clone();
        }
        if let Some(v) = s.presenter {
            self.presenter = v;
        }
        if let Some(v) = s.captures {
            self.captures = v;
        }
        if let Some(v) = s.anchor {
            self.anchor = v;
        }
        if let Some(v) = s.offset {
            self.offset = v.map(|x| if x.is_finite() { x.clamp(-4096.0, 4096.0) } else { 0.0 });
        }
        if let Some(v) = s.scale.filter(|v| v.is_finite()) {
            self.scale = v.clamp(0.75, 2.5);
        }
        if let Some(v) = s.background.filter(|v| v.is_finite()) {
            self.background = v.clamp(0.0, 1.0);
        }
        if let Some(v) = s.text_opacity.filter(|v| v.is_finite()) {
            self.text_opacity = v.clamp(0.5, 1.0);
        }
        if let Some(v) = s.level_colors {
            self.level_colors = v;
        }
        if let Some(v) = s.graph_span_ms {
            self.graph_span_ms = v.clamp(1000, 10_000);
        }
        if let Some(v) = s.safe_area.filter(|v| v.is_finite()) {
            self.safe_area = v.clamp(0.0, 10.0);
        }
        if let Some(v) = s.rate_hz {
            self.rate_hz = [1u32, 2, 4, 10].into_iter().min_by_key(|r| r.abs_diff(v)).unwrap_or(2);
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudSettingsView {
    pub scope: HudScopeRef,
    /// What is stored at this scope.
    pub set: HudScope,
    /// What it resolves to at this scope (with the outer scopes and defaults).
    pub effective: HudConfig,
    pub revision: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenterSetting {
    /// The game presenter when the layer draws, else the desktop presenter (§2.6).
    #[default]
    Auto,
    Game,
    Desktop,
}

/// Who puts the HUD on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presenter {
    /// The Sight layer composites `hud.image` into the game's swapchain.
    Game,
    /// `quiver-sight` draws a click-through layer-shell surface (costs scan-out and VRR
    /// cadence while shown, §2.1).
    Desktop,
}

/// "HUD in captures" (§2.12, decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Captures {
    #[default]
    Shown,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor {
    #[default]
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

// ---------------------------------------------------------------------------------------
// The HUD model (quiverd → quiver-sight)

/// The resolved layout with every value formatted, no pixels (§2.7). `quiver-sight` renders
/// it with the app's components into `hud.image` (game presenter) or a layer-shell surface
/// (desktop presenter). Sent whole on each change; a few KiB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HudModel {
    pub session: String,
    pub product_id: u32,
    /// Increments on every change; quiver-sight re-renders only when it moves, and writes
    /// it into the image's `model_rev`.
    pub rev: u64,
    /// Shown. When false quiver-sight publishes nothing (the layer draws nothing once the
    /// image is stale) and the desktop presenter unmaps.
    pub on: bool,
    /// Layout id and display name.
    pub layout: String,
    pub name: String,
    pub presenter: Presenter,
    /// Why this presenter ("layer drawing", "no Sight layer in the game", …).
    pub presenter_reason: String,
    pub anchor: Anchor,
    /// Logical px.
    pub offset: [f32; 2],
    pub scale: f32,
    /// Base text size in image pixels at scale 1 (1.6% of the swapchain height, §1.2).
    pub text_px: f32,
    pub background: f32,
    pub text_opacity: f32,
    pub level_colors: bool,
    pub orientation: Orientation,
    pub safe_area: f32,
    /// `hud.image` (game presenter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<PathBuf>,
    /// The swapchain the image is laid out for, from the layer's status block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<[u32; 2]>,
    /// `VkFormat` / `VkColorSpaceKHR` of that swapchain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vk_format: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<u32>,
    /// The monitor the game is on (desktop presenter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    pub captures: Captures,
    /// The card's fixed size, `[width, height]` in logical px at scale 1 (the layout's
    /// `width`/`height`, the height by default from its rows: [`hud_card`]). The card never
    /// changes size with its contents: rows pack from the top, values that don't fit are
    /// truncated. `[0, 0]` (a quiverd from before fixed sizes): `quiver-sight` derives one.
    #[serde(default)]
    pub size: [f32; 2],
    pub rows: Vec<HudRow>,
}

/// The HUD card's geometry (SIGHT-HUD §1.4), shared by quiverd (which sizes the card from
/// the layout) and `quiver-sight` (which lays the rows out in these slots). Logical px at
/// scale 1; the image's scale factor (text size × `scale`) or the desktop `scale` multiplies
/// them.
pub mod hud_card {
    use super::{HudCell, Orientation};

    /// A text row's slot height.
    pub const LINE: f32 = 20.0;
    /// One graph row (`rows = 2`: 30 px).
    pub const GRAPH_LINE: f32 = 15.0;
    /// Between rows (block) and between cells.
    pub const ROW_GAP: f32 = 4.0;
    pub const CELL_GAP: f32 = 8.0;
    /// Inside a panel (background on), and around bare text (background off).
    pub const PAD: [f32; 2] = [9.0, 6.0];
    pub const BARE_PAD: [f32; 2] = [2.0, 0.0];
    /// The panel's border, on every side.
    pub const BORDER: f32 = 1.0;
    /// A layout without a width: block and row (one line) orientations.
    pub const DEFAULT_WIDTH_BLOCK: f32 = 260.0;
    pub const DEFAULT_WIDTH_ROW: f32 = 640.0;
    /// What a layout may ask for.
    pub const MIN_WIDTH: f32 = 40.0;
    pub const MAX_WIDTH: f32 = 1200.0;
    pub const MIN_HEIGHT: f32 = 16.0;
    pub const MAX_HEIGHT: f32 = 1000.0;

    /// The built-in presets' widths: what their usual contents need with numbers at their
    /// reserved widths, plus a little room (longer values are truncated). quiverd's presets
    /// and `quiver-sight`'s demo models both use these.
    pub fn preset_width(id: &str) -> Option<f32> {
        Some(match id {
            "fps" => 100.0,
            "compact" => 780.0,
            "standard" => 260.0,
            "full" => 320.0,
            "clock" => 220.0,
            _ => return None,
        })
    }

    /// A panel is drawn (padding, border) when the background isn't (nearly) transparent.
    pub fn panel(background: f32) -> bool {
        background > 0.01
    }

    pub fn default_width(o: Orientation) -> f32 {
        match o {
            Orientation::Row => DEFAULT_WIDTH_ROW,
            Orientation::Block => DEFAULT_WIDTH_BLOCK,
        }
    }

    /// A row's slot: a graph's rows, else one line.
    pub fn slot(graph_rows: Option<u32>) -> f32 {
        match graph_rows {
            Some(r) => (r.clamp(1, 6) as f32 * GRAPH_LINE).max(LINE),
            None => LINE,
        }
    }

    /// The slot of a model row (its tallest cell).
    pub fn row_slot(cells: &[HudCell]) -> f32 {
        cells.iter().map(|c| slot(if let HudCell::Graph { rows, .. } = c { Some(*rows) } else { None })).fold(LINE, f32::max)
    }

    /// The card's height for rows with these slots: stacked (block) or the tallest (row:
    /// everything on one line), plus padding and border.
    pub fn height(o: Orientation, slots: &[f32], panel: bool) -> f32 {
        let content = match o {
            Orientation::Block => slots.iter().sum::<f32>() + ROW_GAP * slots.len().saturating_sub(1) as f32,
            Orientation::Row => slots.iter().copied().fold(0.0, f32::max),
        };
        let pad = if panel { PAD[1] + BORDER } else { BARE_PAD[1] };
        content.max(LINE) + 2.0 * pad
    }

    /// The content box's width for a card `width` wide.
    pub fn inner_width(width: f32, panel: bool) -> f32 {
        let pad = if panel { PAD[0] + BORDER } else { BARE_PAD[0] };
        (width - 2.0 * pad).max(0.0)
    }

    /// A requested size, clamped to what's allowed (non-finite: the fallback).
    pub fn clamp_width(w: f32, fallback: f32) -> f32 {
        if w.is_finite() { w.clamp(MIN_WIDTH, MAX_WIDTH) } else { fallback }
    }

    pub fn clamp_height(h: f32, fallback: f32) -> f32 {
        if h.is_finite() { h.clamp(MIN_HEIGHT, MAX_HEIGHT) } else { fallback }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    /// One line (Compact, FPS only).
    Row,
    #[default]
    Block,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HudRow {
    pub cells: Vec<HudCell>,
}

/// One cell of a HUD row. Values are formatted; levels are theme levels (§1.2), never colours.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HudCell {
    /// A number or text value with an optional dim prefix label ("FPS", "GPU").
    Value {
        metric: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        value: PerfValue,
    },
    /// A state or text shown as a pill ("FG 2×", "DLSS K").
    Badge {
        metric: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        value: PerfValue,
    },
    /// A bar: `fraction` of the metric's range filled.
    Bar {
        metric: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        value: PerfValue,
        fraction: f32,
    },
    /// A small line of recent values, each 0–1 of the range, oldest first.
    Sparkline {
        metric: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        value: PerfValue,
        points: Vec<f32>,
    },
    /// The live frame-time graph: a placeholder rectangle the layer fills on every present
    /// (game presenter) or quiver-sight draws from `points` (desktop presenter).
    Graph {
        series: Vec<String>,
        span_ms: u32,
        /// Rows of text height.
        rows: u32,
        min_ms: f32,
        /// 0: auto.
        max_ms: f32,
        /// 0: none.
        warn_above_ms: f32,
        bad_above_ms: f32,
        /// Recent presented frame times, ms (desktop presenter only; empty otherwise).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        points: Vec<f32>,
    },
    /// Literal text ("→", "·").
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_normal")]
        level: Level,
    },
    /// A thin divider between groups ("│").
    Separator,
}

/// A layout as quiverd resolved it (`HudLayout` request), before values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HudLayout {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub orientation: Orientation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<f32>,
    /// The card's fixed size, logical px at scale 1 (the height follows from the rows unless
    /// the layout sets one).
    #[serde(default)]
    pub size: [f32; 2],
    pub rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HudLayoutInfo {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Why a user layout can't be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `quiver hud status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HudStatus {
    pub session: String,
    /// 0 for another store's game (then `game` names it).
    pub product_id: u32,
    pub enabled: bool,
    pub on: bool,
    pub layout: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presenter: Option<Presenter>,
    pub presenter_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<HudImageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<LayerInfo>,
    /// A `quiver-sight` connection is attached to the session.
    pub renderer_attached: bool,
    pub model_rev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub captures: Captures,
    /// Another store's game (appended; Ubisoft sessions leave it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
}

impl HudStatus {
    pub fn game_ref(&self) -> GameRef {
        self.game.unwrap_or(GameRef::ubisoft(self.product_id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HudImageStatus {
    pub path: PathBuf,
    pub buffer_bytes: u32,
    pub published: u64,
    /// Since quiver-sight's last heartbeat; `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_pid: Option<u32>,
}

/// The Sight layer in the game, from its ring header and status block.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LayerInfo {
    pub pid: u32,
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_ms: Option<u64>,
    pub presents: u64,
    pub drawn: u64,
    pub drawing: bool,
    /// `none`, `format`, `usage`, `queue`, `device_lost`, `no_image`, `internal`, `panicked`.
    pub draw_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<[u32; 2]>,
    pub vk_format: u32,
    pub color_space: u32,
    pub present_mode: u32,
    pub hud_published: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub engine: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    pub vendor_id: u32,
    /// The HUD's keys in this game ([`HudKeysState::keys`]): `active` or `active_wayland`, or why the layer
    /// can't read them. Empty from a quiverd before in-game keys.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub keys: String,
    /// The same as a sentence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub keys_detail: String,
    /// Chord presses the layer read (Shift+F3, Ctrl+Shift+F3), wrapping at 65 536.
    #[serde(default)]
    pub key_toggles: u32,
    #[serde(default)]
    pub key_nexts: u32,
}

/// The HUD's keys (§1.3, decision 4). The Sight layer reads them inside the game (X11
/// keymap polling, or a keyboard on a Wayland-native game's own connection); the Hyprland binds are the fallback for games without the layer (the
/// desktop presenter), never applied by quiverd.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HudKeybinds {
    pub binds: Vec<Keybind>,
    /// Who reads the keys: `layer` (the Sight layer, in the game). Empty from a quiverd
    /// before in-game keys (it only described Hyprland binds).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub handler: String,
    /// One per running game with a Sight layer: whether it reads the keys, or why not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<HudKeysState>,
    /// `hyprctl keyword …` commands that would bind the keys while a Quiver game has focus
    /// (fallback; not run), and the ones that undo them.
    pub on_focus: Vec<String>,
    pub on_blur: Vec<String>,
    /// Hyprland config text for the same binds, for binding them statically for games the
    /// layer can't read keys in (then they fire everywhere, not only over Quiver games).
    pub conf: String,
    /// A Quiver game is focused right now.
    pub game_focused: bool,
    /// Whether quiverd applies the Hyprland binds (it doesn't: the layer reads the keys).
    pub applied: bool,
}

/// A running game's in-game HUD keys.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HudKeysState {
    pub session: String,
    pub product_id: u32,
    /// Another store's game (Ubisoft sessions leave it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
    /// The game process the layer is in.
    pub pid: u32,
    /// `active` (polling X11), `active_wayland` (a keyboard on the game's own Wayland
    /// connection, for Wayland-native windows), or why not: for a Wayland-native window
    /// `starting`, `no_libwayland`, `no_seat`, `no_keyboard`, `wayland_error`, `stopped`, or
    /// `wayland` (an older layer that can't read Wayland keys); otherwise `no_display`,
    /// `no_libx11`, `open_failed`, `no_keycodes`, `io_error`; and `off`
    /// (`QUIVER_SIGHT_KEYS=0`), `internal`, `unknown` (not reported yet, or an older layer).
    pub keys: String,
    pub detail: String,
}

/// Whether a `keys` value ([`HudKeysState::keys`], `LayerInfo::keys`) means the layer
/// reads the HUD's keys in the game.
pub fn keys_read_in_game(keys: &str) -> bool {
    matches!(keys, "active" | "active_wayland")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Keybind {
    /// Hyprland modifier list (`SHIFT`, `CTRL SHIFT`).
    pub mods: String,
    pub key: String,
    pub command: String,
    pub description: String,
}

/// What a launch gets from the perf hub and the HUD.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LaunchPerfPlan {
    pub hud: Option<HudConfig>,
    /// Environment added to the game's (and the bridge's).
    pub env: BTreeMap<String, String>,
    /// Vulkan layers, closest to the application first, that the capture setting implies
    /// (Sight below frame generation and post-processing; obs-vkcapture above Sight for
    /// `hidden`, below it for `shown`).
    pub layer_order: Vec<String>,
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    #[test]
    fn requests_wire() {
        let r: Request = serde_json::from_str(r#"{"method":"perf","params":{"op":"catalog","product_id":3539}}"#).unwrap();
        assert!(matches!(r, Request::Perf(PerfRequest::Catalog { session: None, game: Some(g) }) if g == GameRef::ubisoft(3539)));
        let r: Request = serde_json::from_str(r#"{"method":"perf","params":{"op":"subscribe","session":"s-1"}}"#).unwrap();
        let Request::Perf(PerfRequest::Subscribe { every_ms, window_ms, select, stats, .. }) = r else { panic!() };
        assert_eq!((every_ms, window_ms), (1000, 1000));
        assert!(select.is_empty() && stats.is_empty());
        let r: Request = serde_json::from_str(r#"{"method":"perf","params":{"op":"hud_set","scope":{"product_id":1},"set":{"layout":"standard","captures":"hidden","anchor":"bottom-right"}}}"#).unwrap();
        let Request::Perf(PerfRequest::HudSet { scope, set, .. }) = r else { panic!() };
        assert_eq!(scope, HudScopeRef::game(1));
        assert_eq!(set.captures, Some(Captures::Hidden));
        assert_eq!(set.anchor, Some(Anchor::BottomRight));
        let v = serde_json::to_value(Request::Perf(PerfRequest::HudToggle { session: None, game: None })).unwrap();
        assert_eq!(v, serde_json::json!({"method": "perf", "params": {"op": "hud_toggle"}}));
        // A Ubisoft game is written as before; another store's game by its ref.
        let v = serde_json::to_value(Request::Perf(PerfRequest::HudStatus { session: None, game: Some(GameRef::ubisoft(3539)) })).unwrap();
        assert_eq!(v, serde_json::json!({"method": "perf", "params": {"op": "hud_status", "product_id": 3539}}));
        let v = serde_json::to_value(Request::Perf(PerfRequest::HudStatus { session: None, game: Some(GameRef::steam(870780)) })).unwrap();
        assert_eq!(v, serde_json::json!({"method": "perf", "params": {"op": "hud_status", "game": "steam:870780"}}));
        let back: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Request::Perf(PerfRequest::HudStatus { game: Some(g), .. }) if g == GameRef::steam(870780)));
        // HUD scopes: Ubisoft as before, Steam by ref; both read back.
        assert_eq!(serde_json::to_value(HudScopeRef::game(1)).unwrap(), serde_json::json!({"product_id": 1}));
        assert_eq!(serde_json::to_value(HudScopeRef::global()).unwrap(), serde_json::json!({}));
        let steam = HudScopeRef::of(GameRef::steam(620));
        assert_eq!(serde_json::to_value(&steam).unwrap(), serde_json::json!({"game": "steam:620"}));
        assert_eq!(serde_json::from_value::<HudScopeRef>(serde_json::json!({"game": "steam:620"})).unwrap(), steam);
        assert_eq!(steam.label(), "game steam:620");
        let r: Request = serde_json::from_str(r#"{"method":"perf","params":{"op":"hud_set","scope":{"game":"steam:620"},"set":{"enabled":true}}}"#).unwrap();
        let Request::Perf(PerfRequest::HudSet { scope, .. }) = r else { panic!() };
        assert_eq!(scope, steam);
        let r: Request = serde_json::from_str(r#"{"method":"perf","params":{"op":"launch_env","game":"steam:620"}}"#).unwrap();
        assert!(matches!(r, Request::Perf(PerfRequest::LaunchEnv { game, .. }) if game == GameRef::steam(620)));
        assert_eq!(Stat::parse("p1low"), Some(Stat::P1low));
        assert_eq!(Stat::P01low.as_str(), "p01low");
    }

    #[test]
    fn events_wire() {
        let e = PerfEvent::CatalogChanged { session: "s-1".into(), product_id: 5 };
        let line = serde_json::to_string(&crate::Reply::Perf(e)).unwrap();
        assert_eq!(line, r#"{"perf":{"type":"catalog_changed","session":"s-1","product_id":5}}"#);
        // Clients from before perf don't know the reply and skip it (not a parse of Event).
        assert!(serde_json::from_str::<crate::Event>(&line).is_err());
        let cell = HudCell::Value { metric: "fps".into(), label: Some("FPS".into()), value: PerfValue { value: Some(158.0), text: "158".into(), ..Default::default() } };
        let v = serde_json::to_value(&cell).unwrap();
        assert_eq!(v["kind"], "value");
        assert_eq!(v["value"]["text"], "158");
        assert!(v["value"].get("level").is_none());
    }

    #[test]
    fn config_resolution_clamps() {
        let c = HudConfig::default().with(&HudScope { scale: Some(9.0), rate_hz: Some(3), background: Some(f32::NAN), ..Default::default() });
        assert_eq!(c.scale, 2.5);
        assert!([2, 4].contains(&c.rate_hz));
        assert_eq!(c.background, 0.45);
        assert!(!c.enabled, "the HUD is off by default");
        let pv = PerfValue { text: "12.7".into(), unit: "ms".into(), ..Default::default() };
        assert_eq!(pv.display(), "12.7 ms");
        let pv = PerfValue { text: "97".into(), unit: "%".into(), ..Default::default() };
        assert_eq!(pv.display(), "97%");
    }

    #[test]
    fn card_geometry() {
        use hud_card::*;
        let graph = HudCell::Graph { series: vec![], span_ms: 3000, rows: 2, min_ms: 0.0, max_ms: 0.0, warn_above_ms: 0.0, bad_above_ms: 0.0, points: vec![] };
        assert_eq!(row_slot(&[HudCell::Separator]), LINE);
        assert_eq!(row_slot(&[graph]), 30.0);
        assert_eq!(slot(Some(4)), 60.0);
        // Three lines and a graph in a panel: 20+20+30+20, three gaps, padding and border.
        assert_eq!(height(Orientation::Block, &[LINE, LINE, 30.0, LINE], true), 90.0 + 12.0 + 14.0);
        assert_eq!(height(Orientation::Row, &[LINE, 30.0], false), 30.0);
        assert_eq!(height(Orientation::Block, &[], false), LINE);
        assert_eq!(inner_width(260.0, true), 240.0);
        assert_eq!(clamp_width(5.0, 1.0), MIN_WIDTH);
        assert_eq!(clamp_width(f32::NAN, 260.0), 260.0);
        assert_eq!(clamp_height(1e9, 1.0), MAX_HEIGHT);
        // A model from before fixed sizes parses with size [0, 0].
        let m: HudModel = serde_json::from_value(serde_json::json!({
            "session": "s-1", "product_id": 1, "rev": 1, "on": true, "layout": "fps", "name": "FPS",
            "presenter": "game", "presenter_reason": "", "anchor": "top-left", "offset": [12.0, 12.0], "scale": 1.0,
            "text_px": 17.0, "background": 0.0, "text_opacity": 1.0, "level_colors": true, "orientation": "row",
            "safe_area": 0.0, "captures": "shown", "rows": []
        }))
        .unwrap();
        assert_eq!(m.size, [0.0, 0.0]);
    }
}
