//! The Session section (the `SightSession` mock-up): a now-playing card (box art, the title,
//! profile / Proton / copy chips, this session's and the total play time), the mods in this
//! session with the options revision, the saves (store, cloud state, last save, who shares
//! them), what happened while the game ran, quick toggles (frame generation where a mod has
//! it, the HUD, the frame-rate cap, do not disturb), the achievements' progress, and
//! Screenshot / Quit / Force quit.
//!
//! The data is quiverd's: `SightRequest::Session` (Proton, art, events), the mods state, the
//! save stores and sync reports of the game's store, and the options' session record. Each
//! part fails on its own: a missing one leaves its card out or says why.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use gpui::{AnyElement, Context, IntoElement, ObjectFit, ParentElement, SharedString, Styled, StyledImage, div, img, prelude::*, px};
use quiver_api::epicgame::{EpicSaves, SaveMode, SyncState};
use quiver_api::info::SavesReport;
use quiver_api::mods::{ModsState, ResolvedMod};
use quiver_api::options::{OptionType, SurfaceState};
use quiver_api::savestores::{SaveStoresReport, SaveSync};
use quiver_api::sight::session::{SessionEvent, SessionEventKind, SessionInfo};
use quiver_api::stores::CloudState;
use quiver_api::{Client, GameRef, Request, StoreId};
use quiver_ui::assets::DISPLAY_FONT;
use quiver_ui::components::{BOLD, FocusExt, Icon, MEDIUM, SEMIBOLD, icon};
use quiver_ui::input::Input;
use quiver_ui::options::{Tone, short_value};
use quiver_ui::theme::Theme;

use super::prefs::Pref;
use super::{ago, bar, card, chip, duration, heading, key_text, line, tone_color, value_text};
use crate::panel::state::{Achs, Button, HudRow, Load, PanelData, Row, SessionView};
use crate::panel::view::PanelView;

/// What the Session section shows beyond [`SessionView`].
#[derive(Debug, Clone, Default)]
pub struct Extras {
    /// `None`: quiverd doesn't have the request (older), or it failed (`info_error`).
    pub info: Option<SessionInfo>,
    pub info_error: Option<String>,
    pub mods: Vec<ModLine>,
    pub saves: Load<SavesSummary>,
    pub options: Option<OptionsLine>,
}

/// A mod of the running profile: "DLSS" with "v3 · current".
#[derive(Debug, Clone, PartialEq)]
pub struct ModLine {
    pub name: String,
    pub chip: Option<String>,
}

impl ModLine {
    pub fn of(r: &ResolvedMod) -> ModLine {
        let how = match (r.pinned, r.current) {
            (true, _) => Some("pinned"),
            (false, true) if r.versions > 1 => Some("current"),
            _ => None,
        };
        let label = Some(r.label.trim()).filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("current"));
        let chip = match (label, how) {
            (Some(l), Some(h)) => Some(format!("{l} · {h}")),
            (Some(l), None) => Some(l.to_owned()),
            (None, Some(h)) => Some(h.to_owned()),
            (None, None) => None,
        };
        ModLine { name: r.name.clone(), chip }
    }
}

/// "r14 · 3 changed in game".
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsLine {
    pub launched: u32,
    pub changed: usize,
}

impl OptionsLine {
    pub fn text(&self) -> String {
        match self.changed {
            0 => format!("r{} · none changed in game", self.launched),
            n => format!("r{} · {n} changed in game", self.launched),
        }
    }
}

/// The Saves card.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SavesSummary {
    /// The save store's name ("Default"), or "The game's saves" for stores without save
    /// stores (Epic, Steam).
    pub store: String,
    pub state: String,
    pub tone: Option<Tone>,
    /// The newest save, and which ("slot 3").
    pub last_save: Option<DateTime<Utc>>,
    pub last_what: Option<String>,
    /// The other profiles (and vanilla) on the same saves.
    pub shared_with: Vec<String>,
    /// A line under it ("Ubisoft Connect's cloud syncs the Default store").
    pub note: Option<String>,
}

fn vanilla_name(id: &str) -> Option<&'static str> {
    id.eq_ignore_ascii_case(quiver_api::mods::VANILLA).then_some("Vanilla")
}

