//! Demo answers for Quiver Tweaks (`Request::Tweaks`, design/TWEAKS.md): the real catalog
//! (`options/quiver-tweaks.toml`), values at the three scopes with revisions, and for game and
//! profile scopes what the next launch does with each tweak, worked out from the catalog's
//! bindings against a demo machine (an RTX 5070 laptop, GE-Proton11-12 without NVIDIA's
//! updater) and AC Origins' DLSS mod, which implements some tweaks and conflicts with others.
//! The last session's effects are fixtures. Changes update in memory, as the options demo does.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, anyhow};
use chrono::Utc;
use serde_json::{Value, json};
use quiver_api::options::{
    GroupState, LiveInfo, OpOutcome, OptionOp, OptionState, OptionsChange, OptionsRevision, Outcome, PageState, PresetInfo, Schema, SchemaSource, SurfaceKind, SurfaceState,
    ValueSource, ViaDef, ViaValue, Values, same_value, scalar_text,
};
use quiver_api::tweaks::{
    BoundValue, Capabilities, Considered, DisplayCaps, Effect, Gpu, NgxCaps, PREFER_KEY, Planned, Prefer, ProtonCaps, SURFACE_KEY, Scope, ScopeKind, SessionTweaks,
    TweakEffect, TweakPlanEntry, TweakRevisions, TweakRow, TweaksPlan, TweaksRequest,
};
use quiver_api::{Event, GameRef};

use super::{Detail, State, at, launch_profile};

const CATALOG: &str = include_str!("../../../../../options/quiver-tweaks.toml");
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| toml::from_str(CATALOG).expect("options/quiver-tweaks.toml parses"));
static TW: LazyLock<Mutex<HashMap<usize, TwDemo>>> = LazyLock::new(Mutex::default);

const ORIGINS: u32 = 3539;

/// Revisions per scope (`global`, `game:<ref>`, `profile:<ref>/<id>`), oldest first.
struct TwDemo {
    hist: BTreeMap<String, Vec<OptionsRevision>>,
}

#[allow(clippy::too_many_arguments)]
fn rev(revision: u32, days: i64, hm: (u32, u32), source: &str, message: Option<&str>, changes: &[&str], tweaks: &[(&str, Value)], prefer: &[(&str, &str)]) -> OptionsRevision {
    let mut values: Values = BTreeMap::new();
    if !tweaks.is_empty() {
        values.insert(SURFACE_KEY.into(), tweaks.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect());
    }
    if !prefer.is_empty() {
        values.insert(PREFER_KEY.into(), prefer.iter().map(|(k, v)| ((*k).to_owned(), json!(v))).collect());
    }
    OptionsRevision {
        revision,
        at: at(days, hm.0, hm.1),
        source: source.into(),
        session: None,
        message: message.map(str::to_owned),
        changes: changes.iter().map(|s| (*s).to_owned()).collect(),
        values,
        raw: Default::default(),
    }
}

impl TwDemo {
    fn fixtures() -> Self {
        let mut hist = BTreeMap::new();
        hist.insert(
            "global".to_owned(),
            vec![
                rev(1, 12, (20, 14), "gui", None, &["Global: tex.anisotropy Game's choice → 16×"], &[("tex.anisotropy", json!("16"))], &[]),
                rev(
                    2,
                    6,
                    (21, 2),
                    "gui",
                    Some("Preset: Check DLSS, then the latest model"),
                    &["Global: dlss.sr.model Game's choice → Recommended (latest)", "Global: latency.mode Game's choice → On", "Global: dlss.runtime The game's own → NVIDIA updater"],
                    &[("tex.anisotropy", json!("16")), ("dlss.sr.model", json!("latest")), ("latency.mode", json!("on")), ("dlss.runtime", json!("updater"))],
                    &[],
                ),
                rev(
                    3,
                    2,
                    (19, 40),
                    "cli",
                    Some("Laptop on the 165 Hz panel: cap a bit under"),
                    &["Global: fps.cap 0 (No cap) → 160 fps", "Global: dlss.fg.frames Game's choice → 4×"],
                    &[("tex.anisotropy", json!("16")), ("dlss.sr.model", json!("latest")), ("latency.mode", json!("on")), ("dlss.runtime", json!("updater")), ("fps.cap", json!(160)), ("dlss.fg.frames", json!("4x"))],
                    &[],
                ),
            ],
        );
        hist.insert(format!("game:{ORIGINS}"), vec![rev(1, 1, (21, 30), "gui", None, &["This game: sync.vsync Game's choice → Off"], &[("sync.vsync", json!("off"))], &[])]);
        hist.insert(
            format!("profile:{ORIGINS}/{}", super::ORIGINS_DEFAULT),
            vec![
                rev(1, 1, (21, 34), "gui", None, &["Profile: sync.max_latency Game's choice → 1"], &[("sync.max_latency", json!("1"))], &[]),
                rev(2, 0, (9, 12), "claude-code", Some("Sharper textures under DLSS"), &["Profile: tex.lod_bias Game's choice → No negative bias"], &[("sync.max_latency", json!("1")), ("tex.lod_bias", json!("clamp"))], &[]),
            ],
        );
        TwDemo { hist }
    }

    fn cur(&self, key: &str) -> OptionsRevision {
        self.hist.get(key).and_then(|h| h.last()).cloned().unwrap_or_default()
    }

