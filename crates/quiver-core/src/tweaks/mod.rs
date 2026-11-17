//! **Quiver Tweaks** on disk and in resolution (design/TWEAKS.md; wire types in
//! `quiver_api::tweaks`).
//!
//! ```text
//! <data>/tweaks/global.jsonl                  global scope, whole-state revisions
//! <data>/games/<dir>/options/@game.jsonl      game scope
//! <data>/games/<dir>/options/<profile>.jsonl  profile scope: the profile's options file, key "quiver/tweaks"
//! <data>/games/<dir>/tweaks/state.json        what Quiver left in the prefix (the DLSS indicator)
//! <data>/games/<dir>/tweaks/sessions/<s>/     a session's logs (DXVK, dxvk-nvapi) and effects.json
//! ```
//!
//! Tweaks are an Options surface: `options/quiver-tweaks.toml` (surface `quiver`, kind
//! `platform`) with the same types, pages, `when`, presets and revisions. What's new here:
//!
//! - **Scopes** ([`Scopes`]): a value can be set globally, for a game, or for a profile;
//!   resolution is schema default < global < game < profile. Every revision file holds the
//!   whole state of its scope, values under [`SURFACE_KEY`] and `prefer` choices under
//!   [`PREFER_KEY`].
//! - **Providers** ([`providers`]): a mod surface's `[provides."<tweak>"]` implements a tweak
//!   itself. By default its implementation wins and the tweak's value flows into the mod's
//!   option as a **bound** default layer ([`bound_for`]: mod default < config patch < bound
//!   tweak < the profile's own value), so "cap 160 everywhere" reaches aco-dlss as
//!   `reflex.fps_limit = 160`.
//! - **Mechanisms** ([`compile`]): each tweak's `via` bindings, tried in order against the
//!   capability set ([`caps`]), compiled into the launch's environment.
//! - **Evidence** ([`evidence`]): fixed log lines that confirm a tweak took effect.
//!
//! Games are keyed by [`GameRef`] through [`game_scope_path`] alone; profile scopes exist
//! where the options store has profiles (Ubisoft products, through [`ModStore`]).

pub mod caps;
pub mod compile;
pub mod evidence;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use quiver_api::GameRef;
use quiver_api::options::{
    GroupState, Level, LiveInfo, OptionDef, OptionOp, OptionState, OptionsRevision, Outcome, PageState, PresetInfo, ProvideMode, ProvidesDef, Schema,
    SchemaSource, SurfaceKind, SurfaceState, ValueSource, Values, as_f64, same_value, scalar_text,
};
use quiver_api::tweaks::{DisplayCaps, Effect, PREFER_KEY, Prefer, SURFACE_KEY, ScopeKind, SessionTweaks, TweakRevisions, TweakRow, TweaksPlan};

use crate::mods::ModStore;
use crate::options::{Ctx, Surface};
use crate::settings::{append_jsonl, read_jsonl};

const SCHEMA_TEXT: &str = include_str!("../../../../options/quiver-tweaks.toml");

/// The platform surface's schema (Quiver's catalog, compiled in).
pub fn schema() -> &'static Schema {
    static S: OnceLock<Schema> = OnceLock::new();
    S.get_or_init(|| crate::options::schema::parse(SCHEMA_TEXT, false).expect("options/quiver-tweaks.toml parses (checked by a unit test)"))
}

/// The catalog text (for `options schema`-style display).
pub fn schema_text() -> &'static str {
    SCHEMA_TEXT
}

// -- where scopes live ------------------------------------------------------------------------

pub fn global_path(data: &Path) -> PathBuf {
    data.join("tweaks").join("global.jsonl")
}

/// The game scope's revisions. The only place a game's identity becomes a tweaks path.
pub fn game_scope_path(data: &Path, game: &GameRef) -> PathBuf {
    crate::paths::game_dir_in(data, game).join("options").join("@game.jsonl")
}

/// `<data>/games/<dir>/tweaks`: the prefix state and session logs.
pub fn game_tweaks_dir(data: &Path, game: &GameRef) -> PathBuf {
    crate::paths::game_dir_in(data, game).join("tweaks")
}