/// A Ubisoft game's saves for the running profile (`profile` `None`: vanilla): its save store,
/// whether the launch syncs it with Ubisoft's cloud and what's waiting to go up at exit, its
/// newest slot, and who else uses it.
pub fn ubisoft_saves(mods: &ModsState, profile: Option<&str>, stores: Option<&SaveStoresReport>, slots: Option<&SavesReport>) -> SavesSummary {
    let me = profile.unwrap_or(quiver_api::mods::VANILLA);
    let plan = match profile {
        Some(p) => mods.profiles.iter().find(|x| x.profile.id == p).and_then(|x| x.saves.clone()),
        None => mods.vanilla_saves.clone(),
    };
    let store_id = plan.as_ref().and_then(|p| p.store.clone()).or_else(|| stores.map(|s| s.vanilla.clone()).filter(|_| profile.is_none())).unwrap_or_else(|| quiver_api::savestores::DEFAULT_STORE.into());
    let info = stores.and_then(|r| r.stores.iter().find(|s| s.store.id == store_id));
    let store = plan.as_ref().and_then(|p| p.store_name.clone()).or_else(|| info.map(|i| i.store.name.clone())).unwrap_or_else(|| "Default".into());
    let shared_with: Vec<String> = match info {
        Some(i) => i.used_by.iter().filter(|u| u.id != me).map(|u| vanilla_name(&u.id).map(str::to_owned).unwrap_or_else(|| u.name.clone())).collect(),
        None => {
            let mut v: Vec<String> = mods.profiles.iter().filter(|p| p.profile.id != me && p.saves.as_ref().and_then(|s| s.store.as_deref()) == Some(store_id.as_str())).map(|p| p.profile.name.clone()).collect();
            if profile.is_some() && mods.vanilla_saves.as_ref().and_then(|s| s.store.as_deref()).unwrap_or(quiver_api::savestores::DEFAULT_STORE) == store_id {
                v.push("Vanilla".into());
            }
            v
        }
    };
    let cloud = plan.as_ref().map(|p| p.cloud).unwrap_or_else(|| info.is_some_and(|i| i.store.cloud));
    let no_cloud = mods.save_sync == Some(SaveSync::UbisoftNone);
    // The slots report is of the store that syncs with the cloud.
    let ours = slots.filter(|r| r.store.as_deref().is_none_or(|s| s.eq_ignore_ascii_case(&store)));
    let pending = ours.map_or(0, |r| r.slots.iter().filter(|s| s.pending_upload).count());
    let newest = ours.and_then(|r| r.slots.iter().filter_map(|s| Some((s.local.as_ref()?.modified?, s.slot))).max());
    let (state, tone, note) = if no_cloud {
        ("On this PC".to_owned(), Some(Tone::Muted), Some("Ubisoft Connect doesn't sync this game's saves".to_owned()))
    } else if !cloud {
        ("On this PC".to_owned(), Some(Tone::Muted), plan.as_ref().and_then(|p| p.cloud_note.clone()))
    } else if pending > 0 {
        (format!("{pending} to upload at exit"), Some(Tone::Accent), Some("Quiver sends them to Ubisoft's cloud when the game exits".into()))
    } else {
        ("Cloud in sync".to_owned(), Some(Tone::Ok), None)
    };
    SavesSummary {
        store,
        state,
        tone,
        last_save: newest.map(|n| n.0).or_else(|| info.and_then(|i| i.last_modified)),
        last_what: newest.map(|n| format!("slot {}", n.1)),
        shared_with,
        note,
    }
}

/// Every profile of an Epic or Steam game uses the game's real saves.
fn everyone_else(mods: Option<&ModsState>, profile: Option<&str>) -> Vec<String> {
    let me = profile.unwrap_or(quiver_api::mods::VANILLA);
    let mut v: Vec<String> = mods.map(|m| m.profiles.iter().filter(|p| p.profile.id != me).map(|p| p.profile.name.clone()).collect()).unwrap_or_default();
    if profile.is_some() {
        v.push("Vanilla".into());
    }
    v
}

pub fn epic_saves(s: &EpicSaves, mods: Option<&ModsState>, profile: Option<&str>) -> SavesSummary {
    let newest = s.files.iter().filter_map(|f| Some((f.local.as_ref()?.modified?, f.path.clone()))).max();
    let (state, tone) = match (s.mode, s.state) {
        (SaveMode::InGame, _) => ("Synced in game".to_owned(), Some(Tone::Muted)),
        (_, SyncState::InSync) => ("Cloud in sync".into(), Some(Tone::Ok)),
        (_, SyncState::LocalNewer | SyncState::Both) => ("Uploads at exit".into(), Some(Tone::Accent)),
        (_, SyncState::CloudNewer) => ("Cloud has newer".into(), Some(Tone::Warn)),
        (_, SyncState::Conflict) => ("Conflict".into(), Some(Tone::Bad)),
        (_, SyncState::Empty) => ("No saves yet".into(), Some(Tone::Muted)),
        (_, SyncState::Unknown) => ("Syncs at exit".into(), Some(Tone::Muted)),
    };
    let note = match s.mode {
        SaveMode::InGame => Some(quiver_api::savestores::real_saves_sync(StoreId::Epic, Some(SaveSync::EpicInGame))),
        SaveMode::Launcher => s.last_sync.as_ref().map(|l| format!("Last synced with Epic's cloud {}{}", super::when(l.at, Utc::now()), if l.by.is_empty() { String::new() } else { format!(" ({})", l.by) })),
    };
    SavesSummary {
        store: "The game's saves".into(),
        state,
        tone,
        last_save: newest.as_ref().map(|n| n.0),
        last_what: newest.map(|n| n.1.rsplit('/').next().unwrap_or(&n.1).to_owned()),
        shared_with: everyone_else(mods, profile),
        note,
    }
}