    fn values(&self, key: &str, which: &str) -> BTreeMap<String, Value> {
        self.cur(key).values.get(which).cloned().unwrap_or_default()
    }

    /// The revision (and its source) that set `id` to what it is now at a scope.
    fn set_by(&self, key: &str, id: &str) -> (u32, String) {
        let hist = self.hist.get(key).map(Vec::as_slice).unwrap_or_default();
        let now = hist.last().and_then(|r| r.values.get(SURFACE_KEY)).and_then(|m| m.get(id)).cloned();
        let mut out = hist.last().map(|r| (r.revision, r.source.clone())).unwrap_or_default();
        let mut prev: Option<Value> = None;
        for r in hist {
            let v = r.values.get(SURFACE_KEY).and_then(|m| m.get(id)).cloned();
            if v.is_some() && v == now && prev != now {
                out = (r.revision, r.source.clone());
            }
            prev = v;
        }
        out
    }
}

/// A scope resolved for the demo: its key, the keys below it and the profile it means.
struct At {
    kind: ScopeKind,
    game: Option<GameRef>,
    /// Profile id (`None`: vanilla) a plan for this scope runs.
    profile: Option<String>,
    /// (kind, storage key), lowest first, up to and including this scope.
    layers: Vec<(ScopeKind, String)>,
    /// The layer above (the game scope's default profile), for "overridden by".
    above: Option<(ScopeKind, String)>,
}

fn game_key(g: &GameRef) -> String {
    format!("game:{}", g.key())
}

fn profile_key(g: &GameRef, profile: Option<&str>) -> String {
    format!("profile:{}/{}", g.key(), profile.unwrap_or(quiver_api::mods::VANILLA))
}

fn resolve(d: &Detail, st: &State, scope: &Scope) -> Result<At> {
    Ok(match scope {
        Scope::Global => At { kind: ScopeKind::Global, game: None, profile: None, layers: vec![(ScopeKind::Global, "global".into())], above: None },
        Scope::Game { game } => {
            let profile = match game.as_ubisoft() {
                Some(p) => launch_profile(d, st, p, None)?,
                None => None,
            };
            let above = game.as_ubisoft().map(|_| (ScopeKind::Profile, profile_key(game, profile.as_deref())));
            At { kind: ScopeKind::Game, game: Some(*game), profile, layers: vec![(ScopeKind::Global, "global".into()), (ScopeKind::Game, game_key(game))], above }
        }
        Scope::Profile { game, profile } => {
            let p = game.as_ubisoft().ok_or_else(|| anyhow!("{game} has no profiles: set its tweaks for the game or globally"))?;
            let profile = match profile.as_deref() {
                Some(q) => launch_profile(d, st, p, Some(q))?,
                None => launch_profile(d, st, p, None)?,
            };
            At {
                kind: ScopeKind::Profile,
                game: Some(*game),
                layers: vec![(ScopeKind::Global, "global".into()), (ScopeKind::Game, game_key(game)), (ScopeKind::Profile, profile_key(game, profile.as_deref()))],
                profile,
                above: None,
            }
        }
    })
}

/// One tweak through the layers.
struct Res {
    value: Value,
    inherited: Value,
    from: ValueSource,
    at: BTreeMap<ScopeKind, Value>,
}

fn resolve_all(o: &TwDemo, layers: &[(ScopeKind, String)]) -> BTreeMap<String, Res> {
    let mut out = BTreeMap::new();
    for (id, def) in &SCHEMA.options {
        let default = def.default.clone().unwrap_or(Value::Null);
        let mut r = Res { value: default.clone(), inherited: default, from: ValueSource::Default, at: BTreeMap::new() };
        for (k, key) in layers {
            let Some(v) = o.values(key, SURFACE_KEY).get(id).cloned() else { continue };
            let n = def.normalize(&v).unwrap_or(v);
            r.at.insert(*k, n.clone());
            r.inherited = std::mem::replace(&mut r.value, n);
            let (revision, source) = o.set_by(key, id);
            r.from = ValueSource::Scope { scope: *k, revision, source };
        }
        out.insert(id.clone(), r);
    }
    out
}

fn prefer_of(o: &TwDemo, layers: &[(ScopeKind, String)], id: &str) -> Prefer {
    let mut p = Prefer::Provider;
    for (_, key) in layers {
        if let Some(v) = o.values(key, PREFER_KEY).get(id) {
            p = serde_json::from_value(v.clone()).unwrap_or_default();
        }
    }
    p
}

/// The demo machine.
pub(super) fn caps(game: Option<&GameRef>) -> Capabilities {
    let gpu = Gpu { vendor: "nvidia".into(), name: "NVIDIA GeForce RTX 5070 Laptop GPU".into(), rtx_series: Some(50), arch: Some("blackwell".into()) };
    let steam = game.is_some_and(|g| !g.is_ubisoft());
    Capabilities {
        gpus: vec![gpu.clone(), Gpu { vendor: "amd".into(), name: "AMD Radeon 890M".into(), ..Default::default() }],
        gpu: Some(gpu),
        driver: Some("620.70.06".into()),
        proton: (!steam).then(|| ProtonCaps {
            name: "GE-Proton11-12".into(),
            version: "GE-Proton11-12".into(),
            flavour: "ge".into(),
            dxvk: Some("2.7.1".into()),
            vkd3d: Some("3.0".into()),
            nvapi: Some("0.9.3".into()),
            has: vec!["dxvk:dxvk.latencySleep".into(), "dxvk:dxvk.disableNvLowLatency2".into(), "proton:PROTON_ENABLE_HDR".into()],
            ..Default::default()
        }),
        ngx: NgxCaps { dir: Some("/usr/lib/nvidia/wine".into()), core: true, updater: false },
        route: if steam { "steam".into() } else { "quiver".into() },
        api: if game.is_some_and(|g| g.as_ubisoft() == Some(ORIGINS)) { "dxvk".into() } else { "unknown".into() },
        anti_cheat: false,
        layer: false,
        display: Some(DisplayCaps { name: "eDP-1".into(), refresh_hz: 165.0, vrr: true, hdr: Some(false) }),
        notes: Vec::new(),
    }
}

