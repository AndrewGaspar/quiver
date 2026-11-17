//! Pure helpers for the Tweaks screens (tested below): scope keys and names, where a row's
//! value comes from, what the next launch does with it, what the last session showed, and
//! the machine's capabilities in a few words.

use quiver_api::options::{OptionState, SurfaceState, ValueSource};
use quiver_api::tweaks::{Capabilities, Effect, Planned, Prefer, Scope, ScopeKind, TweakRow, TweaksPlan};

use crate::ui::game::options::logic::{Tone, short_value};

/// A stable key for a scope (staged edits and text fields are kept per scope).
pub fn scope_key(s: &Scope) -> String {
    match s {
        Scope::Global => "global".into(),
        Scope::Game { game } => format!("game:{}", game.key()),
        Scope::Profile { game, profile } => format!("profile:{}/{}", game.key(), profile.as_deref().unwrap_or("")),
    }
}

/// A scope's name in rows ("Global", "This game", "DLSS profile").
pub fn scope_name(kind: ScopeKind, profile: Option<&str>) -> String {
    match (kind, profile) {
        (ScopeKind::Profile, Some(p)) if !p.is_empty() => format!("{p} profile"),
        (k, _) => k.label().to_owned(),
    }
}

/// The row's platform part (absent on other surfaces).
pub fn row(st: &OptionState) -> Option<&TweakRow> {
    st.tweak.as_deref()
}

/// The scope shown sets this tweak (so Reset means something here).
pub fn set_here(st: &OptionState) -> bool {
    match (&st.from, row(st)) {
        (ValueSource::Scope { scope, .. }, Some(t)) => *scope == t.at,
        (ValueSource::Scope { .. }, None) => true,
        _ => false,
    }
}

/// The value comes from a lower scope than the one shown.
pub fn inherited_from(st: &OptionState) -> Option<ScopeKind> {
    match (&st.from, row(st)) {
        (ValueSource::Scope { scope, .. }, Some(t)) if *scope < t.at => Some(*scope),
        _ => None,
    }
}

/// Where the row's value comes from: ("Global · r3", source) or ("Game's choice", None).
pub fn source_text(st: &OptionState, profile: Option<&str>) -> (String, Option<String>) {
    match &st.from {
        ValueSource::Scope { scope, revision, source } => {
            let here = row(st).is_some_and(|t| t.at == *scope);
            let name = scope_name(*scope, profile);
            let text = if here || row(st).is_none() { format!("{name} · r{revision}") } else { format!("from {name} · r{revision}") };
            (text, Some(source.clone()))
        }
        ValueSource::Tweak { tweak, scope, revision } => (format!("from Quiver Tweaks ({}, r{revision}: {tweak})", scope.label()), None),
        _ => ("Game's choice · Quiver changes nothing".into(), None),
    }
}

/// The notice for a row a higher scope sets too ("the DLSS profile sets this too").
pub fn overridden_text(t: &TweakRow, profile: Option<&str>) -> Option<String> {
    let by = t.overridden_by.filter(|b| *b > t.at)?;
    let value = match by {
        ScopeKind::Profile => t.profile.as_ref(),
        ScopeKind::Game => t.game.as_ref(),
        ScopeKind::Global => t.global.as_ref(),
    };
    let what = match by {
        ScopeKind::Profile => match profile {
            Some(p) => format!("The {p} profile"),
            None => "A profile".into(),
        },
        ScopeKind::Game => "This game".into(),
        ScopeKind::Global => "The global scope".into(),
    };
    let v = value.map(|v| format!(" ({})", quiver_api::options::scalar_text(v))).unwrap_or_default();
    Some(format!("{what} sets this too{v}, so a change here doesn't reach those launches"))
}

/// A mechanism's name for people.
pub fn mech_label(mech: &str) -> String {
    match mech {
        "dxvk" => "DXVK".into(),
        "vkd3d" => "vkd3d-proton".into(),
        "nvapi-drs" => "dxvk-nvapi".into(),
        "ngx" => "NVIDIA NGX".into(),
        "env" => "Proton's environment".into(),
        "layer" => "the Sight layer".into(),
        "vk-layer" => "a Vulkan layer".into(),
        "wrapper" => "gamescope".into(),
        "mangohud" => "MangoHud".into(),
        "mod" => "a mod".into(),
        m => m.to_owned(),
    }
}