/// One scope's store.
#[derive(Clone)]
pub enum At<'a> {
    Global(&'a Path),
    Game(&'a Path, GameRef),
    /// A profile's options file (`vanilla` for vanilla); the store knows the game.
    Profile(&'a ModStore, String),
}

impl At<'_> {
    pub fn kind(&self) -> ScopeKind {
        match self {
            At::Global(_) => ScopeKind::Global,
            At::Game(..) => ScopeKind::Game,
            At::Profile(..) => ScopeKind::Profile,
        }
    }

    pub fn history(&self) -> Result<Vec<OptionsRevision>> {
        match self {
            At::Global(d) => read_jsonl(&global_path(d)),
            At::Game(d, g) => read_jsonl(&game_scope_path(d, g)),
            At::Profile(s, p) => crate::options::history(s, p),
        }
    }

    pub fn current(&self) -> Result<OptionsRevision> {
        Ok(self.history()?.pop().unwrap_or_default())
    }

    /// Record a new whole-state revision with `values` for this scope's tweaks (and
    /// `prefer`); a profile's other surfaces are kept as they are.
    pub fn append(&self, tweaks: BTreeMap<String, Value>, prefer: BTreeMap<String, Value>, source: &str, message: Option<String>, changes: Vec<String>) -> Result<OptionsRevision> {
        let prev = self.current()?;
        let mut values = prev.values.clone();
        values.insert(SURFACE_KEY.into(), tweaks);
        values.insert(PREFER_KEY.into(), prefer);
        values.retain(|_, m| !m.is_empty());
        match self {
            At::Profile(s, p) => crate::options::append(s, p, values, prev.raw, source, None, message, changes),
            At::Global(_) | At::Game(..) => {
                let rev = OptionsRevision { revision: prev.revision + 1, at: chrono::Utc::now(), source: source.to_owned(), session: None, message, changes, values, raw: Default::default() };
                let path = match self {
                    At::Global(d) => global_path(d),
                    At::Game(d, g) => game_scope_path(d, g),
                    At::Profile(..) => unreachable!(),
                };
                append_jsonl(&path, &rev)?;
                Ok(rev)
            }
        }
    }
}

fn tweaks_of(r: &OptionsRevision) -> BTreeMap<String, Value> {
    r.values.get(SURFACE_KEY).cloned().unwrap_or_default()
}

fn prefer_of(r: &OptionsRevision) -> BTreeMap<String, Value> {
    r.values.get(PREFER_KEY).cloned().unwrap_or_default()
}

/// The scopes one launch (or one view) resolves through, with their histories (for "who set
/// it, in which revision").
#[derive(Debug, Clone, Default)]
pub struct Scopes {
    pub global: Vec<OptionsRevision>,
    pub game: Option<Vec<OptionsRevision>>,
    pub profile: Option<Vec<OptionsRevision>>,
}

impl Scopes {
    /// Global, plus a game's and a profile's when given.
    pub fn load(data: &Path, game: Option<&GameRef>, profile: Option<(&ModStore, &str)>) -> Result<Scopes> {
        Ok(Scopes {
            global: At::Global(data).history()?,
            game: game.map(|g| At::Game(data, *g).history()).transpose()?,
            profile: profile.map(|(s, p)| At::Profile(s, p.to_owned()).history()).transpose()?,
        })
    }

    fn hist(&self, k: ScopeKind) -> Option<&Vec<OptionsRevision>> {
        match k {
            ScopeKind::Global => Some(&self.global),
            ScopeKind::Game => self.game.as_ref(),
            ScopeKind::Profile => self.profile.as_ref(),
        }
    }

    pub fn current(&self, k: ScopeKind) -> Option<OptionsRevision> {
        self.hist(k).map(|h| h.last().cloned().unwrap_or_default())
    }

    /// The tweaks set at one scope.
    pub fn values(&self, k: ScopeKind) -> BTreeMap<String, Value> {
        self.current(k).map(|r| tweaks_of(&r)).unwrap_or_default()
    }

    pub fn revisions(&self) -> TweakRevisions {
        let rev = |k| self.current(k).map_or(0, |r| r.revision);
        TweakRevisions { global: rev(ScopeKind::Global), game: rev(ScopeKind::Game), profile: rev(ScopeKind::Profile) }
    }

    /// The scopes present, lowest first, up to `upto`.
    pub fn kinds(&self, upto: ScopeKind) -> Vec<ScopeKind> {
        [ScopeKind::Global, ScopeKind::Game, ScopeKind::Profile].into_iter().filter(|k| *k <= upto && self.hist(*k).is_some()).collect()
    }

    /// The revision (and its source) that last set `id` to its current value at `k`.
    fn set_by(&self, k: ScopeKind, key: &str, id: &str) -> (u32, String) {
        let Some(h) = self.hist(k) else { return (0, String::new()) };
        let Some(cur) = h.last() else { return (0, String::new()) };
        let now = cur.values.get(key).and_then(|m| m.get(id));
        let mut at = cur;
        for r in h.iter().rev() {
            if r.values.get(key).and_then(|m| m.get(id)) == now {
                at = r;
            } else {
                break;
            }
        }
        (at.revision, at.source.clone())
    }

    /// Who implements each tweak: the highest scope's choice up to `upto` (default
    /// `provider`), and the scope that set it.
    pub fn prefer(&self, upto: ScopeKind) -> BTreeMap<String, (Prefer, ScopeKind)> {
        let mut out = BTreeMap::new();
        for k in self.kinds(upto) {
            for (id, v) in self.current(k).map(|r| prefer_of(&r)).unwrap_or_default() {
                if let Ok(p) = serde_json::from_value::<Prefer>(v) {
                    out.insert(id, (p, k));
                }
            }
        }
        out
    }
}

/// One tweak resolved through the scopes.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub def: OptionDef,
    /// Normalized; the schema default when no scope sets it.
    pub value: Value,
    /// The highest scope that sets it.
    pub set: Option<ScopeKind>,
    pub from: ValueSource,
    /// The value below `set` (what the row would show without it).
    pub inherited: Value,
    /// Per scope: the stored value (normalized, or as stored when it doesn't fit).
    pub at: BTreeMap<ScopeKind, Value>,
    /// Stored values that don't fit the schema (kept, not applied).
    pub dormant: Vec<String>,
}