/// Whether a binding's `requires` holds on the demo machine (the parts it can answer).
fn requires(b: &ViaDef, c: &Capabilities) -> Result<(), String> {
    let r = &b.requires;
    if b.phase.unwrap_or(1) > 1 {
        return Err(format!("phase {} (not built yet)", b.phase.unwrap_or(2)));
    }
    if let Some(v) = &r.vendor
        && v != c.vendor()
    {
        return Err(format!("needs an {} GPU", v.to_uppercase()));
    }
    if let (Some(want), Some(have)) = (r.rtx_series, c.gpu.as_ref().and_then(|g| g.rtx_series))
        && have < want
    {
        return Err(format!("needs an RTX {want} or newer"));
    }
    if !r.api.is_empty() && !r.api.iter().any(|a| *a == c.api || (c.api == "dxvk" && matches!(a.as_str(), "d3d8" | "d3d9" | "d3d10" | "d3d11"))) {
        return Err(format!("the game's API is {} (this binding is for {})", c.api, r.api.join(", ")));
    }
    if r.dxgi.as_deref() == Some("not-dxvk") && c.api == "dxvk" {
        return Err("the game's DXGI is DXVK's".into());
    }
    if r.ngx.iter().any(|f| f == "nvngx_update.exe") && !c.ngx.updater {
        return Err("your driver package has no nvngx_update.exe, and GE-Proton11-12 doesn't copy it".into());
    }
    if let Some(p) = &c.proton
        && let Some(miss) = r.has.iter().find(|h| !p.has.contains(h) && !h.starts_with("nvapi:"))
    {
        return Err(format!("{} lacks {miss}", p.name));
    }
    Ok(())
}

/// What a binding writes for a value (`Ok(None)`: `@unset`; `Err`: `@skip`).
fn writes(b: &ViaDef, text: &str) -> Result<Option<Vec<String>>, ()> {
    let fmt = |k: &str, v: &str| if b.mech == "dxvk" { format!("DXVK_CONFIG: {k} = {v}") } else if b.mech == "nvapi-drs" { format!("DXVK_NVAPI_DRS_{k}={v}") } else { format!("{k}={v}") };
    match b.map.get(text) {
        Some(ViaValue::Text(t)) if t == "@unset" => Ok(None),
        Some(ViaValue::Text(t)) if t == "@skip" => Err(()),
        Some(ViaValue::Set(m)) => Ok(Some(m.iter().map(|(k, v)| fmt(k, v)).collect())),
        Some(ViaValue::Text(t)) => Ok(Some(b.set.iter().map(|(k, v)| fmt(k, &v.replace("{v}", t))).collect())),
        None => Ok(Some(b.set.iter().map(|(k, v)| fmt(k, &v.replace("{v}", text))).collect())),
    }
}

/// AC Origins' DLSS mod in a profile: (tweak, option, its value for the tweak's, `off`).
fn dlss_provides(id: &str, v: &Value) -> Option<(&'static str, Option<Value>, bool)> {
    let s = scalar_text(v);
    Some(match id {
        "fps.cap" => ("reflex.fps_limit", Some(if s == "vrr" { json!(162) } else { v.clone() }), true),
        "latency.mode" => ("reflex.mode", (s != "app").then(|| json!(s)), true),
        "dlss.sr.model" => ("upscaler.preset", (s != "app").then(|| json!(if s == "latest" { "default".into() } else { s.clone() })), false),
        "dlss.sr.scale" => ("upscaler.render_scale", (s != "app").then(|| json!(s)), false),
        "tex.lod_bias" => ("upscaler.mip_bias", None, false),
        _ => return None,
    })
}

const DLSS_ONE_FRAME: &str = "DLSS: DLSS mod generates exactly one frame per game frame; a multi-frame override breaks its present thread.";

