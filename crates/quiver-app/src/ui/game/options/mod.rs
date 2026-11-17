//! Mod options on the Mods tab (design/MOD-CONFIG.md §6 "App"): each surface's pages as a
//! form generated from its schema, edits staged in an unsaved bar and saved as one revision
//! with the daemon's outcome per change (applied live, saved for a moment or a restart, or
//! rejected, never an optimistic "done"), the profile's changes beside the form, history
//! with its actors and restores, profile comparison, the keys in play, and where each
//! schema came from (with "infer from file" for mods without one).
//!
//! Everything sent from here is the person's own action (source `gui`): the daemon's agent
//! policy (MOD-CONFIG steer 13) applies to agents, whose changes show as theirs in history.

mod changes;
pub(super) mod form;
mod keys;
pub mod logic;
mod source;

use crate::model::Gid;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px};
use quiver_api::options::{
    KeysReport, Level, Live, OpOutcome, OptionDiff, OptionsChange, OptionsRequest, OptionsRevision, SchemaInfo, SessionOptions, SurfaceState,
};
use quiver_api::{Event, Request};

use self::logic::{Edit, RowKey, ops_of};
use super::page::Load;
use crate::app::Quiver;
use crate::err_text;
use crate::nav::Tab;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, chip, chip_count};

/// The Mods tab's views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModsView {
    /// Profiles' mod stacks and the mod store (the tab as it was).
    #[default]
    Stack,
    Options,
    /// History, profile comparison, imports and crash notes.
    Changes,
    Keys,
    /// Where a surface's schema came from; "infer from file" for mods without one.
    Source,
}

impl ModsView {
    pub const TABS: [ModsView; 4] = [ModsView::Stack, ModsView::Options, ModsView::Changes, ModsView::Keys];

    pub fn label(self) -> &'static str {
        match self {
            ModsView::Stack => "Mod stack",
            ModsView::Options => "Options",
            ModsView::Changes => "History & changes",
            ModsView::Keys => "Keys",
            ModsView::Source => "Source",
        }
    }

    fn icon(self) -> Icon {
        match self {
            ModsView::Stack => Icon::Layers,
            ModsView::Options => Icon::SlidersHorizontal,
            ModsView::Changes => Icon::History,
            ModsView::Keys => Icon::Keyboard,
            ModsView::Source => Icon::FileText,
        }
    }
}

/// A text field bound to an option (numbers, text, a list's "add" box), and the text it last
/// showed, so typing can be told from the app setting it.
pub struct FieldInput {
    pub input: Entity<TextInput>,
    pub seen: String,
}

/// An action waiting for a second click.
#[derive(Clone, Debug, PartialEq)]
pub enum Confirm {
    /// Saving changes `danger` options (each line says why).
    Danger { lines: Vec<String>, live: Live },
    Restore(u32),
    RevertSession(String),
}

/// The last save's result line.
#[derive(Clone, Debug, Default)]
pub struct SaveResult {
    pub summary: String,
    pub live: Vec<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct OptionsUi {
    pub view: ModsView,
    /// The game and profile id the loaded data is for.
    pub of: Option<(Gid, String)>,
    pub surfaces: Load<Vec<SurfaceState>>,
    pub history: Load<Vec<OptionsRevision>>,
    pub keys: Load<KeysReport>,
    /// "Compare with": `default` or a profile id, and its differences.
    pub compare: String,
    pub diff: Load<(String, Vec<OptionDiff>)>,
    pub records: Load<Vec<SessionOptions>>,
    /// Source view: the mod shown, its schema (or why there's none), its files and a guess.
    pub source_mod: Option<String>,
    pub schema: Load<(String, SchemaInfo)>,
    pub files: Load<(String, Vec<String>)>,
    pub guess: Load<(String, String, String)>,
    pub guess_error: Option<String>,
    pub show_text: bool,
    /// The selected surface (key) and page (id; `other` for "Other keys").
    pub surface: Option<String>,
    pub page: Option<String>,
    pub level: Level,
    pub changed_only: bool,
    pub search: Option<Entity<TextInput>>,
    /// Unsaved edits per (game, profile id). They outlive the page and profile switches.
    pub staged: BTreeMap<(Gid, String), BTreeMap<RowKey, Edit>>,
    /// Text fields per (game, profile, surface, option).
    pub inputs: HashMap<(Gid, String, String, String), FieldInput>,
    /// The last save's outcome per row (for the loaded game and profile).
    pub outcomes: HashMap<RowKey, OpOutcome>,
    pub result: Option<SaveResult>,
    pub saving: bool,
    pub confirm: Option<Confirm>,
    /// The enum list or key picker open on a row.
    pub picker: Option<RowKey>,
    /// Values the running session reported that weren't saved (a hotkey flip): row → (value, by).
    pub in_game: HashMap<RowKey, (serde_json::Value, String)>,
    /// A crash right after a live change (from the event), for this game.
    pub crash: Option<(Gid, String, SessionOptions)>,
    /// A change someone else made while the page was open.
    pub notice: Option<String>,
    /// The preset the staged edits came from (the revision's message).
    pub preset_note: Option<String>,
    /// Import cards the person dismissed (revision numbers).
    pub dismissed: Vec<u32>,
    /// The Options view's rows as last drawn, for the controller (body rows).
    pub pad_rows: Vec<RowKey>,
}

impl OptionsUi {
    /// Clear what belongs to the loaded game and profile, keeping unsaved edits.
    fn forget_loaded(&mut self) {
        let staged = std::mem::take(&mut self.staged);
        let inputs = std::mem::take(&mut self.inputs);
        let dismissed = std::mem::take(&mut self.dismissed);
        let (view, level, search, changed_only) = (self.view, self.level, self.search.take(), self.changed_only);
        // The Source view is about a mod, not a profile.
        let (source_mod, schema, files, guess, guess_error) =
            (self.source_mod.take(), std::mem::take(&mut self.schema), std::mem::take(&mut self.files), std::mem::take(&mut self.guess), self.guess_error.take());
        *self = OptionsUi {
            view,
            level,
            search,
            changed_only,
            staged,
            inputs,
            dismissed,
            source_mod,
            schema,
            files,
            guess,
            guess_error,
            compare: "default".into(),
            ..Default::default()
        };
    }