/// What the next launch does with a tweak, as the row says it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanLine {
    pub text: String,
    pub tone: Tone,
    /// An action the row offers: who should implement the tweak instead.
    pub action: Option<(String, Prefer)>,
}

pub fn planned_line(p: &Planned, prefer: Prefer) -> Option<PlanLine> {
    let line = |text: String, tone| PlanLine { text, tone, action: None };
    Some(match p {
        Planned::Default => return None,
        Planned::Apply { mech, writes, .. } => {
            let mut l = line(format!("via {}: {}", mech_label(mech), writes.join(", ")), Tone::Ok);
            // The person chose Quiver over a mod that provides it: offer the way back.
            if prefer == Prefer::Quiver {
                l.text.push_str(" · you prefer Quiver's over the mod's");
                l.action = Some(("Use the mod's".into(), Prefer::Provider));
            }
            l
        }
        Planned::Provided { by, option, value, can_prefer_quiver, .. } => {
            let v = value.as_ref().map(|v| format!(" = {}", quiver_api::options::scalar_text(v))).unwrap_or_default();
            PlanLine {
                text: format!("Controlled by {by} ({option}{v})"),
                tone: Tone::Accent,
                action: can_prefer_quiver.then(|| ("Use Quiver's".to_owned(), Prefer::Quiver)),
            }
        }
        Planned::Superseded { by } => line(format!("Your launch setting wins: {by}"), Tone::Warn),
        Planned::Blocked { reason } => line(format!("Blocked: {reason}"), Tone::Bad),
        Planned::Unavailable { reason } => line(format!("Unavailable here: {reason}"), Tone::Muted),
    })
}

/// The last session's effect: (badge, tone, detail).
pub fn effect_badge(e: &Effect) -> (String, Tone, Option<String>) {
    match e {
        Effect::Applied => ("APPLIED".into(), Tone::Muted, Some("nothing observed yet".into())),
        Effect::Confirmed { evidence, .. } => ("CONFIRMED".into(), Tone::Ok, Some(evidence.clone())),
        Effect::Measured { value } => ("MEASURED".into(), Tone::Ok, Some(value.clone())),
        Effect::Unconfirmed { why } => ("UNCONFIRMED".into(), Tone::Warn, Some(why.clone())),
        Effect::Provided { by, value } => (format!("PROVIDED BY {}", by.to_uppercase()), Tone::Accent, value.clone()),
        Effect::Superseded { by } => ("SUPERSEDED".into(), Tone::Warn, Some(by.clone())),
        Effect::Unavailable { reason } => ("UNAVAILABLE".into(), Tone::Muted, Some(reason.clone())),
        Effect::Failed { reason } => ("FAILED".into(), Tone::Bad, Some(reason.clone())),
    }
}

/// The machine's components in short chips: dxvk-nvapi, NGX updater, display.
pub fn caps_chips(c: &Capabilities) -> Vec<(String, Tone)> {
    let mut out = Vec::new();
    if let Some(p) = &c.proton {
        if let Some(v) = &p.dxvk {
            out.push((format!("DXVK {v}"), Tone::Muted));
        }
        if let Some(v) = &p.vkd3d {
            out.push((format!("vkd3d-proton {v}"), Tone::Muted));
        }
        if let Some(v) = &p.nvapi {
            out.push((format!("dxvk-nvapi {v}"), Tone::Muted));
        }
    }
    if c.vendor() == "nvidia" {
        let updater = c.ngx.updater || c.proton.as_ref().is_some_and(|p| p.copies_updater);
        out.push((if updater { "NGX updater ✓".into() } else { "NGX updater ✗".into() }, if updater { Tone::Ok } else { Tone::Warn }));
    }
    if let Some(d) = &c.display {
        let mut t = format!("{} {} Hz", d.name, d.refresh_hz.round());
        if d.vrr {
            t.push_str(" · VRR");
        }
        out.push((t.trim().to_owned(), Tone::Muted));
    }
    out
}

/// Rows set at the shown scope.
pub fn set_count(s: &SurfaceState) -> usize {
    s.options.values().filter(|st| set_here(st)).count()
}