impl Resolved {
    /// Set somewhere and not the schema default: Quiver changes something.
    pub fn active(&self) -> bool {
        self.set.is_some() && !self.def.default.as_ref().is_some_and(|d| same_value(d, &self.value))
    }
}

/// Every tweak of `schema` resolved through the scopes up to `upto`.
pub fn resolve(schema: &Schema, scopes: &Scopes, upto: ScopeKind) -> BTreeMap<String, Resolved> {
    let kinds = scopes.kinds(upto);
    let per: Vec<(ScopeKind, BTreeMap<String, Value>)> = kinds.iter().map(|k| (*k, scopes.values(*k))).collect();
    let mut out = BTreeMap::new();
    for (id, def) in &schema.options {
        let default = def.default.clone().unwrap_or(Value::Null);
        let mut r = Resolved { def: def.clone(), value: default.clone(), set: None, from: ValueSource::Default, inherited: default, at: BTreeMap::new(), dormant: Vec::new() };
        for (k, vals) in &per {
            let Some(v) = vals.get(id) else { continue };
            match def.normalize(v) {
                Ok(n) => {
                    r.at.insert(*k, n.clone());
                    r.inherited = std::mem::replace(&mut r.value, n);
                    r.set = Some(*k);
                    let (revision, source) = scopes.set_by(*k, SURFACE_KEY, id);
                    r.from = ValueSource::Scope { scope: *k, revision, source };
                }
                Err(e) => {
                    r.at.insert(*k, v.clone());
                    r.dormant.push(format!("{}: {} is kept but not applied ({e})", k.label(), scalar_text(v)));
                }
            }
        }
        out.insert(id.clone(), r);
    }
    out
}

/// The tweak values as a lookup (for `when` and `{tweak}` templates).
pub fn effective(resolved: &BTreeMap<String, Resolved>) -> BTreeMap<String, Value> {
    resolved.iter().map(|(id, r)| (id.clone(), r.value.clone())).collect()
}

// -- the display (for `vrr`) -------------------------------------------------------------------

fn display_slot() -> &'static Mutex<Option<DisplayCaps>> {
    static D: OnceLock<Mutex<Option<DisplayCaps>>> = OnceLock::new();
    D.get_or_init(Mutex::default)
}

/// The game's monitor as quiverd's display producer last saw it (for `fps.cap = vrr`).
pub fn set_display(d: Option<DisplayCaps>) {
    *display_slot().lock().unwrap() = d;
}

pub fn display() -> Option<DisplayCaps> {
    display_slot().lock().unwrap().clone()
}

/// A tweak's value made concrete: `vrr` becomes refresh − 3 on the game's monitor.
pub fn concrete(id: &str, v: &Value, display: Option<&DisplayCaps>) -> Result<Value, String> {
    if id == "fps.cap" && v.as_str() == Some("vrr") {
        let d = display.filter(|d| d.refresh_hz > 0.0).ok_or("the refresh rate of the game's monitor isn't known (no Hyprland display information)")?;
        return Ok(Value::from((d.refresh_hz.round() as i64 - 3).max(20)));
    }
    Ok(v.clone())
}

/// A tweak value for people: a sentinel's or an enum value's label (`No cap`, `Game's
/// choice`, `K: DLSS 4 transformer`), else the value with its unit (`160 fps`).
pub fn short_display(def: &OptionDef, v: &Value) -> String {
    if let Some(s) = def.special_of(v) {
        return s.label.clone();
    }
    if let Some(l) = def.values.iter().find(|e| same_value(&e.value, v)).and_then(|e| e.label.clone()) {
        return l;
    }
    def.display(v)
}

/// A value for people, with `vrr` resolved where the display is known.
pub fn display_value(id: &str, def: &OptionDef, v: &Value, display: Option<&DisplayCaps>) -> String {
    match concrete(id, v, display) {
        Ok(c) if &c != v => format!("refresh rate − 3 ({})", def.display(&c)),
        _ => short_display(def, v),
    }
}

// -- providers ---------------------------------------------------------------------------------

/// A mod (or game) surface that implements a tweak, in a profile's stack.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub tweak: String,
    pub surface: String,
    pub title: String,
    pub def: ProvidesDef,
    /// The provider's option.
    pub option: Option<OptionDef>,
    /// The profile's own value for it (it wins over a bound tweak).
    pub profile_value: Option<Value>,
    /// The option's value without the tweak (default, patch).
    pub base: Value,
}