/// The next launch of a game with a profile: one entry per tweak set somewhere.
fn plan(o: &TwDemo, d: &Detail, game: &GameRef, profile: Option<&str>) -> TweaksPlan {
    let layers: Vec<(ScopeKind, String)> = vec![(ScopeKind::Global, "global".into()), (ScopeKind::Game, game_key(game))]
        .into_iter()
        .chain(game.as_ubisoft().map(|_| (ScopeKind::Profile, profile_key(game, profile))))
        .collect();
    let res = resolve_all(o, &layers);
    let c = caps(Some(game));
    let dlss = game.as_ubisoft().is_some_and(|p| super::options::has_dlss_mod(d, p, profile));
    let mut p = TweaksPlan {
        game: Some(*game),
        profile: game.as_ubisoft().map(|_| profile.unwrap_or(quiver_api::mods::VANILLA).to_owned()),
        revisions: TweakRevisions { global: o.cur("global").revision, game: o.cur(&game_key(game)).revision, profile: o.cur(&profile_key(game, profile)).revision },
        ..Default::default()
    };
    for (id, r) in &res {
        if r.from == ValueSource::Default {
            continue;
        }
        let def = &SCHEMA.options[id];
        let text = scalar_text(&r.value);
        let shown = if id == "fps.cap" && text == "vrr" { "refresh rate − 3 (162 fps)".to_owned() } else { quiver_api::options::scalar_text(&r.value) };
        let display = def.values.iter().find(|e| same_value(&e.value, &r.value)).and_then(|e| e.label.clone()).unwrap_or_else(|| def.display(&r.value)).replace(&scalar_text(&r.value), &shown);
        let mut e = TweakPlanEntry { id: id.clone(), label: def.label_or(id).to_owned(), value: r.value.clone(), display, from: r.from.clone(), ..Default::default() };
        let prefer = prefer_of(o, &layers, id);
        if dlss && matches!(id.as_str(), "dlss.fg.frames" | "dlss.fg.model" | "fg.smooth_motion") && text != "app" {
            e.planned = Planned::Blocked { reason: if id == "dlss.fg.frames" { DLSS_ONE_FRAME.into() } else { "DLSS: DLSS mod runs its own Frame Generation".into() } };
            e.considered.push(Considered { mech: "mod".into(), chosen: false, why: "DLSS declares a conflict while its fg.mode = d3d12".into() });
            p.entries.push(e);
            continue;
        }
        if dlss
            && let Some((option, value, can)) = dlss_provides(id, &r.value)
            && prefer == Prefer::Provider
        {
            e.considered.push(Considered { mech: "mod".into(), chosen: true, why: String::new() });
            if let Some(v) = &value {
                p.bound.push(BoundValue { tweak: id.clone(), surface: "ld7s3vq/aco-dlss".into(), option: option.into(), value: v.clone(), scope: r.scope_of() });
            }
            e.planned = Planned::Provided { by: "DLSS".into(), surface: "ld7s3vq/aco-dlss".into(), option: option.into(), value, mode: if id == "tex.lod_bias" { "own".into() } else { "bind".into() }, can_prefer_quiver: can };
            p.entries.push(e);
            continue;
        }
        if dlss && dlss_provides(id, &r.value).is_some() {
            e.considered.push(Considered { mech: "mod".into(), chosen: false, why: "you prefer Quiver's: DLSS's option is set to its off value".into() });
            if id == "fps.cap" {
                e.considered.push(Considered { mech: "dxvk".into(), chosen: false, why: "DLSS: Frame Generation presents two frames per game frame; DXVK's limiter would cap generated frames".into() });
                e.planned = Planned::Blocked { reason: "DLSS: Frame Generation presents two frames per game frame; DXVK's limiter would cap generated frames and halve the game's rate.".into() };
                p.entries.push(e);
                continue;
            }
        }
        let mut whys = Vec::new();
        let mut planned = None;
        for b in def.via.iter().filter(|b| !b.also) {
            if let Err(why) = requires(b, &c) {
                e.considered.push(Considered { mech: b.mech.clone(), chosen: false, why: why.clone() });
                whys.push(why);
                continue;
            }
            let t = if id == "fps.cap" && text == "vrr" { "162".to_owned() } else { text.clone() };
            match writes(b, &t) {
                Err(()) => {
                    let why = format!("{} can't do {}", b.label.clone().unwrap_or_else(|| b.mech.clone()), text);
                    e.considered.push(Considered { mech: b.mech.clone(), chosen: false, why: why.clone() });
                    whys.push(why);
                }
                Ok(None) => {
                    e.considered.push(Considered { mech: b.mech.clone(), chosen: true, why: String::new() });
                    planned = Some(Planned::Default);
                    break;
                }
                Ok(Some(w)) => {
                    e.considered.push(Considered { mech: b.mech.clone(), chosen: true, why: String::new() });
                    for line in &w {
                        match line.strip_prefix("DXVK_CONFIG: ") {
                            Some(x) => p.dxvk_config.push(x.to_owned()),
                            None => {
                                if let Some((k, v)) = line.split_once('=') {
                                    p.env.insert(k.to_owned(), v.to_owned());
                                }
                            }
                        }
                    }
                    if b.copy_updater {
                        p.files.push("copy nvngx_update.exe into the prefix's system32".into());
                    }
                    planned = Some(Planned::Apply { mech: b.mech.clone(), writes: w, apply: b.apply.unwrap_or_default() });
                    break;
                }
            }
        }
        e.planned = planned.unwrap_or_else(|| Planned::Unavailable { reason: whys.last().cloned().unwrap_or_else(|| "no mechanism for it here".into()) });
        p.entries.push(e);
    }
    if !p.dxvk_config.is_empty() {
        p.env.insert("DXVK_CONFIG".into(), p.dxvk_config.join("; "));
    }
    if p.entries.iter().any(|e| matches!(e.planned, Planned::Apply { .. })) {
        p.env.insert("DXVK_LOG_PATH".into(), "~/.local/share/quiver/tweaks/3539/sessions/next".into());
    }
    if !game.is_ubisoft() {
        p.notes.push("this game starts through its store's client: Quiver Tweaks apply to games Quiver launches (and quiver steam-run) for now".into());
    }
    p.caps = c;
    p
}