/// The plan's environment and DXVK_CONFIG as `KEY=value` lines (DXVK_CONFIG last).
pub fn env_lines(p: &TweaksPlan) -> Vec<String> {
    let mut v: Vec<String> = p.env.iter().filter(|(k, _)| *k != "DXVK_CONFIG").map(|(k, v)| format!("{k}={v}")).collect();
    if let Some(d) = p.env.get("DXVK_CONFIG") {
        v.push(format!("DXVK_CONFIG={d}"));
    }
    v
}

/// A value as a row names it: a special's or a choice's label ("No cap", "16×", "160 fps").
pub fn value_label(def: &quiver_api::options::OptionDef, v: &serde_json::Value) -> String {
    match def.special_of(v) {
        Some(m) => m.label.clone(),
        None => short_value(def, v),
    }
}

/// What a reset at this scope falls back to ("Reset to the global value: 160 fps").
pub fn reset_text(st: &OptionState) -> String {
    let t = row(st);
    let below = match t.map(|t| t.at) {
        Some(ScopeKind::Profile) => t.and_then(|t| t.game.as_ref().map(|_| ScopeKind::Game).or(t.global.as_ref().map(|_| ScopeKind::Global))),
        Some(ScopeKind::Game) => t.and_then(|t| t.global.as_ref().map(|_| ScopeKind::Global)),
        _ => None,
    };
    let to = value_label(&st.def, &st.default);
    match below {
        Some(ScopeKind::Game) => format!("Reset to this game's value: {to}"),
        Some(_) => format!("Reset to the global value: {to}"),
        None => "Reset to the game's choice".into(),
    }
}