/// A conflict a surface (or the platform schema) declares, holding now.
#[derive(Debug, Clone)]
pub struct ActiveConflict {
    pub tweak: Option<String>,
    pub mech: Option<String>,
    pub reason: String,
    /// `DLSS` or `Quiver` (built in).
    pub by: String,
}

fn surface_values(ctx: &Ctx, s: &Surface, values: &Values) -> BTreeMap<String, Value> {
    crate::options::eval(ctx, s, values, &Default::default(), None).options.into_iter().map(|(id, e)| (id, e.effective())).collect()
}

fn when_holds(w: Option<&quiver_api::options::When>, vals: &BTreeMap<String, Value>) -> bool {
    w.is_none_or(|w| w.iter().all(|(id, c)| vals.get(id).is_some_and(|v| quiver_api::options::cond_holds(c, v))))
}

/// The providers of a profile's stack whose `provides` (and `when`) hold, topmost last; two
/// for one tweak: the topmost wins (the others are reported as conflicts by the caller).
pub fn providers(ctx: &Ctx) -> (Vec<ProviderInfo>, Vec<ActiveConflict>) {
    let rev = crate::options::current(&ctx.store, &ctx.profile_id).unwrap_or_default();
    let mut provs: BTreeMap<String, ProviderInfo> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for s in &ctx.surfaces {
        let sch = s.schema();
        if sch.provides.is_empty() && sch.conflicts.is_empty() {
            continue;
        }
        let vals = surface_values(ctx, s, &rev.values);
        for (tweak, def) in &sch.provides {
            if !when_holds(def.when.as_ref(), &vals) {
                continue;
            }
            let option = sch.options.get(&def.option).cloned();
            let profile_value = rev.values.get(&s.key).and_then(|m| m.get(&def.option)).cloned();
            let ev = crate::options::eval(ctx, s, &Default::default(), &Default::default(), None);
            let base = ev.options.get(&def.option).map(|e| e.base()).unwrap_or(Value::Null);
            provs.insert(tweak.clone(), ProviderInfo { tweak: tweak.clone(), surface: s.key.clone(), title: s.title(), def: def.clone(), option, profile_value, base });
        }
        for c in &sch.conflicts {
            if when_holds(c.when.as_ref(), &vals) {
                conflicts.push(ActiveConflict { tweak: c.tweak.clone(), mech: c.mech.clone(), reason: c.reason.clone(), by: s.title() });
            }
        }
    }
    (provs.into_values().collect(), conflicts)
}

/// The platform schema's own conflicts that hold for these tweak values.
pub fn builtin_conflicts(schema: &Schema, values: &BTreeMap<String, Value>) -> Vec<ActiveConflict> {
    schema
        .conflicts
        .iter()
        .filter(|c| when_holds(c.when.as_ref(), values))
        .map(|c| ActiveConflict { tweak: c.tweak.clone(), mech: c.mech.clone(), reason: c.reason.clone(), by: "Quiver".into() })
        .collect()
}

/// `{percent / 100}`-style arithmetic in a provider map: one variable (`v`, `percent`, `fps`),
/// optionally one operator and a number.
fn arith(expr: &str, vars: &BTreeMap<&str, f64>) -> Option<f64> {
    let e = expr.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let mut parts = e.split_whitespace();
    let var = parts.next()?;
    let x = vars.get(var).copied().or_else(|| var.parse().ok())?;
    match (parts.next(), parts.next().and_then(|n| n.parse::<f64>().ok())) {
        (None, _) => Some(x),
        (Some("/"), Some(n)) if n != 0.0 => Some(x / n),
        (Some("*"), Some(n)) => Some(x * n),
        (Some("+"), Some(n)) => Some(x + n),
        (Some("-"), Some(n)) => Some(x - n),
        _ => None,
    }
}

/// What a provider's option gets from a tweak value (`None`: `@keep`, the option is left
/// alone), validated against the option.
pub fn provider_value(p: &ProviderInfo, tweak_value: &Value, tweaks: &BTreeMap<String, Value>) -> Result<Option<Value>, String> {
    let text = scalar_text(tweak_value);
    let mapped = match p.def.map.iter().find(|(k, _)| k.eq_ignore_ascii_case(&text)) {
        Some((_, Value::String(s))) if s == "@keep" => return Ok(None),
        Some((_, Value::String(s))) if s.starts_with('{') => {
            let mut vars = BTreeMap::new();
            if let Some(f) = as_f64(tweak_value) {
                vars.insert("v", f);
                vars.insert("fps", f);
            }
            if let Some(f) = tweaks.get("dlss.sr.ratio").and_then(as_f64) {
                vars.insert("percent", f);
            }
            let f = arith(s, &vars).ok_or_else(|| format!("{}'s map entry {s} doesn't evaluate", p.title))?;
            Value::from((f * 1000.0).round() / 1000.0)
        }
        Some((_, v)) => v.clone(),
        None => tweak_value.clone(),
    };
    let Some(def) = &p.option else { return Err(format!("{} has no option {}", p.title, p.def.option)) };
    def.normalize(&mapped).map(Some).map_err(|e| format!("{}'s {} can't take {}: {e}", p.title, p.def.option, scalar_text(&mapped)))
}

