//! Demo answers for mod options (`Request::Options`, design/MOD-CONFIG.md): AC Origins' DLSS
//! mod is described by Quiver's catalog schema (`options/aco-dlss.toml`, the real one), the
//! game's own `ACO.ini` by a small catalog-style game surface, and "Skip intro videos" has no
//! schema (so "infer from file" has something to do). Values, revisions, outcomes and events
//! follow quiverd: a change reaches a running game live only for `live` options, others say
//! when they apply, and every change is a revision with its actor.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, anyhow, bail};
use chrono::{Duration as Span, Utc};
use serde_json::{Value, json};
use quiver_api::Event;
use quiver_api::mods::VANILLA;
use quiver_api::options::{
    Apply, CrashNote, GroupState, KeyBinding, KeyConflict, KeysReport, LintFinding, LiveChange, LiveChannel, LiveInfo, OpOutcome, OptionDiff,
    OptionOp, OptionState, OptionsChange, OptionsRequest, OptionsRevision, Outcome, PageState, Pending, PresetInfo, RawKey, Schema, SchemaInfo,
    SchemaSource, SessionOptions, Severity, SurfaceFile, SurfaceKind, SurfaceState, ValueSource, Values, same_value,
};

use super::{Detail, State, at, launch_profile, name};

/// Quiver's catalog schema for aco-dlss (the real file).
const ACO_DLSS: &str = include_str!("../../../../../options/aco-dlss.toml");
/// The DLSS mod in the demo store (`origins_mods`), and the one without a schema.
const DLSS_MOD: &str = "ld7s3vq";
const SKIP_MOD: &str = "lb5k1yq";
const ORIGINS: u32 = 3539;

/// AC Origins' own graphics settings as a game surface (catalog-style; MOD-CONFIG steer 16).
const GAME_SURFACE: &str = r#"
format = 1
surface = "aco"
title = "AC Origins"
help = "The game's own graphics settings in ACO.ini (Documents). The game's menu writes this file too."

[file.ini]
path = "Assassin's Creed Origins/ACO.ini"
root = "docs"
format = "ini"
owner = "program"

[[page]]
id = "graphics"
title = "Graphics"
options = ["Graphics.EnvironmentQuality", "Graphics.Fov", "Graphics.PixelDensity"]

[option."Graphics.EnvironmentQuality"]
type = "enum"
label = "Environment quality"
values = [{ value = "Low" }, { value = "Medium" }, { value = "High" }, { value = "Very High" }, { value = "Ultra High" }]
level = "basic"
apply = "restart"

[option."Graphics.Fov"]
type = "float"
label = "Field of view"
help = "1.0 is the game's default; the in-game slider goes to 1.15."
min = 0.85
max = 1.15
step = 0.05
precision = 2
level = "basic"
apply = "restart"

[option."Graphics.PixelDensity"]
type = "float"
label = "Resolution scale"
help = "DLSS writes this at launch when its Render scale is set."
min = 0.5
max = 2.0
step = 0.05
precision = 3
level = "basic"
apply = "restart"
"#;

/// What `options infer` guesses from Skip intro videos' `SkipIntro.ini` (as quiverd would).
const SKIP_INFERRED: &str = r#"format = 1
surface = "skip-intro-videos"
title = "Skip intro videos"

[file.ini]
path = "SkipIntro.ini"
format = "ini"

[[page]]
id = "main"
title = "Main"
options = ["Main.SkipLogos", "Main.SkipWarning", "Main.Delay"]

[option."Main.SkipLogos"]
type = "bool"
label = "SkipLogos"
help = "Skip the Ubisoft and studio logos"

[option."Main.SkipWarning"]
type = "bool"
label = "SkipWarning"
help = "Skip the epilepsy warning screen"

[option."Main.Delay"]
type = "int"
label = "Delay"
help = "Milliseconds to wait before skipping (0 = none)"
"#;

const SKIP_INI: &str = "; Skip intro videos for AC Origins\n[Main]\n; Skip the Ubisoft and studio logos\nSkipLogos = 1\n; Skip the epilepsy warning screen\nSkipWarning = 1\n; Milliseconds to wait before skipping (0 = none)\nDelay = 0\n";

/// The person's values in the demo: whole-state revisions per profile id, oldest first.
#[derive(Default)]
pub(super) struct OptDemo {
    history: BTreeMap<String, Vec<OptionsRevision>>,
    /// Local schemas written from the app (`SchemaSet`): mod id → (schema, text, revision).
    local: BTreeMap<String, (Schema, String, u32)>,
    /// Session records, newest first.
    sessions: Vec<SessionOptions>,
    /// What the running game has (surface key → id → value), from when it started.
    has: Option<(String, Values)>,
}

static OPTS: LazyLock<Mutex<HashMap<usize, OptDemo>>> = LazyLock::new(Mutex::default);
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| toml::from_str(ACO_DLSS).expect("options/aco-dlss.toml parses"));
static GAME: LazyLock<Schema> = LazyLock::new(|| toml::from_str(GAME_SURFACE).expect("the demo game surface parses"));

fn dlss_key() -> String {
    format!("{DLSS_MOD}/aco-dlss")
}

const GAME_KEY: &str = "game/aco";

