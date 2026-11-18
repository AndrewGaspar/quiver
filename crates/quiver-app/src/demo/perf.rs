//! Demo perf data (`QUIVER_DEMO=1`): performance summaries for Origins' play history (DLSS
//! 3.8 and 3.7 builds against vanilla), HUD settings at every scope, layouts and binds, and
//! live updates for a game started from the demo. Values are formatted the way quiverd
//! formats them (`quiverd/src/perf/format.rs`), so the screens show what they would.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use anyhow::{Result, bail};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use quiver_api::mods::ResolvedMod;
use quiver_api::perf::{
    FpsStats, HudConfig, HudKeybinds, HudLayoutInfo, HudScope, HudScopeRef, HudSettingsView, HudStatus, Keybind, Level, MeanMax,
    PerfEvent, PerfRequest, PerfSessionInfo, PerfSummary, PerfUpdate, PerfValue, Percentiles, Presenter, PresenterSetting, ProducerInfo,
    SessionSummary, Stat,
};
use quiver_api::{PlaySession, Playtime};

use super::{Demo, State};

/// Origins (the demo's game with mods).
const ORIGINS: u32 = 3539;

#[derive(Default)]
pub struct DemoPerf {
    tx: Mutex<Option<mpsc::UnboundedSender<PerfEvent>>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    hud: BTreeMap<HudScopeRef, (HudScope, u32)>,
    /// sub id → (product, every ms, ms since the last update)
    subs: HashMap<u64, (u32, u32, u32)>,
    next_sub: u64,
    tick: u64,
}

impl DemoPerf {
    pub fn new() -> DemoPerf {
        let me = DemoPerf::default();
        {
            let mut i = me.inner.lock().unwrap();
            i.next_sub = 1;
            // Origins has the HUD on, Standard for the DLSS profile; the rest is global.
            i.hud.insert(HudScopeRef::global(), (HudScope { layout: Some("compact".into()), ..Default::default() }, 3));
            i.hud.insert(
                HudScopeRef::game(ORIGINS),
                (HudScope { enabled: Some(true), on: Some(true), background: Some(0.62), ..Default::default() }, 5),
            );
            i.hud.insert(
                HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(ORIGINS)), profile: Some(super::detail::ORIGINS_DEFAULT.into()) },
                (HudScope { layout: Some("standard".into()), rate_hz: Some(4), ..Default::default() }, 2),
            );
        }
        me
    }

    pub fn events(&self) -> mpsc::UnboundedReceiver<PerfEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.tx.lock().unwrap() = Some(tx);
        rx
    }

    fn emit(&self, e: PerfEvent) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(e);
        }
    }

    fn resolve(&self, scope: &HudScopeRef) -> HudConfig {
        let i = self.inner.lock().unwrap();
        let mut c = HudConfig::default();
        let mut chain = vec![HudScopeRef::global()];
        if let Some(p) = scope.game {
            chain.push(HudScopeRef::of(p));
            if scope.profile.is_some() {
                chain.push(scope.clone());
            }
        }
        for s in chain {
            if let Some((set, _)) = i.hud.get(&s) {
                c = c.with(set);
            }
        }
        c
    }

    fn view(&self, scope: &HudScopeRef) -> HudSettingsView {
        let (set, revision) = self.inner.lock().unwrap().hud.get(scope).cloned().unwrap_or_default();
        HudSettingsView { scope: scope.clone(), set, effective: self.resolve(scope), revision }
    }
}

/// Give Origins' play history the profiles and mod versions the summaries are about.
pub(super) fn decorate(playtime: &mut BTreeMap<u32, Playtime>) {
    let Some(pt) = playtime.get_mut(&ORIGINS) else { return };
    for (i, s) in pt.sessions.iter_mut().enumerate() {
        if let Some(f) = FIXTURES.get(i) {
            set_profile(s, f.profile);
        }
    }
}

fn dlss(version: &str, label: &str, pinned: bool) -> ResolvedMod {
    ResolvedMod {
        entry: if pinned { format!("ld7s3vq@{version}") } else { "ld7s3vq".into() },
        mod_id: "ld7s3vq".into(),
        name: "DLSS + DLSSTweaks".into(),
        version: version.into(),
        label: label.into(),
        pinned,
        current: version == "vt8w1hz",
        retired: false,
        versions: 3,
    }
}

fn skip_intro() -> ResolvedMod {
    ResolvedMod { entry: "lb5k1yq".into(), mod_id: "lb5k1yq".into(), name: "Skip intro videos".into(), version: "lb5k1yq".into(), label: String::new(), versions: 1, ..Default::default() }
}