/// What the provider decides for a tweak: its option's bound value (or `None` to leave it),
/// or why the tweak's value can't flow.
#[derive(Debug, Clone, PartialEq)]
pub enum Provision {
    /// Bound: the tweak's value, mapped (or `None`: `@keep`).
    Bind(Option<Value>),
    /// `mode = "own"`: the provider decides.
    Own,
    /// The person prefers Quiver's mechanism: the option gets `off`.
    Off(Value),
    /// Can't flow (out of the option's range, …).
    Refused(String),
}

pub fn provision(p: &ProviderInfo, r: &Resolved, prefer: Prefer, tweaks: &BTreeMap<String, Value>, display: Option<&DisplayCaps>) -> Provision {
    if p.def.mode == ProvideMode::Own {
        return Provision::Own;
    }
    if !r.active() {
        // Nothing asked for: the provider's own value stands, whoever implements it.
        return Provision::Bind(None);
    }
    if prefer == Prefer::Quiver
        && let Some(off) = &p.def.off
    {
        return Provision::Off(off.clone());
    }
    match concrete(&p.tweak, &r.value, display) {
        Err(e) => Provision::Refused(e),
        Ok(v) => match provider_value(p, &v, tweaks) {
            Ok(v) => Provision::Bind(v),
            Err(e) => Provision::Refused(e),
        },
    }
}

/// The bound tweak values for a profile's surfaces (surface key → option → (source, value)):
/// the default layer between mod config patches and the profile's own values (steer 8).
pub fn bound_for(ctx: &Ctx) -> Result<BTreeMap<String, BTreeMap<String, (ValueSource, Value)>>> {
    let game = GameRef::ubisoft(ctx.store.product_id());
    let scopes = Scopes::load(ctx.store.data_dir(), Some(&game), Some((&ctx.store, &ctx.profile_id)))?;
    let resolved = resolve(schema(), &scopes, ScopeKind::Profile);
    let values = effective(&resolved);
    let prefer = scopes.prefer(ScopeKind::Profile);
    let display = display();
    let (provs, _) = providers(ctx);
    let mut out: BTreeMap<String, BTreeMap<String, (ValueSource, Value)>> = BTreeMap::new();
    for p in &provs {
        let Some(r) = resolved.get(&p.tweak) else { continue };
        let (pref, pref_at) = prefer.get(&p.tweak).copied().map_or((Prefer::Provider, None), |(p, k)| (p, Some(k)));
        let value = match provision(p, r, pref, &values, display.as_ref()) {
            Provision::Bind(Some(v)) => v,
            Provision::Off(v) => v,
            _ => continue,
        };
        let scope = if pref == Prefer::Quiver { pref_at.unwrap_or(ScopeKind::Global) } else { r.set.unwrap_or(ScopeKind::Global) };
        let revision = match &r.from {
            ValueSource::Scope { revision, .. } => *revision,
            _ => 0,
        };
        out.entry(p.surface.clone()).or_default().insert(p.def.option.clone(), (ValueSource::Tweak { tweak: p.tweak.clone(), scope, revision }, value));
    }
    Ok(out)
}

// -- changes at a scope ------------------------------------------------------------------------

/// The result of applying ops to a scope's tweak values (nothing written yet).
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub values: BTreeMap<String, Value>,
    pub prefer: BTreeMap<String, Value>,
    /// (id, outcome if already decided (`Unchanged`, `Rejected`), before, after).
    pub outcomes: Vec<(String, Option<Outcome>, Option<Value>, Option<Value>)>,
    pub changes: Vec<String>,
    pub dangers: Vec<String>,
}

