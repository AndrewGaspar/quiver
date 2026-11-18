//! **Quiver Tweaks** (design/TWEAKS.md): per-game overrides for upscaling, frame generation,
//! frame rate, sync and latency that Quiver applies to any game it launches. Tweaks are an
//! Options surface (`options/quiver-tweaks.toml`, surface `quiver`, kind `platform`) whose
//! options bind to **mechanisms** (`via`: an environment variable, a `DXVK_CONFIG` entry, a
//! driver setting through dxvk-nvapi, NVIDIA's NGX updater, a mod's own option) chosen per
//! launch from a **capability** probe, and whose values live in three **scopes**: global →
//! game → profile.
//!
//! The wire shapes here are appended to, never renamed (MOD-CONFIG C9). Values and changes
//! reuse the Options types ([`OptionOp`], [`OptionsChange`], [`SurfaceState`],
//! [`OptionsRevision`]); what is new is the scope addressing ([`Scope`]), the plan of the next
//! launch ([`TweaksPlan`]) and what each tweak did in a session ([`Effect`]).
//!
//! **Agents** act like the person (2026-11-18, replacing TWEAKS steer 12's Ask): every scope,
//! restores included. Only `danger` tweaks need `confirmed`, from every source.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::GameRef;
use crate::options::{Apply, ConfirmDef, Level, OptionOp, ValueSource};

/// The platform surface's id (`surface = "quiver"`) and the key its values are stored under.
pub const SURFACE: &str = "quiver";
pub const SURFACE_KEY: &str = "quiver/tweaks";
/// Where `prefer` choices are stored next to the values (tweak id → `provider`/`quiver`).
pub const PREFER_KEY: &str = "quiver/prefer";

/// Whether an Options surface query names the platform surface.
pub fn is_platform_surface(q: &str) -> bool {
    let q = q.trim();
    q.eq_ignore_ascii_case(SURFACE) || q.eq_ignore_ascii_case(SURFACE_KEY) || q.eq_ignore_ascii_case("tweaks") || q.eq_ignore_ascii_case("Quiver Tweaks")
}

/// Which of the three layers a value is set at (lowest first).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// Every game.
    #[default]
    Global,
    /// One game, every profile.
    Game,
    /// One profile of one game (vanilla is a profile).
    Profile,
}

impl ScopeKind {
    /// "Global", "This game", "Profile".
    pub fn label(self) -> &'static str {
        match self {
            ScopeKind::Global => "Global",
            ScopeKind::Game => "This game",
            ScopeKind::Profile => "Profile",
        }
    }
}

/// A scope addressed by a request. Games are [`GameRef`]s (`product_id` is read too);
/// profiles by id or name, `None` for the game's default profile. Profile scopes exist for
/// games with mod profiles (Ubisoft products today).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Global,
    Game {
        #[serde(alias = "product_id")]
        game: GameRef,
    },
    Profile {
        #[serde(alias = "product_id")]
        game: GameRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
}

impl Scope {
    pub fn kind(&self) -> ScopeKind {
        match self {
            Scope::Global => ScopeKind::Global,
            Scope::Game { .. } => ScopeKind::Game,
            Scope::Profile { .. } => ScopeKind::Profile,
        }
    }

    pub fn game(&self) -> Option<GameRef> {
        match self {
            Scope::Global => None,
            Scope::Game { game } | Scope::Profile { game, .. } => Some(*game),
        }
    }
}

/// Who implements a tweak a mod also provides (TWEAKS §7.2, steer 7).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Prefer {
    /// The mod's implementation; Quiver's mechanisms stand down and the value flows into the
    /// mod's option.
    #[default]
    Provider,
    /// Quiver's mechanism; the mod's option is set to its `off` value.
    Quiver,
}

/// One tweak row's platform part (`OptionState.tweak`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TweakRow {
    /// The values set at each scope (absent: not set there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Value>,
    /// The scope the row is shown at (its value; lower scopes are the default).
    pub at: ScopeKind,
    /// A higher scope sets it too, so a change here doesn't reach the game ("the DLSS
    /// profile overrides this").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_by: Option<ScopeKind>,
    #[serde(default)]
    pub prefer: Prefer,
    /// What the next launch does (game and profile scopes, where a game is known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned: Option<Planned>,
    /// Chips: where the tweak can work at all ("NVIDIA RTX", "D3D9–11").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chips: Vec<String>,
    /// The game's last session's effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Effect>,
}

// -- capabilities ------------------------------------------------------------------------------

/// One GPU as the probe saw it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Gpu {
    /// `nvidia`, `amd`, `intel`, or the PCI vendor id in hex.
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// RTX 20, 30, 40, 50 (NVIDIA RTX cards only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtx_series: Option<u32>,
    /// `turing`, `ampere`, `ada`, `blackwell` (from the series).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