#[derive(Clone, Copy)]
enum Prof {
    Vanilla,
    /// The DLSS profile on 3.8.10 (current) or 3.7.10 (before 3.8 was added).
    Dlss38,
    Dlss37,
    Bench,
}

fn set_profile(s: &mut PlaySession, p: Prof) {
    let (id, rev, mods) = match p {
        Prof::Vanilla => (None, None, vec![]),
        Prof::Dlss38 => (Some(super::detail::ORIGINS_DEFAULT), Some(1), vec![skip_intro(), dlss("vt8w1hz", "3.8.10", false)]),
        Prof::Dlss37 => (Some(super::detail::ORIGINS_DEFAULT), Some(1), vec![skip_intro(), dlss("vq4n2c8", "3.7.10", false)]),
        Prof::Bench => (Some("pb3x7tm"), Some(1), vec![dlss("vq4n2c8", "3.7.10", true)]),
    };
    s.profile = id.map(str::to_owned);
    s.profile_revision = rev;
    s.mods = mods;
}

struct Fixture {
    profile: Prof,
    summary: fn(seconds: f64) -> PerfSummary,
}

/// One per Origins session in demo.rs, newest first.
const FIXTURES: &[Fixture] = &[
    Fixture { profile: Prof::Dlss38, summary: |s| dlss_summary(s, 116.4, 91.2, 58.3, 29.1, 41.7, 212) },
    Fixture { profile: Prof::Bench, summary: |s| dlss_summary(s, 112.1, 84.0, 56.2, 31.4, 44.9, 61) },
    Fixture { profile: Prof::Dlss37, summary: |s| dlss_summary(s, 109.6, 80.3, 54.9, 32.0, 43.2, 4) },
    Fixture { profile: Prof::Vanilla, summary: |s| vanilla_summary(s, 71.2, 52.4, 380) },
    Fixture { profile: Prof::Vanilla, summary: |s| vanilla_summary(s, 73.8, 55.1, 141) },
    // The HUD was off: no Sight layer, so sensors only.
    Fixture { profile: Prof::Vanilla, summary: sensors_only },
];

fn gpu_name() -> Option<String> {
    Some("NVIDIA GeForce RTX 5070 Laptop GPU".into())
}

fn dlss_summary(seconds: f64, fps: f64, low: f64, base: f64, lat: f64, lat_max: f64, hitches: u64) -> PerfSummary {
    PerfSummary {
        frame_seconds: seconds * 0.98,
        sensor_seconds: seconds,
        frames: (seconds * 0.98 * fps) as u64,
        fps: Some(FpsStats { mean: fps, p1low: Some(low), p01low: Some(low * 0.82) }),
        fps_base: Some(FpsStats { mean: base, p1low: None, p01low: None }),
        frametime_ms: Some(Percentiles { p50: 1000. / fps * 0.97, p95: 1000. / fps * 1.18, p99: 1000. / low }),
        fg_ratio: Some(fps / base),
        latency_ms: Some(MeanMax { mean: lat, max: lat_max }),
        gpu_load: Some(MeanMax { mean: 84.2, max: 99.0 }),
        gpu_temp: Some(MeanMax { mean: 71.0, max: 76.0 }),
        gpu_power: Some(MeanMax { mean: 94.0, max: 112.0 }),
        cpu_load: Some(MeanMax { mean: 41.0, max: 73.0 }),
        cpu_temp: Some(MeanMax { mean: 74.0, max: 81.0 }),
        vram_peak: Some(6_400_000_000),
        hitches,
        gpu: gpu_name(),
        presenter: Some(Presenter::Game),
    }
}

fn vanilla_summary(seconds: f64, fps: f64, low: f64, hitches: u64) -> PerfSummary {
    PerfSummary {
        frame_seconds: seconds * 0.99,
        sensor_seconds: seconds,
        frames: (seconds * 0.99 * fps) as u64,
        fps: Some(FpsStats { mean: fps, p1low: Some(low), p01low: Some(low * 0.8) }),
        fps_base: None,
        frametime_ms: Some(Percentiles { p50: 1000. / fps * 0.98, p95: 1000. / fps * 1.2, p99: 1000. / low }),
        fg_ratio: None,
        latency_ms: None,
        gpu_load: Some(MeanMax { mean: 98.6, max: 100.0 }),
        gpu_temp: Some(MeanMax { mean: 75.0, max: 79.0 }),
        gpu_power: Some(MeanMax { mean: 104.0, max: 115.0 }),
        cpu_load: Some(MeanMax { mean: 38.0, max: 66.0 }),
        cpu_temp: Some(MeanMax { mean: 72.0, max: 79.0 }),
        vram_peak: Some(5_100_000_000),
        hitches,
        gpu: gpu_name(),
        presenter: Some(Presenter::Game),
    }
}