/// Apply ops to the tweak values at `at` (validated). `inherited`: each tweak's value from
/// the scopes below (shown as "before" when the scope doesn't set it).
pub fn apply_ops(schema: &Schema, at: ScopeKind, cur: &OptionsRevision, inherited: &BTreeMap<String, Value>, ops: &[OptionOp]) -> Applied {
    let mut a = Applied { values: tweaks_of(cur), prefer: prefer_of(cur), ..Default::default() };
    let scope = at.label();
    for op in ops {
        let set_one = |a: &mut Applied, id: &str, v: Option<&Value>| {
            let Some(def) = schema.options.get(id) else {
                let near: Vec<&String> = schema.options.keys().filter(|k| k.contains(id) || id.contains(k.rsplit('.').next().unwrap_or(k))).take(4).collect();
                let hint = if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) };
                a.outcomes.push((id.to_owned(), Some(Outcome::Rejected { reason: format!("Quiver Tweaks has no tweak {id}{hint}") }), None, None));
                return;
            };
            let base = inherited.get(id).cloned().or_else(|| def.default.clone()).unwrap_or(Value::Null);
            let had = a.values.get(id).cloned();
            let before = had.clone().unwrap_or_else(|| base.clone());
            let new = match v {
                Some(v) => match def.normalize(v) {
                    Ok(n) => Some(n),
                    Err(why) => {
                        a.outcomes.push((id.to_owned(), Some(Outcome::Rejected { reason: format!("{id}: {why}") }), Some(before), None));
                        return;
                    }
                },
                None => None,
            };
            if had.as_ref().zip(new.as_ref()).is_some_and(|(x, y)| same_value(x, y)) || (had.is_none() && new.is_none()) {
                a.outcomes.push((id.to_owned(), Some(Outcome::Unchanged), Some(before.clone()), Some(before)));
                return;
            }
            match &new {
                Some(n) => a.values.insert(id.to_owned(), n.clone()),
                None => a.values.remove(id),
            };
            let after = new.clone().unwrap_or(base);
            if let Some(d) = &def.danger {
                a.dangers.push(format!("{id}: {d}"));
            }
            a.changes.push(format!("{scope}: {id} {} → {}{}", short_display(def, &before), short_display(def, &after), if new.is_none() { " (reset)" } else { "" }));
            a.outcomes.push((id.to_owned(), None, Some(before), Some(after)));
        };
        match op {
            OptionOp::Set { id, value, .. } => set_one(&mut a, id, Some(value)),
            OptionOp::Reset { id, .. } => set_one(&mut a, id, None),
            OptionOp::ResetAll { .. } => {
                for id in a.values.keys().cloned().collect::<Vec<_>>() {
                    set_one(&mut a, &id, None);
                }
            }
            OptionOp::Preset { id, .. } => match schema.presets.iter().find(|p| p.id == *id || p.label.eq_ignore_ascii_case(id)) {
                None => a.outcomes.push((id.clone(), Some(Outcome::Rejected { reason: format!("Quiver Tweaks has no preset {id} (presets: {})", schema.presets.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", ")) }), None, None)),
                Some(p) => {
                    let start = a.changes.len();
                    for (oid, v) in &p.set {
                        set_one(&mut a, oid, Some(v));
                    }
                    if a.changes.len() > start {
                        a.changes.insert(start, format!("{scope}: preset {}", p.label));
                    }
                }
            },
            OptionOp::Raw { binding, .. } => a.outcomes.push((binding.clone(), Some(Outcome::Rejected { reason: "Quiver Tweaks has no files: set a tweak by id".into() }), None, None)),
        }
    }
    a
}

/// Set who implements a tweak at a scope (`None`: back to the lower scope's choice).
pub fn apply_prefer(schema: &Schema, at: ScopeKind, cur: &OptionsRevision, id: &str, prefer: Option<Prefer>) -> Result<Applied> {
    schema.options.get(id).with_context(|| format!("Quiver Tweaks has no tweak {id}"))?;
    let mut a = Applied { values: tweaks_of(cur), prefer: prefer_of(cur), ..Default::default() };
    let before = a.prefer.get(id).cloned();
    let after = prefer.map(|p| serde_json::to_value(p).unwrap());
    if before == after {
        a.outcomes.push((id.to_owned(), Some(Outcome::Unchanged), before, after));
        return Ok(a);
    }
    match &after {
        Some(v) => a.prefer.insert(id.to_owned(), v.clone()),
        None => a.prefer.remove(id),
    };
    let word = |v: &Option<Value>| v.as_ref().map_or("default (provider)".to_owned(), scalar_text);
    a.changes.push(format!("{}: {id} implemented by {} → {}", at.label(), word(&before), word(&after)));
    a.outcomes.push((id.to_owned(), None, before, after));
    Ok(a)
}

// -- what clients see --------------------------------------------------------------------------