/// What a Proton build contains (probed once per build, cached by its version file).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProtonCaps {
    /// The build's folder name (`GE-Proton11-12`) and its `version` file.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// `ge`, `valve` or `other`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub flavour: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dxvk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vkd3d: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvapi: Option<String>,
    /// Option and variable names found in the components (`dxvk:dxvk.latencySleep`,
    /// `nvapi:NGX_DLSSG_DYNAMIC_TARGET_FRAME_RATE`, `proton:PROTON_ENABLE_HDR`): bindings
    /// require names, not versions, because options move between releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<String>,
    /// The build's script copies the driver's `nvngx_update.exe` into the prefix itself.
    #[serde(default)]
    pub copies_updater: bool,
}

/// The driver's NGX pieces Proton copies into prefixes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NgxCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// `nvngx.dll` and `_nvngx.dll` (the NGX core).
    #[serde(default)]
    pub core: bool,
    /// `nvngx_update.exe` (NVIDIA's updater).
    #[serde(default)]
    pub updater: bool,
}

/// The game's monitor (quiverd's display producer).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DisplayCaps {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub refresh_hz: f64,
    #[serde(default)]
    pub vrr: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<bool>,
}

/// Everything a binding's `requires` can test, computed per launch (TWEAKS §3.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Capabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
    /// The GPU games run on (the dGPU on hybrid laptops), if any GPU was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<Gpu>,
    /// NVIDIA's driver version (`620.70.06`), or Mesa's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proton: Option<ProtonCaps>,
    #[serde(default)]
    pub ngx: NgxCaps,
    /// `quiver` (Quiver's `proton run`) or `steam` (`quiver steam-run`; Steam picks Proton).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub route: String,
    /// The game's graphics API as the Sight layer saw it last (`d3d11`, `d3d12`, `vulkan`,
    /// …), or `unknown`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api: String,
    #[serde(default)]
    pub anti_cheat: bool,
    /// The Sight layer runs this launch.
    #[serde(default)]
    pub layer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayCaps>,
    /// What couldn't be determined, in words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Capabilities {
    /// "NVIDIA GeForce RTX 5070 Laptop GPU · driver 620.70.06 · GE-Proton11-12".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match &self.gpu {
            Some(g) if !g.name.is_empty() => parts.push(g.name.clone()),
            Some(g) => parts.push(g.vendor.to_uppercase()),
            None => parts.push("no GPU found".into()),
        }
        if let Some(d) = &self.driver {
            parts.push(format!("driver {d}"));
        }
        match &self.proton {
            Some(p) => parts.push(p.name.clone()),
            None if self.route == "steam" => parts.push("Proton chosen by Steam".into()),
            None => {}
        }
        parts.join(" · ")
    }

    pub fn vendor(&self) -> &str {
        self.gpu.as_ref().map_or("", |g| g.vendor.as_str())
    }
}

// -- the plan of a launch ----------------------------------------------------------------------

/// What the next launch does with one tweak.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Planned {
    /// Game's choice / off: Quiver writes nothing.
    #[default]
    Default,
    /// Written through a mechanism (`writes` in words: `DXVK_CONFIG: dxgi.maxFrameRate = 160`).
    Apply {
        mech: String,
        writes: Vec<String>,
        #[serde(default)]
        apply: Apply,
    },
    /// A mod (or game surface) implements it; its option gets the bound value.
    Provided {
        /// The provider's title (`DLSS`) and surface key.
        by: String,
        surface: String,
        option: String,
        /// The value the provider's option resolves to (`None`: the provider's own, `@keep`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        /// `bind` or `own`.
        mode: String,
        /// The provider has an `off` value, so the person can prefer Quiver's mechanism.
        #[serde(default)]
        can_prefer_quiver: bool,
    },
    /// The person's own launch setting wins.
    Superseded { by: String },
    /// A conflict (a provider's, or between tweaks) keeps it from applying.
    Blocked { reason: String },
    /// No binding's `requires` held.
    Unavailable { reason: String },
}

impl Planned {
    /// One phrase for people.
    pub fn describe(&self) -> String {
        match self {
            Planned::Default => "game's choice".into(),
            Planned::Apply { mech, writes, .. } => format!("via {mech}: {}", writes.join(", ")),
            Planned::Provided { by, option, value: Some(v), .. } => format!("by {by} ({option} = {})", crate::options::scalar_text(v)),
            Planned::Provided { by, option, .. } => format!("by {by} ({option})"),
            Planned::Superseded { by } => format!("superseded: {by}"),
            Planned::Blocked { reason } => format!("blocked: {reason}"),
            Planned::Unavailable { reason } => format!("unavailable: {reason}"),
        }
    }
}