/// The shipped file's values (aco_dlss.ini v3.8.10 in the demo), where the schema has none.
fn shipped_default(id: &str, def: &quiver_api::options::OptionDef) -> Value {
    let known = match id {
        "upscaler.enabled" | "fg.enabled" | "reflex.markers" | "fg.hudless" | "fg.pacing" | "fg.present_gate" | "fg.descriptor_heap" => Some(json!(true)),
        "upscaler.preset" => Some(json!("M")),
        "upscaler.render_scale" => Some(json!(0.5)),
        "upscaler.toggle_key" => Some(json!("F10")),
        "upscaler.mip_bias" => Some(json!("auto")),
        "reflex.mode" => Some(json!("on")),
        "fg.mode" => Some(json!("d3d12")),
        "fg.toggle_key" => Some(json!("F9")),
        "fg.refresh_hz" => Some(json!(0)),
        "fg.present_thread" => Some(json!("1")),
        "Graphics.EnvironmentQuality" => Some(json!("High")),
        "Graphics.Fov" => Some(json!(1.0)),
        "Graphics.PixelDensity" => Some(json!(1.0)),
        _ => None,
    };
    if let Some(v) = known.or_else(|| def.default.clone()) {
        return v;
    }
    use quiver_api::options::OptionType as T;
    match def.ty {
        T::Bool => json!(false),
        T::Enum => def.values.first().map(|e| e.value.clone()).unwrap_or(Value::Null),
        T::Int | T::Float => {
            if let Some(s) = def.special.first() {
                return s.value.clone();
            }
            let (lo, hi) = (def.min.unwrap_or(0.0), def.max.unwrap_or(f64::MAX));
            let v = if (lo..=hi).contains(&1.0) { 1.0 } else { lo };
            if def.ty == T::Int { json!(v as i64) } else { json!(v) }
        }
        T::Key => json!(def.none.clone().unwrap_or_else(|| "none".into())),
        T::List => json!([]),
        _ => json!(""),
    }
}

fn change_line(title: &str, id: &str, def: &quiver_api::options::OptionDef, a: &Value, b: &Value) -> String {
    format!("{title} {id} {} → {}", def.display(a), def.display(b))
}

#[allow(clippy::too_many_arguments)]
fn rev(revision: u32, days: i64, (h, m): (u32, u32), source: &str, session: Option<&str>, message: Option<&str>, changes: &[&str], values: Values) -> OptionsRevision {
    OptionsRevision {
        revision,
        at: at(days, h, m),
        source: source.into(),
        session: session.map(str::to_owned),
        message: message.map(str::to_owned),
        changes: changes.iter().map(|s| (*s).to_owned()).collect(),
        values,
        raw: Default::default(),
    }
}

/// The DLSS profile's history (MOD-CONFIG's mock-ups: r10–r16).
fn dlss_history() -> Vec<OptionsRevision> {
    let k = dlss_key();
    let mut v: Values = BTreeMap::new();
    let mut out = Vec::new();
    let set = |v: &mut Values, key: &str, id: &str, val: Value| {
        v.entry(key.into()).or_default().insert(id.into(), val);
    };
    set(&mut v, &k, "upscaler.preset", json!("K"));
    out.push(rev(10, 9, (20, 4), "gui", None, None, &["DLSS upscaler.preset M  M: DLSS 4.5, performance → K: DLSS 4 transformer"], v.clone()));
    set(&mut v, &k, "fg.enabled", json!(false));
    out.push(rev(11, 6, (22, 41), "file", None, Some("aco_dlss.ini was edited outside Quiver"), &["DLSS fg.enabled on → off"], v.clone()));
    set(&mut v, &k, "reflex.mode", json!("boost"));
    out.push(rev(12, 5, (18, 40), "aco-dlss", None, Some("Boost keeps clocks up for FG pacing."), &["DLSS reflex.mode on  On → boost  On + Boost"], v.clone()));
    v.get_mut(&k).unwrap().remove("fg.enabled");
    set(&mut v, &k, "fg.refresh_hz", json!(165));
    out.push(rev(13, 3, (19, 12), "gui", None, None, &["DLSS fg.enabled off → on", "DLSS fg.refresh_hz 0 (Not set) → 165 Hz"], v.clone()));
    set(&mut v, &k, "world.npc_density", json!(2.0));
    set(&mut v, &k, "world.lod_distance", json!(1.5));
    set(&mut v, &k, "upscaler.render_scale", json!(0.58));
    out.push(rev(
        14,
        1,
        (21, 19),
        "in-game",
        Some("s-4kq2mx"),
        Some("Changed in game 21:04–21:19"),
        &["DLSS world.npc_density 1 × → 2 ×", "DLSS world.lod_distance 1 × → 1.5 ×", "DLSS upscaler.render_scale 0.500 Performance → 0.580 Balanced"],
        v.clone(),
    ));
    set(&mut v, GAME_KEY, "Graphics.EnvironmentQuality", json!("Very High"));
    set(&mut v, GAME_KEY, "Graphics.Fov", json!(1.15));
    out.push(rev(
        15,
        1,
        (21, 22),
        "game",
        Some("s-4kq2mx"),
        Some("Changed in the game's own menu"),
        &["AC Origins Graphics.EnvironmentQuality High → Very High", "AC Origins Graphics.Fov 1.00 → 1.15"],
        v.clone(),
    ));
    out
}

fn demo_sessions(dlss: &str) -> Vec<SessionOptions> {
    let crash_at = at(4, 21, 3);
    let crashed = SessionOptions {
        session: "s-9tq1vb".into(),
        product_id: ORIGINS,
        profile: dlss.into(),
        launched_revision: 12,
        started: crash_at - Span::minutes(23),
        ended: Some(crash_at),
        live_changes: vec![LiveChange { at: crash_at - Span::seconds(40), surface: dlss_key(), id: "fg.present_thread".into(), source: "gui".into(), value: json!("0") }],
        crash: Some(CrashNote {
            seconds_after: 40,
            change: format!("{} fg.present_thread", dlss_key()),
            exit_code: Some(3),
            message: "AC Origins crashed 40 s after DLSS Present thread was set. Revert this session's in-game changes?".into(),
        }),
    };
    let ingame = SessionOptions {
        session: "s-4kq2mx".into(),
        product_id: ORIGINS,
        profile: dlss.into(),
        launched_revision: 13,
        started: at(1, 20, 41),
        ended: Some(at(1, 22, 55)),
        live_changes: Vec::new(),
        crash: None,
    };
    vec![ingame, crashed]
}

impl OptDemo {
    fn fixtures() -> Self {
        let dlss = super::ORIGINS_DEFAULT.to_owned();
        let mut history = BTreeMap::new();
        history.insert(dlss.clone(), dlss_history());
        Self { history, sessions: demo_sessions(&dlss), ..Default::default() }
    }

    fn hist(&self, profile: &str) -> &[OptionsRevision] {
        self.history.get(profile).map(Vec::as_slice).unwrap_or_default()
    }

