//! Actions for the Quick tab, their focus across panel opens, and pinning from Mods and Tweaks.
use quiver_api::options::{OptionState, OptionType, same_value};
use quiver_api::sight::{PinKind, QuickPin};
use quiver_ui::components::Icon;
use serde_json::Value;

use crate::panel::state::{PRESETS, PanelData, adjustable, shown_in_game, step_value};
use crate::toasts::ToastData;
use crate::replay::{self, State as ReplayState};

/// Frame-rate caps the tile steps through (`fps.cap`: 0 is "No cap", `vrr` the refresh rate
/// less three).
pub const CAPS: [&str; 7] = ["0", "30", "60", "90", "120", "144", "vrr"];

#[derive(Debug, Clone, PartialEq)]
pub enum TileKind {
    /// A notice's button (its id, the button's).
    Notice { id: String, button: String },
    /// A mod's frame generation switch: (surface key, option id).
    FrameGen { surface: String, id: String },
    HudPreset,
    FpsCap,
    Pin(QuickPin),
    Screenshot,
    /// gpu-screen-recorder's replay buffer: its pid.
    Replay { pid: u32 },
    StartReplay { seconds: u32 },
    SaveReplay,
    StopReplay,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub kind: TileKind,
    pub label: String,
    pub value: String,
    /// A switch's state (the value's colour).
    pub on: Option<bool>,
    pub icon: Icon,
}

fn tile(kind: TileKind, label: impl Into<String>, value: impl Into<String>, on: Option<bool>, icon: Icon) -> Tile {
    Tile { kind, label: label.into(), value: value.into(), on, icon }
}

/// An option's value in a few words ("On", "Balanced", "2.0×").
pub fn value_words(st: &OptionState) -> String {
    match st.def.ty {
        OptionType::Bool => if st.value.as_bool().unwrap_or(false) { "On" } else { "Off" }.into(),
        _ => match st.def.special_of(&st.value) {
            Some(m) => m.label.clone(),
            None => quiver_ui::options::short_value(&st.def, &st.value),
        },
    }
}

/// `fps.cap`'s value in words.
pub fn cap_words(v: &Value) -> String {
    match v {
        Value::String(s) if s == "vrr" => "Refresh − 3".into(),
        v => match v.as_f64() {
            Some(n) if n <= 0.0 => "No cap".into(),
            Some(n) => format!("{} fps", n.round() as i64),
            None => v.to_string(),
        },
    }
}

/// The cap after `cur` in [`CAPS`] (a value between two goes to the next one up).
pub fn next_cap(cur: &Value) -> Value {
    let as_value = |s: &str| if s == "vrr" { Value::String("vrr".into()) } else { Value::from(s.parse::<i64>().unwrap_or(0)) };
    let i = CAPS.iter().position(|c| same_value(&as_value(c), cur)).map(|i| i + 1).unwrap_or_else(|| {
        let n = cur.as_f64().unwrap_or(0.0);
        CAPS.iter().position(|c| c.parse::<f64>().is_ok_and(|x| x > n)).unwrap_or(0)
    });
    as_value(CAPS[i % CAPS.len()])
}

fn find_option<'a>(d: &'a PanelData, surface: &str, id: &str) -> Option<&'a OptionState> {
    d.options.ready()?.iter().find(|s| s.key == surface)?.options.get(id)
}

/// The mod's live frame generation switch, if one exposes it: a `fg.enabled` toggle shown in
/// game, with the key the mod flips it with.
fn frame_gen(d: &PanelData) -> Option<Tile> {
    for s in d.options.ready()? {
        let Some(st) = s.options.get("fg.enabled").filter(|st| st.def.ty == OptionType::Bool && shown_in_game(st) && adjustable(st)) else { continue };
        let on = st.value.as_bool().unwrap_or(false);
        let key = s.actions.iter().filter(|(id, _)| id.starts_with("fg.")).find_map(|(_, a)| a.hotkey.clone());
        let value = match key {
            Some(k) => format!("{} · {k}", if on { "On" } else { "Off" }),
            None => (if on { "On" } else { "Off" }).to_owned(),
        };
        let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| "Frame Generation".into());
        return Some(tile(TileKind::FrameGen { surface: s.key.clone(), id: "fg.enabled".into() }, label, value, Some(on), Icon::Layers));
    }
    None
}

