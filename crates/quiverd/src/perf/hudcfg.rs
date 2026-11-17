//! HUD settings (SIGHT-HUD §1.4): global, per game and per profile, resolved profile >
//! game > global > defaults, stored with revisions like game settings, plus each game's
//! last toggle state and preset (decision 12), which isn't a revision.
//!
//! `<data>/hud/settings.json`, `<data>/hud/history.jsonl`, `<data>/hud/state.json`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use quiver_api::perf::{HudConfig, HudScope, HudScopeRef, HudSettingsView};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub global: HudScope,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub games: BTreeMap<u32, HudScope>,
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
    #[serde(default)]
    games: BTreeMap<u32, Last>,
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

const KEYS: [&str; 14] = [
    "enabled", "on", "layout", "presenter", "captures", "anchor", "offset", "scale", "background", "text_opacity", "level_colors", "graph_span_ms", "safe_area", "rate_hz",
];

fn profile_key(product: u32, profile: &str) -> String {
    format!("{product}/{profile}")
}

fn merge(into: &mut HudScope, set: &HudScope) {
    macro_rules! take {
        ($($f:ident),*) => { $( if set.$f.is_some() { into.$f = set.$f.clone(); } )* };
    }
    take!(enabled, on, layout, presenter, captures, anchor, offset, scale, background, text_opacity, level_colors, graph_span_ms, safe_area, rate_hz);
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

    pub fn load(&self) -> Settings {
        match std::fs::read_to_string(self.path()) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; using HUD defaults", self.path().display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
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
        match (scope.product_id, &scope.profile) {
            (None, None) => Some(&s.global),
            (Some(p), None) => s.games.get(&p),
            (Some(p), Some(prof)) => s.profiles.get(&profile_key(p, prof)),
            (None, Some(_)) => None,
        }
    }

    /// Settings resolved for a game and profile (or the global scope), without the last
    /// toggle state.
    pub fn resolve_scope(&self, s: &Settings, scope: &HudScopeRef) -> HudConfig {
        let mut c = HudConfig::default().with(&s.global);
        if let Some(p) = scope.product_id {
            if let Some(g) = s.games.get(&p) {
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
    pub fn for_launch(&self, product_id: u32, profile: Option<&str>) -> HudConfig {
        let s = self.load();
        let mut c = self.resolve_scope(&s, &HudScopeRef { product_id: Some(product_id), profile: profile.map(str::to_owned) });
        if let Some(last) = self.load_state().games.get(&product_id) {
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
        if scope.profile.is_some() && scope.product_id.is_none() {
            bail!("a profile scope needs a game");
        }
        let s = self.load();
        Ok(HudSettingsView {
            scope: scope.clone(),
            set: Self::scope_of(&s, scope).cloned().unwrap_or_default(),
            effective: self.resolve_scope(&s, scope),
            revision: s.revision,
        })
    }

    /// Stores `set` and removes `unset` at `scope`, as a new revision. Setting `on` or
    /// `layout` for a game forgets the game's last toggle state for that field.
    pub fn apply(&self, scope: &HudScopeRef, set: &HudScope, unset_keys: &[String], source: &str, message: Option<&str>) -> Result<HudSettingsView> {
        if scope.profile.is_some() && scope.product_id.is_none() {
            bail!("a profile scope needs a game");
        }
        let mut s = self.load();
        {
            let target = match (scope.product_id, &scope.profile) {
                (None, _) => &mut s.global,
                (Some(p), None) => s.games.entry(p).or_default(),
                (Some(p), Some(prof)) => s.profiles.entry(profile_key(p, prof)).or_default(),
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
        if let Some(p) = scope.product_id {
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
    pub fn remember(&self, product_id: u32, on: Option<bool>, layout: Option<&str>) -> Result<()> {
        let mut st = self.load_state();
        let last = st.games.entry(product_id).or_default();
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
        assert!(!h.for_launch(1, None).enabled, "off by default");
        h.apply(&HudScopeRef::global(), &HudScope { layout: Some("compact".into()), ..Default::default() }, &[], "cli", None).unwrap();
        h.apply(&HudScopeRef::game(1), &HudScope { enabled: Some(true), layout: Some("standard".into()), ..Default::default() }, &[], "cli", None).unwrap();
        let prof = HudScopeRef { product_id: Some(1), profile: Some("p-dlss".into()) };
        let v = h.apply(&prof, &HudScope { layout: Some("dlss-tuning".into()), on: Some(true), captures: Some(Captures::Hidden), ..Default::default() }, &[], "cli", Some("tuning")).unwrap();
        assert_eq!(v.revision, 3);
        assert_eq!(v.effective.layout, "dlss-tuning");
        assert!(v.effective.enabled && v.effective.on);
        let vanilla = h.for_launch(1, None);
        assert_eq!((vanilla.layout.as_str(), vanilla.on, vanilla.captures), ("standard", false, Captures::Shown));
        assert_eq!(h.for_launch(2, None).layout, "compact");
        // Unset a key; an unknown key is an error.
        let v = h.apply(&prof, &HudScope::default(), &["layout".into()], "cli", None).unwrap();
        assert_eq!(v.effective.layout, "standard");
        assert!(h.apply(&prof, &HudScope::default(), &["colour".into()], "cli", None).is_err());
        assert!(h.view(&HudScopeRef { product_id: None, profile: Some("x".into()) }).is_err());
        let hist = std::fs::read_to_string(t.path().join("history.jsonl")).unwrap();
        assert_eq!(hist.lines().count(), 4);
        assert!(hist.contains("\"message\":\"tuning\""));
    }

    #[test]
    fn last_state_is_remembered_until_set_explicitly() {
        let t = tempfile::tempdir().unwrap();
        let h = HudSettings::new(t.path().to_owned());
        h.remember(5, Some(true), Some("full")).unwrap();
        let c = h.for_launch(5, None);
        assert_eq!((c.on, c.layout.as_str()), (true, "full"));
        // A setting for the game's `on` wins over (and clears) the remembered state.
        h.apply(&HudScopeRef::game(5), &HudScope { on: Some(false), ..Default::default() }, &[], "cli", None).unwrap();
        let c = h.for_launch(5, None);
        assert_eq!((c.on, c.layout.as_str()), (false, "full"));
    }
}