pub fn steam_saves(cloud: Option<&CloudState>, sync: Option<SaveSync>, mods: Option<&ModsState>, profile: Option<&str>) -> SavesSummary {
    let (state, tone) = match (sync, cloud) {
        (Some(SaveSync::SteamNone), _) => ("On this PC".to_owned(), Some(Tone::Muted)),
        (_, Some(c)) if c.unsynced > 0 => (format!("{} files waiting", c.unsynced), Some(Tone::Accent)),
        (_, Some(_)) => ("Steam Cloud in sync".into(), Some(Tone::Ok)),
        (_, None) => ("Steam syncs them".into(), Some(Tone::Muted)),
    };
    SavesSummary {
        store: "The game's saves".into(),
        state,
        tone,
        last_save: cloud.and_then(|c| c.last_upload),
        last_what: cloud.and_then(|c| c.last_upload).map(|_| "uploaded".into()),
        shared_with: everyone_else(mods, profile),
        note: sync.map(|s| quiver_api::savestores::real_saves_sync(StoreId::Steam, Some(s))),
    }
}

/// The running profile's options: the revision it launched with and how many options were
/// changed live since (in game, from the app, by agents).
pub fn options_line(records: &[quiver_api::options::SessionOptions], session: &str) -> Option<OptionsLine> {
    let r = records.iter().find(|r| r.session == session)?;
    let changed: std::collections::BTreeSet<(&str, &str)> = r.live_changes.iter().map(|c| (c.surface.as_str(), c.id.as_str())).collect();
    Some(OptionsLine { launched: r.launched_revision, changed: changed.len() })
}

/// "While you were playing", newest first: quiverd's events and the achievements unlocked
/// since the session started (quiverd's list misses unlocks from before it restarted), each
/// line once.
pub fn events(info: Option<&SessionInfo>, achs: Option<&Achs>, started: Option<DateTime<Utc>>) -> Vec<SessionEvent> {
    let mut out: Vec<SessionEvent> = info.map(|i| i.events.clone()).unwrap_or_default();
    if let (Some(a), Some(t0)) = (achs, started) {
        for x in a.list.iter().filter(|x| x.unlocked && x.unlocked_at.is_some_and(|t| t >= t0)) {
            let text = format!("Achievement unlocked: {}", x.name);
            if !out.iter().any(|e| e.kind == SessionEventKind::Achievement && e.text == text) {
                out.push(SessionEvent { at: x.unlocked_at.unwrap(), kind: SessionEventKind::Achievement, text, by: None });
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.at));
    out
}

/// A mod's frame generation switch in the running profile: a visible on/off option
/// `fg.enabled` (its toggle key from `fg.toggle_key`). (surface key, option id, the mod's
/// title, the key.)
pub fn fg_toggle(options: &[SurfaceState]) -> Option<(String, String, String, Option<String>)> {
    options.iter().find_map(|s| {
        let (id, _) = s.options.iter().find(|(id, st)| (id.as_str() == "fg.enabled" || id.ends_with(".fg.enabled")) && st.def.ty == OptionType::Bool && crate::panel::state::shown_in_game(st))?;
        let key = s.options.get(&id.replace("fg.enabled", "fg.toggle_key")).map(|k| short_value(&k.def, &k.value)).filter(|k| !k.is_empty() && k != "—" && !k.eq_ignore_ascii_case("none"));
        Some((s.key.clone(), id.clone(), s.title.clone(), key))
    })
}

/// The Quick toggles and buttons, in focus order.
pub fn rows(d: &PanelData) -> Vec<Row> {
    let mut out = Vec::new();
    match &d.session {
        Load::Loading => out.push(Row::Note("Loading the session…".into())),
        Load::Failed(e) => out.push(Row::Note(format!("Couldn't load the session: {e}"))),
        Load::Unavailable(w) => out.push(Row::Note(w.clone())),
        Load::Ready(_) => {}
    }
    if let Some((surface, id, ..)) = d.options.ready().and_then(|o| fg_toggle(o)) {
        out.push(Row::Opt { surface, id });
    }
    if let Some(h) = d.hud.ready() {
        out.push(Row::Hud(if h.live() { HudRow::Shown } else { HudRow::Enable }));
        if h.live() {
            out.push(Row::Hud(HudRow::Preset));
        }
    }
    if d.tweaks.ready().is_some_and(|t| t.options.contains_key("fps.cap")) {
        out.push(Row::Tweak { id: "fps.cap".into() });
    }
    out.push(Row::Pref(Pref::Dnd));
    out.push(Row::Button(Button::Screenshot));
    out.push(Row::Button(Button::Quit));
    out.push(Row::Button(Button::ForceQuit));
    out
}

/// Loads [`Extras`] for the running session.
pub async fn load(client: &Client, s: &SessionView) -> Extras {
    let mut x = Extras::default();
    match client.call::<SessionInfo>(Request::Sight(quiver_api::sight::SightRequest::Session { session: s.session.clone() })).await {
        Ok(i) => x.info = Some(i),
        Err(e) => x.info_error = Some(format!("{e:#}")),
    }
    let mods: Option<ModsState> = client.call(Request::Mods(quiver_api::mods::ModsRequest::List { game: s.game.into() })).await.ok();
    let profile = s.profile();
    if let Some(m) = &mods
        && let Some(p) = profile.and_then(|p| m.profiles.iter().find(|x| x.profile.id == p))
    {
        x.mods = p.resolved.iter().map(ModLine::of).collect();
    }
    x.saves = as_load(saves(client, s.game, mods.as_ref(), profile).await);
    let records: Result<Vec<quiver_api::options::SessionOptions>> = client
        .call(Request::Options(quiver_api::options::OptionsRequest::SessionRecords { game: s.game.into(), profile: Some(s.profile_id.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into())), limit: Some(4) }))
        .await;
    x.options = records.ok().and_then(|r| options_line(&r, &s.session));
    return x;

    fn as_load<T>(r: Result<T>) -> Load<T> {
        match r {
            Ok(t) => Load::Ready(t),
            Err(e) => Load::Failed(format!("{e:#}")),
        }
    }
}