/// One binding the resolver looked at, and why it was or wasn't taken.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Considered {
    pub mech: String,
    pub chosen: bool,
    /// Why not (empty when chosen).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// One tweak in a launch plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TweakPlanEntry {
    pub id: String,
    pub label: String,
    pub value: Value,
    /// The value for people (`160 fps`, `K: DLSS 4 transformer`, `refresh rate − 3 (162 fps)`).
    pub display: String,
    pub from: ValueSource,
    pub planned: Planned,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub considered: Vec<Considered>,
    /// The evidence that would confirm it (from the chosen binding).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm: Vec<ConfirmDef>,
}

/// The three revisions a launch ran with (`PlaySession.tweaks`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TweakRevisions {
    pub global: u32,
    pub game: u32,
    pub profile: u32,
}

/// A tweak value written into a provider's option (a default layer, TWEAKS §7.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BoundValue {
    pub tweak: String,
    pub surface: String,
    pub option: String,
    pub value: Value,
    pub scope: ScopeKind,
}

/// What the next launch of a game (and profile) gets from Tweaks (`TweaksRequest::Plan`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TweaksPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameRef>,
    /// Profile id (`vanilla` for vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub caps: Capabilities,
    pub revisions: TweakRevisions,
    /// Every tweak with a value set at some scope (others are game's choice).
    pub entries: Vec<TweakPlanEntry>,
    /// The environment the launch adds or changes (`DXVK_CONFIG` already merged with the
    /// person's own entries).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Quiver's `DXVK_CONFIG` entries (`dxgi.maxFrameRate = 160`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dxvk_config: Vec<String>,
    /// Values flowing into providers' options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bound: Vec<BoundValue>,
    /// Files the launch puts in the prefix, in words (`copy nvngx_update.exe …`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Things the person should know (their launch settings download DLSS DLLs from a
    /// third-party mirror, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The session's log dir (only for a launch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<PathBuf>,
}

impl TweaksPlan {
    pub fn entry(&self, id: &str) -> Option<&TweakPlanEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

// -- effects -----------------------------------------------------------------------------------

/// Did the program use it? Per tweak and session (TWEAKS §6). The UI says exactly what the
/// evidence says and never more.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    /// Written to its mechanism; nothing observed yet.
    #[default]
    Applied,
    /// The program showed it used it.
    Confirmed { evidence: String, at: DateTime<Utc> },
    /// For caps: what the present ring measured.
    Measured { value: String },
    /// Applied, no evidence by the end of the session.
    Unconfirmed { why: String },
    /// A mod or game implements it.
    Provided {
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// The person's own launch setting won.
    Superseded { by: String },
    /// No binding's `requires` held, or a conflict blocked it.
    Unavailable { reason: String },
    /// The mechanism refused.
    Failed { reason: String },
}

impl Effect {
    /// `applied`, `confirmed`, …: the `state` metric's value.
    pub fn name(&self) -> &'static str {
        match self {
            Effect::Applied => "applied",
            Effect::Confirmed { .. } => "confirmed",
            Effect::Measured { .. } => "measured",
            Effect::Unconfirmed { .. } => "unconfirmed",
            Effect::Provided { .. } => "provided",
            Effect::Superseded { .. } => "superseded",
            Effect::Unavailable { .. } => "unavailable",
            Effect::Failed { .. } => "failed",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Effect::Applied => "applied, nothing observed yet".into(),
            Effect::Confirmed { evidence, at } => format!("confirmed: {evidence} ({})", at.with_timezone(&chrono::Local).format("%H:%M:%S")),
            Effect::Measured { value } => format!("measured: {value}"),
            Effect::Unconfirmed { why } => format!("unconfirmed: {why}"),
            Effect::Provided { by, value: Some(v) } => format!("provided by {by}: {v}"),
            Effect::Provided { by, value: None } => format!("provided by {by}"),
            Effect::Superseded { by } => format!("superseded: {by}"),
            Effect::Unavailable { reason } => format!("unavailable: {reason}"),
            Effect::Failed { reason } => format!("failed: {reason}"),
        }
    }

    /// Settled: no later evidence changes it.
    pub fn is_final(&self) -> bool {
        !matches!(self, Effect::Applied)
    }
}

/// One tweak's effect in a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TweakEffect {
    pub id: String,
    pub label: String,
    /// The value as people read it.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mech: Option<String>,
    pub effect: Effect,
}