/// A pin's tile, when its option is there and adjustable here.
fn pin_tile(d: &PanelData, p: &QuickPin) -> Option<Tile> {
    let st = match p.kind {
        PinKind::Option => find_option(d, &p.surface, &p.id)?,
        PinKind::Tweak => d.tweaks.ready()?.options.get(&p.id)?,
    };
    if !adjustable(st) {
        return None;
    }
    let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| if p.label.is_empty() { p.id.clone() } else { p.label.clone() });
    let on = (st.def.ty == OptionType::Bool).then(|| st.value.as_bool().unwrap_or(false));
    Some(tile(TileKind::Pin(p.clone()), label, value_words(st), on, Icon::Pin))
}

/// The tiles for a session, left to right (module docs).
pub fn tiles(d: &PanelData, pins: &[QuickPin], notices: &[ToastData], replay: ReplayState) -> Vec<Tile> {
    let mut out = Vec::new();
    match replay {
        ReplayState::Off => out.push(start_replay_tile(replay::DEFAULT_SECONDS)),
        ReplayState::Active(seconds) => out.push(tile(TileKind::SaveReplay, format!("Save last {seconds} s"), "Replay", Some(true), Icon::History)),
        ReplayState::External { pid, seconds } => out.push(tile(TileKind::Replay { pid }, format!("Save last {seconds} s"), "Replay", Some(true), Icon::History)),
        _ => {}
    }
    out.push(tile(TileKind::Screenshot, "Screenshot", "Game only", None, Icon::Image));
    for p in pins {
        if let Some(t) = pin_tile(d, p) {
            out.push(t);
        }
    }
    for n in notices {
        let Some(id) = &n.notice else { continue };
        for b in &n.buttons {
            out.push(tile(TileKind::Notice { id: id.clone(), button: b.id.clone() }, b.label.clone(), n.title.clone(), None, Icon::Bot));
        }
    }
    if let Some(t) = frame_gen(d) {
        out.push(t);
    }
    if let Some(h) = d.hud.ready() {
        let live = h.live();
        let shown = h.status.as_ref().is_some_and(|s| s.on);
        let layout = h.status.as_ref().filter(|_| live).map(|s| s.layout.clone()).unwrap_or_else(|| h.settings.effective.layout.clone());
        let name = PRESETS.iter().find(|(id, _)| *id == layout).map(|(_, n)| (*n).to_owned()).unwrap_or(layout);
        let value = if live && !shown { format!("Hidden · {name}") } else { name };
        out.push(tile(TileKind::HudPreset, "HUD", value, None, Icon::Activity));
    }
    if let Some(st) = d.tweaks.ready().and_then(|t| t.options.get("fps.cap")) {
        out.push(tile(TileKind::FpsCap, "Frame rate cap", cap_words(&st.value), None, Icon::Clock));
    }
    match replay {
        ReplayState::Active(_) => out.push(tile(TileKind::StopReplay, "Stop buffer", "End capture", None, Icon::History)),
        ReplayState::Starting => out.push(tile(TileKind::StopReplay, "Cancel replay buffer", "Starting…", None, Icon::History)),
        _ => {}
    }
    out.push(tile(TileKind::Quit, "Quit game", "Confirm…", None, Icon::LogOut));
    out
}

fn start_replay_tile(seconds: u32) -> Tile {
    tile(TileKind::StartReplay { seconds }, "Start replay buffer", format!("▲ ▼  {seconds} s"), None, Icon::History)
}

/// What a tile does: the value to set (options and Tweaks), or the action.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// (surface key, option id, value)
    SetOption(String, String, Value),
    /// (tweak id, value)
    SetTweak(String, Value),
    /// The next HUD preset (shown).
    HudPreset(String),
    /// The HUD is hidden: show it.
    HudShow,
    Notice { id: String, button: String },
    Screenshot,
    Replay(replay::Action),
}