async fn saves(client: &Client, game: GameRef, mods: Option<&ModsState>, profile: Option<&str>) -> Result<SavesSummary> {
    match game.store {
        StoreId::Ubisoft => {
            let id = game.as_ubisoft().unwrap_or(0);
            let m = mods.ok_or_else(|| anyhow::anyhow!("no mods state"))?;
            let stores: Option<SaveStoresReport> = client.call(Request::SaveStores { product_id: id }).await.ok();
            // This machine only: no Ubisoft request.
            let slots: Option<SavesReport> = client.call(Request::Saves { product_id: id, cloud: false }).await.ok();
            Ok(ubisoft_saves(m, profile, stores.as_ref(), slots.as_ref()))
        }
        StoreId::Epic => {
            let r: EpicSaves = client.call(Request::Epic(quiver_api::epic::EpicRequest::Game(quiver_api::epicgame::EpicGameRequest::Saves { game, cloud: false }))).await?;
            Ok(epic_saves(&r, mods, profile))
        }
        StoreId::Steam => {
            let d: Option<quiver_api::stores::GameDetail> = client.call(Request::GameDetail { game, refresh: false }).await.ok();
            Ok(steam_saves(d.as_ref().and_then(|d| d.cloud.as_ref()), mods.and_then(|m| m.save_sync), mods, profile))
        }
        other => anyhow::bail!("{} saves aren't read yet", other.label()),
    }
}

/// `~/Pictures/Quiver/…` for a path under the home folder.
pub fn home_relative(p: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from).and_then(|h| p.strip_prefix(h).ok().map(std::path::Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

// ---- Rendering -------------------------------------------------------------------------------

/// The section's body: two columns of cards.
pub fn body(v: &PanelView, th: &Theme, rows: &[Row], focused: Option<usize>, cx: &mut Context<PanelView>) -> AnyElement {
    let now = Utc::now();
    let s = v.data.session.ready();
    let x = v.data.extras.clone();
    let mut left = div().flex_1().min_w_0().flex().flex_col().gap(px(16.));
    let mut right = div().w(gpui::relative(0.42)).flex_none().flex().flex_col().gap(px(16.));
    for r in rows {
        if let Row::Note(t) = r {
            left = left.child(div().text_size(px(13.)).text_color(th.tx3).child(t.clone()));
        }
    }
    if let Some(s) = s {
        left = left.child(now_playing(v, th, s, x.info.as_ref(), now));
        left = left.child(div().flex().gap(px(16.)).child(mods_card(th, s, &x).flex_1().min_w_0()).child(saves_card(th, &x.saves, now).flex_1().min_w_0()));
        left = left.child(events_card(th, &x, v.data.achievements.ready(), s.started, now));
    }
    // Quick toggles.
    let toggles: Vec<AnyElement> = rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::Opt { .. } | Row::Hud(_) | Row::Tweak { .. } | Row::Pref(_))).map(|(i, r)| toggle(v, th, i, r, focused == Some(i), cx)).collect();
    right = right.child(card(th).child(heading(th, "Quick toggles")).child(div().flex().flex_col().gap(px(8.)).children(toggles)));
    if let Some(a) = v.data.achievements.ready() {
        right = right.child(achievements_card(th, a, s.and_then(|s| s.started)));
    }
    let buttons: Vec<AnyElement> = rows.iter().enumerate().filter_map(|(i, r)| if let Row::Button(b) = r { Some(button(v, th, i, *b, focused == Some(i), cx)) } else { None }).collect();
    let mut actions = card(th).child(div().flex().flex_wrap().gap(px(10.)).children(buttons));
    if let Some((t, tone)) = v.shot.clone() {
        actions = actions.child(div().pt(px(10.)).text_size(px(12.)).text_color(tone_color(th, tone)).child(t));
    } else if let Some(why) = x.info.as_ref().and_then(|i| i.capture_off.clone()) {
        actions = actions.child(div().pt(px(10.)).text_size(px(12.)).text_color(th.tx3).child(format!("Screenshots: {why}")));
    }
    right = right.child(actions);
    div().flex().gap(px(16.)).w_full().child(left).child(right).into_any_element()
}