    pub fn staged_for(&self, game: Gid, profile: &str) -> BTreeMap<RowKey, Edit> {
        self.staged.get(&(game, profile.to_owned())).cloned().unwrap_or_default()
    }

    pub fn loaded_surfaces(&self) -> &[SurfaceState] {
        self.surfaces.ready().map(Vec::as_slice).unwrap_or_default()
    }

    pub fn surface(&self, key: &str) -> Option<&SurfaceState> {
        self.loaded_surfaces().iter().find(|s| s.key == key)
    }
}

impl Quiver {
    /// The profile the Mods tab shows (`vanilla` or a profile id).
    pub(super) fn mods_profile(&self) -> String {
        self.gp.prof_sel.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into())
    }

    /// The profile's display name.
    pub(super) fn mods_profile_name(&self, profile: &str) -> String {
        self.gp.profiles().into_iter().find(|p| p.id == profile).map(|p| p.name).unwrap_or_else(|| "Vanilla".into())
    }

    /// Load what the current view needs for (game, profile), once.
    pub(super) fn options_ensure(&mut self, id: Gid, cx: &mut Context<Self>) {
        let profile = self.mods_profile();
        if self.gp.opt.of.as_ref() != Some(&(id, profile.clone())) {
            self.gp.opt.forget_loaded();
            self.gp.opt.of = Some((id, profile.clone()));
            self.load_option_surfaces(id, cx);
            self.load_option_history(id, cx);
            self.load_option_keys(id, cx);
        }
        match self.gp.opt.view {
            ModsView::Changes => {
                if self.gp.opt.diff.is_idle() {
                    self.load_option_diff(id, cx);
                }
                if self.gp.opt.records.is_idle() {
                    self.load_option_records(id, cx);
                }
            }
            ModsView::Source => {
                if let Some(m) = self.gp.opt.source_mod.clone() {
                    if self.gp.opt.schema.is_idle() {
                        self.load_option_schema(id, &m, cx);
                    }
                    if self.gp.opt.files.is_idle() {
                        self.load_option_files(id, &m, cx);
                    }
                }
            }
            _ => {}
        }
    }

    fn options_req(&self) -> Option<(Gid, String)> {
        self.gp.opt.of.clone()
    }

    pub(super) fn load_option_surfaces(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some((g, profile)) = self.options_req().filter(|(g, _)| *g == id) else { return };
        if self.gp.opt.surfaces.ready().is_none() {
            self.gp.opt.surfaces = Load::Loading;
        }
        let p = profile.clone();
        self.call::<Vec<SurfaceState>, _>(Request::Options(OptionsRequest::List { product_id: g.pid(), profile: Some(profile), level: None }), cx, move |this, r, cx| {
            if this.gp.opt.of.as_ref() != Some(&(g, p.clone())) {
                return;
            }
            match r {
                Ok(list) => {
                    this.gp.opt.surfaces = Load::Ready(list);
                    this.options_pick_defaults();
                    this.options_sync_inputs(g, &p, cx);
                }
                Err(e) => this.gp.opt.surfaces = Load::Failed(err_text(&e)),
            }
        });
    }