/// The platform surface at a scope, as clients render it. `plan`: the next launch's plan (game
/// and profile scopes), `last`: the game's last session.
pub fn state(schema: &Schema, scopes: &Scopes, at: ScopeKind, level: Option<Level>, plan: Option<&TweaksPlan>, last: Option<&SessionTweaks>) -> SurfaceState {
    let resolved = resolve(schema, scopes, at);
    let all = resolve(schema, scopes, ScopeKind::Profile);
    let prefer = scopes.prefer(at);
    let values = effective(&resolved);
    let get = |id: &str| values.get(id).cloned();
    let display = plan.and_then(|p| p.caps.display.clone()).or_else(display);
    let mut options = BTreeMap::new();
    for (id, r) in &resolved {
        if level.is_some_and(|l| r.def.level > l) {
            continue;
        }
        let top = all.get(id).and_then(|a| a.set);
        let row = TweakRow {
            global: r.at.get(&ScopeKind::Global).cloned(),
            game: r.at.get(&ScopeKind::Game).cloned(),
            profile: all.get(id).and_then(|a| a.at.get(&ScopeKind::Profile).cloned()),
            at,
            overridden_by: top.filter(|t| *t > at),
            prefer: prefer.get(id).map_or(Prefer::Provider, |p| p.0),
            planned: plan.and_then(|p| p.entry(id)).map(|e| e.planned.clone()),
            chips: chips(&r.def),
            last: last.and_then(|s| s.effect(id)).map(|e| e.effect.clone()),
        };
        let (value, default) = match r.set {
            Some(k) if k == at => (r.value.clone(), r.inherited.clone()),
            _ => (r.value.clone(), r.value.clone()),
        };
        let mut text = r.def.to_text(&value);
        if let Ok(c) = concrete(id, &value, display.as_ref())
            && c != value
        {
            text = format!("{text} ({})", scalar_text(&c));
        }
        options.insert(
            id.clone(),
            OptionState {
                def: r.def.clone(),
                text,
                visible: r.def.visible(&get),
                value,
                default,
                from: r.from.clone(),
                pending: Default::default(),
                dormant: (!r.dormant.is_empty()).then(|| r.dormant.join("; ")),
                invalid: None,
                tweak: Some(Box::new(row)),
            },
        );
    }
    let mut placed = BTreeSet::new();
    let mut pages = Vec::new();
    for p in &schema.pages {
        let ids: Vec<String> = p.options.iter().filter(|i| options.contains_key(*i)).cloned().collect();
        placed.extend(ids.iter().cloned());
        let mut groups = Vec::new();
        if !ids.is_empty() {
            groups.push(GroupState { title: String::new(), options: ids });
        }
        for g in &p.groups {
            let ids: Vec<String> = g.options.iter().filter(|i| options.contains_key(*i)).cloned().collect();
            placed.extend(ids.iter().cloned());
            if !ids.is_empty() {
                groups.push(GroupState { title: g.title.clone(), options: ids });
            }
        }
        if !groups.is_empty() {
            pages.push(PageState { id: p.id.clone(), title: p.title.clone(), groups });
        }
    }
    let rest: Vec<String> = options.keys().filter(|k| !placed.contains(*k)).cloned().collect();
    if !rest.is_empty() {
        pages.push(PageState { id: "other".into(), title: "Other".into(), groups: vec![GroupState { title: String::new(), options: rest }] });
    }
    let mut lint: Vec<String> = Vec::new();
    if let Some(p) = plan {
        lint.extend(p.warnings.iter().cloned());
    }
    SurfaceState {
        key: SURFACE_KEY.into(),
        surface: schema.surface.clone(),
        title: schema.title.clone().unwrap_or_else(|| "Quiver Tweaks".into()),
        help: schema.help.clone(),
        kind: SurfaceKind::Platform,
        mod_id: None,
        version: None,
        version_label: Some(at.label().into()),
        schema_source: SchemaSource::Catalog,
        schema_note: "Quiver catalog (options/quiver-tweaks.toml)".into(),
        files: Vec::new(),
        live: LiveInfo::default(),
        pages,
        options,
        presets: schema.presets.iter().map(|p| PresetInfo { id: p.id.clone(), label: p.label.clone(), help: p.help.clone(), set: p.set.clone() }).collect(),
        metrics: Vec::new(),
        actions: BTreeMap::new(),
        raw: Vec::new(),
        dormant: Vec::new(),
        lint,
    }
}

/// Where a tweak can work at all, in words (the bindings' labels, phase 1 only).
pub fn chips(def: &OptionDef) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in def.via.iter().filter(|b| b.phase.unwrap_or(1) <= compile::PHASE && !b.also) {
        if let Some(l) = &b.label
            && !out.contains(l)
        {
            out.push(l.clone());
        }
    }
    out
}

// -- a launch's plan ---------------------------------------------------------------------------

/// The capability set for a launch: the machine (under `/`), the Proton build (probed and
/// cached; `None` on the Steam route, where Steam picks it), the route and the display.
pub fn capabilities(data: &Path, build: Option<&Path>, route: &str) -> quiver_api::tweaks::Capabilities {
    let mut caps = caps::machine(Path::new("/"));
    caps.proton = build.map(|b| caps::probe_proton_cached(data, b, &caps::needles(schema())));
    caps.route = route.to_owned();
    caps.display = display();
    caps
}

/// The plan of a game's next launch (or of `tweaks plan`): every scope resolved, providers
/// and conflicts from the profile's stack (`ctx`, when the game has mod profiles), compiled
/// against `caps`. `user_env`: the launch's environment before tweaks.
pub fn plan_for(
    data: &Path,
    game: &GameRef,
    ctx: Option<&Ctx>,
    caps: quiver_api::tweaks::Capabilities,
    user_env: &BTreeMap<String, String>,
    log_dir: Option<&Path>,
) -> Result<compile::Compiled> {
    let scopes = Scopes::load(data, Some(game), ctx.map(|c| (&c.store, c.profile_id.as_str())))?;
    let resolved = resolve(schema(), &scopes, ScopeKind::Profile);
    let prefer = scopes.prefer(ScopeKind::Profile);
    let (providers, mut conflicts) = ctx.map(providers).unwrap_or_default();
    conflicts.extend(builtin_conflicts(schema(), &effective(&resolved)));
    let state = load_prefix_state(data, game);
    let mut caps = caps;
    if caps.api.is_empty() || caps.api == "unknown" {
        caps.api = state.api.clone().unwrap_or_else(|| "unknown".into());
    }
    let mut c = compile::compile(&compile::Inputs {
        schema: schema(),
        resolved: &resolved,
        prefer: &prefer,
        caps: &caps,
        providers: &providers,
        conflicts: &conflicts,
        user_env,
        log_dir,
        indicator_was_on: state.indicator_on,
    });
    c.plan.game = Some(*game);
    c.plan.profile = ctx.map(|c| c.profile_id.clone());
    c.plan.revisions = scopes.revisions();
    Ok(c)
}

