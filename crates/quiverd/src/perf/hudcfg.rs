//! HUD settings (SIGHT-HUD §1.4): global, per game and per profile, resolved profile >
//! game > global > defaults, stored with revisions like game settings, plus each game's
//! last toggle state and preset (decision 12), which isn't a revision.
//!
//! `<data>/hud/settings.json`, `<data>/hud/history.jsonl`, `<data>/hud/state.json`.
//!
//! **Version 2** (2026-11): the Sight layer is in every launch (`enabled` on by default) and
//! the HUD hidden until Shift+F3 (`on` off). Files from before ([`migrate_v1`]): a scope
//! that had `enabled = true` (the person turned the HUD on there) keeps the HUD shown at
//! launch (`on = true`, unless `on` was set); a game's or profile's `enabled = false` stays
//! (launches unchanged, as asked); the global scope's `enabled` goes, being the old
//! default either way (off) or the new one (on).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use quiver_api::GameRef;
use quiver_api::perf::{HudConfig, HudScope, HudScopeRef, HudSettingsView};

/// The settings file's version ([`Settings::version`]).
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// 0/1: before the layer was on by default ([`migrate_v1`]); [`VERSION`] now.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub global: HudScope,
    /// By `GameRef::key`: `3539` (as these files always had it), `steam:620`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub games: BTreeMap<String, HudScope>,
    /// `"<product>/<profile id>"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, HudScope>,
}

/// A game's last HUD state: shown or not, and the preset the chord left it on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Last {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    /// By `GameRef::key`, as `Settings::games`.
    #[serde(default)]
    games: BTreeMap<String, Last>,
}

#[derive(Serialize)]
struct HistoryLine<'a> {
    revision: u32,
    at: chrono::DateTime<Utc>,
    source: &'a str,
    scope: &'a HudScopeRef,
    set: &'a HudScope,
    unset: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
}

pub struct HudSettings {
    dir: PathBuf,
}

const KEYS: [&str; 16] = [
    "enabled", "on", "layout", "presenter", "captures", "anchor", "offset", "scale", "background", "text_opacity", "level_colors", "graph_span_ms", "safe_area", "rate_hz", "ngx_log",
    "reflex_timings",
];

fn profile_key(game: &GameRef, profile: &str) -> String {
    format!("{}/{profile}", game.key())
}

fn merge(into: &mut HudScope, set: &HudScope) {
    macro_rules! take {
        ($($f:ident),*) => { $( if set.$f.is_some() { into.$f = set.$f.clone(); } )* };
    }
    take!(enabled, on, layout, presenter, captures, anchor, offset, scale, background, text_opacity, level_colors, graph_span_ms, safe_area, rate_hz, ngx_log, reflex_timings);
}

/// A version 1 file as version 2 (module docs). Returns the scopes whose meaning had to
/// change (shown at launch now), for the history.
pub fn migrate_v1(s: &mut Settings) -> Vec<(HudScopeRef, HudScope)> {
    let mut changed = Vec::new();
    let mut shown = |scope: &mut HudScope, at: HudScopeRef| {
        if scope.enabled == Some(true) && scope.on.is_none() {
            scope.on = Some(true);
            changed.push((at, HudScope { on: Some(true), ..Default::default() }));
        }
    };
    shown(&mut s.global, HudScopeRef::global());
    s.global.enabled = None;
    for (k, v) in s.games.iter_mut() {
        if let Ok(g) = GameRef::parse(k) {
            shown(v, HudScopeRef::of(g));
        }
    }
    for (k, v) in s.profiles.iter_mut() {
        if let Some((g, prof)) = k.split_once('/').and_then(|(g, p)| Some((GameRef::parse(g).ok()?, p))) {
            shown(v, HudScopeRef { game: Some(g), profile: Some(prof.to_owned()) });
        }
    }
    s.version = VERSION;
    changed
}

fn unset(into: &mut HudScope, key: &str) -> Result<()> {
    match key {
        "enabled" => into.enabled = None,
        "on" => into.on = None,
        "layout" => into.layout = None,
        "presenter" => into.presenter = None,
        "captures" => into.captures = None,
        "anchor" => into.anchor = None,
        "offset" => into.offset = None,
        "scale" => into.scale = None,
        "background" => into.background = None,
        "text_opacity" => into.text_opacity = None,
        "level_colors" => into.level_colors = None,
        "graph_span_ms" => into.graph_span_ms = None,
        "safe_area" => into.safe_area = None,
        "rate_hz" => into.rate_hz = None,
        "ngx_log" => into.ngx_log = None,
        "reflex_timings" => into.reflex_timings = None,
        k => bail!("unknown HUD setting '{k}' ({})", KEYS.join(", ")),
    }
    Ok(())
}