    pub(super) fn load_option_history(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some((g, profile)) = self.options_req().filter(|(g, _)| *g == id) else { return };
        let p = profile.clone();
        self.call::<Vec<OptionsRevision>, _>(Request::Options(OptionsRequest::History { product_id: g.pid(), profile: Some(profile) }), cx, move |this, r, _| {
            if this.gp.opt.of.as_ref() == Some(&(g, p)) {
                this.gp.opt.history = match r {
                    Ok(h) => Load::Ready(h),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub(super) fn load_option_keys(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some((g, profile)) = self.options_req().filter(|(g, _)| *g == id) else { return };
        let p = profile.clone();
        self.call::<KeysReport, _>(Request::Options(OptionsRequest::Keys { product_id: g.pid(), profile: Some(profile) }), cx, move |this, r, _| {
            if this.gp.opt.of.as_ref() == Some(&(g, p)) {
                this.gp.opt.keys = match r {
                    Ok(k) => Load::Ready(k),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub(super) fn load_option_diff(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some((g, profile)) = self.options_req().filter(|(g, _)| *g == id) else { return };
        let against = if self.gp.opt.compare.is_empty() { "default".to_owned() } else { self.gp.opt.compare.clone() };
        if self.gp.opt.diff.ready().is_none() {
            self.gp.opt.diff = Load::Loading;
        }
        let (p, a) = (profile.clone(), against.clone());
        self.call::<Vec<OptionDiff>, _>(Request::Options(OptionsRequest::Diff { product_id: g.pid(), profile: Some(profile), against: Some(against) }), cx, move |this, r, _| {
            if this.gp.opt.of.as_ref() == Some(&(g, p)) {
                this.gp.opt.diff = match r {
                    Ok(d) => Load::Ready((a, d)),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub(super) fn load_option_records(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some((g, profile)) = self.options_req().filter(|(g, _)| *g == id) else { return };
        self.gp.opt.records = Load::Loading;
        let p = profile.clone();
        self.call::<Vec<SessionOptions>, _>(
            Request::Options(OptionsRequest::SessionRecords { product_id: g.pid(), profile: Some(profile), limit: Some(10) }),
            cx,
            move |this, r, _| {
                if this.gp.opt.of.as_ref() == Some(&(g, p)) {
                    // Daemons from before the request: no records, no crash notes (not an error).
                    this.gp.opt.records = Load::Ready(r.unwrap_or_default());
                }
            },
        );
    }

    pub(super) fn load_option_schema(&mut self, id: Gid, mod_id: &str, cx: &mut Context<Self>) {
        self.gp.opt.schema = Load::Loading;
        let m = mod_id.to_owned();
        self.call::<SchemaInfo, _>(Request::Options(OptionsRequest::Schema { product_id: id.pid(), mod_id: m.clone(), version: None }), cx, move |this, r, _| {
            if this.gp.opt.source_mod.as_deref() == Some(m.as_str()) {
                this.gp.opt.schema = match r {
                    Ok(s) => Load::Ready((m, s)),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    pub(super) fn load_option_files(&mut self, id: Gid, mod_id: &str, cx: &mut Context<Self>) {
        self.gp.opt.files = Load::Loading;
        let m = mod_id.to_owned();
        self.call::<Vec<String>, _>(Request::Options(OptionsRequest::Files { product_id: id.pid(), mod_id: m.clone() }), cx, move |this, r, _| {
            if this.gp.opt.source_mod.as_deref() == Some(m.as_str()) {
                this.gp.opt.files = match r {
                    Ok(f) => Load::Ready((m, f)),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    /// Reload everything shown for the loaded game and profile (after a save or an event).
    pub(super) fn options_reload(&mut self, cx: &mut Context<Self>) {
        let Some((g, _)) = self.gp.opt.of.clone() else { return };
        self.load_option_surfaces(g, cx);
        self.load_option_history(g, cx);
        self.load_option_keys(g, cx);
        if !self.gp.opt.diff.is_idle() {
            self.load_option_diff(g, cx);
        }
        if !self.gp.opt.records.is_idle() {
            self.load_option_records(g, cx);
        }
    }

    /// A surface and page to show when none is picked (or the picked one went away).
    fn options_pick_defaults(&mut self) {
        let o = &mut self.gp.opt;
        let list = o.surfaces.ready().cloned().unwrap_or_default();
        let ok = o.surface.as_ref().is_some_and(|k| list.iter().any(|s| &s.key == k));
        if !ok {
            o.surface = list.iter().find(|s| s.kind == quiver_api::options::SurfaceKind::Mod).or(list.first()).map(|s| s.key.clone());
            o.page = None;
        }
        if let Some(s) = o.surface.as_ref().and_then(|k| list.iter().find(|s| &s.key == k)) {
            let page_ok = o.page.as_ref().is_some_and(|p| s.pages.iter().any(|x| &x.id == p) || (p == "other" && !s.raw.is_empty()) || (p == "dormant" && !s.dormant.is_empty()));
            if !page_ok {
                o.page = s.pages.first().map(|p| p.id.clone());
            }
        }
    }

    /// Text fields without an unsaved edit show the saved value again.
    fn options_sync_inputs(&mut self, g: Gid, profile: &str, cx: &mut Context<Self>) {
        let staged = self.gp.opt.staged_for(g, profile);
        let surfaces = self.gp.opt.loaded_surfaces().to_vec();
        for ((ig, ip, sk, id), f) in self.gp.opt.inputs.iter_mut() {
            if *ig != g || ip != profile || staged.contains_key(&(sk.clone(), id.clone())) || id.starts_with('+') {
                continue;
            }
            let Some(st) = surfaces.iter().find(|s| &s.key == sk).and_then(|s| s.options.get(id)) else { continue };
            let text = st.def.to_text(&st.value);
            if f.seen != text {
                f.seen = text.clone();
                f.input.update(cx, |t, cx| t.set_text(text, cx));
            }
        }
    }

    /// Stage an edit (or drop it, when it puts back the saved value) and show it in the
    /// row's text field, if it has one.
    pub(super) fn options_stage(&mut self, key: RowKey, edit: Option<Edit>, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        let st = self.gp.opt.surface(&key.0).and_then(|s| s.options.get(&key.1)).cloned();
        let edit = match (edit, &st) {
            // Setting the saved value again, or resetting what is the default already: nothing.
            (Some(Edit::Set(v)), Some(st)) if st.def.normalize(&v).is_ok_and(|n| quiver_api::options::same_value(&n, &st.value)) => None,
            (Some(Edit::Reset), Some(st)) if !logic::is_changed(st) => None,
            (e, _) => e,
        };
        let map = self.gp.opt.staged.entry((g, p.clone())).or_default();
        match &edit {
            Some(e) => {
                map.insert(key.clone(), e.clone());
            }
            None => {
                map.remove(&key);
            }
        }
        self.gp.opt.outcomes.remove(&key);
        if let (Some(st), Some(f)) = (st, self.gp.opt.inputs.get_mut(&(g, p, key.0.clone(), key.1.clone()))) {
            let v = logic::draft_value(&st, edit.as_ref());
            let text = st.def.to_text(&v);
            if f.seen != text {
                f.seen = text.clone();
                f.input.update(cx, |t, cx| t.set_text(text, cx));
            }
        }
        self.gp.opt.result = None;
        self.gp.opt.confirm = None;
    }

    /// A text field for an option (created once, showing `text`).
    pub(super) fn option_input(&mut self, key: &RowKey, text: &str, placeholder: &str, cx: &mut Context<Self>) -> Entity<TextInput> {
        let Some((g, p)) = self.gp.opt.of.clone() else { return self.new_option_input(text, placeholder, cx) };
        let k = (g, p, key.0.clone(), key.1.clone());
        if let Some(f) = self.gp.opt.inputs.get(&k) {
            return f.input.clone();
        }
        let input = self.new_option_input(text, placeholder, cx);
        self.gp.opt.inputs.insert(k, FieldInput { input: input.clone(), seen: text.to_owned() });
        input
    }

    pub(in crate::ui) fn new_option_input(&self, text: &str, placeholder: &str, cx: &mut Context<Self>) -> Entity<TextInput> {
        let text = text.to_owned();
        let e = cx.new(|cx| {
            let mut t = TextInput::new(cx, placeholder, false).compact(false);
            t.set_text(text, cx);
            t
        });
        cx.observe(&e, |_, _, cx| cx.notify()).detach();
        e
    }

    /// Typing in a field stages the text (the daemon parses and validates it).
    pub(super) fn options_read_inputs(&mut self, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        let mut typed: Vec<(RowKey, String)> = Vec::new();
        for ((ig, ip, sk, id), f) in self.gp.opt.inputs.iter_mut() {
            if *ig != g || *ip != p || id.starts_with('+') {
                continue;
            }
            let t = f.input.read(cx).text().to_owned();
            if t != f.seen {
                f.seen = t.clone();
                typed.push(((sk.clone(), id.clone()), t));
            }
        }
        for (key, t) in typed {
            let saved = self.gp.opt.surface(&key.0).and_then(|s| match key.1.strip_prefix("raw:") {
                Some(b) => s.raw.iter().find(|r| r.binding == b).map(|r| r.text.clone()),
                None => s.options.get(&key.1).map(|st| st.text.clone()),
            });
            let map = self.gp.opt.staged.entry((g, p.clone())).or_default();
            match saved {
                Some(s) if t.trim() == s.trim() => {
                    map.remove(&key);
                }
                _ => {
                    map.insert(key.clone(), Edit::Set(serde_json::Value::String(t)));
                }
            }
            self.gp.opt.outcomes.remove(&key);
            self.gp.opt.result = None;
            self.gp.opt.confirm = None;
        }
    }

    pub(super) fn options_discard(&mut self, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        let keys: Vec<RowKey> = self.gp.opt.staged_for(g, &p).into_keys().collect();
        for k in keys {
            self.options_stage(k, None, cx);
        }
        self.gp.opt.staged.remove(&(g, p));
        self.gp.opt.preset_note = None;
        self.gp.opt.confirm = None;
    }

    /// Save the staged edits as one revision. `danger` options ask first (in the bar).
    pub(super) fn options_save(&mut self, live: Live, confirmed: bool, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        let staged = self.gp.opt.staged_for(g, &p);
        if staged.is_empty() || self.gp.opt.saving {
            return;
        }
        let dangers: Vec<String> = staged
            .keys()
            .filter_map(|k| {
                let s = self.gp.opt.surface(&k.0)?;
                let st = s.options.get(&k.1)?;
                st.def.danger.as_ref().map(|why| format!("{}: {why}", st.def.label_or(&k.1)))
            })
            .collect();
        if !dangers.is_empty() && !confirmed {
            self.gp.opt.confirm = Some(Confirm::Danger { lines: dangers, live });
            return;
        }
        let ops = ops_of(&staged);
        let message = self.gp.opt.preset_note.clone();
        self.gp.opt.saving = true;
        self.gp.opt.confirm = None;
        let pp = p.clone();
        self.call::<OptionsChange, _>(
            Request::Options(OptionsRequest::Set { product_id: g.pid(), profile: Some(p), ops, live, message, confirmed: !dangers.is_empty() && confirmed }),
            cx,
            move |this, r, cx| {
                this.gp.opt.saving = false;
                match r {
                    Ok(c) => this.options_saved(g, &pp, c, cx),
                    Err(e) => this.gp.opt.result = Some(SaveResult { error: Some(err_text(&e)), ..Default::default() }),
                }
            },
        );
    }

    fn options_saved(&mut self, g: Gid, p: &str, c: OptionsChange, cx: &mut Context<Self>) {
        let mut keep = BTreeMap::new();
        let staged = self.gp.opt.staged_for(g, p);
        for o in &c.outcomes {
            let k = (o.surface.clone(), o.id.clone());
            // A rejected value stays in the bar, so it can be fixed or discarded.
            if matches!(o.outcome, quiver_api::options::Outcome::Rejected { .. })
                && let Some(e) = staged.get(&k)
            {
                keep.insert(k.clone(), e.clone());
            }
            self.gp.opt.outcomes.insert(k, o.clone());
        }
        if keep.is_empty() {
            self.gp.opt.staged.remove(&(g, p.to_owned()));
        } else {
            self.gp.opt.staged.insert((g, p.to_owned()), keep);
        }
        self.gp.opt.preset_note = None;
        self.gp.opt.result = Some(SaveResult { summary: logic::outcome_summary(c.revision, &c.outcomes), live: c.live, warnings: c.warnings, error: None });
        self.options_reload(cx);
    }

    /// Restore an earlier revision (as a new one, live where the game runs).
    pub(super) fn options_restore(&mut self, revision: u32, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        self.gp.opt.confirm = None;
        self.call::<OptionsChange, _>(Request::Options(OptionsRequest::Restore { product_id: g.pid(), profile: Some(p), revision, confirmed: false }), cx, move |this, r, cx| match r {
            Ok(c) => {
                for o in &c.outcomes {
                    this.gp.opt.outcomes.insert((o.surface.clone(), o.id.clone()), o.clone());
                }
                let summary = if c.revision == 0 { format!("r{revision} has the same values; nothing to restore") } else { format!("Restored r{revision} · {}", logic::outcome_summary(c.revision, &c.outcomes)) };
                this.gp.opt.result = Some(SaveResult { summary: summary.clone(), live: c.live, warnings: c.warnings, error: None });
                this.show_toast(summary, cx);
                this.options_reload(cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    /// Put back the values a session launched with (after a crash, or "revert this session").
    pub(super) fn options_revert_session(&mut self, session: String, cx: &mut Context<Self>) {
        let Some((g, _)) = self.gp.opt.of.clone() else { return };
        self.gp.opt.confirm = None;
        self.call::<OptionsChange, _>(Request::Options(OptionsRequest::RevertSession { product_id: g.pid(), session: session.clone(), confirmed: false }), cx, move |this, r, cx| match r {
            Ok(c) => {
                let summary = if c.revision == 0 { format!("Session {session} changed nothing that's still set") } else { format!("Reverted session {session} · {}", logic::outcome_summary(c.revision, &c.outcomes)) };
                if this.gp.opt.crash.as_ref().is_some_and(|x| x.2.session == session) {
                    this.gp.opt.crash = None;
                }
                this.show_toast(summary, cx);
                this.options_reload(cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    /// Open the Options view on a surface (and option: its page, its picker).
    pub(super) fn options_jump(&mut self, surface: &str, option: Option<&str>) {
        self.gp.opt.view = ModsView::Options;
        self.gp.opt.surface = Some(surface.to_owned());
        if let Some(id) = option
            && let Some(page) = self.gp.opt.surface(surface).and_then(|s| logic::page_of(s, id))
        {
            self.gp.opt.page = Some(page);
            self.gp.opt.picker = Some((surface.to_owned(), id.to_owned()));
        } else {
            self.gp.opt.page = self.gp.opt.surface(surface).and_then(|s| s.pages.first()).map(|p| p.id.clone());
        }
    }

    /// Options events (folded into `Event` from `Reply::Options`) for the open game.
    pub fn options_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        let loaded = self.gp.opt.of.clone();
        match e {
            Event::OptionsChanged { product_id, profile, revision, source, changes } => {
                if loaded.as_ref() == Some(&(Gid::from(*product_id), profile.clone())) {
                    if source != "gui" {
                        let who = logic::who(source);
                        let labels = self.option_labels();
                        let what = changes.first().map(|c| format!(": {}", changes::pretty(&labels, c))).map(|c| format!("{c}{}", if changes.len() > 1 { format!(" (+{} more)", changes.len() - 1) } else { String::new() })).unwrap_or_default();
                        self.gp.opt.notice = Some(format!("r{revision} from {}{what}", who.phrase()));
                    }
                    self.options_reload(cx);
                }
            }
            Event::OptionsLive { product_id, surface, values, by, saved, .. } => {
                if loaded.as_ref().is_some_and(|(g, _)| g == product_id) {
                    for (id, v) in values {
                        let k = (surface.clone(), id.clone());
                        if *saved {
                            self.gp.opt.in_game.remove(&k);
                        } else {
                            self.gp.opt.in_game.insert(k, (v.clone(), by.clone()));
                        }
                    }
                }
            }
            // What someone else's change did in the running game (Sight, the CLI, an agent):
            // the rows say it too. The app's own saves show the reply's outcomes already.
            Event::OptionsOutcome { product_id, profile, by, outcomes, .. } if by != "gui" && loaded.as_ref() == Some(&(Gid::from(*product_id), profile.clone())) => {
                for o in outcomes {
                    self.gp.opt.outcomes.insert((o.surface.clone(), o.id.clone()), o.clone());
                }
            }
            Event::OptionsCrash { product_id, profile, note } => {
                self.gp.opt.crash = Some((Gid::from(*product_id), profile.clone(), note.clone()));
                if loaded.as_ref().is_some_and(|(g, _)| g == product_id) {
                    self.load_option_records(Gid::from(*product_id), cx);
                }
            }
            Event::ModChannel { product_id, .. } if loaded.as_ref().is_some_and(|(g, _)| g == product_id) => self.load_option_surfaces(Gid::from(*product_id), cx),
            _ => {}
        }
    }

    /// The game started or stopped: what reaches it live changes.
    pub fn options_game_state(&mut self, product_id: Gid, running: bool, cx: &mut Context<Self>) {
        if self.gp.opt.of.as_ref().is_none_or(|(g, _)| *g != product_id) {
            return;
        }
        if !running {
            self.gp.opt.in_game.clear();
        }
        self.load_option_surfaces(product_id, cx);
        // The game's processes are known a moment after it starts (the live channel needs them).
        if running {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |this, cx| this.load_option_surfaces(product_id, cx));
            })
            .detach();
        }
    }

    // -- controller and keyboard: body rows after the profile cards ------------------------

    /// The first body row after the profile cards (Vanilla, the profiles, "New profile").
    pub(super) fn options_body_base(&self) -> usize {
        2 + self.gp.profiles().len()
    }

    /// The unsaved bar's Save and Discard, when it shows.
    pub(super) fn options_bar_rows(&self) -> usize {
        match &self.gp.opt.of {
            Some((g, p)) if *g == self.gp.id && !self.gp.opt.staged_for(*g, p).is_empty() => 2,
            _ => 0,
        }
    }

    /// Body rows after the profile cards: the views, the bar's buttons, the Options rows.
    pub(in crate::ui::game) fn options_body_len(&self, _cx: &gpui::App) -> usize {
        let rows = if self.gp.opt.view == ModsView::Options { self.gp.opt.pad_rows.len() } else { 0 };
        ModsView::TABS.len() + self.options_bar_rows() + rows
    }

    pub(in crate::ui::game) fn options_body_activate(&mut self, j: usize, cx: &mut Context<Self>) {
        let tabs = ModsView::TABS.len();
        if j < tabs {
            self.gp.opt.view = ModsView::TABS[j];
            return;
        }
        let rows = if self.gp.opt.view == ModsView::Options { self.gp.opt.pad_rows.len() } else { 0 };
        let j = j - tabs;
        if j < rows {
            if let Some(k) = self.gp.opt.pad_rows.get(j).cloned() {
                self.options_pad_activate(k, cx);
            }
            return;
        }
        let j = j - rows;
        if j < self.options_bar_rows() {
            match j {
                0 => {
                    let (live, confirmed) = match &self.gp.opt.confirm {
                        Some(Confirm::Danger { live, .. }) => (*live, true),
                        _ => (Live::IfRunning, false),
                    };
                    self.options_save(live, confirmed, cx)
                }
                _ => self.options_discard(cx),
            }
        }
    }

    /// A on an option: flip a switch, take the next choice, step a number up (round to the
    /// start after the end), the next key; text opens for typing.
    fn options_pad_activate(&mut self, k: RowKey, cx: &mut Context<Self>) {
        let Some((g, p)) = self.gp.opt.of.clone() else { return };
        let Some(st) = self.gp.opt.surface(&k.0).and_then(|s| s.options.get(&k.1)).cloned() else { return };
        let draft = logic::draft_value(&st, self.gp.opt.staged_for(g, &p).get(&k));
        match logic::pad_next(&st, &draft, self.gp.opt.level) {
            Some(v) => self.options_stage(k, Some(Edit::Set(v)), cx),
            None => self.gp.opt.picker = Some(k),
        }
    }

    /// The Mods tab's view switch.
    pub(super) fn render_mods_views(&mut self, th: &Theme, id: Gid, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let profile = self.mods_profile();
        let staged = self.gp.opt.staged_for(id, &profile).len();
        let loaded = self.gp.opt.of.as_ref() == Some(&(id, profile.clone()));
        let changed: usize = if loaded { self.gp.opt.loaded_surfaces().iter().map(|s| s.options.values().filter(|o| logic::is_changed(o)).count()).sum() } else { 0 };
        let conflicts = if loaded { self.gp.opt.keys.ready().map(|k| k.conflicts.len()).unwrap_or(0) } else { 0 };
        let cur = self.gp.opt.view;
        let base = self.options_body_base();
        let focus: Vec<bool> = (0..ModsView::TABS.len()).map(|i| self.focused(crate::nav::Target::Body(base + i))).collect();
        div().flex().flex_wrap().items_center().gap(px(8.)).children(ModsView::TABS.into_iter().enumerate().map(|(i, v)| {
            let on = cur == v || (v == ModsView::Options && cur == ModsView::Source);
            let mut c = chip(th, Some(v.icon()), v.label(), on).id(("mods-view", i)).cursor_pointer().focus_ring(focus[i], th, 13.);
            c = match v {
                ModsView::Options if changed + staged > 0 => c.child(chip_count(th, changed, on)).when(staged > 0, |d| d.child(div().size(px(6.)).rounded_full().bg(th.warn))),
                ModsView::Keys if conflicts > 0 => c.child(div().text_color(th.bad).child(conflicts.to_string())),
                _ => c,
            };
            c.on_click(cx.listener(move |this, _, _, cx| {
                this.gp.opt.view = v;
                cx.notify();
            }))
        }))
    }

    /// The body of a Mods tab view other than the stack.
    pub(super) fn render_options_view(&mut self, th: &Theme, g: &crate::model::GameRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = g.id();
        self.options_ensure(id, cx);
        self.options_read_inputs(cx);
        match self.gp.opt.view {
            ModsView::Options => self.render_options_form(th, g, cx),
            ModsView::Changes => self.render_options_changes(th, g, cx),
            ModsView::Keys => self.render_options_keys(th, g, cx),
            ModsView::Source => self.render_options_source(th, g, cx),
            ModsView::Stack => div().into_any_element(),
        }
    }

    /// Demo mode: `QUIVER_DEMO_GLOBAL=options=form|changes|keys|source[,options-running]
    /// [,options-surface=KEY][,options-page=ID]` opens AC Origins' Mods tab on that view (for
    /// screenshots).
    pub fn start_options_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let flags: Vec<String> = std::env::var("QUIVER_DEMO_GLOBAL").map(|v| v.split(',').map(|s| s.trim().to_owned()).collect()).unwrap_or_default();
        let view = flags.iter().find_map(|f| f.strip_prefix("options=")).map(str::to_owned);
        let Some(view) = view else { return };
        let running = flags.iter().any(|f| f == "options-running");
        let in_game = flags.iter().any(|f| f == "options-in-game");
        let surface = flags.iter().find_map(|f| f.strip_prefix("options-page=")).map(str::to_owned);
        let pick = flags.iter().find_map(|f| f.strip_prefix("options-surface=")).map(str::to_owned);
        let level = flags.iter().any(|f| f == "options-advanced");
        let staged = flags.iter().any(|f| f == "options-unsaved");
        let save = flags.iter().any(|f| f == "options-save");
        let source_mod = flags.iter().find_map(|f| f.strip_prefix("options-source=")).unwrap_or("lb5k1yq").to_owned();
        let scroll: Option<f32> = flags.iter().find_map(|f| f.strip_prefix("options-scroll=")).and_then(|v| v.parse().ok());
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                if this.update(cx, |this, _| !this.games.is_empty()).unwrap_or(true) {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if running {
                    this.call::<u64, _>(Request::Play { game: quiver_api::GameRef::ubisoft(3539), profile: None }, cx, |_, _, _| {});
                }
                this.open_game(Gid::from(3539), cx);
                this.nav.tab = Tab::Mods;
                // The DLSS profile (the demo's default for Play), unless mods come from a daemon.
                if std::env::var_os("QUIVER_DEMO_DAEMON").is_none() {
                    this.select_profile(Gid::from(3539), Some("pd1s5xk".into()), cx);
                }
                this.gp.opt.view = match view.as_str() {
                    "changes" | "history" => ModsView::Changes,
                    "keys" => ModsView::Keys,
                    "source" => ModsView::Source,
                    _ => ModsView::Options,
                };
                if view == "source" {
                    this.gp.opt.source_mod = Some(source_mod.clone());
                }
                if level {
                    this.gp.opt.level = Level::Advanced;
                }
                cx.notify();
            });
            cx.background_executor().timer(Duration::from_millis(1200)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(k) = &pick {
                    this.options_jump(k, None);
                }
                if let Some(p) = &surface {
                    this.gp.opt.page = Some(p.clone());
                }
                if let Some(y) = scroll {
                    this.body_scroll.set_offset(gpui::point(px(0.), px(-y)));
                }
                if staged {
                    let key = (this.gp.opt.surface.clone().unwrap_or_else(|| "ld7s3vq/aco-dlss".into()), "fg.refresh_hz".to_owned());
                    this.options_stage(key, Some(Edit::Set(serde_json::json!(144))), cx);
                }
                if this.gp.opt.view == ModsView::Source
                    && source_mod == "lb5k1yq"
                    && let Some((g, _)) = this.gp.opt.of.clone()
                {
                    this.options_infer(g, "lb5k1yq".into(), "SkipIntro.ini".into(), cx);
                }
                cx.notify();
            });
            if save {
                cx.background_executor().timer(Duration::from_millis(600)).await;
                let _ = this.update(cx, |this, cx| {
                    // A second edit that waits for a moment (Present thread: swapchain creation).
                    let key = (this.gp.opt.surface.clone().unwrap_or_else(|| "ld7s3vq/aco-dlss".into()), "fg.present_thread".to_owned());
                    this.options_stage(key, Some(Edit::Set(serde_json::json!("0"))), cx);
                    this.options_save(Live::IfRunning, false, cx);
                });
            }
            if in_game {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let _ = this.update(cx, |this, _| {
                    if let Some(crate::source::Source::Demo(d)) = &this.source
                        && let Err(e) = d.options_in_game()
                    {
                        tracing::warn!("demo in-game change: {e:#}");
                    }
                });
            }
        })
        .detach();
    }

    /// The `quiver` command for what a view shows, for the CLI lines.
    pub(super) fn options_cli(&self, id: Gid, what: &str) -> SharedString {
        let p = self.mods_profile();
        let name = self.mods_profile_name(&p);
        format!("quiver mods {id} options {what} --profile {}", super::page::sh_quote(&name)).into()
    }
}