fn sensors_only(seconds: f64) -> PerfSummary {
    PerfSummary {
        sensor_seconds: seconds,
        gpu_load: Some(MeanMax { mean: 97.0, max: 100.0 }),
        gpu_temp: Some(MeanMax { mean: 74.0, max: 78.0 }),
        gpu_power: Some(MeanMax { mean: 101.0, max: 114.0 }),
        cpu_load: Some(MeanMax { mean: 36.0, max: 61.0 }),
        cpu_temp: Some(MeanMax { mean: 71.0, max: 77.0 }),
        gpu: gpu_name(),
        ..Default::default()
    }
}

fn summaries(st: &State, product_id: u32, limit: usize) -> Vec<SessionSummary> {
    let Some(pt) = st.playtime.get(&product_id) else { return vec![] };
    let mut out = Vec::new();
    // Sessions played in this demo run (newest first) have no summary; the fixtures follow
    // the history demo.rs starts with.
    let fixed = pt.sessions.len().saturating_sub(FIXTURES.len());
    for (i, s) in pt.sessions.iter().enumerate() {
        let f = if product_id == ORIGINS { i.checked_sub(fixed).and_then(|k| FIXTURES.get(k)) } else { None };
        let perf = match f {
            Some(f) => (f.summary)(s.seconds as f64),
            // Every other game: one plain session with the HUD on.
            None if product_id != ORIGINS && i == 0 => vanilla_summary(s.seconds as f64, 88.0 + (product_id % 30) as f64, 61.0, 37),
            None => continue,
        };
        out.push(SessionSummary { started: s.started, seconds: s.seconds, profile: s.profile.clone(), perf });
    }
    out.truncate(limit.max(1));
    out
}

fn running(st: &State, product_id: Option<u32>) -> Option<u32> {
    st.sessions.iter().map(|s| s.product_id).find(|p| product_id.is_none_or(|q| q == *p))
}

fn session_id(product: u32) -> String {
    format!("s-demo{product}")
}

fn session_info(st: &State, product: u32) -> PerfSessionInfo {
    let started = st.sessions.iter().find(|s| s.product_id == product).map(|s| s.started).unwrap_or_else(Utc::now);
    let p = |id: &str, label: &str, metrics: usize, detail: &str| ProducerInfo {
        id: id.into(),
        label: label.into(),
        connected: true,
        ring: if id == "layer" { "ok".into() } else { String::new() },
        clock: "ok".into(),
        metrics,
        detail: detail.into(),
        ..Default::default()
    };
    let mut producers = vec![p("layer", "Sight layer", 1, "pid 48211, DXVK, NVIDIA GeForce RTX 5070 Laptop GPU"), p("system", "System", 18, ""), p("display", "Display", 5, "")];
    if product == ORIGINS {
        producers.push(p("mod:aco-dlss", "aco-dlss mod", 6, "d3d11.dll (SDK quiver_mod.h 2.0, proto 2)"));
    }
    PerfSessionInfo {
        session: session_id(product),
        product_id: product,
        game: None,
        profile: (product == ORIGINS).then(|| super::detail::ORIGINS_DEFAULT.into()),
        started,
        dir: Some(format!("/dev/shm/quiver-1000/{}", session_id(product)).into()),
        hud_enabled: true,
        producers,
        presenter: Some(Presenter::Game),
        dev: false,
    }
}

fn value(v: f64, text: String, unit: &str, level: Level) -> PerfValue {
    PerfValue { value: Some(v), text, unit: unit.into(), level, stale: false }
}