impl HudSettings {
    pub fn new(dir: PathBuf) -> HudSettings {
        HudSettings { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// The settings, as version 2 whatever the file's version (written back by the next
    /// change, or by [`Self::migrate`]).
    pub fn load(&self) -> Settings {
        let mut s = match std::fs::read_to_string(self.path()) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; using HUD defaults", self.path().display());
                Settings::default()
            }),
            Err(_) => return Settings { version: VERSION, ..Settings::default() },
        };
        if s.version < VERSION {
            migrate_v1(&mut s);
        }
        s
    }

    /// Writes a version 1 file back as version 2, with a history line per scope whose
    /// meaning changed. Once, at startup; a file that is already current is left alone.
    pub fn migrate(&self) -> Result<()> {
        let Ok(t) = std::fs::read_to_string(self.path()) else { return Ok(()) };
        let Ok(mut s) = serde_json::from_str::<Settings>(&t) else { return Ok(()) };
        if s.version >= VERSION {
            return Ok(());
        }
        let changed = migrate_v1(&mut s);
        s.revision += 1;
        self.write_json(&self.path(), &s)?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("history.jsonl")).context("HUD history")?;
        let msg = "the Sight layer is in every launch now, the HUD hidden until Shift+F3; a scope that had the HUD on keeps it shown at launch";
        for (scope, set) in &changed {
            let line = HistoryLine { revision: s.revision, at: Utc::now(), source: "migration", scope, set, unset: &[], message: Some(msg) };
            writeln!(f, "{}", serde_json::to_string(&line)?)?;
        }
        if changed.is_empty() {
            let line = HistoryLine { revision: s.revision, at: Utc::now(), source: "migration", scope: &HudScopeRef::global(), set: &HudScope::default(), unset: &[], message: Some(msg) };
            writeln!(f, "{}", serde_json::to_string(&line)?)?;
        }
        tracing::info!(scopes = changed.len(), "HUD settings migrated to version {VERSION}");
        Ok(())
    }

    /// `enabled` as set at the game's own scope or its profile's (the profile's wins): only
    /// these put the layer into a game with anti-cheat.
    pub fn layer_set_for_game(&self, game: &GameRef, profile: Option<&str>) -> Option<bool> {
        let s = self.load();
        let prof = profile.and_then(|p| s.profiles.get(&profile_key(game, p))).and_then(|x| x.enabled);
        prof.or_else(|| s.games.get(&game.key()).and_then(|x| x.enabled))
    }

    fn load_state(&self) -> State {
        std::fs::read_to_string(self.state_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn write_json(&self, path: &Path, v: &impl Serialize) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn scope_of<'a>(s: &'a Settings, scope: &HudScopeRef) -> Option<&'a HudScope> {
        match (&scope.game, &scope.profile) {
            (None, None) => Some(&s.global),
            (Some(g), None) => s.games.get(&g.key()),
            (Some(g), Some(prof)) => s.profiles.get(&profile_key(g, prof)),
            (None, Some(_)) => None,
        }
    }

    /// Settings resolved for a game and profile (or the global scope), without the last
    /// toggle state.
    pub fn resolve_scope(&self, s: &Settings, scope: &HudScopeRef) -> HudConfig {
        let mut c = HudConfig::default().with(&s.global);
        if let Some(p) = &scope.game {
            if let Some(g) = s.games.get(&p.key()) {
                c = c.with(g);
            }
            if let Some(prof) = &scope.profile
                && let Some(x) = s.profiles.get(&profile_key(p, prof))
            {
                c = c.with(x);
            }
        }
        c
    }

    /// What a launch of the game (with this profile) starts with: the settings, then the
    /// game's last toggle state and preset.
    pub fn for_launch(&self, game: &GameRef, profile: Option<&str>) -> HudConfig {
        let s = self.load();
        let mut c = self.resolve_scope(&s, &HudScopeRef { game: Some(*game), profile: profile.map(str::to_owned) });
        if let Some(last) = self.load_state().games.get(&game.key()) {
            if let Some(on) = last.on {
                c.on = on;
            }
            if let Some(l) = &last.layout {
                c.layout = l.clone();
            }
        }
        c
    }

    pub fn view(&self, scope: &HudScopeRef) -> Result<HudSettingsView> {
        if scope.profile.is_some() && scope.game.is_none() {
            bail!("a profile scope needs a game");
        }
        let s = self.load();
        Ok(HudSettingsView {
            scope: scope.clone(),
            set: Self::scope_of(&s, scope).cloned().unwrap_or_default(),
            effective: self.resolve_scope(&s, scope),
            revision: s.revision,
            anti_cheat: None,
        })
    }

    /// Stores `set` and removes `unset` at `scope`, as a new revision. Setting `on` or
    /// `layout` for a game forgets the game's last toggle state for that field.
    pub fn apply(&self, scope: &HudScopeRef, set: &HudScope, unset_keys: &[String], source: &str, message: Option<&str>) -> Result<HudSettingsView> {
        if scope.profile.is_some() && scope.game.is_none() {
            bail!("a profile scope needs a game");
        }
        let mut s = self.load();
        {
            let target = match (&scope.game, &scope.profile) {
                (None, _) => &mut s.global,
                (Some(g), None) => s.games.entry(g.key()).or_default(),
                (Some(g), Some(prof)) => s.profiles.entry(profile_key(g, prof)).or_default(),
            };
            merge(target, set);
            for k in unset_keys {
                unset(target, k)?;
            }
        }
        s.games.retain(|_, v| *v != HudScope::default());
        s.profiles.retain(|_, v| *v != HudScope::default());
        s.revision += 1;
        self.write_json(&self.path(), &s)?;
        let line = HistoryLine { revision: s.revision, at: Utc::now(), source, scope, set, unset: unset_keys, message };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("history.jsonl")).context("HUD history")?;
        writeln!(f, "{}", serde_json::to_string(&line)?)?;
        if let Some(p) = scope.game.map(|g| g.key()) {
            let touches = |k: &str| unset_keys.iter().any(|u| u == k);
            let (on, layout) = (set.on.is_some() || touches("on"), set.layout.is_some() || touches("layout"));
            if on || layout {
                let mut st = self.load_state();
                if let Some(last) = st.games.get_mut(&p) {
                    if on {
                        last.on = None;
                    }
                    if layout {
                        last.layout = None;
                    }
                }
                self.write_json(&self.state_path(), &st)?;
            }
        }
        self.view(scope)
    }

    /// Remembers a game's toggle state or preset (the chord, `quiver hud toggle|next`).
    pub fn remember(&self, game: &GameRef, on: Option<bool>, layout: Option<&str>) -> Result<()> {
        let mut st = self.load_state();
        let last = st.games.entry(game.key()).or_default();
        if on.is_some() {
            last.on = on;
        }
        if let Some(l) = layout {
            last.layout = Some(l.to_owned());
        }
        self.write_json(&self.state_path(), &st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::perf::Captures;

    #[test]
    fn scopes_resolve_profile_over_game_over_global() {
        let t = tempfile::tempdir().unwrap();
        let h = HudSettings::new(t.path().to_owned());
        let d = h.for_launch(&GameRef::ubisoft(1), None);
        assert!(d.enabled && !d.on, "the layer by default, the HUD hidden");
        h.apply(&HudScopeRef::global(), &HudScope { layout: Some("compact".into()), ..Default::default() }, &[], "cli", None).unwrap();
        h.apply(&HudScopeRef::game(1), &HudScope { enabled: Some(true), layout: Some("standard".into()), ..Default::default() }, &[], "cli", None).unwrap();
        let prof = HudScopeRef { game: Some(GameRef::ubisoft(1)), profile: Some("p-dlss".into()) };
        let v = h.apply(&prof, &HudScope { layout: Some("dlss-tuning".into()), on: Some(true), captures: Some(Captures::Hidden), ..Default::default() }, &[], "cli", Some("tuning")).unwrap();
        assert_eq!(v.revision, 3);
        assert_eq!(v.effective.layout, "dlss-tuning");
        assert!(v.effective.enabled && v.effective.on);
        let vanilla = h.for_launch(&GameRef::ubisoft(1), None);
        assert_eq!((vanilla.layout.as_str(), vanilla.on, vanilla.captures), ("standard", false, Captures::Shown));
        assert_eq!(h.for_launch(&GameRef::ubisoft(2), None).layout, "compact");
        // Unset a key; an unknown key is an error.
        let v = h.apply(&prof, &HudScope::default(), &["layout".into()], "cli", None).unwrap();
        assert_eq!(v.effective.layout, "standard");
        assert!(h.apply(&prof, &HudScope::default(), &["colour".into()], "cli", None).is_err());
        assert!(h.view(&HudScopeRef { game: None, profile: Some("x".into()) }).is_err());
        let hist = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
        assert_eq!(hist.lines().count(), 4);
        assert!(hist.contains("\"message\":\"tuning\""));
    }

    #[test]
    fn steam_games_have_a_scope_and_ubisoft_files_stay_as_they_were() {
        let t = tempfile::tempdir().unwrap();
        // A file an older quiverd wrote: numeric keys.
        std::fs::write(t.path().join("settings.json"), r#"{"revision":1,"global":{},"games":{"3539":{"enabled":true}}}"#).unwrap();
        let h = HudSettings::new(t.path().to_owned());
        assert!(h.for_launch(&GameRef::ubisoft(3539), None).enabled);
        let control = GameRef::steam(870780);
        assert_eq!(h.layer_set_for_game(&control, None), None);
        h.apply(&HudScopeRef::of(control), &HudScope { enabled: Some(false), ..Default::default() }, &[], "cli", None).unwrap();
        assert!(!h.for_launch(&control, None).enabled);
        assert_eq!(h.layer_set_for_game(&control, None), Some(false));
        assert!(h.for_launch(&GameRef::ubisoft(3539), None).enabled);
        assert!(h.for_launch(&GameRef::steam(620), None).enabled);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(t.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(v["games"]["3539"]["enabled"], true, "Ubisoft keys unchanged");
        assert_eq!(v["version"], VERSION, "written back as the current version");
        assert_eq!(v["games"]["steam:870780"]["enabled"], false);
        h.remember(&control, Some(true), None).unwrap();
        assert!(h.for_launch(&control, None).on);
    }

    #[test]
    fn version_1_files_keep_enabled_games_shown() {
        let t = tempfile::tempdir().unwrap();
        // Before: the HUD off by default; 3539 turned on, 5 on and set hidden, 7 turned off,
        // a profile of 3539 turned on, and `quiver hud disable` once for every game.
        std::fs::write(
            t.path().join("settings.json"),
            r#"{"revision":4,"global":{"enabled":false,"layout":"standard"},
               "games":{"3539":{"enabled":true},"5":{"enabled":true,"on":false},"7":{"enabled":false},"steam:620":{"enabled":true}},
               "profiles":{"3539/p-1":{"enabled":true}}}"#,
        )
        .unwrap();
        let h = HudSettings::new(t.path().to_owned());
        let shown = |g: GameRef| {
            let c = h.for_launch(&g, None);
            (c.enabled, c.on)
        };
        assert_eq!(shown(GameRef::ubisoft(3539)), (true, true), "turned on before: shown at launch");
        assert_eq!(shown(GameRef::steam(620)), (true, true));
        assert_eq!(shown(GameRef::ubisoft(5)), (true, false), "its own `on` stays");
        assert_eq!(shown(GameRef::ubisoft(7)), (false, false), "turned off: launches stay unchanged");
        assert_eq!(shown(GameRef::ubisoft(9)), (true, false), "the rest: the layer, hidden (the global `enabled = false` was the old default)");
        assert_eq!(h.for_launch(&GameRef::ubisoft(9), None).layout, "standard");
        assert!(h.for_launch(&GameRef::ubisoft(3539), Some("p-1")).on);
        // Written back once, with history.
        h.migrate().unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(t.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!((v["version"].as_u64(), v["revision"].as_u64()), (Some(VERSION as u64), Some(5)));
        assert!(v["global"].get("enabled").is_none());
        assert_eq!(v["games"]["3539"], serde_json::json!({"enabled": true, "on": true}));
        assert_eq!(v["games"]["7"], serde_json::json!({"enabled": false}));
        let hist = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
        assert_eq!(hist.lines().count(), 3, "3539, steam:620 and the profile: {hist}");
        assert!(hist.lines().all(|l| l.contains(r#""source":"migration""#)));
        h.migrate().unwrap();
        assert_eq!(std::fs::read_to_string(t.path().join("history.jsonl")).unwrap().lines().count(), 3, "once");
        // Explicit at the game (or profile) scope: what overrides anti-cheat.
        assert_eq!(h.layer_set_for_game(&GameRef::ubisoft(3539), None), Some(true));
        assert_eq!(h.layer_set_for_game(&GameRef::ubisoft(9), None), None);
        h.apply(&HudScopeRef::global(), &HudScope { enabled: Some(true), ..Default::default() }, &[], "cli", None).unwrap();
        assert_eq!(h.layer_set_for_game(&GameRef::ubisoft(9), None), None, "the global scope doesn't count");
        // No file at all: nothing to migrate.
        let empty = tempfile::tempdir().unwrap();
        HudSettings::new(empty.path().to_owned()).migrate().unwrap();
        assert!(!empty.path().join("settings.json").exists());
    }

    #[test]
    fn last_state_is_remembered_until_set_explicitly() {
        let t = tempfile::tempdir().unwrap();
        let h = HudSettings::new(t.path().to_owned());
        let five = GameRef::ubisoft(5);
        h.remember(&five, Some(true), Some("full")).unwrap();
        let c = h.for_launch(&five, None);
        assert_eq!((c.on, c.layout.as_str()), (true, "full"));
        // A setting for the game's `on` wins over (and clears) the remembered state.
        h.apply(&HudScopeRef::game(5), &HudScope { on: Some(false), ..Default::default() }, &[], "cli", None).unwrap();
        let c = h.for_launch(&five, None);
        assert_eq!((c.on, c.layout.as_str()), (false, "full"));
    }
}