fn now_playing(v: &PanelView, th: &Theme, s: &SessionView, info: Option<&SessionInfo>, now: DateTime<Utc>) -> gpui::Div {
    let art: AnyElement = match info.and_then(|i| i.art_file.clone()) {
        Some(p) => {
            let tile = info.is_some_and(|i| i.art_tile);
            div()
                .flex_none()
                .w(px(96.))
                .h(px(112.))
                .rounded(px(10.))
                .overflow_hidden()
                .bg(th.ink4)
                .child(img(PathBuf::from(p)).w(px(96.)).h(px(112.)).rounded(px(10.)).object_fit(if tile { ObjectFit::Fill } else { ObjectFit::Contain }))
                .into_any_element()
        }
        None => div().flex_none().w(px(96.)).h(px(112.)).rounded(px(10.)).bg(th.ink4).flex().items_center().justify_center().child(icon(Icon::Gamepad, 34., th.tx4)).into_any_element(),
    };
    let profile = s.profile_name.clone().unwrap_or_else(|| "Vanilla".into());
    let mut chips = div().flex().flex_wrap().gap(px(8.)).child(chip(th, format!("Profile: {profile}"), Some(Tone::Accent)));
    match info.and_then(|i| i.proton.clone()) {
        Some(p) => chips = chips.child(chip(th, p, None)),
        None if s.copy.contains("Steam") => chips = chips.child(chip(th, "Steam's Proton", None)),
        None => {}
    }
    chips = chips.child(chip(th, s.copy.clone(), None));
    let this = s.started.map(|t| (now - t).num_seconds().max(0) as u64);
    let total = s.total_secs.map(|t| t + this.unwrap_or(0));
    let _ = v;
    card(th)
        .flex_row()
        .items_center()
        .gap(px(20.))
        .child(art)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(13.)).text_color(th.tx3).child("Now playing"))
                .child(div().mt(px(2.)).mb(px(10.)).font_family(DISPLAY_FONT).text_size(px(28.)).font_weight(BOLD).text_color(th.tx1).line_clamp(2).child(s.name.clone()))
                .child(chips),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .child(div().text_size(px(13.)).text_color(th.tx3).child("This session"))
                .child(div().mt(px(2.)).font_family(DISPLAY_FONT).text_size(px(30.)).font_weight(BOLD).text_color(th.tx1).child(this.map(duration).unwrap_or_else(|| "—".into())))
                .when_some(total, |d, t| d.child(div().mt(px(2.)).text_size(px(13.)).text_color(th.tx2).child(format!("{} in total", duration(t)))))
                .when_some(s.reported, |d, r| {
                    d.child(div().mt(px(4.)).text_size(px(12.)).text_color(if r { th.tx3 } else { th.warn }).child(if r { "Play time reported to Ubisoft" } else { "Play time not reported" }))
                }),
        )
}

fn mods_card(th: &Theme, s: &SessionView, x: &super::session::Extras) -> gpui::Div {
    let mut c = card(th).child(heading(th, "Mods in this session"));
    if x.mods.is_empty() {
        let none = if s.mods.is_empty() { "None: the game as it ships".to_owned() } else { s.mods.join(" · ") };
        c = c.child(line(th, key_text(th, none), div()));
    }
    for m in &x.mods {
        c = c.child(line(th, value_text(th, m.name.clone()), div().when_some(m.chip.clone(), |d, t| d.child(chip(th, t, None)))));
    }
    if let Some(o) = &x.options {
        c = c.child(line(th, key_text(th, "Options"), value_text(th, o.text())));
    }
    c
}

fn saves_card(th: &Theme, saves: &Load<SavesSummary>, now: DateTime<Utc>) -> gpui::Div {
    let c = card(th).child(heading(th, "Saves"));
    match saves {
        Load::Ready(s) => {
            let last = match (&s.last_what, s.last_save) {
                (Some(w), Some(t)) => format!("{w} · {}", ago(t, now)),
                (None, Some(t)) => ago(t, now),
                _ => "—".into(),
            };
            let shared = if s.shared_with.is_empty() { "No other profile".to_owned() } else { s.shared_with.join(", ") };
            c.child(line(th, value_text(th, s.store.clone()), chip(th, s.state.clone(), s.tone)))
                .child(line(th, key_text(th, "Last save"), value_text(th, last)))
                .child(line(th, key_text(th, "Shared with"), value_text(th, shared)))
                .when_some(s.note.clone(), |d, n| d.child(div().pt(px(6.)).text_size(px(12.)).text_color(th.tx3).line_clamp(2).child(n)))
        }
        Load::Loading => c.child(key_text(th, "Loading…")),
        Load::Failed(e) | Load::Unavailable(e) => c.child(div().text_size(px(13.)).text_color(th.tx3).line_clamp(3).child(format!("Couldn't read the saves: {e}"))),
    }
}