    fn current(&self, profile: &str) -> OptionsRevision {
        self.history.get(profile).and_then(|h| h.last()).cloned().unwrap_or_default()
    }
}

/// One surface of a profile, resolved: its schema and where it comes from.
struct Resolved {
    key: String,
    kind: SurfaceKind,
    mod_id: Option<String>,
    version: Option<String>,
    version_label: Option<String>,
    source: SchemaSource,
    note: String,
    schema: Schema,
    file: SurfaceFile,
}

/// The profile's surfaces (vanilla gets the game's own).
fn surfaces(d: &Detail, o: &OptDemo, product_id: u32, profile: Option<&str>) -> Vec<Resolved> {
    let mut out = Vec::new();
    if product_id != ORIGINS {
        return out;
    }
    let mods = d.mods.get(&product_id);
    let stack = profile.and_then(|p| mods.and_then(|m| m.live().find(|r| r.profile.id == p)).map(|r| m_info(d, product_id, r))).unwrap_or_default();
    for r in &stack {
        let file = |path: &str, root| SurfaceFile {
            id: "ini".into(),
            def: quiver_api::options::FileDef { path: path.into(), root, ..Default::default() },
            rel: path.into(),
            scope: format!("install:{}", profile.unwrap_or(VANILLA)),
            ..Default::default()
        };
        if r.mod_id == DLSS_MOD {
            out.push(Resolved {
                key: dlss_key(),
                kind: SurfaceKind::Mod,
                mod_id: Some(r.mod_id.clone()),
                version: Some(r.version.clone()),
                version_label: Some(r.label.clone()),
                source: SchemaSource::Catalog,
                note: "Quiver catalog (audited 2026-11-16 against aco_dlss.ini)".into(),
                schema: SCHEMA.clone(),
                file: file("aco_dlss.ini", quiver_api::options::FileRoot::Install),
            });
        } else if let Some((s, _, rev)) = o.local.get(&r.mod_id) {
            out.push(Resolved {
                key: format!("{}/{}", r.mod_id, s.surface),
                kind: SurfaceKind::Mod,
                mod_id: Some(r.mod_id.clone()),
                version: Some(r.version.clone()),
                version_label: Some(r.label.clone()),
                source: SchemaSource::Local,
                note: format!("local, written in the app (r{rev})"),
                schema: s.clone(),
                file: file(s.files.values().next().map(|f| f.path.as_str()).unwrap_or("SkipIntro.ini"), quiver_api::options::FileRoot::Install),
            });
        }
    }
    out.push(Resolved {
        key: GAME_KEY.into(),
        kind: SurfaceKind::Game,
        mod_id: None,
        version: None,
        version_label: None,
        source: SchemaSource::Catalog,
        note: "Quiver catalog (the game's graphics settings)".into(),
        schema: GAME.clone(),
        file: SurfaceFile {
            id: "ini".into(),
            def: quiver_api::options::FileDef { path: "Assassin's Creed Origins/ACO.ini".into(), root: quiver_api::options::FileRoot::Docs, ..Default::default() },
            rel: "drive_c/users/steamuser/Documents/Assassin's Creed Origins/ACO.ini".into(),
            scope: "prefix".into(),
            ..Default::default()
        },
    });
    out
}

fn m_info(d: &Detail, product_id: u32, r: &quiver_api::mods::ProfileRevision) -> Vec<quiver_api::mods::ResolvedMod> {
    d.mods.get(&product_id).map(|m| m.info(product_id, r).resolved).unwrap_or_default()
}

/// The session running this profile, if the demo game runs it.
fn running_with(d: &Detail, product_id: u32, profile: &str) -> bool {
    d.running.get(&product_id).is_some_and(|(p, _)| p.as_deref().unwrap_or(VANILLA) == profile)
}

fn session_id(d: &Detail, product_id: u32) -> Option<String> {
    d.running.get(&product_id).map(|(_, t)| format!("s-{}", (t.timestamp() % 1_000_000).abs()))
}