/// A live update: Origins on DLSS 3.8 in the desert, with a little wobble.
fn update(sub: u64, product: u32, tick: u64) -> PerfUpdate {
    let w = ((tick as f64) * 0.7).sin();
    let w2 = ((tick as f64) * 0.31).cos();
    let dlss = product == ORIGINS;
    let fps = if dlss { 118.0 + 4.0 * w } else { 92.0 + 3.0 * w };
    let mut values: BTreeMap<String, BTreeMap<Stat, PerfValue>> = BTreeMap::new();
    let mut put = |id: &str, stat: Stat, v: PerfValue| {
        values.entry(id.into()).or_default().insert(stat, v);
    };
    put("fps", Stat::Mean, value(fps, format!("{fps:.0}"), "fps", Level::Normal));
    put("fps.p1low", Stat::Mean, value(fps * 0.8, format!("{:.0}", fps * 0.8), "fps", Level::Normal));
    put("frametime", Stat::Mean, value(1000. / fps, format!("{:.1}", 1000. / fps), "ms", Level::Normal));
    if dlss {
        let base = fps / 2.0;
        put("fps.base", Stat::Mean, value(base, format!("{base:.0}"), "fps", Level::Normal));
        let lat = 28.0 + 2.0 * w2;
        put("latency.render", Stat::Mean, value(lat, format!("{lat:.0}"), "ms", Level::Normal));
        put("fg.ratio", Stat::Last, value(2.0, "2.0".into(), "×", Level::Normal));
        put("upscaler", Stat::Last, PerfValue { text: "DLSS K".into(), ..Default::default() });
    }
    let gpu = 87.0 + 3.0 * w2;
    put("gpu.load", Stat::Mean, value(gpu, format!("{gpu:.0}"), "%", Level::Normal));
    let temp = 72.0 + w;
    put("gpu.temp", Stat::Last, value(temp, format!("{temp:.0}"), "°", Level::Normal));
    put("gpu.power", Stat::Mean, value(96.0, "96".into(), "W", Level::Normal));
    let cpu = 41.0 + 5.0 * w;
    put("cpu.load", Stat::Mean, value(cpu, format!("{cpu:.0}"), "%", Level::Normal));
    let ct = 86.0 + w2;
    put("cpu.temp", Stat::Last, value(ct, format!("{ct:.0}"), "°", if ct > 85.0 { Level::Warn } else { Level::Normal }));
    PerfUpdate { sub, session: session_id(product), product_id: product, at: Utc::now(), values }
}

/// Every 500 ms (demo.rs): subscriptions that are due get an update.
pub(super) fn tick(demo: &Demo) {
    let mut out = Vec::new();
    {
        let mut i = demo.perf.inner.lock().unwrap();
        i.tick += 1;
        let tick = i.tick;
        for (sub, (product, every, since)) in i.subs.iter_mut() {
            *since += 500;
            if *since >= *every {
                *since = 0;
                out.push(update(*sub, *product, tick));
            }
        }
    }
    for u in out {
        demo.perf.emit(PerfEvent::Update(u));
    }
}

/// A game started or stopped in the demo: what quiverd's perf hub broadcasts.
pub(super) fn session_event(demo: &Demo, product_id: u32, ended: bool) {
    if ended {
        demo.perf.inner.lock().unwrap().subs.retain(|_, (p, _, _)| *p != product_id);
    }
    demo.perf.emit(PerfEvent::Session { session: session_id(product_id), product_id, ended, summary: None, game: None });
}