fn events_card(th: &Theme, x: &Extras, achs: Option<&Achs>, started: Option<DateTime<Utc>>, now: DateTime<Utc>) -> gpui::Div {
    let ev = events(x.info.as_ref(), achs, started);
    let mut c = card(th).child(heading(th, "While you were playing"));
    if ev.is_empty() {
        let why = match &x.info_error {
            Some(e) => format!("Nothing to show ({e})"),
            None => "Nothing yet: unlocks, finished downloads and mod changes show up here.".into(),
        };
        return c.child(div().text_size(px(13.)).text_color(th.tx3).line_clamp(2).child(why));
    }
    for e in ev.iter().take(6) {
        let ic = match e.kind {
            SessionEventKind::Achievement => Icon::Trophy,
            SessionEventKind::Download => Icon::Download,
            SessionEventKind::Mods => Icon::Package,
            SessionEventKind::Options => Icon::SlidersHorizontal,
            SessionEventKind::Saves => Icon::Cloud,
            SessionEventKind::Screenshot => Icon::Image,
            SessionEventKind::Agent => Icon::Bot,
            SessionEventKind::Other => Icon::Info,
        };
        let text = match &e.by {
            Some(by) => format!("{} ({by})", e.text),
            None => e.text.clone(),
        };
        let at = e.at.with_timezone(&chrono::Local);
        let t = if (now - e.at).num_hours() < 20 { at.format("%H:%M").to_string() } else { at.format("%-d %b %H:%M").to_string() };
        c = c.child(line(th, div().flex().items_center().gap(px(10.)).min_w_0().child(icon(ic, 15., th.tx3)).child(value_text(th, text)), key_text(th, t).flex_none()));
    }
    c
}

fn achievements_card(th: &Theme, a: &Achs, started: Option<DateTime<Utc>>) -> gpui::Div {
    let p = super::achievements::progress(a, started);
    card(th)
        .child(heading(th, "Achievements"))
        .child(
            div()
                .flex()
                .items_end()
                .gap(px(8.))
                .child(div().font_family(DISPLAY_FONT).text_size(px(28.)).font_weight(BOLD).text_color(th.tx1).child(a.unlocked.to_string()))
                .child(div().pb(px(4.)).text_size(px(14.)).text_color(th.tx2).child(format!("of {} · {} this session", a.total, p.this_session))),
        )
        .child(div().mt(px(10.)).child(bar(th.ink4, th.warn, p.fraction, 6.)))
}

/// A quick toggle: a label with its producer and key in grey, a switch or a value.
fn toggle(v: &PanelView, th: &Theme, i: usize, r: &Row, focused: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let (label, sub, value): (String, Option<String>, Result<bool, String>) = match r {
        Row::Opt { surface, .. } => {
            let on = v.draft_of(r).and_then(|x| x.as_bool()).unwrap_or(false);
            let fg = v.data.options.ready().and_then(|o| fg_toggle(o)).filter(|f| &f.0 == surface);
            let sub = fg.map(|(_, _, title, key)| [Some(title), key].into_iter().flatten().collect::<Vec<_>>().join(" · "));
            ("Frame generation".to_owned(), sub, Ok(on))
        }
        Row::Hud(HudRow::Shown) => ("HUD".into(), Some("Shift+F3".into()), Ok(v.hud_value(HudRow::Shown) == "on")),
        Row::Hud(HudRow::Enable) => ("Sight layer".into(), Some("next launch".into()), Ok(v.hud_value(HudRow::Enable) == "on")),
        Row::Hud(HudRow::Preset) => {
            let p = v.hud_value(HudRow::Preset);
            ("HUD preset".into(), Some("Ctrl+Shift+F3".into()), Err(crate::panel::state::PRESETS.iter().find(|x| x.0 == p).map(|x| x.1.to_owned()).unwrap_or(p)))
        }
        Row::Tweak { .. } => {
            let st = v.option_state(r);
            let val = match (st, v.draft_of(r)) {
                (Some(st), Some(x)) => short_value(&st.def, &x),
                _ => "—".into(),
            };
            let by = st.and_then(|s| s.tweak.as_ref()).and_then(|t| match &t.planned {
                Some(quiver_api::tweaks::Planned::Provided { by, .. }) => Some(by.clone()),
                _ => None,
            });
            ("Frame rate cap".into(), by.or(Some("next launch".into())), Err(val))
        }
        Row::Pref(Pref::Dnd) => ("Do not disturb".into(), Some("holds toasts".into()), Ok(v.data.prefs.dnd)),
        _ => return div().into_any_element(),
    };
    let control = match value {
        Ok(on) => switch(th, on).into_any_element(),
        Err(t) => div()
            .flex()
            .items_center()
            .gap(px(6.))
            .when(focused, |d| d.child(div().text_color(th.tx3).child("◀")))
            .child(div().text_color(th.tx2).font_weight(SEMIBOLD).child(t))
            .when(focused, |d| d.child(div().text_color(th.tx3).child("▶")))
            .into_any_element(),
    };
    div()
        .id(SharedString::from(format!("row-{i}")))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .px(px(14.))
        .py(px(12.))
        .rounded(px(10.))
        .bg(if focused { th.ink3 } else { th.ink1 })
        .border_1()
        .border_color(if focused { th.line2 } else { th.line1 })
        .text_size(px(14.))
        .font_weight(MEDIUM)
        .cursor_pointer()
        .focus_ring(focused, th, 10.)
        .when(focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)))
        .child(div().min_w_0().truncate().text_color(th.tx1).child(label).when_some(sub, |d, s| d.child(div().text_color(th.tx3).child(format!(" · {s}")))).flex().items_center())
        .child(control)
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            this.input(Input::Activate, cx);
        }))
        .into_any_element()
}