fn state(s: &Resolved, hist: &[OptionsRevision], running: bool, session: Option<String>, has: Option<&Values>) -> SurfaceState {
    let cur = hist.last().cloned().unwrap_or_default();
    let mine = cur.values.get(&s.key).cloned().unwrap_or_default();
    let defaults: BTreeMap<String, Value> = s.schema.options.iter().map(|(id, def)| (id.clone(), shipped_default(id, def))).collect();
    let effective = |id: &str| mine.get(id).cloned().or_else(|| defaults.get(id).cloned());
    // Which revision set each value: the last one that changed it to what it is now.
    let hist_from = |id: &str| -> ValueSource {
        let Some(now) = mine.get(id) else { return ValueSource::Default };
        let mut from = ValueSource::Profile { revision: cur.revision, source: cur.source.clone() };
        let mut prev: Option<&Value> = None;
        for r in hist {
            let v = r.values.get(&s.key).and_then(|m| m.get(id));
            if v.is_some_and(|v| same_value(v, now)) && prev.is_none_or(|p| !same_value(p, now)) {
                from = ValueSource::Profile { revision: r.revision, source: r.source.clone() };
            }
            prev = v;
        }
        from
    };
    let options = s
        .schema
        .options
        .iter()
        .map(|(id, def)| {
            let value = effective(id).unwrap_or(Value::Null);
            let pending = match (running, has.and_then(|h| h.get(&s.key)).and_then(|m| m.get(id))) {
                (true, Some(was)) if !same_value(was, &value) => match def.apply {
                    Apply::At => Pending::At { at: def.at.clone().unwrap_or_default() },
                    Apply::Restart | Apply::Launch => Pending::Restart,
                    Apply::Live => Pending::None,
                },
                _ => Pending::None,
            };
            let st = OptionState {
                def: def.clone(),
                text: def.to_text(&value),
                value: value.clone(),
                default: defaults.get(id).cloned().unwrap_or(Value::Null),
                from: hist_from(id),
                visible: def.visible(&effective),
                pending,
                dormant: None,
                invalid: None,
                tweak: None,
            };
            (id.clone(), st)
        })
        .collect();
    let mut listed = std::collections::BTreeSet::new();
    let pages: Vec<PageState> = s
        .schema
        .pages
        .iter()
        .map(|p| {
            let mut groups = vec![GroupState { title: String::new(), options: p.options.clone() }];
            groups.extend(p.groups.iter().map(|g| GroupState { title: g.title.clone(), options: g.options.clone() }));
            for g in &groups {
                listed.extend(g.options.iter().cloned());
            }
            PageState { id: p.id.clone(), title: p.title.clone(), groups }
        })
        .collect();
    let mut pages = pages;
    let rest: Vec<String> = s.schema.options.keys().filter(|id| !listed.contains(*id)).cloned().collect();
    if !rest.is_empty() {
        pages.push(PageState { id: "other".into(), title: "Other".into(), groups: vec![GroupState { title: String::new(), options: rest }] });
    }
    // Every key is described by the catalog; the skip-intro guess has none left either.
    let raw: Vec<RawKey> = Vec::new();
    let lint = if s.source == SchemaSource::Local { vec!["note: guessed types; apply timing unknown (restart)".to_owned()] } else { Vec::new() };
    SurfaceState {
        key: s.key.clone(),
        surface: s.schema.surface.clone(),
        title: s.schema.title.clone().unwrap_or_else(|| s.schema.surface.clone()),
        help: s.schema.help.clone(),
        kind: s.kind,
        mod_id: s.mod_id.clone(),
        version: s.version.clone(),
        version_label: s.version_label.clone(),
        schema_source: s.source,
        schema_note: s.note.clone(),
        files: vec![s.file.clone()],
        live: LiveInfo {
            channel: if s.kind == SurfaceKind::Mod && s.schema.files.values().any(|f| f.reload != quiver_api::options::Reload::None) { LiveChannel::File } else { LiveChannel::None },
            connected: running,
            session,
        },
        pages,
        options,
        presets: s.schema.presets.iter().map(|p| PresetInfo { id: p.id.clone(), label: p.label.clone(), help: p.help.clone(), set: p.set.clone() }).collect(),
        metrics: s.schema.metrics.keys().map(|m| format!("{}.{m}", s.schema.surface)).collect(),
        actions: Default::default(),
        raw,
        dormant: Vec::new(),
        lint,
    }
}

/// Answer an options request (`detail.rs` hands them over). Events go into `events`.
pub(super) fn handle(key: usize, d: &mut Detail, st: &mut State, req: OptionsRequest, events: &mut Vec<Event>) -> Result<Value> {
    let mut all = OPTS.lock().unwrap();
    let o = all.entry(key).or_insert_with(OptDemo::fixtures);
    let profile_of = |product_id: u32, p: Option<&str>| -> Result<String> { Ok(launch_profile(d, st, product_id, p)?.unwrap_or_else(|| VANILLA.into())) };
    Ok(match req {
        OptionsRequest::List { product_id, profile, level } => {
            let p = profile_of(product_id, profile.as_deref())?;
            let running = running_with(d, product_id, &p);
            snapshot_has(o, d, product_id, &p, running);
            let has = o.has.as_ref().filter(|(pp, _)| *pp == p).map(|(_, v)| v.clone());
            let hist = o.hist(&p).to_vec();
            let pid = (p != VANILLA).then_some(p.as_str());
            let mut out: Vec<SurfaceState> =
                surfaces(d, o, product_id, pid).iter().map(|s| state(s, &hist, running, running.then(|| session_id(d, product_id)).flatten(), has.as_ref())).collect();
            // Quiver Tweaks at this profile's scope, last (as quiverd lists it).
            out.push(super::tweaks::profile_surface(key, d, st, product_id, &p)?);
            if let Some(l) = level {
                for s in &mut out {
                    s.options.retain(|_, x| x.def.level <= l);
                }
            }
            json!(out)
        }
        OptionsRequest::Set { product_id, profile, ops, live, message, confirmed } => {
            let p = profile_of(product_id, profile.as_deref())?;
            // Ops on Quiver Tweaks go to the profile's tweak scope (as quiverd routes them).
            let (tw, ops): (Vec<OptionOp>, Vec<OptionOp>) = ops.into_iter().partition(|op| quiver_api::tweaks::is_platform_surface(op_surface(op)));
            let mut out = if ops.is_empty() {
                OptionsChange { product_id, profile: p.clone(), ..Default::default() }
            } else {
                set(o, d, st, product_id, &p, ops, live, message.clone(), confirmed, "gui", events)?
            };
            if !tw.is_empty() {
                let t = super::tweaks::profile_set(key, d, st, product_id, &p, tw, message, events)?;
                out.revision = out.revision.max(t.revision);
                out.outcomes.extend(t.outcomes);
                out.changes.extend(t.changes);
                out.warnings.extend(t.warnings);
            }
            json!(out)
        }
        OptionsRequest::History { product_id, profile } => {
            let p = profile_of(product_id, profile.as_deref())?;
            json!(o.history.get(&p).cloned().unwrap_or_default())
        }
        OptionsRequest::Restore { product_id, profile, revision, .. } => {
            let p = profile_of(product_id, profile.as_deref())?;
            let target = match revision {
                0 => OptionsRevision::default(),
                r => o.history.get(&p).and_then(|h| h.iter().find(|x| x.revision == r)).cloned().ok_or_else(|| anyhow!("no options revision r{r}"))?,
            };
            let ops = ops_towards(&o.current(&p), &target);
            json!(set(o, d, st, product_id, &p, ops, quiver_api::options::Live::IfRunning, Some(format!("restored r{revision}")), true, "gui", events)?)
        }
        OptionsRequest::RevertSession { product_id, session, .. } => {
            let rec = o.sessions.iter().find(|s| s.session == session).cloned().ok_or_else(|| anyhow!("no options record for session {session} of this game"))?;
            let target = o.history.get(&rec.profile).and_then(|h| h.iter().find(|x| x.revision == rec.launched_revision)).cloned().unwrap_or_default();
            let ops = ops_towards(&o.current(&rec.profile), &target);
            let msg = format!("reverted session {session} (back to r{})", rec.launched_revision);
            json!(set(o, d, st, product_id, &rec.profile, ops, quiver_api::options::Live::IfRunning, Some(msg), true, "gui", events)?)
        }
        OptionsRequest::Diff { product_id, profile, against } => {
            let p = profile_of(product_id, profile.as_deref())?;
            json!(diff(o, d, st, product_id, &p, against.as_deref().unwrap_or("default"))?)
        }
        OptionsRequest::Keys { product_id, profile } => {
            let p = profile_of(product_id, profile.as_deref())?;
            json!(keys(o, d, product_id, &p))
        }
        OptionsRequest::Schema { product_id, mod_id, .. } => json!(schema_info(o, d, product_id, &mod_id)?),
        OptionsRequest::SchemaSet { product_id, mod_id, toml: text, .. } => {
            let schema: Schema = toml::from_str(&text).map_err(|e| anyhow!("the schema doesn't parse: {e}"))?;
            let n = o.local.get(&mod_id).map_or(1, |x| x.2 + 1);
            o.local.insert(mod_id.clone(), (schema, text, n));
            events.push(Event::ModsChanged { product_id });
            json!(schema_info(o, d, product_id, &mod_id)?)
        }
        OptionsRequest::Infer { mod_id, file, .. } => {
            if mod_id != SKIP_MOD || !file.eq_ignore_ascii_case("SkipIntro.ini") {
                bail!("demo mode can guess a schema for Skip intro videos' SkipIntro.ini only");
            }
            json!(SKIP_INFERRED)
        }
        OptionsRequest::Files { mod_id, .. } => json!(match mod_id.as_str() {
            SKIP_MOD => vec!["SkipIntro.ini"],
            DLSS_MOD => vec!["aco_dlss.ini", "README.txt"],
            _ => vec![],
        }),
        OptionsRequest::Sessions => json!(st.sessions),
        OptionsRequest::SessionRecords { product_id, profile, limit } => {
            let want = match profile {
                Some(p) => Some(profile_of(product_id, Some(&p))?),
                None => None,
            };
            let mut v: Vec<SessionOptions> = o.sessions.iter().filter(|s| s.product_id == product_id && want.as_ref().is_none_or(|w| &s.profile == w)).cloned().collect();
            v.truncate(limit.unwrap_or(20));
            json!(v)
        }
        OptionsRequest::Invoke { .. } => bail!("actions need a mod connected over the mod SDK; none is in demo mode"),
        OptionsRequest::Lint { .. } => bail!("not available in demo mode: options lint"),
    })
}