pub(super) fn handle(demo: &Demo, st: &State, req: PerfRequest) -> Result<Value> {
    let perf = &demo.perf;
    Ok(match req {
        PerfRequest::Sessions => json!(st.sessions.iter().map(|s| session_info(st, s.product_id)).collect::<Vec<_>>()),
        PerfRequest::Summaries { game, limit } => json!(game.as_ubisoft().map(|p| summaries(st, p, limit)).unwrap_or_default()),
        PerfRequest::Subscribe { game, session, every_ms, .. } => {
            let product = session.as_deref().and_then(|s| s.strip_prefix("s-demo")).and_then(|p| p.parse().ok()).or(game.and_then(|g| g.as_ubisoft()));
            let Some(product) = running(st, product) else { bail!("no game is running") };
            let mut i = perf.inner.lock().unwrap();
            let sub = i.next_sub;
            i.next_sub += 1;
            // Due on the next tick.
            i.subs.insert(sub, (product, every_ms.max(100), every_ms));
            json!(sub)
        }
        PerfRequest::Unsubscribe { sub } => {
            perf.inner.lock().unwrap().subs.remove(&sub);
            Value::Null
        }
        PerfRequest::HudGet { scope } => json!(perf.view(&scope)),
        PerfRequest::HudSet { scope, set, unset, .. } => {
            {
                let mut i = perf.inner.lock().unwrap();
                let e = i.hud.entry(scope.clone()).or_default();
                let cur = &mut e.0;
                macro_rules! take {
                    ($($f:ident),*) => { $( if set.$f.is_some() { cur.$f = set.$f.clone(); } )* };
                }
                take!(enabled, on, layout, presenter, captures, anchor, offset, scale, background, text_opacity, level_colors, graph_span_ms, safe_area, rate_hz);
                for k in &unset {
                    match k.as_str() {
                        "enabled" => cur.enabled = None,
                        "on" => cur.on = None,
                        "layout" => cur.layout = None,
                        "presenter" => cur.presenter = None,
                        "captures" => cur.captures = None,
                        "anchor" => cur.anchor = None,
                        "offset" => cur.offset = None,
                        "scale" => cur.scale = None,
                        "background" => cur.background = None,
                        "text_opacity" => cur.text_opacity = None,
                        "rate_hz" => cur.rate_hz = None,
                        k => bail!("unknown HUD setting '{k}'"),
                    }
                }
                e.1 += 1;
            }
            perf.emit(PerfEvent::HudChanged { product_id: scope.product_id(), session: None });
            json!(perf.view(&scope))
        }
        PerfRequest::HudLayouts => json!([
            HudLayoutInfo { id: "fps".into(), name: "FPS only".into(), builtin: true, path: None, error: None },
            HudLayoutInfo { id: "compact".into(), name: "Compact".into(), builtin: true, path: None, error: None },
            HudLayoutInfo { id: "standard".into(), name: "Standard".into(), builtin: true, path: None, error: None },
            HudLayoutInfo { id: "full".into(), name: "Full".into(), builtin: true, path: None, error: None },
            HudLayoutInfo { id: "clock".into(), name: "Clock".into(), builtin: true, path: None, error: None },
            HudLayoutInfo {
                id: "dlss-tuning".into(),
                name: "DLSS tuning".into(),
                builtin: false,
                path: Some(super::home().join(".local/share/quiver/hud/layouts/dlss-tuning.toml")),
                error: None,
            },
        ]),
        PerfRequest::HudKeybinds => json!(HudKeybinds {
            binds: vec![
                Keybind { mods: "SHIFT".into(), key: "F3".into(), command: "quiver hud toggle".into(), description: "Show or hide the HUD".into() },
                Keybind { mods: "CTRL SHIFT".into(), key: "F3".into(), command: "quiver hud next".into(), description: "Next HUD preset".into() },
            ],
            on_focus: vec!["keyword bind SHIFT,F3,exec,quiver hud toggle".into(), "keyword bind CTRL SHIFT,F3,exec,quiver hud next".into()],
            on_blur: vec!["keyword unbind SHIFT,F3".into(), "keyword unbind CTRL SHIFT,F3".into()],
            conf: "bindd = SHIFT, F3, Show or hide the HUD, exec, quiver hud toggle\nbindd = CTRL SHIFT, F3, Next HUD preset, exec, quiver hud next\n".into(),
            game_focused: false,
            applied: false,
        }),
        PerfRequest::HudStatus { game, session } => {
            let product_id = game.and_then(|g| g.as_ubisoft());
            let product = session.as_deref().and_then(|s| s.strip_prefix("s-demo")).and_then(|p| p.parse().ok()).or(product_id);
            let Some(product) = running(st, product) else {
                bail!("{}", match product_id {
                    Some(p) => format!("product {p} isn't running"),
                    None => "no game is running".into(),
                })
            };
            let cfg = perf.resolve(&HudScopeRef { game: Some(quiver_api::GameRef::ubisoft(product)), profile: (product == ORIGINS).then(|| super::detail::ORIGINS_DEFAULT.into()) });
            let desktop = cfg.presenter == PresenterSetting::Desktop;
            json!(HudStatus {
                session: session_id(product),
                product_id: product,
                enabled: cfg.enabled,
                on: cfg.on,
                layout: cfg.layout,
                presenter: Some(if desktop { Presenter::Desktop } else { Presenter::Game }),
                presenter_reason: if desktop { "set to draw over the game".into() } else { "layer drawing".into() },
                renderer_attached: true,
                model_rev: 41,
                captures: cfg.captures,
                ..Default::default()
            })
        }
        PerfRequest::HudToggle { .. } | PerfRequest::HudNext { .. } => bail!("demo mode: the HUD keys work while a game runs"),
        other => bail!("not available in demo mode: perf {:?}", std::mem::discriminant(&other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_summaries_match_its_history() {
        let mut st = super::super::build_state();
        decorate(&mut st.playtime);
        let s = summaries(&st, ORIGINS, 20);
        assert_eq!(s.len(), FIXTURES.len());
        let pt = &st.playtime[&ORIGINS];
        assert!(s.iter().zip(&pt.sessions).all(|(a, b)| a.started == b.started && a.profile == b.profile));
        assert!(s[0].perf.fps_base.is_some() && s[3].perf.fps_base.is_none());
        assert!(s.last().unwrap().perf.fps.is_none(), "a sensors-only session");
        let u = update(1, ORIGINS, 3);
        assert!(u.values.contains_key("fps.base") && u.values["cpu.temp"][&Stat::Last].level == Level::Warn);
    }
}