// -- prefix state ------------------------------------------------------------------------------

/// What Quiver left in a game's prefix that a later launch undoes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrefixState {
    /// The DLSS indicator was turned on (it persists in the prefix's registry).
    #[serde(default)]
    pub indicator_on: bool,
    /// The game's API as the Sight layer last saw it (`dxvk`, `d3d12`, `vulkan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// The game's API from the Sight layer's engine name (`DXVK` → `dxvk`, D3D8–11; `vkd3d` →
/// `d3d12`; anything else a native Vulkan game).
pub fn api_of_engine(engine: &str) -> Option<&'static str> {
    let e = engine.trim().to_ascii_lowercase();
    if e.is_empty() {
        None
    } else if e.starts_with("dxvk") {
        Some("dxvk")
    } else if e.starts_with("vkd3d") {
        Some("d3d12")
    } else {
        Some("vulkan")
    }
}

/// Remember the API the Sight layer saw this game use (for the next launch's `requires`).
pub fn save_api(data: &Path, game: &GameRef, api: &str) -> Result<()> {
    let mut s = load_prefix_state(data, game);
    if s.api.as_deref() != Some(api) {
        s.api = Some(api.to_owned());
        save_prefix_state(data, game, &s)?;
    }
    Ok(())
}

fn state_path(data: &Path, game: &GameRef) -> PathBuf {
    game_tweaks_dir(data, game).join("state.json")
}

pub fn load_prefix_state(data: &Path, game: &GameRef) -> PrefixState {
    std::fs::read(state_path(data, game)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_prefix_state(data: &Path, game: &GameRef, s: &PrefixState) -> Result<()> {
    let p = state_path(data, game);
    std::fs::create_dir_all(p.parent().context("path")?)?;
    std::fs::write(p, serde_json::to_vec_pretty(s)?)?;
    Ok(())
}

// -- sessions ----------------------------------------------------------------------------------

/// A session's log dir (created by the caller).
pub fn session_dir(data: &Path, game: &GameRef, session: &str) -> PathBuf {
    game_tweaks_dir(data, game).join("sessions").join(session)
}

pub fn save_session(dir: &Path, s: &SessionTweaks) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".effects.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, dir.join("effects.json"))?;
    Ok(())
}

/// A game's recorded sessions, newest first.
pub fn sessions(data: &Path, game: &GameRef, limit: usize) -> Vec<SessionTweaks> {
    let dir = game_tweaks_dir(data, game).join("sessions");
    let mut out: Vec<SessionTweaks> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path().join("effects.json")).ok()?).ok())
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started));
    out.truncate(limit);
    out
}

/// Keep the newest `keep` session dirs of a game.
pub fn prune_sessions(data: &Path, game: &GameRef, keep: usize) {
    let dir = game_tweaks_dir(data, game).join("sessions");
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    dirs.sort();
    let n = dirs.len().saturating_sub(keep);
    for (_, d) in dirs.into_iter().take(n) {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// The last session's effect per tweak, for the "Last session" column.
pub fn last_session(data: &Path, game: &GameRef) -> Option<SessionTweaks> {
    sessions(data, game, 1).into_iter().next()
}

/// Effects that come from the plan alone (provided, superseded, unavailable); the others
/// start as `applied` and wait for evidence.
pub fn initial_effects(plan: &TweaksPlan) -> Vec<quiver_api::tweaks::TweakEffect> {
    use quiver_api::tweaks::{Planned, TweakEffect};
    plan.entries
        .iter()
        .filter(|e| e.planned != Planned::Default)
        .map(|e| {
            let (mech, effect) = match &e.planned {
                Planned::Apply { mech, .. } => (Some(mech.clone()), Effect::Applied),
                Planned::Provided { by, value, .. } => (Some("mod".into()), Effect::Provided { by: by.clone(), value: value.as_ref().map(scalar_text) }),
                Planned::Superseded { by } => (None, Effect::Superseded { by: by.clone() }),
                Planned::Blocked { reason } | Planned::Unavailable { reason } => (None, Effect::Unavailable { reason: reason.clone() }),
                Planned::Default => (None, Effect::Applied),
            };
            TweakEffect { id: e.id.clone(), label: e.label.clone(), value: e.display.clone(), mech, effect }
        })
        .collect()
}

#[cfg(test)]
mod tests;