fn op_surface(op: &OptionOp) -> &str {
    match op {
        OptionOp::Set { surface, .. } | OptionOp::Reset { surface, .. } | OptionOp::Preset { surface, .. } | OptionOp::ResetAll { surface } | OptionOp::Raw { surface, .. } => surface,
    }
}

/// The DLSS mod is in a profile's stack (it implements some tweaks).
pub(super) fn has_dlss_mod(d: &Detail, product_id: u32, profile: Option<&str>) -> bool {
    let Some(p) = profile else { return false };
    d.mods.get(&product_id).and_then(|m| m.live().find(|r| r.profile.id == p)).is_some_and(|r| m_info(d, product_id, r).iter().any(|x| x.mod_id == DLSS_MOD))
}

/// When the demo game starts with a profile, what it launched with is what it "has".
fn snapshot_has(o: &mut OptDemo, d: &Detail, product_id: u32, profile: &str, running: bool) {
    if !running {
        if o.has.as_ref().is_some_and(|(p, _)| p == profile) && !d.running.contains_key(&product_id) {
            o.has = None;
        }
        return;
    }
    if o.has.as_ref().is_none_or(|(p, _)| p != profile) {
        let hist = o.hist(profile).to_vec();
        let pid = (profile != VANILLA).then_some(profile);
        let has = surfaces(d, o, product_id, pid)
            .iter()
            .map(|s| {
                let st = state(s, &hist, false, None, None);
                (s.key.clone(), st.options.into_iter().map(|(id, x)| (id, x.value)).collect())
            })
            .collect();
        o.has = Some((profile.to_owned(), has));
    }
}

fn ops_towards(cur: &OptionsRevision, target: &OptionsRevision) -> Vec<OptionOp> {
    let mut ops = Vec::new();
    let keys: std::collections::BTreeSet<&String> = cur.values.keys().chain(target.values.keys()).collect();
    for key in keys {
        let (a, b) = (cur.values.get(key).cloned().unwrap_or_default(), target.values.get(key).cloned().unwrap_or_default());
        for (id, v) in &b {
            if a.get(id) != Some(v) {
                ops.push(OptionOp::Set { surface: key.clone(), id: id.clone(), value: v.clone() });
            }
        }
        for id in a.keys().filter(|id| !b.contains_key(*id)) {
            ops.push(OptionOp::Reset { surface: key.clone(), id: id.clone() });
        }
    }
    ops
}