impl Res {
    fn scope_of(&self) -> ScopeKind {
        match &self.from {
            ValueSource::Scope { scope, .. } => *scope,
            _ => ScopeKind::Global,
        }
    }
}

/// AC Origins' last session (DLSS profile), as quiverd records it.
fn last_session(game: &GameRef) -> Option<SessionTweaks> {
    if game.as_ubisoft() != Some(ORIGINS) {
        return None;
    }
    let started = at(1, 21, 4);
    let e = |id: &str, value: &str, mech: Option<&str>, effect: Effect| TweakEffect { id: id.into(), label: SCHEMA.options[id].label_or(id).into(), value: value.into(), mech: mech.map(str::to_owned), effect };
    Some(SessionTweaks {
        session: "s-4kq2mx".into(),
        game: *game,
        profile: super::ORIGINS_DEFAULT.into(),
        route: "quiver".into(),
        started,
        ended: Some(at(1, 22, 31)),
        revisions: TweakRevisions { global: 3, game: 1, profile: 1 },
        log_dir: None,
        effects: vec![
            e("fps.cap", "160 fps", Some("mod"), Effect::Provided { by: "DLSS".into(), value: Some("reflex.fps_limit = 160 (aco-dlss reported it)".into()) }),
            e("latency.mode", "On", Some("mod"), Effect::Provided { by: "DLSS".into(), value: Some("reflex.mode = on".into()) }),
            e("sync.vsync", "Off", Some("dxvk"), Effect::Confirmed { evidence: "DXVK: Present mode IMMEDIATE".into(), at: started + chrono::Duration::seconds(41) }),
            e("tex.anisotropy", "16×", Some("dxvk"), Effect::Unconfirmed { why: "DXVK logs no line for it".into() }),
            e("dlss.sr.model", "Recommended (latest)", Some("mod"), Effect::Provided { by: "DLSS".into(), value: Some("upscaler.preset = default".into()) }),
            e("dlss.runtime", "NVIDIA updater", None, Effect::Unavailable { reason: "no nvngx_update.exe in the driver package".into() }),
            e("dlss.fg.frames", "4×", None, Effect::Unavailable { reason: "blocked by DLSS (one generated frame)".into() }),
        ],
        env: BTreeMap::new(),
        warnings: Vec::new(),
    })
}

/// The platform surface at a scope.
fn state(o: &TwDemo, d: &Detail, st: &State, scope: &Scope) -> Result<SurfaceState> {
    let a = resolve(d, st, scope)?;
    let res = resolve_all(o, &a.layers);
    let above: BTreeMap<String, Value> = a.above.as_ref().map(|(_, k)| o.values(k, SURFACE_KEY)).unwrap_or_default();
    let plan = a.game.as_ref().map(|g| plan(o, d, g, a.profile.as_deref()));
    let last = a.game.as_ref().and_then(last_session);
    let values: BTreeMap<String, Value> = res.iter().map(|(id, r)| (id.clone(), r.value.clone())).collect();
    let get = |id: &str| values.get(id).cloned();
    let mut options = BTreeMap::new();
    for (id, r) in &res {
        let def = SCHEMA.options[id].clone();
        let set_here = matches!(&r.from, ValueSource::Scope { scope, .. } if *scope == a.kind);
        let higher = above.get(id).cloned();
        let row = TweakRow {
            global: r.at.get(&ScopeKind::Global).cloned(),
            game: r.at.get(&ScopeKind::Game).cloned(),
            profile: r.at.get(&ScopeKind::Profile).cloned().or(higher.clone()),
            at: a.kind,
            overridden_by: higher.map(|_| ScopeKind::Profile),
            prefer: prefer_of(o, &a.layers, id),
            planned: plan.as_ref().and_then(|p| p.entry(id)).map(|e| e.planned.clone()),
            chips: def.via.iter().filter(|b| b.phase.unwrap_or(1) <= 1 && !b.also).filter_map(|b| b.label.clone()).fold(Vec::new(), |mut v, l| {
                if !v.contains(&l) {
                    v.push(l);
                }
                v
            }),
            last: last.as_ref().and_then(|s| s.effect(id)).map(|e| e.effect.clone()),
        };
        let default = if set_here { r.inherited.clone() } else { r.value.clone() };
        let mut text = def.to_text(&r.value);
        if id == "fps.cap" && scalar_text(&r.value) == "vrr" {
            text = format!("{text} (162)");
        }
        options.insert(
            id.clone(),
            OptionState {
                visible: def.visible(&get),
                text,
                value: r.value.clone(),
                default,
                from: r.from.clone(),
                pending: Default::default(),
                dormant: None,
                invalid: None,
                tweak: Some(Box::new(row)),
                def,
            },
        );
    }
    let pages = SCHEMA
        .pages
        .iter()
        .map(|p| {
            let mut groups = vec![GroupState { title: String::new(), options: p.options.clone() }];
            groups.extend(p.groups.iter().map(|g| GroupState { title: g.title.clone(), options: g.options.clone() }));
            PageState { id: p.id.clone(), title: p.title.clone(), groups }
        })
        .collect();
    Ok(SurfaceState {
        key: SURFACE_KEY.into(),
        surface: SCHEMA.surface.clone(),
        title: SCHEMA.title.clone().unwrap_or_else(|| "Quiver Tweaks".into()),
        help: SCHEMA.help.clone(),
        kind: SurfaceKind::Platform,
        mod_id: None,
        version: None,
        version_label: Some(a.kind.label().into()),
        schema_source: SchemaSource::Catalog,
        schema_note: "Quiver catalog (options/quiver-tweaks.toml)".into(),
        files: Vec::new(),
        live: LiveInfo::default(),
        pages,
        options,
        presets: SCHEMA.presets.iter().map(|p| PresetInfo { id: p.id.clone(), label: p.label.clone(), help: p.help.clone(), set: p.set.clone() }).collect(),
        metrics: Vec::new(),
        actions: BTreeMap::new(),
        raw: Vec::new(),
        dormant: Vec::new(),
        lint: plan.map(|p| p.warnings).unwrap_or_default(),
    })
}