/// What applying `t` does with the session's data.
pub fn act(t: &Tile, d: &PanelData) -> Option<Act> {
    Some(match &t.kind {
        TileKind::Notice { id, button } => Act::Notice { id: id.clone(), button: button.clone() },
        TileKind::FrameGen { surface, id } => {
            let st = find_option(d, surface, id)?;
            Act::SetOption(surface.clone(), id.clone(), Value::Bool(!st.value.as_bool().unwrap_or(false)))
        }
        TileKind::HudPreset => {
            let h = d.hud.ready()?;
            if h.live() && !h.status.as_ref().is_some_and(|s| s.on) {
                return Some(Act::HudShow);
            }
            let cur = h.status.as_ref().filter(|_| h.live()).map(|s| s.layout.clone()).unwrap_or_else(|| h.settings.effective.layout.clone());
            let ids: Vec<String> = PRESETS.iter().map(|p| p.0.to_owned()).collect();
            Act::HudPreset(crate::panel::state::cycle(&ids, &cur, 1))
        }
        TileKind::FpsCap => {
            let st = d.tweaks.ready()?.options.get("fps.cap")?;
            Act::SetTweak("fps.cap".into(), next_cap(&st.value))
        }
        TileKind::Pin(p) => match p.kind {
            PinKind::Option => {
                let st = find_option(d, &p.surface, &p.id)?;
                Act::SetOption(p.surface.clone(), p.id.clone(), step_value(st, &st.value, 0)?)
            }
            PinKind::Tweak => {
                let st = d.tweaks.ready()?.options.get(&p.id)?;
                Act::SetTweak(p.id.clone(), step_value(st, &st.value, 0)?)
            }
        },
        TileKind::Screenshot => Act::Screenshot,
        TileKind::Replay { pid } => Act::Replay(replay::Action::SaveExternal(*pid)),
        TileKind::StartReplay { seconds } => Act::Replay(replay::Action::Start(*seconds)),
        TileKind::SaveReplay => Act::Replay(replay::Action::Save),
        TileKind::StopReplay => Act::Replay(replay::Action::Stop),
        TileKind::Quit => return None, // The panel handles Quit in its confirmation dialog.
    })
}

// ---- Pinning ("pin to quick menu", Y in a Sight page) ---------------------------------------

/// What Y on a row does: pin (or unpin) it, where, and what to say.
#[derive(Debug, Clone, PartialEq)]
pub struct PinChange {
    pub scope: String,
    pub pin: QuickPin,
    pub on: bool,
    pub said: String,
}

/// Y on `row`: an option or a Tweak (adjustable here) is pinned to the running profile's quick
/// menu (else the game's), or unpinned when it's in `pinned` already.
pub fn pin_row(d: &PanelData, row: &crate::panel::state::Row, pinned: &std::collections::BTreeMap<String, Vec<QuickPin>>) -> Result<PinChange, String> {
    use crate::panel::state::Row;
    let Some(view) = d.session.ready() else { return Err("Not loaded yet".into()) };
    let (pin, st) = match row {
        Row::Opt { surface, id } => (QuickPin { kind: PinKind::Option, surface: surface.clone(), id: id.clone(), label: String::new() }, find_option(d, surface, id)),
        Row::Tweak { id } => (QuickPin { kind: PinKind::Tweak, surface: String::new(), id: id.clone(), label: String::new() }, d.tweaks.ready().and_then(|t| t.options.get(id))),
        _ => return Err("Only options and Tweaks pin to the quick menu".into()),
    };
    let Some(st) = st else { return Err("That option isn't in this profile".into()) };
    if !adjustable(st) {
        return Err("That one can't be changed from the quick menu".into());
    }
    let label = st.def.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| pin.id.clone());
    let pin = QuickPin { label: label.clone(), ..pin };
    let profile_scope = quiver_api::sight::pin_scope(&view.game, view.profile());
    let game_scope = quiver_api::sight::pin_scope(&view.game, None);
    let owner = [&profile_scope, &game_scope].into_iter().find(|scope| {
        pinned.get(*scope).is_some_and(|pins| pins.iter().any(|p| p.same_item(&pin)))
    });
    let on = owner.is_none();
    let scope = owner.unwrap_or(&profile_scope).clone();
    let said = if on { format!("{label} pinned to the quick menu") } else { format!("{label} unpinned from the quick menu") };
    Ok(PinChange { scope, pin, on, said })
}