#[allow(clippy::too_many_arguments)]
fn set(
    o: &mut OptDemo,
    d: &Detail,
    st: &State,
    product_id: u32,
    profile: &str,
    ops: Vec<OptionOp>,
    live: quiver_api::options::Live,
    message: Option<String>,
    confirmed: bool,
    source: &str,
    events: &mut Vec<Event>,
) -> Result<OptionsChange> {
    use quiver_api::options::Live;
    let pid = (profile != VANILLA).then_some(profile);
    let surfs = surfaces(d, o, product_id, pid);
    let running = running_with(d, product_id, profile) && live != Live::No;
    let cur = o.current(profile);
    let mut values = cur.values.clone();
    let mut outcomes = Vec::new();
    let mut changes = Vec::new();
    let mut dangers = Vec::new();
    let find = |q: &str| surfs.iter().find(|s| s.key == q || s.schema.surface == q || s.schema.title.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(q)));
    let mut sets: Vec<(String, String, Option<Value>)> = Vec::new();
    for op in ops {
        match op {
            OptionOp::Set { surface, id, value } => sets.push((surface, id, Some(value))),
            OptionOp::Reset { surface, id } => sets.push((surface, id, None)),
            OptionOp::Preset { surface, id } => {
                let s = find(&surface).ok_or_else(|| anyhow!("no options surface '{surface}'"))?;
                let p = s.schema.presets.iter().find(|p| p.id == id).ok_or_else(|| anyhow!("{} has no preset {id}", s.key))?;
                for (oid, v) in &p.set {
                    sets.push((s.key.clone(), oid.clone(), Some(v.clone())));
                }
            }
            OptionOp::ResetAll { surface } => {
                let s = find(&surface).ok_or_else(|| anyhow!("no options surface '{surface}'"))?;
                for oid in values.get(&s.key).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                    sets.push((s.key.clone(), oid, None));
                }
            }
            OptionOp::Raw { surface, binding, .. } => {
                outcomes.push(OpOutcome { surface, id: binding, outcome: Outcome::Rejected { reason: "demo mode has no undescribed keys".into() }, before: None, after: None })
            }
        }
    }
    let mut touched = Vec::new();
    for (surface, id, value) in sets {
        let Some(s) = find(&surface) else {
            outcomes.push(OpOutcome { surface, id, outcome: Outcome::Rejected { reason: "no such options surface in this profile".into() }, before: None, after: None });
            continue;
        };
        let title = s.schema.title.clone().unwrap_or_default();
        let Some(def) = s.schema.options.get(&id) else {
            outcomes.push(OpOutcome { surface: s.key.clone(), id: id.clone(), outcome: Outcome::Rejected { reason: format!("{title} has no option {id}") }, before: None, after: None });
            continue;
        };
        let dflt = shipped_default(&id, def);
        let before = values.get(&s.key).and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| dflt.clone());
        let after = match &value {
            Some(v) => match def.normalize(v) {
                Ok(n) => n,
                Err(why) => {
                    outcomes.push(OpOutcome { surface: s.key.clone(), id, outcome: Outcome::Rejected { reason: format!("{title} {why}") }, before: Some(before), after: None });
                    continue;
                }
            },
            None => dflt.clone(),
        };
        if same_value(&before, &after) && (value.is_some() || values.get(&s.key).is_none_or(|m| !m.contains_key(&id))) {
            outcomes.push(OpOutcome { surface: s.key.clone(), id, outcome: Outcome::Unchanged, before: Some(before), after: Some(after) });
            continue;
        }
        if let Some(why) = &def.danger {
            dangers.push(format!("{} {}: {why}", title, def.label_or(&id)));
        }
        let m = values.entry(s.key.clone()).or_default();
        if value.is_some() && !same_value(&after, &dflt) {
            m.insert(id.clone(), after.clone());
        } else {
            m.remove(&id);
        }
        changes.push(change_line(&title, &id, def, &before, &after));
        let outcome = match (running, def.apply) {
            (false, _) => Outcome::SavedRestart { running: false },
            (true, Apply::Live) if s.kind == SurfaceKind::Mod => Outcome::AppliedLive { channel: LiveChannel::File, latency: def.latency.clone() },
            (true, Apply::Live) => Outcome::SavedPending { detail: "the running game can't be reached (no live channel for this file); applies next launch".into() },
            (true, Apply::At) => Outcome::SavedAt { at: def.at.clone().unwrap_or_default() },
            (true, _) => Outcome::SavedRestart { running: true },
        };
        touched.push((s.key.clone(), id.clone(), after.clone(), matches!(outcome, Outcome::AppliedLive { .. })));
        outcomes.push(OpOutcome { surface: s.key.clone(), id, outcome, before: Some(before), after: Some(after) });
    }
    values.retain(|_, m| !m.is_empty());
    if !dangers.is_empty() && !confirmed {
        bail!("needs confirmation (dangerous option): {}. Send it again with confirmation (the CLI asks; agents: confirmed)", dangers.join("; "));
    }
    let mut out = OptionsChange { product_id, profile: profile.to_owned(), revision: 0, outcomes, changes: changes.clone(), ..Default::default() };
    if changes.is_empty() {
        return Ok(out);
    }
    let session = running_with(d, product_id, profile).then(|| session_id(d, product_id)).flatten();
    if live != Live::Only {
        let h = o.history.entry(profile.to_owned()).or_default();
        let n = h.last().map_or(1, |r| r.revision + 1);
        h.push(OptionsRevision { revision: n, at: Utc::now(), source: source.into(), session: session.clone(), message, changes: changes.clone(), values, raw: Default::default() });
        out.revision = n;
        events.push(Event::OptionsChanged { product_id, profile: profile.to_owned(), revision: n, source: source.into(), changes: changes.clone() });
    }
    if let (true, Some(sid)) = (running, session) {
        let mut by_surface: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
        for (key, id, v, applied) in &touched {
            by_surface.entry(key.clone()).or_default().insert(id.clone(), v.clone());
            if *applied && let Some((_, has)) = o.has.as_mut() {
                has.entry(key.clone()).or_default().insert(id.clone(), v.clone());
            }
        }
        for (surface, values) in by_surface {
            events.push(Event::OptionsLive { session: sid.clone(), product_id, surface, values, by: source.into(), saved: live != Live::Only });
        }
        out.live.push(format!("{} is running (DLSS): rewrote aco_dlss.ini", name(st, product_id)));
    }
    Ok(out)
}