fn switch(th: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(38.))
        .h(px(22.))
        .rounded(px(11.))
        .bg(if on { th.acc } else { th.ink5 })
        .flex()
        .items_center()
        .when(on, |d| d.justify_end())
        .px(px(3.))
        .child(div().size(px(16.)).rounded(px(8.)).bg(if on { th.acc_ink } else { th.tx2 }))
}

fn button(v: &PanelView, th: &Theme, i: usize, b: Button, focused: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let (label, ic, col, border) = match b {
        Button::Screenshot => ("Screenshot", Icon::Image, th.tx1, th.line2),
        Button::Quit => ("Quit game", Icon::LogOut, th.tx1, th.line2),
        Button::ForceQuit => ("Force quit…", Icon::CircleX, th.bad, th.bad.opacity(0.4)),
    };
    div()
        .id(SharedString::from(format!("row-{i}")))
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(40.))
        .px(px(16.))
        .rounded(px(10.))
        .bg(if focused { th.ink4 } else { th.ink1 })
        .border_1()
        .border_color(border)
        .text_size(px(14.))
        .font_weight(SEMIBOLD)
        .text_color(col)
        .cursor_pointer()
        .focus_ring(focused, th, 10.)
        .when(focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)))
        .child(icon(ic, 16., col))
        .child(label)
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            this.input(Input::Activate, cx);
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::info::{SaveSlot, SlotFile};
    use quiver_api::mods::{Profile, ProfileInfo, SavesPlan};
    use quiver_api::savestores::{SaveStore, SaveStoreInfo, StoreUser};

    fn profile(id: &str, name: &str, store: &str, cloud: bool) -> ProfileInfo {
        ProfileInfo {
            profile: Profile { id: id.into(), name: name.into(), ..Default::default() },
            saves: Some(SavesPlan { store: Some(store.into()), store_name: Some(if store == "default" { "Default".into() } else { format!("{name} saves") }), cloud, ..Default::default() }),
            ..Default::default()
        }
    }

    fn mods() -> ModsState {
        ModsState {
            profiles: vec![profile("p-dlss", "DLSS", "default", true), profile("p-hard", "Hardcore", "s1", false), profile("p-photo", "Photo", "default", true)],
            vanilla_saves: Some(SavesPlan { store: Some("default".into()), cloud: true, ..Default::default() }),
            save_sync: Some(SaveSync::Ubisoft),
            ..Default::default()
        }
    }

    #[test]
    fn ubisoft_saves_card() {
        let now = Utc::now();
        let stores = SaveStoresReport {
            stores: vec![SaveStoreInfo {
                store: SaveStore { id: "default".into(), name: "Default".into(), cloud: true, ..Default::default() },
                used_by: vec![StoreUser { id: "vanilla".into(), name: "vanilla".into() }, StoreUser { id: "p-dlss".into(), name: "DLSS".into() }, StoreUser { id: "p-photo".into(), name: "Photo".into() }],
                last_modified: Some(now - chrono::Duration::hours(3)),
                ..Default::default()
            }],
            vanilla: "default".into(),
            ..Default::default()
        };
        let slot = |n: u32, mins: i64, pending: bool| SaveSlot { slot: n, local: Some(SlotFile { size: 1, modified: Some(now - chrono::Duration::minutes(mins)) }), pending_upload: pending, ..Default::default() };
        let mut slots = SavesReport { slots: vec![slot(1, 300, false), slot(3, 2, false)], store: Some("Default".into()), ..Default::default() };
        let s = ubisoft_saves(&mods(), Some("p-dlss"), Some(&stores), Some(&slots));
        assert_eq!((s.store.as_str(), s.state.as_str(), s.tone), ("Default", "Cloud in sync", Some(Tone::Ok)));
        assert_eq!(s.last_what.as_deref(), Some("slot 3"));
        assert_eq!(ago(s.last_save.unwrap(), now), "2 min ago");
        assert_eq!(s.shared_with, ["Vanilla", "Photo"]);
        slots.slots[1].pending_upload = true;
        let s = ubisoft_saves(&mods(), Some("p-dlss"), Some(&stores), Some(&slots));
        assert_eq!((s.state.as_str(), s.tone), ("1 to upload at exit", Some(Tone::Accent)));
        // A store of its own, kept on this PC; nobody shares it; no report: the profiles say.
        let s = ubisoft_saves(&mods(), Some("p-hard"), None, None);
        assert_eq!((s.store.as_str(), s.state.as_str()), ("Hardcore saves", "On this PC"));
        assert!(s.shared_with.is_empty());
        // Vanilla, without the stores report: the profiles on Default.
        let s = ubisoft_saves(&mods(), None, None, None);
        assert_eq!(s.shared_with, ["DLSS", "Photo"]);
        // A game without cloud saves.
        let m = ModsState { save_sync: Some(SaveSync::UbisoftNone), ..mods() };
        assert_eq!(ubisoft_saves(&m, None, None, None).state, "On this PC");
    }

    #[test]
    fn epic_and_steam_saves_cards() {
        use quiver_api::epicgame::{SaveFile, SaveSide};
        let now = Utc::now();
        let mut e = EpicSaves::new(GameRef::parse_ref("epic:x").unwrap());
        e.mode = SaveMode::Launcher;
        e.state = SyncState::LocalNewer;
        e.files = vec![SaveFile { path: "Saves/slot_1.sav".into(), local: Some(SaveSide { size: 1, modified: Some(now - chrono::Duration::minutes(5)), sha1: String::new() }), cloud: None, state: Default::default() }];
        let m = ModsState { profiles: vec![profile("p-t", "Torchbearer", "default", true)], ..Default::default() };
        let s = epic_saves(&e, Some(&m), Some("p-t"));
        assert_eq!((s.state.as_str(), s.last_what.as_deref()), ("Uploads at exit", Some("slot_1.sav")));
        assert_eq!(s.shared_with, ["Vanilla"]);
        e.mode = SaveMode::InGame;
        assert_eq!(epic_saves(&e, None, None).state, "Synced in game");
        let c = CloudState { files: 3, bytes: 1, last_upload: Some(now), unsynced: 0 };
        assert_eq!(steam_saves(Some(&c), Some(SaveSync::SteamCloud), None, None).state, "Steam Cloud in sync");
        assert_eq!(steam_saves(Some(&CloudState { unsynced: 2, ..c.clone() }), Some(SaveSync::SteamCloud), None, None).state, "2 files waiting");
        assert_eq!(steam_saves(None, Some(SaveSync::SteamNone), None, None).state, "On this PC");
    }

    #[test]
    fn mod_lines_and_options() {
        let r = |label: &str, pinned, current, versions| ResolvedMod { name: "DLSS".into(), label: label.into(), pinned, current, versions, ..Default::default() };
        assert_eq!(ModLine::of(&r("v3", false, true, 3)).chip.as_deref(), Some("v3 · current"));
        assert_eq!(ModLine::of(&r("v2", true, false, 3)).chip.as_deref(), Some("v2 · pinned"));
        assert_eq!(ModLine::of(&r("1", false, true, 1)).chip.as_deref(), Some("1"));
        assert_eq!(ModLine::of(&r("", false, true, 1)).chip, None);
        use quiver_api::options::{LiveChange, SessionOptions};
        let ch = |id: &str| LiveChange { at: Utc::now(), surface: "m/dlss".into(), id: id.into(), source: "sight".into(), value: serde_json::json!(1) };
        let rec = SessionOptions { session: "s-1".into(), launched_revision: 14, live_changes: vec![ch("a"), ch("b"), ch("a"), ch("c")], ..Default::default() };
        assert_eq!(options_line(&[rec], "s-1").unwrap().text(), "r14 · 3 changed in game");
        assert_eq!(options_line(&[], "s-1"), None);
    }

    #[test]
    fn events_newest_first_with_unlocks() {
        use crate::panel::state::Ach;
        let t0 = Utc::now() - chrono::Duration::hours(1);
        let info = SessionInfo {
            events: vec![
                SessionEvent { at: t0 + chrono::Duration::minutes(10), kind: SessionEventKind::Download, text: "Download finished: Far Cry 5".into(), by: None },
                SessionEvent { at: t0 + chrono::Duration::minutes(30), kind: SessionEventKind::Achievement, text: "Achievement unlocked: Hidden Ones".into(), by: None },
            ],
            ..Default::default()
        };
        let a = |name: &str, mins: i64| Ach { id: name.into(), name: name.into(), unlocked: true, unlocked_at: Some(t0 + chrono::Duration::minutes(mins)), ..Default::default() };
        let achs = Achs::sorted(vec![a("Hidden Ones", 30), a("First Visit", 45), a("Old one", -600)]);
        let ev = events(Some(&info), Some(&achs), Some(t0));
        let texts: Vec<&str> = ev.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["Achievement unlocked: First Visit", "Achievement unlocked: Hidden Ones", "Download finished: Far Cry 5"]);
    }

    #[test]
    fn quick_toggles() {
        let d = crate::demo::sample_panel();
        let r = rows(&d);
        let ids: Vec<String> = r
            .iter()
            .map(|r| match r {
                Row::Opt { id, .. } => id.clone(),
                Row::Hud(h) => format!("{h:?}"),
                Row::Tweak { id } => id.clone(),
                Row::Pref(p) => format!("{p:?}"),
                Row::Button(b) => format!("{b:?}"),
                _ => "?".into(),
            })
            .collect();
        assert_eq!(ids, ["fg.enabled", "Shown", "Preset", "fps.cap", "Dnd", "Screenshot", "Quit", "ForceQuit"]);
        assert!(r.iter().all(|r| r.focusable()));
        let (_, _, title, key) = fg_toggle(d.options.ready().unwrap()).unwrap();
        assert_eq!((title.as_str(), key.as_deref()), ("DLSS", Some("F9")));
        // Loading: a note first, Quit is still there.
        let r = rows(&PanelData::default());
        assert!(matches!(&r[0], Row::Note(_)) && r.contains(&Row::Button(Button::ForceQuit)));
    }
}