/// A session's tweaks: what the launch applied and what happened (`TweaksRequest::Status`,
/// `Sessions`). Stored as `effects.json` in the session's log dir.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTweaks {
    pub session: String,
    #[serde(flatten, with = "crate::game::persisted")]
    pub game: GameRef,
    /// Profile id (`vanilla` for vanilla).
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub route: String,
    pub started: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<DateTime<Utc>>,
    pub revisions: TweakRevisions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<PathBuf>,
    pub effects: Vec<TweakEffect>,
    /// The environment the launch got from Tweaks.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SessionTweaks {
    pub fn effect(&self, id: &str) -> Option<&TweakEffect> {
        self.effects.iter().find(|e| e.id == id)
    }
}

// -- requests ----------------------------------------------------------------------------------

/// Requests about Quiver Tweaks (`Request::Tweaks`). Values and changes are the Options
/// shapes; `ops` name the surface `quiver` (any surface field is accepted and ignored).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TweaksRequest {
    /// The platform surface at a scope (`SurfaceState`): effective values with where each
    /// comes from (lower scopes as the default), and for game and profile scopes what the next
    /// launch does with each tweak (`OptionState.tweak`).
    List {
        #[serde(default)]
        scope: Scope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<Level>,
    },
    /// Change tweak values at a scope (`OptionsChange`; its `product_id` is 0 except for
    /// Ubisoft games). `danger` tweaks need `confirmed`, from every source.
    Set {
        #[serde(default)]
        scope: Scope,
        ops: Vec<OptionOp>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        confirmed: bool,
    },
    /// Who implements a tweak a mod also provides, stored at a scope (`OptionsChange`).
    Prefer {
        #[serde(default)]
        scope: Scope,
        id: String,
        prefer: Prefer,
        #[serde(default)]
        message: Option<String>,
    },
    /// A scope's revisions (`Vec<OptionsRevision>`; a profile scope's are the profile's
    /// options revisions).
    History {
        #[serde(default)]
        scope: Scope,
    },
    /// Make an earlier revision of a scope's tweaks current again (`OptionsChange`).
    Restore {
        #[serde(default)]
        scope: Scope,
        revision: u32,
    },
    /// What the next launch gets (`TweaksPlan`): capabilities, per tweak the bindings
    /// considered and the one chosen, the exact environment.
    Plan {
        #[serde(alias = "product_id")]
        game: GameRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// The capability set (`Capabilities`): for a game's launch, or (`None`) the machine
    /// with the default Proton build.
    Capabilities {
        #[serde(default, skip_serializing_if = "Option::is_none", alias = "product_id")]
        game: Option<GameRef>,
    },
    /// Running sessions' tweaks and effects (`Vec<SessionTweaks>`).
    Status,
    /// Recorded sessions' tweaks and effects, newest first (`Vec<SessionTweaks>`).
    Sessions {
        #[serde(alias = "product_id")]
        game: GameRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scopes_on_the_wire() {
        assert_eq!(serde_json::to_value(Scope::Global).unwrap(), json!({"scope": "global"}));
        assert_eq!(serde_json::to_value(Scope::Game { game: GameRef::ubisoft(3539) }).unwrap(), json!({"scope": "game", "game": "ubisoft:3539"}));
        assert_eq!(serde_json::from_value::<Scope>(json!({"scope": "game", "game": "steam:620"})).unwrap(), Scope::Game { game: GameRef::steam(620) });
        let p: Scope = serde_json::from_value(json!({"scope": "profile", "product_id": 1, "profile": "DLSS"})).unwrap();
        assert_eq!(p, Scope::Profile { game: GameRef::ubisoft(1), profile: Some("DLSS".into()) });
        let r: TweaksRequest = serde_json::from_value(json!({"op": "list"})).unwrap();
        assert!(matches!(r, TweaksRequest::List { scope: Scope::Global, level: None }));
        let r: TweaksRequest = serde_json::from_value(json!({"op": "set", "scope": {"scope": "game", "product_id": 5}, "ops": [{"op": "set", "surface": "quiver", "id": "fps.cap", "value": 160}]})).unwrap();
        assert!(matches!(r, TweaksRequest::Set { scope: Scope::Game { game }, .. } if game == GameRef::ubisoft(5)));
    }

    #[test]
    fn effects_on_the_wire() {
        let e = Effect::Unavailable { reason: "NVIDIA only".into() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v, json!({"effect": "unavailable", "reason": "NVIDIA only"}));
        assert_eq!(serde_json::from_value::<Effect>(v).unwrap(), e);
        assert_eq!(e.name(), "unavailable");
        assert!(!Effect::Applied.is_final());
        let p = Planned::Provided { by: "DLSS".into(), surface: "m1/aco-dlss".into(), option: "reflex.fps_limit".into(), value: Some(json!(160)), mode: "bind".into(), can_prefer_quiver: true };
        assert_eq!(p.describe(), "by DLSS (reflex.fps_limit = 160)");
        assert!(is_platform_surface("quiver") && is_platform_surface("quiver/tweaks") && !is_platform_surface("aco-dlss"));
    }
}