fn storage_key(a: &At) -> String {
    a.layers.last().map(|l| l.1.clone()).unwrap_or_else(|| "global".into())
}

enum Change {
    Ops(Vec<OptionOp>),
    Prefer(String, Prefer),
    Restore(u32),
}

#[allow(clippy::too_many_arguments)]
fn set(o: &mut TwDemo, d: &Detail, st: &State, scope: &Scope, change: Change, message: Option<String>, source: &str, events: &mut Vec<Event>) -> Result<OptionsChange> {
    let a = resolve(d, st, scope)?;
    let key = storage_key(&a);
    let below: Vec<(ScopeKind, String)> = a.layers[..a.layers.len() - 1].to_vec();
    let inherited = resolve_all(o, &below);
    let cur = o.cur(&key);
    struct Acc {
        tweaks: BTreeMap<String, Value>,
        prefer: BTreeMap<String, Value>,
        outcomes: Vec<OpOutcome>,
        changes: Vec<String>,
    }
    let mut acc = Acc { tweaks: cur.values.get(SURFACE_KEY).cloned().unwrap_or_default(), prefer: cur.values.get(PREFER_KEY).cloned().unwrap_or_default(), outcomes: Vec::new(), changes: Vec::new() };
    let label = a.kind.label();
    let running = a.game.is_some_and(|g| g.as_ubisoft().is_some_and(|p| d.running.contains_key(&p)));
    let set_one = |acc: &mut Acc, id: &str, v: Option<&Value>| {
        let Some(def) = SCHEMA.options.get(id) else {
            acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id: id.into(), outcome: Outcome::Rejected { reason: format!("Quiver Tweaks has no tweak {id}") }, before: None, after: None });
            return;
        };
        let base = inherited.get(id).map(|r| r.value.clone()).unwrap_or(Value::Null);
        let had = acc.tweaks.get(id).cloned();
        let before = had.clone().unwrap_or_else(|| base.clone());
        let new = match v {
            Some(v) => match def.normalize(v) {
                Ok(n) => Some(n),
                Err(why) => {
                    acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id: id.into(), outcome: Outcome::Rejected { reason: format!("{id}: {why}") }, before: Some(before), after: None });
                    return;
                }
            },
            None => None,
        };
        if had.as_ref().zip(new.as_ref()).is_some_and(|(x, y)| same_value(x, y)) || (had.is_none() && new.is_none()) {
            acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id: id.into(), outcome: Outcome::Unchanged, before: Some(before.clone()), after: Some(before) });
            return;
        }
        match &new {
            Some(n) => acc.tweaks.insert(id.to_owned(), n.clone()),
            None => acc.tweaks.remove(id),
        };
        let after = new.clone().unwrap_or(base);
        acc.changes.push(format!("{label}: {id} {} → {}{}", def.display(&before), def.display(&after), if new.is_none() { " (reset)" } else { "" }));
        acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id: id.into(), outcome: Outcome::SavedRestart { running }, before: Some(before), after: Some(after) });
    };
    match change {
        Change::Ops(ops) => {
            for op in ops {
                match op {
                    OptionOp::Set { id, value, .. } => set_one(&mut acc, &id, Some(&value)),
                    OptionOp::Reset { id, .. } => set_one(&mut acc, &id, None),
                    OptionOp::ResetAll { .. } => {
                        for id in acc.tweaks.keys().cloned().collect::<Vec<_>>() {
                            set_one(&mut acc, &id, None);
                        }
                    }
                    OptionOp::Preset { id, .. } => {
                        let p = SCHEMA.presets.iter().find(|p| p.id == id || p.label.eq_ignore_ascii_case(&id)).ok_or_else(|| anyhow!("Quiver Tweaks has no preset {id}"))?;
                        for (oid, v) in &p.set {
                            set_one(&mut acc, oid, Some(v));
                        }
                    }
                    OptionOp::Raw { binding, .. } => acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id: binding, outcome: Outcome::Rejected { reason: "Quiver Tweaks has no files: set a tweak by id".into() }, before: None, after: None }),
                }
            }
        }
        Change::Prefer(id, p) => {
            SCHEMA.options.get(&id).ok_or_else(|| anyhow!("Quiver Tweaks has no tweak {id}"))?;
            let after = json!(p);
            if acc.prefer.get(&id) != Some(&after) {
                acc.changes.push(format!("{label}: {id} implemented by {} → {}", acc.prefer.get(&id).map(scalar_text).unwrap_or_else(|| "default (provider)".into()), scalar_text(&after)));
                acc.prefer.insert(id.clone(), after.clone());
            }
            acc.outcomes.push(OpOutcome { surface: SURFACE_KEY.into(), id, outcome: Outcome::SavedRestart { running }, before: None, after: Some(after) });
        }
        Change::Restore(n) => {
            let target = if n == 0 { OptionsRevision::default() } else { o.hist.get(&key).and_then(|h| h.iter().find(|r| r.revision == n)).cloned().ok_or_else(|| anyhow!("no revision r{n} in this scope"))? };
            acc.tweaks = target.values.get(SURFACE_KEY).cloned().unwrap_or_default();
            acc.prefer = target.values.get(PREFER_KEY).cloned().unwrap_or_default();
            if cur.values.get(SURFACE_KEY).cloned().unwrap_or_default() != acc.tweaks || cur.values.get(PREFER_KEY).cloned().unwrap_or_default() != acc.prefer {
                acc.changes.push(format!("{label}: restored r{n}"));
            }
        }
    }
    let Acc { tweaks, prefer, outcomes, changes } = acc;
    let (product_id, profile) = match scope {
        Scope::Profile { game, .. } => (game.as_ubisoft().unwrap_or(0), a.profile.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into())),
        Scope::Game { game } => (game.as_ubisoft().unwrap_or(0), String::new()),
        Scope::Global => (0, String::new()),
    };
    let mut out = OptionsChange { product_id, profile, revision: 0, outcomes, changes: changes.clone(), live: Vec::new(), warnings: Vec::new() };
    if changes.is_empty() {
        return Ok(out);
    }
    let mut values: Values = BTreeMap::new();
    if !tweaks.is_empty() {
        values.insert(SURFACE_KEY.into(), tweaks);
    }
    if !prefer.is_empty() {
        values.insert(PREFER_KEY.into(), prefer);
    }
    let h = o.hist.entry(key).or_default();
    let n = h.last().map_or(1, |r| r.revision + 1);
    h.push(OptionsRevision { revision: n, at: Utc::now(), source: source.into(), session: None, message, changes: changes.clone(), values, raw: Default::default() });
    out.revision = n;
    if running {
        out.warnings.push("tweaks apply at the next launch: the running game keeps what it started with".into());
    }
    events.push(Event::TweaksChanged { scope: scope.clone(), revision: n, source: source.into(), changes });
    Ok(out)
}