/// The `quiver tweaks …` arguments for a scope (`--global`, `3539`, `3539 --profile DLSS`).
pub fn cli_scope(s: &Scope, profile_name: Option<&str>) -> String {
    let quote = |n: &str| if n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { n.to_owned() } else { format!("'{}'", n.replace('\'', "'\\''")) };
    match s {
        Scope::Global => "--global".into(),
        Scope::Game { game } => game.key(),
        Scope::Profile { game, profile } => format!("{} --profile {}", game.key(), quote(profile_name.or(profile.as_deref()).unwrap_or("vanilla"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::GameRef;
    use quiver_api::options::{OptionDef, OptionType};
    use serde_json::json;

    fn st(from: ValueSource, at: ScopeKind) -> OptionState {
        OptionState {
            def: OptionDef { ty: OptionType::Float, unit: Some("fps".into()), ..Default::default() },
            value: json!(160),
            default: json!(0),
            from,
            tweak: Some(Box::new(TweakRow { at, global: Some(json!(160)), ..Default::default() })),
            ..Default::default()
        }
    }

    #[test]
    fn scopes_and_where_values_come_from() {
        let g = GameRef::ubisoft(3539);
        assert_eq!(scope_key(&Scope::Global), "global");
        assert_eq!(scope_key(&Scope::Game { game: g }), "game:3539");
        assert_eq!(scope_key(&Scope::Profile { game: g, profile: Some("pd1s5xk".into()) }), "profile:3539/pd1s5xk");
        assert_eq!(scope_name(ScopeKind::Profile, Some("DLSS")), "DLSS profile");
        assert_eq!(scope_name(ScopeKind::Game, Some("DLSS")), "This game");

        let global = ValueSource::Scope { scope: ScopeKind::Global, revision: 3, source: "gui".into() };
        let here = st(global.clone(), ScopeKind::Global);
        assert!(set_here(&here));
        assert_eq!(source_text(&here, None).0, "Global · r3");
        let inherited = st(global, ScopeKind::Game);
        assert!(!set_here(&inherited));
        assert_eq!(inherited_from(&inherited), Some(ScopeKind::Global));
        assert_eq!(source_text(&inherited, None).0, "from Global · r3");
        let default = st(ValueSource::Default, ScopeKind::Game);
        assert!(!set_here(&default) && inherited_from(&default).is_none());
        assert!(source_text(&default, None).0.starts_with("Game's choice"));

        let mut game = st(ValueSource::Scope { scope: ScopeKind::Game, revision: 1, source: "cli".into() }, ScopeKind::Game);
        assert_eq!(reset_text(&game), "Reset to the global value: 0 fps");
        game.tweak.as_mut().unwrap().global = None;
        assert_eq!(reset_text(&game), "Reset to the game's choice");
    }

    #[test]
    fn overridden_rows_say_by_whom() {
        let t = TweakRow { at: ScopeKind::Game, overridden_by: Some(ScopeKind::Profile), profile: Some(json!("1")), ..Default::default() };
        assert_eq!(overridden_text(&t, Some("DLSS")).unwrap(), "The DLSS profile sets this too (1), so a change here doesn't reach those launches");
        let lower = TweakRow { at: ScopeKind::Profile, overridden_by: Some(ScopeKind::Game), ..Default::default() };
        assert!(overridden_text(&lower, None).is_none(), "only a higher scope overrides");
    }

    #[test]
    fn plans_in_words() {
        let apply = Planned::Apply { mech: "dxvk".into(), writes: vec!["DXVK_CONFIG: dxgi.maxFrameRate = 160".into()], apply: Default::default() };
        let l = planned_line(&apply, Prefer::Provider).unwrap();
        assert_eq!(l.text, "via DXVK: DXVK_CONFIG: dxgi.maxFrameRate = 160");
        assert_eq!((l.tone, l.action), (Tone::Ok, None));
        assert_eq!(planned_line(&apply, Prefer::Quiver).unwrap().action, Some(("Use the mod's".into(), Prefer::Provider)));

        let p = Planned::Provided { by: "DLSS".into(), surface: "m/aco-dlss".into(), option: "reflex.fps_limit".into(), value: Some(json!(160)), mode: "bind".into(), can_prefer_quiver: true };
        let l = planned_line(&p, Prefer::Provider).unwrap();
        assert_eq!(l.text, "Controlled by DLSS (reflex.fps_limit = 160)");
        assert_eq!(l.action, Some(("Use Quiver's".into(), Prefer::Quiver)));
        let own = Planned::Provided { by: "DLSS".into(), surface: "m".into(), option: "upscaler.mip_bias".into(), value: None, mode: "own".into(), can_prefer_quiver: false };
        assert_eq!(planned_line(&own, Prefer::Provider).unwrap().action, None);

        assert_eq!(planned_line(&Planned::Blocked { reason: "DLSS: one frame".into() }, Prefer::Provider).unwrap().tone, Tone::Bad);
        assert_eq!(planned_line(&Planned::Unavailable { reason: "NVIDIA only".into() }, Prefer::Provider).unwrap().text, "Unavailable here: NVIDIA only");
        assert!(planned_line(&Planned::Default, Prefer::Provider).is_none());
        assert_eq!(mech_label("nvapi-drs"), "dxvk-nvapi");
    }

    #[test]
    fn effects_and_capabilities() {
        let (b, t, d) = effect_badge(&Effect::Unconfirmed { why: "DXVK logs no line for it".into() });
        assert_eq!((b.as_str(), t, d.as_deref()), ("UNCONFIRMED", Tone::Warn, Some("DXVK logs no line for it")));
        assert_eq!(effect_badge(&Effect::Provided { by: "DLSS".into(), value: Some("160".into()) }).0, "PROVIDED BY DLSS");

        let c = Capabilities {
            gpu: Some(quiver_api::tweaks::Gpu { vendor: "nvidia".into(), name: "NVIDIA GeForce RTX 5070 Laptop GPU".into(), rtx_series: Some(50), arch: None }),
            driver: Some("620.70.06".into()),
            proton: Some(quiver_api::tweaks::ProtonCaps { name: "GE-Proton11-12".into(), nvapi: Some("0.9.3".into()), ..Default::default() }),
            display: Some(quiver_api::tweaks::DisplayCaps { name: "eDP-1".into(), refresh_hz: 165.0, vrr: true, hdr: None }),
            ..Default::default()
        };
        let chips: Vec<String> = caps_chips(&c).into_iter().map(|c| c.0).collect();
        assert_eq!(chips, vec!["dxvk-nvapi 0.9.3", "NGX updater ✗", "eDP-1 165 Hz · VRR"]);

        let mut p = TweaksPlan::default();
        p.env.insert("DXVK_CONFIG".into(), "dxgi.maxFrameRate = 160".into());
        p.env.insert("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE".into(), "on".into());
        assert_eq!(env_lines(&p), vec!["DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE=on", "DXVK_CONFIG=dxgi.maxFrameRate = 160"]);
        assert_eq!(cli_scope(&Scope::Profile { game: GameRef::ubisoft(3539), profile: Some("x".into()) }, Some("My mods")), "3539 --profile 'My mods'");
    }
}