/// Stored with panel navigation. Track the action itself as pins, notices and
/// capture state can change the tile order between two visits.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    kind: Option<TileKind>,
    seconds: Option<u32>,
}

impl Selection {
    pub fn focus(&self, tiles: &[Tile]) -> usize {
        self.kind.as_ref().and_then(|k| tiles.iter().position(|t| same_kind(k, &t.kind))).unwrap_or(0)
    }

    pub fn select(&mut self, tile: &Tile) {
        self.kind = Some(tile.kind.clone());
    }

    pub fn length(&self) -> u32 { self.seconds.unwrap_or(replay::DEFAULT_SECONDS) }

    pub fn step_length(&mut self, up: bool) {
        let choices = replay::LENGTHS;
        let i = choices.iter().position(|s| *s == self.length()).unwrap_or(1);
        self.seconds = Some(choices[(i + if up { 1 } else { choices.len() - 1 }) % choices.len()]);
    }
}

fn same_kind(a: &TileKind, b: &TileKind) -> bool {
    use TileKind::*;
    match (a, b) {
        (StartReplay { .. } | SaveReplay | Replay { .. }, StartReplay { .. } | SaveReplay | Replay { .. }) => true,
        (Pin(x), Pin(y)) => x.same_item(y),
        (Notice { id: a, button: x }, Notice { id: b, button: y }) => a == b && x == y,
        _ => a == b,
    }
}

// ---- Save replay: gpu-screen-recorder's replay buffer --------------------------------------

/// A running `gpu-screen-recorder` with a replay buffer (`-r SECONDS`): its pid and seconds.
/// It saves the buffer on SIGUSR1.
pub fn replay_buffer(exclude: impl Fn(u32) -> bool) -> Option<(u32, u32)> {
    let dir = std::fs::read_dir("/proc").ok()?;
    let uid = unsafe { libc::getuid() };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if exclude(pid) { continue; }
        let Ok(md) = std::fs::metadata(e.path()) else { continue };
        if std::os::unix::fs::MetadataExt::uid(&md) != uid {
            continue;
        }
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        if let Some(secs) = replay_args(&cmd) {
            return Some((pid, secs));
        }
    }
    None
}