fn with<T>(key: usize, f: impl FnOnce(&mut TwDemo) -> T) -> T {
    let mut all = TW.lock().unwrap();
    f(all.entry(key).or_insert_with(TwDemo::fixtures))
}

/// Answer a tweaks request (`detail.rs` hands them over).
pub(super) fn handle(key: usize, d: &Detail, st: &State, req: TweaksRequest, events: &mut Vec<Event>) -> Result<Value> {
    with(key, |o| {
        Ok(match req {
            TweaksRequest::List { scope, level } => {
                let mut s = state(o, d, st, &scope)?;
                if let Some(l) = level {
                    s.options.retain(|_, x| x.def.level <= l);
                }
                json!(s)
            }
            TweaksRequest::Set { scope, ops, message, .. } => json!(set(o, d, st, &scope, Change::Ops(ops), message, "gui", events)?),
            TweaksRequest::Prefer { scope, id, prefer, message, .. } => json!(set(o, d, st, &scope, Change::Prefer(id, prefer), message, "gui", events)?),
            TweaksRequest::Restore { scope, revision, .. } => json!(set(o, d, st, &scope, Change::Restore(revision), Some("restore".into()), "gui", events)?),
            TweaksRequest::History { scope } => {
                let a = resolve(d, st, &scope)?;
                json!(o.hist.get(&storage_key(&a)).cloned().unwrap_or_default())
            }
            TweaksRequest::Plan { game, profile } => {
                let p = match game.as_ubisoft() {
                    Some(pid) => launch_profile(d, st, pid, profile.as_deref())?,
                    None => None,
                };
                json!(plan(o, d, &game, p.as_deref()))
            }
            TweaksRequest::Capabilities { game } => json!(caps(game.as_ref())),
            TweaksRequest::Status => json!(Vec::<SessionTweaks>::new()),
            TweaksRequest::Sessions { game, limit } => json!(last_session(&game).into_iter().take(limit.unwrap_or(20)).collect::<Vec<_>>()),
        })
    })
}

/// The platform surface at a profile's scope, for the Options view (listed last on the wire).
pub(super) fn profile_surface(key: usize, d: &Detail, st: &State, product_id: u32, profile: &str) -> Result<SurfaceState> {
    with(key, |o| state(o, d, st, &Scope::Profile { game: GameRef::ubisoft(product_id), profile: Some(profile.to_owned()) }))
}