fn diff(o: &OptDemo, d: &Detail, st: &State, product_id: u32, profile: &str, against: &str) -> Result<Vec<OptionDiff>> {
    let pid = (profile != VANILLA).then_some(profile);
    let surfs = surfaces(d, o, product_id, pid);
    let cur = o.current(profile);
    let other: Option<OptionsRevision> = if against.eq_ignore_ascii_case("default") {
        None
    } else if let Some(r) = against.strip_prefix(['r', 'R']).and_then(|n| n.parse::<u32>().ok()) {
        Some(o.history.get(profile).and_then(|h| h.iter().find(|x| x.revision == r)).cloned().ok_or_else(|| anyhow!("no options revision r{r}"))?)
    } else {
        let p = launch_profile(d, st, product_id, Some(against))?.unwrap_or_else(|| VANILLA.into());
        Some(o.current(&p))
    };
    let other_surfaces: Option<Vec<String>> = match (against.eq_ignore_ascii_case("default"), against.strip_prefix(['r', 'R']).and_then(|n| n.parse::<u32>().ok())) {
        (true, _) | (_, Some(_)) => None,
        _ => {
            let p = launch_profile(d, st, product_id, Some(against))?;
            Some(surfaces(d, o, product_id, p.as_deref()).into_iter().map(|s| s.key).collect())
        }
    };
    let mut out = Vec::new();
    for s in &surfs {
        let title = s.schema.title.clone().unwrap_or_default();
        let missing = other_surfaces.as_ref().is_some_and(|keys| !keys.contains(&s.key));
        for (id, def) in &s.schema.options {
            let dflt = shipped_default(id, def);
            let a = cur.values.get(&s.key).and_then(|m| m.get(id)).cloned().unwrap_or_else(|| dflt.clone());
            let b = match &other {
                Some(_) if missing => Value::Null,
                Some(r) => r.values.get(&s.key).and_then(|m| m.get(id)).cloned().unwrap_or_else(|| dflt.clone()),
                None => dflt.clone(),
            };
            if !same_value(&a, &b) {
                out.push(OptionDiff {
                    surface: s.key.clone(),
                    title: title.clone(),
                    id: id.clone(),
                    label: def.label.clone(),
                    value: def.display(&a),
                    against: if b.is_null() { format!("not in {against}'s stack") } else { def.display(&b) },
                    value_json: a,
                    against_json: b,
                });
            }
        }
    }
    Ok(out)
}

fn keys(o: &OptDemo, d: &Detail, product_id: u32, profile: &str) -> KeysReport {
    let pid = (profile != VANILLA).then_some(profile);
    let hist = o.hist(profile).to_vec();
    let mut bindings = Vec::new();
    for s in surfaces(d, o, product_id, pid) {
        let st = state(&s, &hist, false, None, None);
        for (id, x) in &st.options {
            if x.def.ty == quiver_api::options::OptionType::Key && x.visible {
                let k = quiver_api::options::scalar_text(&x.value);
                if k.eq_ignore_ascii_case(x.def.none.as_deref().unwrap_or("none")) {
                    continue;
                }
                bindings.push(KeyBinding { key: k.clone(), raw: k, owner: st.title.clone(), surface: s.key.clone(), id: id.clone(), label: x.def.label_or(id).to_owned(), kind: "option".into() });
            }
        }
    }
    let k = |key: &str, raw: &str, owner: &str, label: &str, kind: &str| KeyBinding { key: key.into(), raw: raw.into(), owner: owner.into(), label: label.into(), kind: kind.into(), ..Default::default() };
    bindings.push(k("Shift+F3", "Shift+F3", "Quiver HUD", "Show or hide the performance HUD", "runtime"));
    bindings.push(k("Ctrl+Shift+F3", "Ctrl+Shift+F3", "Quiver HUD", "Next HUD preset", "runtime"));
    bindings.push(k("Shift+F2", "Shift+F2", "Sight", "Open or close Sight", "overlay"));
    bindings.push(k("Shift+F2", "Shift_L+F2", "MangoHud", "Start or stop MangoHud logging", "known"));
    bindings.push(k("Shift+F12", "Shift_R+F12", "MangoHud", "Show or hide MangoHud", "known"));
    bindings.push(k("Shift+F4", "Shift_L+F4", "MangoHud", "Reload MangoHud's config", "known"));
    bindings.push(k("Shift+Tab", "Shift+Tab", "Steam", "Steam overlay (Steam launches)", "known"));
    bindings.push(k("F12", "F12", "Steam", "Steam screenshot (Steam launches)", "known"));
    let mut conflicts: Vec<KeyConflict> = Vec::new();
    for (i, b) in bindings.iter().enumerate() {
        match conflicts.iter_mut().find(|c| c.key == b.key) {
            Some(c) => c.bindings.push(i),
            None => conflicts.push(KeyConflict { key: b.key.clone(), bindings: vec![i] }),
        }
    }
    conflicts.retain(|c| c.bindings.len() > 1 && c.bindings.iter().any(|&i| matches!(bindings[i].kind.as_str(), "option" | "runtime" | "overlay")));
    KeysReport { product_id, profile: profile.into(), bindings, conflicts }
}

fn schema_info(o: &OptDemo, d: &Detail, product_id: u32, mod_id: &str) -> Result<SchemaInfo> {
    let mods = d.mods.get(&product_id).ok_or_else(|| anyhow!("no mods"))?;
    let rec = mods.all_mods().into_iter().find(|r| r.id == mod_id || r.name.eq_ignore_ascii_case(mod_id)).ok_or_else(|| anyhow!("no mod '{mod_id}'"))?;
    if rec.id == DLSS_MOD {
        let mut lint = vec![LintFinding { severity: Severity::Note, at: "file ini".into(), message: "every key of aco_dlss.ini is described (63 options)".into() }];
        lint.push(LintFinding { severity: Severity::Note, at: "option fg.extra".into(), message: "overlay = false: edited in the app only".into() });
        return Ok(SchemaInfo {
            key: dlss_key(),
            mod_id: rec.id.clone(),
            version: rec.current.clone(),
            version_label: rec.label_of(&rec.current),
            source: SchemaSource::Catalog,
            note: "Quiver catalog (audited 2026-11-16 against aco_dlss.ini)".into(),
            schema: SCHEMA.clone(),
            text: Some(ACO_DLSS.to_owned()),
            lint,
            revision: None,
        });
    }
    if let Some((s, text, n)) = o.local.get(&rec.id) {
        return Ok(SchemaInfo {
            key: format!("{}/{}", rec.id, s.surface),
            mod_id: rec.id.clone(),
            version: rec.current.clone(),
            version_label: rec.label_of(&rec.current),
            source: SchemaSource::Local,
            note: format!("local, written in the app (r{n})"),
            schema: s.clone(),
            text: Some(text.clone()),
            lint: vec![LintFinding { severity: Severity::Warning, at: "options".into(), message: "every option applies at restart (the guess can't know better)".into() }],
            revision: Some(*n),
        });
    }
    let _ = SKIP_INI;
    bail!(
        "{} {} has no options schema (none shipped, none in Quiver's catalog, no local one); guess one: options infer {} FILE",
        rec.name,
        rec.label_of(&rec.current),
        rec.name
    )
}

