//! `quiver perf …` and `quiver hud …`: performance metrics and the pinned HUD
//! (design/SIGHT-HUD.md). Read-only except `hud` settings and the toggle.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_api::perf::{
    Anchor, Captures, HudKeybinds, HudLayout, HudLayoutInfo, HudScope, HudScopeRef, HudSettingsView, HudStatus, LaunchPerfPlan, MetricInfo, MetricState, PerfEvent, PerfRequest,
    PerfSessionInfo, PerfSummary, PerfUpdate, PerfValue, PresenterSetting, SessionSummary, Stat,
};
use quiver_api::{Client, Event, GameRef, Request};

use crate::print_json;

#[derive(Subcommand)]
pub enum PerfAction {
    /// Live numbers for a running game, one line per update (or JSON lines).
    Watch {
        /// Game (name or product id); default: the running one.
        game: Option<String>,
        /// Metric ids, roles or globs (`gpu.*`), comma-separated; default: the headline set.
        #[arg(long, value_delimiter = ',')]
        metrics: Vec<String>,
        /// Statistics (`mean`, `p1low`, `max`, …), comma-separated; default per metric.
        #[arg(long, value_delimiter = ',')]
        stats: Vec<String>,
        /// Update interval (`500ms`, `1s`, `2s`; at least 100 ms).
        #[arg(long, default_value = "1s")]
        every: String,
        /// Window the stats cover: `1s`, `10s` or `session`.
        #[arg(long, default_value = "1s")]
        window: String,
        /// Exit after this many updates.
        #[arg(long)]
        count: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// What's measured for a game and who provides it (a running game's live catalog, or
    /// the built-in one).
    Catalog {
        game: Option<String>,
        /// Include debug-level metrics.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Running sessions with perf state: producers, presenter.
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Performance summaries of a game's recent play sessions.
    History {
        game: String,
        #[arg(short, default_value_t = 10)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// Development: a perf session without a game, with a session dir a test writer can
    /// play the Sight layer in. Prints its environment.
    #[command(hide = true)]
    DevSession {
        game: String,
        #[arg(long)]
        stop: bool,
        /// Also start quiver-sight for it (HUD renderer and toasts), as a launch does.
        #[arg(long)]
        sight: bool,
    },
}

#[derive(Subcommand)]
pub enum HudAction {
    /// Show or hide the HUD of the running game (Shift+F3).
    Toggle { game: Option<String> },
    /// The next preset: FPS only, Compact, Standard, Full (Ctrl+Shift+F3).
    Next { game: Option<String> },
    /// Which presenter draws the HUD and why, the layer's state, the image's age.
    Status {
        game: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Layouts: `list`, `show ID`, `set ID [--game G [--profile P]]`.
    Layout {
        #[command(subcommand)]
        action: LayoutAction,
    },
    /// Use Quiver's HUD for a game's launches (or every game's, without GAME). Off by default.
    Enable {
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Stop using Quiver's HUD for a game's launches (launches go back to unchanged).
    Disable {
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
    /// "HUD in captures": `shown` or `hidden` (OBS game capture follows Vulkan layer order).
    Captures {
        value: String,
        #[arg(long)]
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
    /// HUD settings at a scope: `key=value` to set, `key=` to unset. Keys: enabled, on,
    /// layout, presenter (auto|game|desktop), captures, anchor (top-left, …), offset (x,y),
    /// scale, background, text_opacity, level_colors, graph_span (ms), safe_area, rate (Hz).
    Set {
        #[arg(required = true)]
        changes: Vec<String>,
        #[arg(long)]
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// HUD settings at a scope and what they resolve to.
    Get {
        #[arg(long)]
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The HUD's keys: read in the game by the Sight layer, and whether each running game's
    /// layer can. `--conf` prints Hyprland binds, the fallback for games without the layer.
    Keybinds {
        #[arg(long)]
        conf: bool,
        #[arg(long)]
        json: bool,
    },
    /// The environment a launch of the game gets from the HUD (session dir, layer, order).
    Env {
        game: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum LayoutAction {
    List {
        #[arg(long)]
        json: bool,
    },
    Show { id: String },
    /// Pick the layout at a scope (global without --game).
    Set {
        id: String,
        #[arg(long)]
        game: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
}

/// A product id from a game argument: a number as is, else an installed game's name.
async fn product(client: &Client, game: &str) -> Result<u32> {
    if let Ok(id) = game.parse() {
        return Ok(id);
    }
    Ok(crate::mods::resolve_installed(client, game).await?.product_id)
}

/// A game from an argument: a ref as is (`3539`, `steam:870780`), else an installed Ubisoft
/// game's name, else a Steam game's (the HUD works for Steam games started through
/// `quiver steam-run`).
pub(crate) async fn game_ref(client: &Client, game: &str) -> Result<GameRef> {
    if let Some(r) = GameRef::parse_ref(game) {
        return Ok(r);
    }
    match crate::mods::resolve_installed(client, game).await {
        Ok(g) => Ok(GameRef::ubisoft(g.product_id)),
        Err(e) => match crate::resolve_any(client, game).await {
            Ok(x) if x.game.as_steam().is_some() => Ok(x.game),
            _ => Err(e),
        },
    }
}

async fn game_opt(client: &Client, game: Option<&str>) -> Result<Option<GameRef>> {
    match game {
        Some(g) => Ok(Some(game_ref(client, g).await?)),
        None => Ok(None),
    }
}

async fn scope(client: &Client, game: Option<&str>, profile: Option<String>) -> Result<HudScopeRef> {
    let game = game_opt(client, game).await?;
    if profile.is_some() && game.is_none() {
        bail!("--profile needs --game");
    }
    Ok(HudScopeRef { game, profile })
}

fn perf(r: PerfRequest) -> Request {
    Request::Perf(r)
}

fn millis(s: &str) -> Result<u32> {
    let s = s.trim();
    let v = if let Some(ms) = s.strip_suffix("ms") {
        ms.trim().parse::<f64>()?
    } else if let Some(sec) = s.strip_suffix('s') {
        sec.trim().parse::<f64>()? * 1000.0
    } else {
        s.parse::<f64>()?
    };
    if !(v.is_finite() && v >= 0.0) {
        bail!("bad duration '{s}'");
    }
    Ok(v as u32)
}

pub async fn run(client: &Client, action: PerfAction) -> Result<()> {
    match action {
        PerfAction::Watch { game, metrics, stats, every, window, count, json } => {
            let stats = stats.iter().map(|s| Stat::parse(s).with_context(|| format!("unknown stat '{s}'"))).collect::<Result<Vec<_>>>()?;
            let every_ms = millis(&every)?.max(100);
            let window_ms = if window == "session" { 0 } else { millis(&window)? };
            let game = game_opt(client, game.as_deref()).await?;
            watch(client, game, metrics, stats, every_ms, window_ms, count, json).await?
        }
        PerfAction::Catalog { game, all, json } => {
            let game = match game_opt(client, game.as_deref()).await? {
                Some(g) => Some(g),
                None => {
                    let s: Vec<PerfSessionInfo> = client.call(perf(PerfRequest::Sessions)).await?;
                    s.first().map(|s| s.game_ref()).or(Some(GameRef::ubisoft(0)))
                }
            };
            let cat: Vec<MetricInfo> = client.call(perf(PerfRequest::Catalog { session: None, game })).await?;
            if json {
                return print_json(&cat);
            }
            print_catalog(&cat, all);
        }
        PerfAction::Sessions { json } => {
            let s: Vec<PerfSessionInfo> = client.call(perf(PerfRequest::Sessions)).await?;
            if json {
                return print_json(&s);
            }
            if s.is_empty() {
                println!("No running session has perf state.");
            }
            for x in s {
                println!(
                    "{}  {}  started {}{}{}",
                    x.session,
                    match x.game {
                        Some(g) => g.to_string(),
                        None => format!("product {}", x.product_id),
                    },
                    x.started.with_timezone(&chrono::Local).format("%H:%M:%S"),
                    if x.hud_enabled { format!("  HUD ({})", x.presenter.map(|p| format!("{p:?}").to_lowercase()).unwrap_or_else(|| "pending".into())) } else { "  HUD off".into() },
                    if x.dev { "  [dev]" } else { "" }
                );
                if let Some(d) = &x.dir {
                    println!("  dir {}", d.display());
                }
                for p in &x.producers {
                    let mut extra = Vec::new();
                    if !p.ring.is_empty() {
                        extra.push(format!("ring {}", p.ring));
                    }
                    if p.records > 0 || p.lost > 0 || p.rejected > 0 {
                        extra.push(format!("{} records, {} lost, {} rejected", p.records, p.lost, p.rejected));
                    }
                    if !p.clock.is_empty() {
                        extra.push(format!("clock {}", p.clock));
                    }
                    if !p.detail.is_empty() {
                        extra.push(p.detail.clone());
                    }
                    println!("  {:<16} {:<12} {:>3} metrics  {}", p.label, if p.connected { "connected" } else { "—" }, p.metrics, extra.join("; "));
                }
            }
        }
        PerfAction::History { game, n, json } => {
            let game = game_ref(client, &game).await?;
            let v: Vec<SessionSummary> = client.call(perf(PerfRequest::Summaries { game: game.into(), limit: n })).await?;
            if json {
                return print_json(&v);
            }
            if v.is_empty() {
                println!("No play sessions with a performance summary yet.");
            }
            for s in v {
                println!("{}  {:>4} min  {}", s.started.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), s.seconds / 60, summary_line(&s.perf));
            }
        }
        PerfAction::DevSession { game, stop, sight } => {
            let product_id = product(client, &game).await?;
            let v: serde_json::Value = client.call(perf(PerfRequest::DevSession { product_id, stop, sight })).await?;
            if stop {
                println!("ended {}", v["session"].as_str().unwrap_or("?"));
                if let Ok(s) = serde_json::from_value::<PerfSummary>(v["summary"].clone()) {
                    println!("  {}", summary_line(&s));
                }
            } else {
                for (k, val) in v["env"].as_object().into_iter().flatten() {
                    println!("export {k}={}", val.as_str().unwrap_or_default());
                }
            }
        }
    }
    Ok(())
}

fn print_catalog(cat: &[MetricInfo], all: bool) {
    let mut last_producer = String::new();
    for m in cat {
        let d = &m.descriptor;
        if !all && d.visibility == Some(quiver_api::perf::Visibility::Debug) {
            continue;
        }
        let producer = if m.canonical { "Canonical (from roles)".to_owned() } else { m.producer_label.clone() };
        if producer != last_producer {
            println!("{producer}");
            last_producer = producer;
        }
        let state = match m.state {
            MetricState::Declared => "declared",
            MetricState::Live => "live",
            MetricState::Stale => "stale",
            MetricState::Gone => "gone",
        };
        let last = m.last.as_ref().filter(|v| !v.stale && v.text != "—").map(PerfValue::display).unwrap_or_default();
        let mut notes = Vec::new();
        if let Some(s) = &m.source {
            notes.push(format!("from {s}"));
        }
        if !m.roles.is_empty() {
            notes.push(format!("fills {}", m.roles.join(", ")));
        }
        if !m.conflicts.is_empty() {
            notes.push(format!("lost {} to another producer", m.conflicts.join(", ")));
        }
        println!(
            "  {:<22} {:<22} {:<9} {:>12}  {}",
            d.id,
            d.label.as_deref().unwrap_or(""),
            state,
            last,
            notes.join("; ")
        );
    }
}

/// Fixed labels for the watch line, in order; anything else selected follows as `id=v`.
const WATCH_LINE: [(&str, &str); 14] = [
    ("fps", "FPS"),
    ("frametime", "FT"),
    ("fps.p1low", "1%↓"),
    ("latency.render", "LAT"),
    ("gpu.load", "GPU"),
    ("gpu.temp", ""),
    ("gpu.power", ""),
    ("cpu.load", "CPU"),
    ("cpu.temp", ""),
    ("mem.used", "RAM"),
    ("bat.level", "BAT"),
    ("display.mode", "DISP"),
    ("upscaler", ""),
    ("fg.ratio", "FG"),
];

fn first(v: &BTreeMap<Stat, PerfValue>) -> Option<&PerfValue> {
    v.values().next()
}

/// `21:14:03  FPS 79→158  FT 12.7  1%↓ 64  LAT 21  GPU 97% 71° 92W  CPU 38%`.
pub fn watch_line(u: &PerfUpdate) -> String {
    let mut parts: Vec<String> = Vec::new();
    let val = |id: &str| u.values.get(id).and_then(first).filter(|v| !v.text.is_empty());
    let mut used: Vec<&str> = vec!["fps.base"];
    for (id, label) in WATCH_LINE {
        used.push(id);
        let Some(v) = val(id) else { continue };
        // Nothing provides it (no latency, no upscaler): leave it out, except FPS itself.
        if (v.stale || v.text == "—") && id != "fps" {
            continue;
        }
        let mut text = v.display().replace(' ', "");
        if id == "fps"
            && let Some(b) = val("fps.base").filter(|b| !b.stale)
            && b.value.zip(v.value).is_some_and(|(b, p)| p > b * 1.3)
        {
            text = format!("{}→{text}", b.text);
        }
        if id == "frametime" {
            text = v.text.clone();
        }
        let joined = matches!(id, "gpu.temp" | "gpu.power" | "cpu.temp");
        if joined && let Some(last) = parts.last_mut() {
            last.push(' ');
            last.push_str(&text);
        } else if label.is_empty() {
            parts.push(text);
        } else {
            parts.push(format!("{label} {text}"));
        }
    }
    for (id, per) in &u.values {
        if used.contains(&id.as_str()) {
            continue;
        }
        for (stat, v) in per {
            parts.push(if per.len() > 1 { format!("{id}.{}={}", stat.as_str(), v.display()) } else { format!("{id}={}", v.display()) });
        }
    }
    format!("{}  {}", u.at.with_timezone(&chrono::Local).format("%H:%M:%S"), parts.join("  "))
}

pub fn summary_line(s: &PerfSummary) -> String {
    let mut p = Vec::new();
    if let Some(f) = &s.fps {
        let base = s.fps_base.map(|b| format!("{:.0}→", b.mean)).unwrap_or_default();
        p.push(format!("FPS {base}{:.0}", f.mean));
        if let Some(l) = f.p1low {
            p.push(format!("1%↓ {l:.0}"));
        }
    }
    if let Some(ft) = &s.frametime_ms {
        p.push(format!("FT p50 {:.1} p99 {:.1} ms", ft.p50, ft.p99));
    }
    if s.hitches > 0 {
        p.push(format!("{} hitches", s.hitches));
    }
    if let Some(l) = &s.latency_ms {
        p.push(format!("LAT {:.0} ms", l.mean));
    }
    if let Some(g) = &s.gpu_load {
        p.push(format!("GPU {:.0}% (max {:.0})", g.mean, g.max));
    }
    if let Some(t) = &s.gpu_temp {
        p.push(format!("{:.0}°", t.max));
    }
    if let Some(c) = &s.cpu_load {
        p.push(format!("CPU {:.0}%", c.mean));
    }
    if let Some(pr) = s.presenter {
        p.push(format!("HUD on {}", format!("{pr:?}").to_lowercase()));
    }
    if p.is_empty() {
        "no frame or sensor data".into()
    } else {
        p.join("  ")
    }
}

#[allow(clippy::too_many_arguments)]
async fn watch(client: &Client, game: Option<GameRef>, select: Vec<String>, stats: Vec<Stat>, every_ms: u32, window_ms: u32, count: Option<usize>, json: bool) -> Result<()> {
    let mut events = client.subscribe().await?;
    let mut perf_rx = client.subscribe_perf().await?;
    let subscribe = || async {
        client
            .call::<u64>(perf(PerfRequest::Subscribe { session: None, game, select: select.clone(), every_ms, window_ms, stats: stats.clone() }))
            .await
    };
    let mut sub = match subscribe().await {
        Ok(s) => Some(s),
        Err(e) if game.is_some() && e.to_string().contains("isn't running") => {
            eprintln!("waiting for the game to start…");
            None
        }
        Err(e) => return Err(e),
    };
    let mut session: Option<String> = None;
    let mut seen = 0usize;
    loop {
        tokio::select! {
            e = events.recv() => match e {
                Some(Event::Reconnected) => {
                    eprintln!("quiverd restarted; subscribing again");
                    sub = subscribe().await.ok();
                }
                Some(_) => {}
                None => bail!("lost connection to quiverd"),
            },
            e = perf_rx.recv() => match e {
                Some(PerfEvent::Update(u)) if Some(u.sub) == sub => {
                    session.get_or_insert_with(|| u.session.clone());
                    if json {
                        println!("{}", serde_json::to_string(&u)?);
                    } else {
                        println!("{}", watch_line(&u));
                    }
                    seen += 1;
                    if count.is_some_and(|c| seen >= c) {
                        return Ok(());
                    }
                }
                Some(PerfEvent::Session { product_id: p, game: g, ended: false, .. }) if sub.is_none() && game.is_none_or(|x| x == g.unwrap_or(GameRef::ubisoft(p))) => {
                    sub = subscribe().await.ok();
                }
                Some(PerfEvent::Session { session: s, ended: true, summary, .. }) if session.as_deref() == Some(s.as_str()) => {
                    if !json {
                        println!("session ended: {}", summary.as_deref().map(summary_line).unwrap_or_else(|| "no data".into()));
                    }
                    return Ok(());
                }
                Some(_) => {}
                None => bail!("lost connection to quiverd"),
            },
        }
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(k: &str, v: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(v.to_owned())).with_context(|| format!("{k}: bad value '{v}'"))
}

fn kv_scope(changes: &[String]) -> Result<(HudScope, Vec<String>)> {
    let mut set = HudScope::default();
    let mut unset = Vec::new();
    for c in changes {
        let (k, v) = c.split_once('=').with_context(|| format!("expected key=value, got '{c}'"))?;
        let k = match k {
            "graph_span" => "graph_span_ms",
            "rate" => "rate_hz",
            k => k,
        };
        if v.is_empty() {
            unset.push(k.to_owned());
            continue;
        }
        let b = || -> Result<bool> {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Ok(true),
                "0" | "false" | "off" | "no" => Ok(false),
                _ => bail!("{k}: expected on/off, got '{v}'"),
            }
        };
        let f = || v.trim_end_matches('%').parse::<f32>().with_context(|| format!("{k}: expected a number, got '{v}'"));
        match k {
            "enabled" => set.enabled = Some(b()?),
            "on" => set.on = Some(b()?),
            "layout" => set.layout = Some(v.to_owned()),
            "presenter" => set.presenter = Some(parse_enum::<PresenterSetting>(k, v)?),
            "captures" => set.captures = Some(parse_enum::<Captures>(k, v)?),
            "anchor" => set.anchor = Some(parse_enum::<Anchor>(k, v)?),
            "offset" => {
                let (x, y) = v.split_once(',').context("offset: expected x,y")?;
                set.offset = Some([x.trim().parse()?, y.trim().parse()?]);
            }
            "scale" => set.scale = Some(f()?),
            "background" => set.background = Some(if v.ends_with('%') { f()? / 100.0 } else { f()? }),
            "text_opacity" => set.text_opacity = Some(if v.ends_with('%') { f()? / 100.0 } else { f()? }),
            "level_colors" => set.level_colors = Some(b()?),
            "graph_span_ms" => set.graph_span_ms = Some(millis(v)?),
            "safe_area" => set.safe_area = Some(f()?),
            "rate_hz" => set.rate_hz = Some(v.trim_end_matches("Hz").parse()?),
            k => bail!("unknown HUD setting '{k}'"),
        }
    }
    Ok((set, unset))
}

fn print_view(v: &HudSettingsView) {
    let e = &v.effective;
    println!("HUD settings ({}), revision {}", v.scope.label(), v.revision);
    println!(
        "  enabled {}  on at launch {}  layout {}  presenter {:?}  captures {:?}",
        if e.enabled { "yes" } else { "no" },
        if e.on { "yes" } else { "no" },
        e.layout,
        e.presenter,
        e.captures
    );
    println!(
        "  anchor {:?}  offset {:?}  scale {}  background {:.0}%  text {:.0}%  levels {}  graph {} s  safe area {}%  {} Hz",
        e.anchor,
        e.offset,
        e.scale,
        e.background * 100.0,
        e.text_opacity * 100.0,
        if e.level_colors { "on" } else { "off" },
        e.graph_span_ms as f32 / 1000.0,
        e.safe_area,
        e.rate_hz
    );
    let set = serde_json::to_value(&v.set).unwrap_or_default();
    if set.as_object().is_some_and(|o| !o.is_empty()) {
        println!("  set here: {set}");
    }
}

fn print_status(s: &HudStatus) {
    println!(
        "{} (product {}): HUD {}{}, layout {}",
        s.session,
        s.product_id,
        if !s.enabled { "not enabled" } else if s.on { "shown" } else { "hidden" },
        if s.renderer_attached { "" } else { " (quiver-sight not attached)" },
        s.layout
    );
    match s.presenter {
        Some(p) => println!("  presenter: {} — {}", format!("{p:?}").to_lowercase(), s.presenter_reason),
        None => println!("  presenter: — {}", s.presenter_reason),
    }
    if let Some(l) = &s.layer {
        println!(
            "  layer: pid {}{}  presents {}  drawn {}  draw error {}  {}  {}",
            l.pid,
            if l.alive { "" } else { " (gone)" },
            l.presents,
            l.drawn,
            l.draw_error,
            l.extent.map(|[w, h]| format!("{w}×{h}")).unwrap_or_default(),
            [l.engine.as_str(), l.device.as_str()].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" on ")
        );
        if !l.keys.is_empty() {
            println!("  keys: {} — {}  (presses: {} toggle, {} next)", l.keys, l.keys_detail, l.key_toggles, l.key_nexts);
        }
    } else {
        println!("  layer: not seen");
    }
    if let Some(i) = &s.image {
        println!(
            "  image: {}  published {}  {}",
            i.path.display(),
            i.published,
            i.heartbeat_age_ms.map(|a| format!("quiver-sight heartbeat {a} ms ago")).unwrap_or_else(|| "no renderer yet".into())
        );
    }
    println!("  captures: {:?}  model rev {}", s.captures, s.model_rev);
    if let Some(e) = &s.last_error {
        println!("  last error: {e}");
    }
}

pub async fn hud(client: &Client, action: HudAction) -> Result<()> {
    match action {
        HudAction::Toggle { game } => {
            let game = game_opt(client, game.as_deref()).await?;
            let s: HudStatus = client.call(perf(PerfRequest::HudToggle { session: None, game })).await?;
            println!("HUD {} ({})", if s.on { "shown" } else { "hidden" }, s.layout);
        }
        HudAction::Next { game } => {
            let game = game_opt(client, game.as_deref()).await?;
            let s: HudStatus = client.call(perf(PerfRequest::HudNext { session: None, game })).await?;
            println!("HUD: {}", s.layout);
        }
        HudAction::Status { game, json } => {
            let game = game_opt(client, game.as_deref()).await?;
            let s: HudStatus = client.call(perf(PerfRequest::HudStatus { session: None, game })).await?;
            if json {
                return print_json(&s);
            }
            print_status(&s);
        }
        HudAction::Layout { action: LayoutAction::List { json } } => {
            let l: Vec<HudLayoutInfo> = client.call(perf(PerfRequest::HudLayouts)).await?;
            if json {
                return print_json(&l);
            }
            for i in l {
                println!(
                    "  {:<14} {:<16} {}{}",
                    i.id,
                    i.name,
                    if i.builtin { "built-in".to_owned() } else { i.path.map(|p| p.display().to_string()).unwrap_or_default() },
                    i.error.map(|e| format!("  ERROR: {e}")).unwrap_or_default()
                );
            }
        }
        HudAction::Layout { action: LayoutAction::Show { id } } => {
            let l: HudLayout = client.call(perf(PerfRequest::HudLayout { id })).await?;
            print_json(&l)?;
        }
        HudAction::Layout { action: LayoutAction::Set { id, game, profile } } => {
            let scope = scope(client, game.as_deref(), profile).await?;
            let set = HudScope { layout: Some(id), ..Default::default() };
            let v: HudSettingsView = client.call(perf(PerfRequest::HudSet { scope, set, unset: vec![], message: None })).await?;
            print_view(&v);
        }
        HudAction::Enable { game, profile } => set_enabled(client, game, profile, true).await?,
        HudAction::Disable { game, profile } => set_enabled(client, game, profile, false).await?,
        HudAction::Captures { value, game, profile } => {
            let captures: Captures = serde_json::from_value(serde_json::Value::String(value.clone())).with_context(|| format!("expected shown or hidden, got '{value}'"))?;
            let scope = scope(client, game.as_deref(), profile).await?;
            let v: HudSettingsView = client.call(perf(PerfRequest::HudSet { scope, set: HudScope { captures: Some(captures), ..Default::default() }, unset: vec![], message: None })).await?;
            print_view(&v);
        }
        HudAction::Set { changes, game, profile, message } => {
            let (set, unset) = kv_scope(&changes)?;
            let scope = scope(client, game.as_deref(), profile).await?;
            let v: HudSettingsView = client.call(perf(PerfRequest::HudSet { scope, set, unset, message })).await?;
            print_view(&v);
        }
        HudAction::Get { game, profile, json } => {
            let scope = scope(client, game.as_deref(), profile).await?;
            let v: HudSettingsView = client.call(perf(PerfRequest::HudGet { scope })).await?;
            if json {
                return print_json(&v);
            }
            print_view(&v);
        }
        HudAction::Keybinds { conf, json } => {
            let k: HudKeybinds = client.call(perf(PerfRequest::HudKeybinds)).await?;
            if json {
                return print_json(&k);
            }
            if conf {
                print!("{}", k.conf);
                return Ok(());
            }
            for b in &k.binds {
                println!("  {:<12} {:<4} {}  ({})", b.mods, b.key, b.description, b.command);
            }
            println!("The Sight layer reads these keys inside the game (X11/XWayland keymap polling); no compositor binds needed.");
            if k.games.is_empty() {
                println!("No running game has the layer.");
            }
            for g in &k.games {
                println!("  {} (pid {}): {} — {}", g.game.map(|r| r.to_string()).unwrap_or_else(|| format!("product {}", g.product_id)), g.pid, g.keys, g.detail);
            }
            println!("Games without the layer (anti-cheat, OpenGL, Wayland-native windows) can use Hyprland binds instead;");
            println!("quiverd never applies them. `quiver hud keybinds --conf` prints them for your Hyprland config.");
        }
        HudAction::Env { game, profile, json } => {
            let game = game_ref(client, &game).await?;
            let p: LaunchPerfPlan = client.call(perf(PerfRequest::LaunchEnv { game: game.into(), profile })).await?;
            if json {
                return print_json(&p);
            }
            for (k, v) in &p.env {
                println!("{k}={v}");
            }
            if !p.layer_order.is_empty() {
                println!("# layers, application first: {}", p.layer_order.join(" → "));
            }
            for n in &p.notes {
                println!("# {n}");
            }
        }
    }
    Ok(())
}

async fn set_enabled(client: &Client, game: Option<String>, profile: Option<String>, on: bool) -> Result<()> {
    let scope = scope(client, game.as_deref(), profile).await?;
    let game = scope.game;
    let v: HudSettingsView = client.call(perf(PerfRequest::HudSet { scope, set: HudScope { enabled: Some(on), ..Default::default() }, unset: vec![], message: None })).await?;
    print_view(&v);
    if on {
        println!("The next launch gets the Sight layer and the HUD; Shift+F3 shows it, Ctrl+Shift+F3 cycles presets.");
    }
    // A Steam game gets the HUD only through `quiver steam-run` (opt-in per game).
    if let Some(g) = game.filter(|g| g.as_steam().is_some()) {
        crate::exports::hook_offer(client, g, "the HUD").await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse() {
        let (s, u) = kv_scope(&["captures=hidden".into(), "anchor=bottom-right".into(), "background=30%".into(), "graph_span=5s".into(), "layout=".into(), "offset=8,24".into(), "on=yes".into()]).unwrap();
        assert_eq!(s.captures, Some(Captures::Hidden));
        assert_eq!(s.anchor, Some(Anchor::BottomRight));
        assert!((s.background.unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(s.graph_span_ms, Some(5000));
        assert_eq!(s.offset, Some([8.0, 24.0]));
        assert_eq!(s.on, Some(true));
        assert_eq!(u, ["layout"]);
        assert!(kv_scope(&["colour=pink".into()]).is_err());
        assert!(kv_scope(&["presenter=sometimes".into()]).is_err());
        assert_eq!(millis("500ms").unwrap(), 500);
        assert_eq!(millis("2s").unwrap(), 2000);
    }

    #[test]
    fn the_watch_line() {
        let v = |text: &str, unit: &str, value: f64| BTreeMap::from([(Stat::Mean, PerfValue { value: Some(value), text: text.into(), unit: unit.into(), ..Default::default() })]);
        let mut values = BTreeMap::new();
        values.insert("fps".to_string(), v("158", "", 158.0));
        values.insert("fps.base".to_string(), v("79", "", 79.0));
        values.insert("frametime".to_string(), v("12.7", "ms", 12.7));
        values.insert("fps.p1low".to_string(), v("64", "", 64.0));
        values.insert("latency.render".to_string(), v("21", "ms", 21.0));
        values.insert("gpu.load".to_string(), v("97", "%", 97.0));
        values.insert("gpu.temp".to_string(), v("71", "°", 71.0));
        values.insert("gpu.power".to_string(), v("92", "W", 92.0));
        values.insert("cpu.load".to_string(), v("38", "%", 38.0));
        values.insert("aco-dlss.dlss.ms".to_string(), v("1.6", "ms", 1.6));
        let u = PerfUpdate { sub: 1, session: "s".into(), product_id: 1, at: chrono::Utc::now(), values };
        let line = watch_line(&u);
        let line = line.split_once("  ").unwrap().1;
        assert_eq!(line, "FPS 79→158  FT 12.7  1%↓ 64  LAT 21ms  GPU 97% 71° 92W  CPU 38%  aco-dlss.dlss.ms=1.6 ms");
    }
}