/// Options ops on the platform surface, saved at the profile's scope.
#[allow(clippy::too_many_arguments)]
pub(super) fn profile_set(key: usize, d: &Detail, st: &State, product_id: u32, profile: &str, ops: Vec<OptionOp>, message: Option<String>, events: &mut Vec<Event>) -> Result<OptionsChange> {
    with(key, |o| set(o, d, st, &Scope::Profile { game: GameRef::ubisoft(product_id), profile: Some(profile.to_owned()) }, Change::Ops(ops), message, "gui", events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_parses_with_bindings_and_presets() {
        assert_eq!(SCHEMA.kind, Some(SurfaceKind::Platform));
        assert!(SCHEMA.options.contains_key("fps.cap") && SCHEMA.presets.iter().any(|p| p.id == "competitive"));
        let c = caps(None);
        assert_eq!(c.summary(), "NVIDIA GeForce RTX 5070 Laptop GPU · driver 620.70.06 · GE-Proton11-12");
        let b = &SCHEMA.options["sync.vsync"].via[0];
        assert_eq!(writes(b, "off").unwrap().unwrap(), vec!["DXVK_CONFIG: d3d9.presentInterval = 0", "DXVK_CONFIG: dxgi.syncInterval = 0"]);
        assert_eq!(writes(b, "app").unwrap(), None);
    }

    #[tokio::test]
    async fn scopes_plans_and_changes() {
        use quiver_api::Request;
        use quiver_api::options::OptionsRequest;
        let (demo, _events) = crate::demo::Demo::start();
        let ask = |r: TweaksRequest| demo.handle(Request::Tweaks(r));
        let origins = GameRef::ubisoft(ORIGINS);
        let list = |scope: Scope| -> SurfaceState { serde_json::from_value(ask(TweaksRequest::List { scope, level: None }).unwrap()).unwrap() };

        // Global: values with their revisions, no plan.
        let g = list(Scope::Global);
        assert_eq!(g.kind, SurfaceKind::Platform);
        assert!(matches!(&g.options["fps.cap"].from, ValueSource::Scope { scope: ScopeKind::Global, revision: 3, source } if source == "cli"));
        assert!(g.options["fps.cap"].tweak.as_ref().unwrap().planned.is_none());

        // The DLSS profile: the mod provides the cap, blocks multi-frame generation; the
        // updater is unavailable; the vsync of the game's scope goes through DXVK.
        let p = list(Scope::Profile { game: origins, profile: Some(super::super::ORIGINS_DEFAULT.into()) });
        let row = |id: &str| p.options[id].tweak.clone().unwrap();
        assert!(matches!(row("fps.cap").planned, Some(Planned::Provided { ref by, can_prefer_quiver: true, .. }) if by == "DLSS"));
        assert!(matches!(row("dlss.fg.frames").planned, Some(Planned::Blocked { .. })));
        assert!(matches!(row("dlss.runtime").planned, Some(Planned::Unavailable { .. })));
        assert!(matches!(row("sync.vsync").planned, Some(Planned::Apply { ref mech, .. }) if mech == "dxvk"));
        assert!(matches!(row("tex.anisotropy").last, Some(Effect::Unconfirmed { .. })));
        assert_eq!(row("sync.vsync").game, Some(json!("off")));

        // The game's scope sees the profile's value as an override.
        let gs = list(Scope::Game { game: origins });
        assert_eq!(gs.options["sync.max_latency"].tweak.as_ref().unwrap().overridden_by, Some(ScopeKind::Profile));

        // Set, prefer and restore are revisions of the scope.
        let set = |scope: Scope, ops| -> OptionsChange { serde_json::from_value(ask(TweaksRequest::Set { scope, ops, message: None, confirmed: false }).unwrap()).unwrap() };
        let c = set(Scope::Game { game: origins }, vec![OptionOp::Set { surface: "quiver".into(), id: "tex.anisotropy".into(), value: json!("8") }]);
        assert_eq!(c.revision, 2);
        assert!(matches!(c.outcomes[0].outcome, Outcome::SavedRestart { running: false }));
        let c = set(Scope::Global, vec![OptionOp::Set { surface: "quiver".into(), id: "fps.cap".into(), value: json!(9999) }]);
        assert!(matches!(c.outcomes[0].outcome, Outcome::Rejected { .. }) && c.revision == 0);
        let profile = Scope::Profile { game: origins, profile: Some(super::super::ORIGINS_DEFAULT.into()) };
        let c: OptionsChange = serde_json::from_value(ask(TweaksRequest::Prefer { scope: profile.clone(), id: "fps.cap".into(), prefer: Prefer::Quiver, message: None }).unwrap()).unwrap();
        assert_eq!(c.revision, 3);
        let p = list(profile.clone());
        assert!(matches!(p.options["fps.cap"].tweak.as_ref().unwrap().planned, Some(Planned::Blocked { .. })), "DXVK's limiter conflicts with DLSS FG");
        let c: OptionsChange = serde_json::from_value(ask(TweaksRequest::Restore { scope: profile.clone(), revision: 2 }).unwrap()).unwrap();
        assert_eq!(c.revision, 4);
        let h: Vec<OptionsRevision> = serde_json::from_value(ask(TweaksRequest::History { scope: profile }).unwrap()).unwrap();
        assert_eq!(h.len(), 4);

        // The Options view lists the platform surface last and saves it at the profile.
        let surfaces: Vec<SurfaceState> = serde_json::from_value(demo.handle(Request::Options(OptionsRequest::List { game: ORIGINS.into(), profile: None, level: None })).unwrap()).unwrap();
        assert_eq!(surfaces.last().map(|s| s.key.as_str()), Some(SURFACE_KEY));
        let ops = vec![OptionOp::Set { surface: SURFACE_KEY.into(), id: "hdr".into(), value: json!("on") }];
        let c: OptionsChange = serde_json::from_value(
            demo.handle(Request::Options(OptionsRequest::Set { game: ORIGINS.into(), profile: None, ops, live: quiver_api::options::Live::IfRunning, message: None, confirmed: false })).unwrap(),
        )
        .unwrap();
        assert_eq!((c.revision, c.outcomes[0].surface.as_str()), (5, SURFACE_KEY));
    }
}