/// The shipped text of a demo mod's file (the Source view shows it beside a guess).
pub fn file_text(mod_id: &str, file: &str) -> Option<&'static str> {
    (mod_id == SKIP_MOD && file.eq_ignore_ascii_case("SkipIntro.ini")).then_some(SKIP_INI)
}

/// Demo: the person changes crowd density in game (Sight), as quiverd reports an in-game
/// burst: a revision from `in-game` and the live values.
pub(super) fn simulate_in_game(key: usize, d: &Detail, st: &State, events: &mut Vec<Event>) -> Result<()> {
    let mut all = OPTS.lock().unwrap();
    let o = all.entry(key).or_insert_with(OptDemo::fixtures);
    let Some((p, _)) = d.running.get(&ORIGINS).cloned() else { bail!("AC Origins isn't running") };
    let p = p.unwrap_or_else(|| VANILLA.into());
    let cur = o.current(&p).values.get(&dlss_key()).and_then(|m| m.get("world.npc_density")).and_then(|v| v.as_f64()).unwrap_or(1.0);
    let next = if cur >= 3.0 { 2.0 } else { cur + 0.5 };
    let ops = vec![OptionOp::Set { surface: dlss_key(), id: "world.npc_density".into(), value: json!(next) }];
    set(o, d, st, ORIGINS, &p, ops, quiver_api::options::Live::IfRunning, Some("Changed in game".into()), true, "in-game", events)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_schema_and_game_surface_parse() {
        assert_eq!(SCHEMA.surface, "aco-dlss");
        assert!(SCHEMA.options.len() > 50);
        assert_eq!(GAME.options.len(), 3);
        let guess: Schema = toml::from_str(SKIP_INFERRED).unwrap();
        assert_eq!(guess.options.len(), 3);
        // Every default fits its option.
        for (id, def) in &SCHEMA.options {
            let v = shipped_default(id, def);
            assert!(def.validate(&v).is_ok(), "{id}: {v}");
        }
    }

    #[test]
    fn history_is_ordered_and_whole_state() {
        let h = dlss_history();
        assert!(h.windows(2).all(|w| w[0].revision < w[1].revision));
        let last = h.last().unwrap();
        assert_eq!(last.values[&dlss_key()]["fg.refresh_hz"], json!(165));
        assert_eq!(last.source, "game");
    }

    /// The demo answers like quiverd: truthful outcomes, live only while running, danger asks.
    #[tokio::test]
    async fn outcomes_follow_the_running_game() {
        use quiver_api::Request;
        use quiver_api::options::{Live, OptionsChange};
        let (demo, _events) = crate::demo::Demo::start();
        let ask = |r: OptionsRequest| demo.handle(Request::Options(r));
        let list: Vec<SurfaceState> = serde_json::from_value(ask(OptionsRequest::List { product_id: ORIGINS, profile: None, level: None }).unwrap()).unwrap();
        let dlss = list.iter().find(|s| s.key == dlss_key()).expect("the DLSS surface");
        assert_eq!(dlss.schema_source, SchemaSource::Catalog);
        assert!(matches!(&dlss.options["fg.refresh_hz"].from, ValueSource::Profile { revision: 13, source } if source == "gui"));
        assert!(dlss.live.session.is_none());
        let set = |id: &str, v: Value, confirmed: bool| -> Result<OptionsChange> {
            let ops = vec![OptionOp::Set { surface: dlss_key(), id: id.into(), value: v }];
            Ok(serde_json::from_value(ask(OptionsRequest::Set { product_id: ORIGINS, profile: None, ops, live: Live::IfRunning, message: None, confirmed })?)?)
        };
        let c = set("world.npc_density", json!(3), false).unwrap();
        assert_eq!(c.outcomes[0].outcome, Outcome::SavedRestart { running: false });
        assert_eq!(c.revision, 16);
        assert!(matches!(&set("world.npc_density", json!(9), false).unwrap().outcomes[0].outcome, Outcome::Rejected { reason } if reason.contains("above the maximum")));
        let e = set("fg.hide_handle64", json!(true), false).unwrap_err();
        assert!(e.to_string().contains("needs confirmation"), "{e}");
        assert!(set("fg.hide_handle64", json!(true), true).is_ok());
        // Running: live options apply live, the others say when.
        demo.handle(Request::Play { game: quiver_api::GameRef::ubisoft(ORIGINS), profile: None }).unwrap();
        let c = set("world.npc_density", json!(4), false).unwrap();
        assert!(matches!(c.outcomes[0].outcome, Outcome::AppliedLive { channel: LiveChannel::File, .. }));
        let c = set("fg.present_thread", json!("0"), false).unwrap();
        assert!(matches!(&c.outcomes[0].outcome, Outcome::SavedAt { at } if at == "swapchain creation"));
        let c = set("fg.mode", json!("off"), false).unwrap();
        assert_eq!(c.outcomes[0].outcome, Outcome::SavedRestart { running: true });
        // History has them, newest last, by the app.
        let h: Vec<OptionsRevision> = serde_json::from_value(ask(OptionsRequest::History { product_id: ORIGINS, profile: None }).unwrap()).unwrap();
        assert_eq!(h.last().unwrap().source, "gui");
        // Keys: F9 and F10 from DLSS, and Sight's Shift+F2 against MangoHud's.
        let k: KeysReport = serde_json::from_value(ask(OptionsRequest::Keys { product_id: ORIGINS, profile: None }).unwrap()).unwrap();
        assert!(k.bindings.iter().any(|b| b.key == "F10" && b.kind == "option"));
        assert_eq!(k.conflicts.len(), 1);
    }
}