/// `gpu-screen-recorder … -r 60 …` (NUL-separated) → 60.
pub fn replay_args(cmdline: &[u8]) -> Option<u32> {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let exe = args.first()?;
    if !exe.rsplit(|b| *b == b'/').next().is_some_and(|n| n == b"gpu-screen-recorder") {
        return None;
    }
    let i = args.iter().position(|a| *a == b"-r")?;
    std::str::from_utf8(args.get(i + 1)?).ok()?.parse().ok().filter(|s: &u32| *s > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::state::{HudState, Load};
    use quiver_api::options::{ActionInfo, OptionDef, SurfaceState};
    use quiver_api::perf::{HudSettingsView, HudStatus};
    use quiver_api::sight::NoticeButton;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn opt(ty: OptionType, label: &str, value: Value) -> OptionState {
        OptionState { def: OptionDef { ty, label: Some(label.into()), ..Default::default() }, value, visible: true, ..Default::default() }
    }

    fn data() -> PanelData {
        let mut o = BTreeMap::new();
        o.insert("fg.enabled".to_owned(), opt(OptionType::Bool, "Frame Generation", json!(true)));
        o.insert("world.npc_density".to_owned(), OptionState { def: OptionDef { ty: OptionType::Float, label: Some("Crowd density".into()), min: Some(0.5), max: Some(3.0), step: Some(0.5), ..Default::default() }, value: json!(2.0), visible: true, ..Default::default() });
        let mut actions = BTreeMap::new();
        actions.insert("fg.toggle".to_owned(), ActionInfo { label: "Toggle FG".into(), hotkey: Some("F9".into()), ..Default::default() });
        let dlss = SurfaceState { key: "m1/aco-dlss".into(), title: "DLSS".into(), options: o, actions, ..Default::default() };
        let mut t = BTreeMap::new();
        t.insert("fps.cap".to_owned(), OptionState { def: OptionDef { ty: OptionType::Float, label: Some("Max frame rate".into()), ..Default::default() }, value: json!(120), visible: true, ..Default::default() });
        let tweaks = SurfaceState { key: "quiver".into(), options: t, ..Default::default() };
        let mut effective = quiver_api::perf::HudConfig::default();
        effective.layout = "compact".into();
        let settings = HudSettingsView { scope: Default::default(), set: Default::default(), effective, revision: 1, anti_cheat: None };
        let hud = HudState { status: Some(HudStatus { enabled: true, on: true, layout: "compact".into(), ..Default::default() }), settings };
        PanelData { options: Load::Ready(vec![dlss]), tweaks: Load::Ready(tweaks), hud: Load::Ready(hud), ..Default::default() }
    }

    fn pin() -> QuickPin {
        QuickPin { kind: PinKind::Option, surface: "m1/aco-dlss".into(), id: "world.npc_density".into(), label: "Crowd density".into() }
    }

    #[test]
    fn tiles_like_the_mock() {
        let t = tiles(&data(), &[pin()], &[], ReplayState::Off);
        let names: Vec<(&str, &str)> = t.iter().map(|t| (t.label.as_str(), t.value.as_str())).collect();
        assert_eq!(names, [("Start replay buffer", "▲ ▼  60 s"), ("Screenshot", "Game only"), ("Crowd density", "2"), ("Frame Generation", "On · F9"), ("HUD", "Compact"), ("Frame rate cap", "120 fps"), ("Quit game", "Confirm…")]);
        // A replay buffer and a notice's buttons; no frame generation without the option.
        let mut d = data();
        if let Load::Ready(v) = &mut d.options {
            v[0].options.remove("fg.enabled");
        }
        let n = ToastData { notice: Some("n-3".into()), title: "DLSS v4 is ready".into(), buttons: vec![NoticeButton { id: "keep_v3".into(), label: "Keep v3 for now".into(), primary: true }], ..Default::default() };
        let t = tiles(&d, &[], &[n], ReplayState::External { pid: 77, seconds: 60 });
        assert_eq!(t[2].kind, TileKind::Notice { id: "n-3".into(), button: "keep_v3".into() });
        assert!(!t.iter().any(|t| matches!(t.kind, TileKind::FrameGen { .. })));
        assert!(t.iter().any(|t| t.kind == TileKind::Replay { pid: 77 }));
        // A pin that's gone from the profile has no tile.
        let gone = QuickPin { id: "x.y".into(), ..pin() };
        assert_eq!(tiles(&data(), &[gone], &[], ReplayState::Off).len(), 6);
    }

    #[test]
    fn focus_follows_the_action_when_tiles_change() {
        let mut focus = Selection::default();
        let before = tiles(&data(), &[pin()], &[], ReplayState::Off);
        let after = tiles(&data(), &[], &[], ReplayState::Active(120));
        for kind in [TileKind::HudPreset, TileKind::Screenshot] {
            focus.select(before.iter().find(|t| t.kind == kind).unwrap());
            assert_eq!(after[focus.focus(&after)].kind, kind);
        }
        focus.select(&before[0]);
        assert_eq!(after[focus.focus(&after)].kind, TileKind::SaveReplay);
        focus.select(before.iter().find(|t| matches!(t.kind, TileKind::Pin(_))).unwrap());
        assert_eq!(after[focus.focus(&after)].kind, TileKind::SaveReplay);
    }

    #[test]
    fn what_tiles_do() {
        let d = data();
        let t = tiles(&d, &[pin()], &[], ReplayState::Off);
        assert_eq!(act(&t[3], &d), Some(Act::SetOption("m1/aco-dlss".into(), "fg.enabled".into(), json!(false))));
        assert_eq!(act(&t[4], &d), Some(Act::HudPreset("standard".into())));
        assert_eq!(act(&t[5], &d), Some(Act::SetTweak("fps.cap".into(), json!(144))));
        assert_eq!(act(&t[2], &d), Some(Act::SetOption("m1/aco-dlss".into(), "world.npc_density".into(), json!(2.5))));
        assert_eq!(act(&t[1], &d), Some(Act::Screenshot));
        assert_eq!(act(&t[0], &d), Some(Act::Replay(replay::Action::Start(60))));
        assert_eq!(act(&t[6], &d), None, "Quit requires the panel confirmation");
    }

    #[test]
    fn pinning_rows() {
        use crate::panel::state::{Row, SessionView};
        let mut d = data();
        let row = Row::Opt { surface: "m1/aco-dlss".into(), id: "world.npc_density".into() };
        assert!(pin_row(&d, &row, &Default::default()).is_err(), "not loaded");
        d.session = Load::Ready(SessionView { game: quiver_api::GameRef::ubisoft(3539), profile_id: Some("dlss".into()), ..Default::default() });
        let p = pin_row(&d, &row, &Default::default()).unwrap();
        assert_eq!((p.scope.as_str(), p.on, p.pin.label.as_str()), ("ubisoft:3539/dlss", true, "Crowd density"));
        // Pinned already: Y unpins.
        let mut pins = std::collections::BTreeMap::from([(p.scope.clone(), vec![p.pin.clone()])]);
        let p2 = pin_row(&d, &row, &pins).unwrap();
        assert!(!p2.on && p2.said.contains("unpinned"));
        // A game pin seen through a profile must be removed from its owning scope.
        pins.clear();
        pins.insert("ubisoft:3539".into(), vec![p.pin.clone()]);
        let inherited = pin_row(&d, &row, &pins).unwrap();
        assert!(!inherited.on);
        assert_eq!(inherited.scope, "ubisoft:3539");
        // A Tweak, at the game's scope under vanilla.
        d.session = Load::Ready(SessionView { game: quiver_api::GameRef::ubisoft(3539), profile_id: Some("vanilla".into()), ..Default::default() });
        let t = pin_row(&d, &Row::Tweak { id: "fps.cap".into() }, &Default::default()).unwrap();
        assert_eq!((t.scope.as_str(), t.pin.kind), ("ubisoft:3539", PinKind::Tweak));
        // Not an option: refused.
        assert!(pin_row(&d, &Row::Note("x".into()), &Default::default()).is_err());
        // Pinned, its tile shows in the quick menu.
        let tiles = tiles(&d, &[t.pin], &[], ReplayState::Off);
        assert!(tiles.iter().any(|x| matches!(&x.kind, TileKind::Pin(p) if p.id == "fps.cap")));
    }

    #[test]
    fn caps_step() {
        assert_eq!(next_cap(&json!(0)), json!(30));
        assert_eq!(next_cap(&json!(144)), json!("vrr"));
        assert_eq!(next_cap(&json!("vrr")), json!(0));
        assert_eq!(next_cap(&json!(100)), json!(120), "between two: the next one up");
        assert_eq!((cap_words(&json!(0)), cap_words(&json!(118.0)), cap_words(&json!("vrr"))), ("No cap".into(), "118 fps".into(), "Refresh − 3".into()));
    }

    #[test]
    fn replay_buffers() {
        assert_eq!(replay_args(b"/usr/bin/gpu-screen-recorder\0-w\0screen\0-r\x0060\0-o\0/tmp\0"), Some(60));
        assert_eq!(replay_args(b"gpu-screen-recorder\0-w\0screen\0-o\0/tmp/a.mp4\0"), None, "recording, no replay");
        assert_eq!(replay_args(b"/usr/bin/obs\0-r\x0060\0"), None);
    }
}
