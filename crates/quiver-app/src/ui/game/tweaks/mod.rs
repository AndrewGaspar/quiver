//! Quiver Tweaks in the app (design/TWEAKS.md §8.3, steer 13): **Settings → Tweaks** (the
//! global scope, with the machine's capabilities in the header) and the game page's
//! **Tweaks** tab (the game's own scope or one of its profiles). Both draw the platform
//! surface with the Options form's controls ([`Form::Tweaks`]), stage edits and save them as
//! one revision of the scope (`TweaksRequest::Set`, source `gui`). Rows say where each value comes from, where the
//! tweak can work at all, what the next launch does with it (and who implements it, with
//! "Use Quiver's" where a mod provides it) and what the last session showed.

pub mod logic;

use quiver_ui::depth::DepthExt;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::options::{Level, OpOutcome, OptionState, OptionsChange, OptionsRevision, Outcome, SurfaceState, same_value};
use quiver_api::tweaks::{Capabilities, Prefer, Scope, ScopeKind, TweaksPlan, TweaksRequest};

use super::options::form::{Form, banner, sec, tone_badge, tone_colors, when, who_badge};
use super::options::logic::{self as ol, Edit, RowKey, Tone};
use super::options::{FieldInput, SaveResult};
use super::page::Load;
use crate::app::Quiver;
use crate::err_text;
use crate::model::{GameRow, Gid};
use crate::nav::{Tab, Target};
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, SEMIBOLD, Size, button, card, chip, icon, mono, soft_shadow};

/// A panel under the form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    None,
    /// What the next launch gets (`TweaksRequest::Plan`).
    Explain,
    History,
}

/// A focusable control of a Tweaks screen, in focus order (controller and keyboard).
#[derive(Clone, Debug, PartialEq)]
pub enum TwTarget {
    /// The Tweaks tab's scope picker: this game (`None`) or a profile id.
    Scope(Option<String>),
    Preset(usize),
    Row(RowKey),
    Explain,
    History,
    Save,
    Discard,
}

#[derive(Default)]
pub struct TweaksUi {
    /// The scope the loaded data is for.
    pub scope: Option<Scope>,
    pub surface: Load<SurfaceState>,
    pub history: Load<Vec<OptionsRevision>>,
    /// The machine with the default Proton build (Settings' header).
    pub caps: Load<Capabilities>,
    /// The next launch of the game (and profile) shown.
    pub plan: Load<TweaksPlan>,
    pub panel: Panel,
    pub level: Level,
    /// The Tweaks tab's scope per game: `None` this game, else a profile id.
    pub pick: HashMap<Gid, Option<String>>,
    /// Unsaved edits per scope (`logic::scope_key`); they outlive page switches.
    pub staged: BTreeMap<String, BTreeMap<RowKey, Edit>>,
    /// Text fields per (scope key, tweak id).
    pub inputs: HashMap<(String, String), FieldInput>,
    /// The last save's outcome per tweak id (for the loaded scope).
    pub outcomes: HashMap<String, OpOutcome>,
    pub result: Option<SaveResult>,
    pub saving: bool,
    /// Danger tweaks staged: the lines asking for a second click.
    pub danger: Option<Vec<String>>,
    pub picker: Option<RowKey>,
    /// The preset the staged edits came from (the revision's message).
    pub preset_note: Option<String>,
    /// A change someone else made while the screen was open.
    pub notice: Option<String>,
    /// History: a restore waiting for its second click.
    pub restore: Option<u32>,
    /// The rows as last drawn (controller order).
    pub pad_rows: Vec<RowKey>,
}

impl TweaksUi {
    fn forget_loaded(&mut self) {
        let keep = TweaksUi {
            caps: std::mem::take(&mut self.caps),
            panel: self.panel,
            level: self.level,
            pick: std::mem::take(&mut self.pick),
            staged: std::mem::take(&mut self.staged),
            inputs: std::mem::take(&mut self.inputs),
            ..Default::default()
        };
        *self = keep;
    }

    fn key(&self) -> Option<String> {
        self.scope.as_ref().map(logic::scope_key)
    }

    pub fn staged_for(&self, scope: &Scope) -> BTreeMap<RowKey, Edit> {
        self.staged.get(&logic::scope_key(scope)).cloned().unwrap_or_default()
    }

    fn loaded(&self) -> Option<&SurfaceState> {
        self.surface.ready()
    }

    fn option(&self, id: &str) -> Option<&OptionState> {
        self.loaded().and_then(|s| s.options.get(id))
    }
}

/// The platform surface's key (rows are keyed like the Options view's).
fn surface_key() -> String {
    quiver_api::tweaks::SURFACE_KEY.to_owned()
}

impl Quiver {
    // -- scope and loading -----------------------------------------------------------------

    /// The scope the Tweaks tab shows for a game: its own, or the picked profile's (Ubisoft
    /// games only: other stores' games have no profiles).
    pub fn tweaks_game_scope(&self, id: Gid) -> Scope {
        match self.tw.pick.get(&id).cloned().flatten() {
            Some(p) if id.is_ubisoft() => Scope::Profile { game: id.0, profile: Some(p) },
            _ => Scope::Game { game: id.0 },
        }
    }

    /// Open the Tweaks tab of a game at a scope (`None`: this game).
    pub fn tweaks_open_tab(&mut self, id: Gid, profile: Option<String>, _cx: &mut Context<Self>) {
        self.tw.pick.insert(id, profile);
        self.nav.select_tab(Tab::Tweaks);
    }

    /// A profile's display name (`vanilla` → Vanilla), for the open game.
    fn tweaks_profile_name(&self, scope: &Scope) -> Option<String> {
        match scope {
            Scope::Profile { profile, .. } => {
                let p = profile.clone().unwrap_or_else(|| quiver_api::mods::VANILLA.into());
                Some(self.gp.profiles().into_iter().find(|x| x.id == p).map(|x| x.name).unwrap_or_else(|| if p == quiver_api::mods::VANILLA { "Vanilla".into() } else { p }))
            }
            _ => None,
        }
    }

    /// Load what a scope's screen needs, once.
    fn tweaks_ensure(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.tw.scope.as_ref() != Some(&scope) {
            self.tw.forget_loaded();
            self.tw.scope = Some(scope);
            self.tweaks_reload(cx);
        }
        if self.tw.caps.is_idle() {
            self.tw.caps = Load::Loading;
            self.call::<Capabilities, _>(Request::Tweaks(TweaksRequest::Capabilities { game: None }), cx, |this, r, _| {
                this.tw.caps = match r {
                    Ok(c) => Load::Ready(c),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            });
        }
        self.tweaks_read_inputs(cx);
    }

    /// Reload the loaded scope's surface, history and plan.
    fn tweaks_reload(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.tw.scope.clone() else { return };
        if self.tw.surface.ready().is_none() {
            self.tw.surface = Load::Loading;
        }
        let s = scope.clone();
        self.call::<SurfaceState, _>(Request::Tweaks(TweaksRequest::List { scope: scope.clone(), level: None }), cx, move |this, r, cx| {
            if this.tw.scope.as_ref() != Some(&s) {
                return;
            }
            match r {
                Ok(st) => {
                    this.tw.surface = Load::Ready(st);
                    this.tweaks_sync_inputs(cx);
                }
                Err(e) => this.tw.surface = Load::Failed(err_text(&e)),
            }
        });
        let s = scope.clone();
        self.call::<Vec<OptionsRevision>, _>(Request::Tweaks(TweaksRequest::History { scope: scope.clone() }), cx, move |this, r, _| {
            if this.tw.scope.as_ref() == Some(&s) {
                this.tw.history = match r {
                    Ok(h) => Load::Ready(h),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
        let profile = match &scope {
            Scope::Profile { profile, .. } => profile.clone(),
            _ => None,
        };
        if let Some(game) = scope.game() {
            let s = scope.clone();
            if self.tw.plan.ready().is_none() {
                self.tw.plan = Load::Loading;
            }
            self.call::<TweaksPlan, _>(Request::Tweaks(TweaksRequest::Plan { game, profile }), cx, move |this, r, _| {
                if this.tw.scope.as_ref() == Some(&s) {
                    this.tw.plan = match r {
                        Ok(p) => Load::Ready(p),
                        Err(e) => Load::Failed(err_text(&e)),
                    };
                }
            });
        }
    }

    // -- staging, text fields, saving ---------------------------------------------------------

    pub fn tweaks_unsaved(&self, scope: &Scope) -> bool {
        self.tw.staged.get(&logic::scope_key(scope)).is_some_and(|m| !m.is_empty())
    }

    /// Stage an edit (or drop it when it puts back the saved value) and show it in the row's
    /// text field, if it has one.
    pub fn tweaks_stage(&mut self, key: RowKey, edit: Option<Edit>, cx: &mut Context<Self>) {
        let Some(sk) = self.tw.key() else { return };
        let st = self.tw.option(&key.1).cloned();
        let edit = match (edit, &st) {
            (Some(Edit::Set(v)), Some(st)) if st.def.normalize(&v).is_ok_and(|n| same_value(&n, &st.value)) && (logic::set_here(st) || logic::inherited_from(st).is_none()) => None,
            (Some(Edit::Reset), Some(st)) if !logic::set_here(st) => None,
            (e, _) => e,
        };
        let map = self.tw.staged.entry(sk.clone()).or_default();
        match &edit {
            Some(e) => {
                map.insert(key.clone(), e.clone());
            }
            None => {
                map.remove(&key);
            }
        }
        self.tw.outcomes.remove(&key.1);
        if let (Some(st), Some(f)) = (st, self.tw.inputs.get_mut(&(sk, key.1.clone()))) {
            let text = st.def.to_text(&ol::draft_value(&st, edit.as_ref()));
            if f.seen != text {
                f.seen = text.clone();
                f.input.update(cx, |t, cx| t.set_text(text, cx));
            }
        }
        self.tw.result = None;
        self.tw.danger = None;
    }

    /// A text field for a tweak (created once per scope, showing `text`).
    pub(in crate::ui) fn tweaks_input(&mut self, key: &RowKey, text: &str, placeholder: &str, cx: &mut Context<Self>) -> Entity<TextInput> {
        let Some(sk) = self.tw.key() else { return self.new_option_input(text, placeholder, cx) };
        let k = (sk, key.1.clone());
        if let Some(f) = self.tw.inputs.get(&k) {
            return f.input.clone();
        }
        let input = self.new_option_input(text, placeholder, cx);
        self.tw.inputs.insert(k, FieldInput { input: input.clone(), seen: text.to_owned() });
        input
    }

    /// Fields without an unsaved edit show the saved value again.
    fn tweaks_sync_inputs(&mut self, cx: &mut Context<Self>) {
        let Some(sk) = self.tw.key() else { return };
        let staged = self.tw.staged.get(&sk).cloned().unwrap_or_default();
        let Some(s) = self.tw.loaded().cloned() else { return };
        for ((isk, id), f) in self.tw.inputs.iter_mut() {
            if *isk != sk || staged.contains_key(&(s.key.clone(), id.clone())) {
                continue;
            }
            let Some(st) = s.options.get(id) else { continue };
            let text = st.def.to_text(&st.value);
            if f.seen != text {
                f.seen = text.clone();
                f.input.update(cx, |t, cx| t.set_text(text, cx));
            }
        }
    }

    /// Typing in a field stages the text (the daemon parses and validates it).
    fn tweaks_read_inputs(&mut self, cx: &mut Context<Self>) {
        let Some(sk) = self.tw.key() else { return };
        let mut typed: Vec<(String, String)> = Vec::new();
        for ((isk, id), f) in self.tw.inputs.iter_mut() {
            if *isk != sk {
                continue;
            }
            let t = f.input.read(cx).text().to_owned();
            if t != f.seen {
                f.seen = t.clone();
                typed.push((id.clone(), t));
            }
        }
        for (id, t) in typed {
            let key = (surface_key(), id.clone());
            let saved = self.tw.option(&id).map(|st| st.def.to_text(&st.value));
            let map = self.tw.staged.entry(sk.clone()).or_default();
            match saved {
                Some(s) if t.trim() == s.trim() => {
                    map.remove(&key);
                }
                _ => {
                    map.insert(key, Edit::Set(serde_json::Value::String(t)));
                }
            }
            self.tw.outcomes.remove(&id);
            self.tw.result = None;
        }
    }

    fn tweaks_discard(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.tw.scope.clone() else { return };
        for k in self.tw.staged_for(&scope).into_keys() {
            self.tweaks_stage(k, None, cx);
        }
        self.tw.staged.remove(&logic::scope_key(&scope));
        self.tw.preset_note = None;
        self.tw.danger = None;
    }

    /// Save the staged edits as one revision of the scope. `danger` tweaks ask first.
    fn tweaks_save(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.tw.scope.clone() else { return };
        let staged = self.tw.staged_for(&scope);
        if staged.is_empty() || self.tw.saving {
            return;
        }
        let dangers: Vec<String> =
            staged.keys().filter_map(|k| self.tw.option(&k.1).and_then(|st| st.def.danger.as_ref().map(|why| format!("{}: {why}", st.def.label_or(&k.1))))).collect();
        if !dangers.is_empty() && !confirmed {
            self.tw.danger = Some(dangers);
            return;
        }
        let ops = ol::ops_of(&staged);
        let message = self.tw.preset_note.clone();
        self.tw.saving = true;
        self.tw.danger = None;
        let s = scope.clone();
        self.call::<OptionsChange, _>(Request::Tweaks(TweaksRequest::Set { session: None, scope, ops, message, confirmed: !dangers.is_empty() && confirmed }), cx, move |this, r, cx| {
            this.tw.saving = false;
            if this.tw.scope.as_ref() != Some(&s) {
                return;
            }
            match r {
                Ok(c) => this.tweaks_saved(&s, c, cx),
                Err(e) => this.tw.result = Some(SaveResult { error: Some(err_text(&e)), ..Default::default() }),
            }
        });
    }

    fn tweaks_saved(&mut self, scope: &Scope, c: OptionsChange, cx: &mut Context<Self>) {
        let staged = self.tw.staged_for(scope);
        let mut keep = BTreeMap::new();
        for o in &c.outcomes {
            let k = (surface_key(), o.id.clone());
            // A rejected value stays in the bar, so it can be fixed or discarded.
            if matches!(o.outcome, Outcome::Rejected { .. })
                && let Some(e) = staged.get(&k)
            {
                keep.insert(k, e.clone());
            }
            self.tw.outcomes.insert(o.id.clone(), o.clone());
        }
        let sk = logic::scope_key(scope);
        if keep.is_empty() {
            self.tw.staged.remove(&sk);
        } else {
            self.tw.staged.insert(sk, keep);
        }
        self.tw.preset_note = None;
        let changed = c.revision > 0;
        self.tw.result = Some(SaveResult { summary: ol::outcome_summary(c.revision, &c.outcomes), live: c.live, warnings: c.warnings, error: None });
        self.tweaks_reload(cx);
        // A Steam game that isn't hooked doesn't get them: offer the hook (steer 3b).
        if let Scope::Game { game } = scope
            && changed
            && game.as_steam().is_some()
        {
            self.steam_hook_offer(Gid(*game), "these tweaks", cx);
        }
    }

    /// Who implements a tweak a mod provides, at the loaded scope.
    fn tweaks_prefer(&mut self, id: String, prefer: Prefer, cx: &mut Context<Self>) {
        let Some(scope) = self.tw.scope.clone() else { return };
        self.call::<OptionsChange, _>(Request::Tweaks(TweaksRequest::Prefer { scope, id, prefer, message: None }), cx, |this, r, cx| match r {
            Ok(c) => {
                let what = c.changes.first().cloned().unwrap_or_else(|| "Unchanged".into());
                this.show_toast(if c.revision > 0 { format!("r{}: {what}", c.revision) } else { what }, cx);
                this.tweaks_reload(cx);
            }
            Err(e) => this.show_error(&e, cx),
        });
    }

    fn tweaks_restore(&mut self, revision: u32, cx: &mut Context<Self>) {
        let Some(scope) = self.tw.scope.clone() else { return };
        self.tw.restore = None;
        self.call::<OptionsChange, _>(Request::Tweaks(TweaksRequest::Restore { scope, revision }), cx, move |this, r, cx| match r {
            Ok(c) => {
                let summary = if c.revision == 0 { format!("r{revision} has the same values; nothing to restore") } else { format!("Restored r{revision} as r{}", c.revision) };
                this.tw.result = Some(SaveResult { summary: summary.clone(), warnings: c.warnings, ..Default::default() });
                this.show_toast(summary, cx);
                this.tweaks_reload(cx);
            }
            Err(e) => this.show_error(&e, cx),
        });
    }

    fn tweaks_apply_preset(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(p) = self.tw.loaded().and_then(|s| s.presets.get(i)).cloned() else { return };
        for (id, v) in &p.set {
            self.tweaks_stage((surface_key(), id.clone()), Some(Edit::Set(v.clone())), cx);
        }
        self.tw.preset_note = Some(format!("Preset: {}", p.label));
    }

    /// Tweaks events: a scope changed (reload if it reaches the loaded one), an effect
    /// arrived for the loaded game.
    pub fn tweaks_event(&mut self, e: &quiver_api::Event, cx: &mut Context<Self>) {
        let Some(loaded) = self.tw.scope.clone() else { return };
        match e {
            quiver_api::Event::TweaksChanged { scope, revision, source, changes } => {
                let reaches = *scope == Scope::Global || (scope.game().is_some() && scope.game() == loaded.game() && scope.kind() <= loaded.kind()) || *scope == loaded;
                if !reaches {
                    return;
                }
                if source != "gui" {
                    let who = ol::who(source).phrase();
                    let what = changes.first().map(|c| format!(": {c}")).unwrap_or_default();
                    let more = if changes.len() > 1 { format!(" (+{} more)", changes.len() - 1) } else { String::new() };
                    self.tw.notice = Some(format!("{} r{revision} from {who}{what}{more}", logic::scope_name(scope.kind(), None)));
                }
                self.tweaks_reload(cx);
            }
            quiver_api::Event::TweaksEffect { game, .. } if loaded.game() == Some(*game) => self.tweaks_reload(cx),
            _ => {}
        }
    }

    // -- controller ---------------------------------------------------------------------------

    /// The focusable controls of the Tweaks screen for `game` (`None`: Settings → Tweaks).
    pub fn tweaks_targets(&self, game: Option<Gid>) -> Vec<TwTarget> {
        let mut v = Vec::new();
        if let Some(id) = game
            && id.is_ubisoft()
        {
            v.push(TwTarget::Scope(None));
            if self.gp.id == id && self.gp.mods.ready().is_some() {
                v.push(TwTarget::Scope(Some(quiver_api::mods::VANILLA.into())));
                v.extend(self.gp.profiles().into_iter().map(|p| TwTarget::Scope(Some(p.id))));
            }
        }
        let n = self.tw.loaded().map_or(0, |s| s.presets.len());
        v.extend((0..n).map(TwTarget::Preset));
        v.extend(self.tw.pad_rows.iter().cloned().map(TwTarget::Row));
        if game.is_some() {
            v.push(TwTarget::Explain);
        }
        v.push(TwTarget::History);
        if self.tw.scope.as_ref().is_some_and(|s| self.tweaks_unsaved(s)) {
            v.extend([TwTarget::Save, TwTarget::Discard]);
        }
        v
    }

    fn tw_focused(&self, game: Option<Gid>, t: &TwTarget) -> bool {
        self.tweaks_targets(game).iter().position(|x| x == t).is_some_and(|i| self.focused(Target::Body(i)))
    }

    pub fn tweaks_activate(&mut self, game: Option<Gid>, i: usize, cx: &mut Context<Self>) {
        let Some(t) = self.tweaks_targets(game).get(i).cloned() else { return };
        match t {
            TwTarget::Scope(p) => {
                if let Some(id) = game {
                    self.tw.pick.insert(id, p);
                }
            }
            TwTarget::Preset(i) => self.tweaks_apply_preset(i, cx),
            TwTarget::Row(k) => {
                let Some(st) = self.tw.option(&k.1).cloned() else { return };
                let staged = self.tw.scope.as_ref().map(|s| self.tw.staged_for(s)).unwrap_or_default();
                let draft = ol::draft_value(&st, staged.get(&k));
                match ol::pad_next(&st, &draft, self.tw.level) {
                    Some(v) => self.tweaks_stage(k, Some(Edit::Set(v)), cx),
                    None => self.tw.picker = Some(k),
                }
            }
            TwTarget::Explain => self.tw.panel = if self.tw.panel == Panel::Explain { Panel::None } else { Panel::Explain },
            TwTarget::History => self.tw.panel = if self.tw.panel == Panel::History { Panel::None } else { Panel::History },
            TwTarget::Save => {
                let confirmed = self.tw.danger.is_some();
                self.tweaks_save(confirmed, cx)
            }
            TwTarget::Discard => self.tweaks_discard(cx),
        }
    }

    // -- screens ------------------------------------------------------------------------------

    /// Settings → Tweaks: the global scope.
    pub fn settings_tweaks(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.tweaks_ensure(Scope::Global, cx);
        let caps = match &self.tw.caps {
            Load::Ready(c) => Some(c.clone()),
            _ => None,
        };
        let mut out = vec![
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_size(px(20.)).font_weight(gpui::FontWeight::BOLD).child("Tweaks"))
                .child(div().text_color(th.tx3).child(
                    "Overrides Quiver applies to every game it launches: frame rate, sync, latency, DLSS and frame generation. Each starts at the game's choice; a game's Tweaks tab, and its profiles, can change any of them.",
                ))
                .into_any_element(),
            self.render_caps_card(th, caps.as_ref(), None),
        ];
        out.extend(self.render_tweaks_body(th, None, cx));
        out
    }

    /// The machine's capabilities: the summary line and the components' chips.
    fn render_caps_card(&self, th: &Theme, caps: Option<&Capabilities>, title: Option<&str>) -> AnyElement {
        let (summary, chips, notes) = match (caps, &self.tw.caps) {
            (Some(c), _) => (c.summary(), logic::caps_chips(c), c.notes.clone()),
            (None, Load::Failed(e)) => (format!("Capabilities unknown: {e}"), Vec::new(), Vec::new()),
            _ => ("Probing the GPU, driver and Proton build…".into(), Vec::new(), Vec::new()),
        };
        card(th)
            .px(px(16.))
            .py(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex().flex_none().items_center().justify_center().size(px(30.)).rounded(px(8.)).bg(th.ink4).child(icon(Icon::Cpu, 15., th.tx2)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(200.))
                            .child(div().text_size(px(11.)).text_color(th.tx3).child(title.unwrap_or("This machine, with the default Proton build").to_owned()))
                            .child(div().font_weight(SEMIBOLD).child(summary)),
                    )
                    .children(chips.into_iter().map(|(c, tone)| tone_badge(th, c, tone))),
            )
            .children(notes.into_iter().map(|n| div().text_size(px(12.)).text_color(th.tx3).child(n)))
            .into_any_element()
    }

    /// The game page's Tweaks tab.
    pub(in crate::ui) fn render_tweaks_tab(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let scope = self.tweaks_game_scope(id);
        // A profile that went away: back to the game's scope.
        if let Scope::Profile { profile: Some(p), .. } = &scope
            && self.gp.id == id
            && self.gp.mods.ready().is_some()
            && p != quiver_api::mods::VANILLA
            && !self.gp.profiles().iter().any(|x| &x.id == p)
        {
            self.tw.pick.insert(id, None);
        }
        let scope = self.tweaks_game_scope(id);
        self.tweaks_ensure(scope.clone(), cx);
        let name = crate::model::clean_name(&g.entry.name);
        let caps = self.tw.plan.ready().map(|p| p.caps.clone()).or_else(|| self.tw.caps.ready().cloned());
        let mut out: Vec<AnyElement> = Vec::new();

        // Header: what this is, the capabilities, the running game.
        let running = g.running().then(|| {
            let p = self.default_profile(id).map(|p| self.gp.profiles().into_iter().find(|x| x.id == p).map(|x| x.name).unwrap_or(p));
            format!("Running{}", p.map(|p| format!(" · {p}")).unwrap_or_default())
        });
        out.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::BOLD).child(format!("Tweaks · {name}")))
                .child(div().text_size(px(12.5)).text_color(th.tx3).child(if id.is_ubisoft() {
                    "Applied at launch: global values, then this game's, then the profile's; the most specific wins"
                } else {
                    "Applied at launch: global values, then this game's"
                }))
                .child(div().flex_1())
                .when_some(running, |d, r| d.child(tone_badge(th, r.to_uppercase(), Tone::Ok)))
                .into_any_element(),
        );
        out.push(self.render_caps_card(th, caps.as_ref(), Some(if id.is_ubisoft() { "This game's next launch" } else { "This machine" })));
        if id.is_ubisoft() {
            out.push(self.render_scope_picker(th, id, &scope, cx));
        } else if id.0.as_steam().is_some() {
            out.push(self.render_steam_hook_banner(th, g, cx));
        } else {
            out.push(
                banner(
                    th,
                    Tone::Accent,
                    Icon::Info,
                    format!("Tweaks apply to games Quiver launches. {name} starts through its store's client, so it gets these when Quiver runs it (quiver steam-run, or Steam set to launch through Quiver); started by Steam directly, it gets none. Steam games have one scope: the game."),
                )
                .into_any_element(),
            );
        }
        out.extend(self.render_tweaks_body(th, Some(id), cx));
        let pad = if self.tweaks_unsaved(&scope) { 80. } else { 0. };
        div().flex().flex_col().gap(px(14.)).pb(px(pad)).children(out).into_any_element()
    }

    /// This game | Vanilla | profiles: the scope the tab edits.
    fn render_scope_picker(&mut self, th: &Theme, id: Gid, scope: &Scope, cx: &mut Context<Self>) -> AnyElement {
        let mut items: Vec<(Option<String>, String)> = vec![(None, "This game".into())];
        let have_mods = self.gp.id == id && self.gp.mods.ready().is_some();
        if have_mods {
            items.push((Some(quiver_api::mods::VANILLA.into()), "Vanilla".into()));
            items.extend(self.gp.profiles().into_iter().map(|p| (Some(p.id), p.name)));
        }
        let cur: Option<String> = match scope {
            Scope::Profile { profile, .. } => profile.clone(),
            _ => None,
        };
        let plays = self.default_profile(id).unwrap_or_else(|| quiver_api::mods::VANILLA.into());
        let segs: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, (p, label))| {
                let on = p == cur;
                let focused = self.tw_focused(Some(id), &TwTarget::Scope(p.clone()));
                let is_play = have_mods && p.as_deref() == Some(plays.as_str());
                let ic = match &p {
                    None => Icon::Gamepad,
                    Some(v) if v == quiver_api::mods::VANILLA => Icon::ShieldCheck,
                    Some(_) => Icon::Layers,
                };
                div()
                    .id(("tw-scope", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .h(px(30.))
                    .rounded(px(8.))
                    .text_size(px(12.5))
                    .font_weight(if on { SEMIBOLD } else { MEDIUM })
                    .text_color(if on { th.acc_ink } else { th.tx2 })
                    .when(on, |d| d.bg(th.acc))
                    .when(!on, |d| d.hover(|s| s.bg(th.ink4)))
                    .cursor_pointer()
                    .focus_ring(focused, th, 8.)
                    .child(icon(ic, 13., if on { th.acc_ink } else { th.tx3 }))
                    .child(label)
                    .when(is_play, |d| d.child(div().size(px(6.)).rounded_full().bg(if on { th.acc_ink } else { th.ok })))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tw.pick.insert(id, p.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let help = match scope {
            Scope::Profile { .. } => format!(
                "Only launches with the {} profile. Its values win over this game's and the global ones; a mod in it may implement a tweak itself.",
                self.tweaks_profile_name(scope).unwrap_or_default()
            ),
            _ => "Every launch of this game, whichever profile it plays. Values here win over the global ones; a profile can override them.".into(),
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(div().text_size(px(12.)).text_color(th.tx3).child("Scope"))
            .child(div().flex().flex_wrap().gap(px(2.)).p(px(3.)).rounded(px(10.)).bg(th.ink2).border_1().border_color(th.line2).children(segs))
            .when(have_mods, |d| d.child(div().flex().items_center().gap(px(5.)).text_size(px(11.5)).text_color(th.tx3).child(div().size(px(6.)).rounded_full().bg(th.ok)).child("plays by default")))
            .child(div().w_full().text_size(px(12.)).text_color(th.tx3).child(help))
            .into_any_element()
    }

    /// Banners, presets and level, the pages as cards, the panels: shared by both screens.
    fn render_tweaks_body(&mut self, th: &Theme, game: Option<Gid>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        let s = match &self.tw.surface {
            Load::Ready(s) => s.clone(),
            Load::Failed(e) => {
                out.push(card(th).p(px(20.)).text_color(th.bad).child(format!("Couldn't load tweaks: {e}")).into_any_element());
                return out;
            }
            _ => {
                out.push(card(th).p(px(20.)).text_color(th.tx3).child("Loading tweaks…").into_any_element());
                return out;
            }
        };
        let Some(scope) = self.tw.scope.clone() else { return out };
        let staged = self.tw.staged_for(&scope);
        let profile = self.tweaks_profile_name(&scope);
        let level = self.tw.level;

        // Banners: a change from elsewhere, the last save, the plan's warnings.
        if let Some(n) = self.tw.notice.clone() {
            out.push(
                banner(th, Tone::Accent, Icon::RefreshCw, format!("Updated while you were here: {n}"))
                    .child(button("tw-notice-x", th, Kind::Ghost, Size::Small, Some(Icon::X), "").on_click(cx.listener(|this, _, _, cx| {
                        this.tw.notice = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some(r) = self.tw.result.clone() {
            match &r.error {
                Some(e) => out.push(banner(th, Tone::Bad, Icon::CircleX, format!("Not saved: {e}")).into_any_element()),
                None => {
                    let tone = if r.summary.contains("rejected") { Tone::Warn } else { Tone::Ok };
                    let next = if game.is_some() { ". Tweaks apply at the next launch" } else { ". Games get it at their next launch" };
                    out.push(banner(th, tone, Icon::CircleCheck, format!("{}{next}", r.summary)).into_any_element());
                    for w in &r.warnings {
                        out.push(banner(th, Tone::Warn, Icon::TriangleAlert, w.clone()).into_any_element());
                    }
                }
            }
        }
        for w in &s.lint {
            out.push(banner(th, Tone::Warn, Icon::TriangleAlert, w.clone()).into_any_element());
        }

        // Presets and the level switch.
        let presets: Vec<AnyElement> = s
            .presets
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let focused = self.tw_focused(game, &TwTarget::Preset(i));
                let mut c = chip(th, Some(Icon::Sparkles), p.label.clone(), false).id(("tw-preset", i)).cursor_pointer().focus_ring(focused, th, 13.);
                if let Some(h) = p.help.clone() {
                    c = c.tooltip(crate::ui::responsive::tip(th, h));
                }
                c.on_click(cx.listener(move |this, _, _, cx| {
                    this.tweaks_apply_preset(i, cx);
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        let levels = div().flex().items_center().gap(px(2.)).p(px(2.)).rounded_full().bg(th.ink3).border_1().border_color(th.line2).children([(Level::Basic, "Basic"), (Level::Advanced, "Advanced")].into_iter().map(|(l, label)| {
            let on = level == l;
            div()
                .id(SharedString::from(format!("tw-lvl-{label}")))
                .px(px(10.))
                .h(px(24.))
                .flex()
                .items_center()
                .rounded_full()
                .text_size(px(12.))
                .font_weight(if on { SEMIBOLD } else { MEDIUM })
                .text_color(if on { th.tx1 } else { th.tx3 })
                .when(on, |d| d.bg(th.ink5))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tw.level = l;
                    cx.notify();
                }))
        }));
        let set_n = logic::set_count(&s);
        let here = logic::scope_name(scope.kind(), profile.as_deref());
        out.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(div().text_size(px(12.5)).text_color(th.tx3).child("Presets"))
                .children(presets)
                .child(div().flex_1())
                .child(div().text_size(px(12.)).text_color(th.tx3).child(match set_n {
                    0 => format!("{here}: nothing set"),
                    1 => format!("{here}: 1 tweak set"),
                    n => format!("{here}: {n} tweaks set"),
                }))
                .child(levels)
                .into_any_element(),
        );

        // The pages.
        let hist = self.tw.history.ready().cloned().unwrap_or_default();
        let wide = self.tweaks_wide(game);
        self.tw.pad_rows.clear();
        let mut hidden = 0usize;
        for p in &s.pages {
            let mut groups: Vec<AnyElement> = Vec::new();
            for (gi, gr) in p.groups.iter().enumerate() {
                let mut rows: Vec<AnyElement> = Vec::new();
                for oid in &gr.options {
                    let Some(st) = s.options.get(oid) else { continue };
                    if !ol::at_level(&st.def, level) {
                        hidden += 1;
                        continue;
                    }
                    if !ol::visible_with(&s, &staged, &st.def) {
                        continue;
                    }
                    let key = (s.key.clone(), oid.clone());
                    rows.push(self.tweak_row(th, game, &s, oid, st, &staged, &hist, profile.as_deref(), wide, cx));
                    self.tw.pad_rows.push(key);
                }
                if rows.is_empty() {
                    continue;
                }
                if !gr.title.is_empty() {
                    groups.push(sec(th, gr.title.clone()).pt(px(if gi == 0 { 4. } else { 12. })).into_any_element());
                }
                groups.push(div().flex().flex_col().children(rows).into_any_element());
            }
            if groups.is_empty() {
                continue;
            }
            let head = div()
                .flex()
                .items_center()
                .gap(px(20.))
                .pt(px(12.))
                .pb(px(8.))
                .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child(p.title.clone()))
                .child(div().flex_1())
                .when(wide && game.is_some(), |d| d.child(div().w(px(300.)).flex_none().child(sec(th, "Value"))).child(div().w(px(170.)).flex_none().child(sec(th, "Last session"))));
            out.push(card(th).px(px(16.)).pt(px(2.)).pb(px(4.)).child(head).children(groups).into_any_element());
        }
        if hidden > 0 && level == Level::Basic {
            out.push(
                div()
                    .id("tw-more")
                    .text_size(px(12.))
                    .text_color(th.acc_hi)
                    .cursor_pointer()
                    .child(format!("{hidden} more advanced tweak{}: show them", if hidden == 1 { "" } else { "s" }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tw.level = Level::Advanced;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        // Footer: what this scope changes, Explain, History.
        let changes: Vec<String> = s
            .options
            .iter()
            .filter(|(_, st)| logic::set_here(st))
            .map(|(id, st)| format!("{} = {}", st.def.label_or(id), logic::value_label(&st.def, &st.value)))
            .collect();
        let what_scope = match (scope.kind(), &profile) {
            (ScopeKind::Global, _) => "the global scope".to_owned(),
            (ScopeKind::Game, _) => "this game".to_owned(),
            (ScopeKind::Profile, p) => format!("the {} profile", p.clone().unwrap_or_default()),
        };
        let explain_on = self.tw.panel == Panel::Explain;
        let history_on = self.tw.panel == Panel::History;
        let fe = self.tw_focused(game, &TwTarget::Explain);
        let fh = self.tw_focused(game, &TwTarget::History);
        out.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(220.))
                        .text_size(px(12.5))
                        .text_color(th.tx2)
                        .child(if changes.is_empty() { format!("What {what_scope} changes: nothing") } else { format!("What {what_scope} changes ({}): {}", changes.len(), changes.join(" · ")) }),
                )
                .when(game.is_some(), |d| {
                    d.child(
                        button("tw-explain", th, if explain_on { Kind::Primary } else { Kind::Secondary }, Size::Small, Some(Icon::SquareTerminal), "Explain next launch")
                            .focus_ring(fe, th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tw.panel = if this.tw.panel == Panel::Explain { Panel::None } else { Panel::Explain };
                                cx.notify();
                            })),
                    )
                })
                .child(
                    button("tw-history", th, if history_on { Kind::Primary } else { Kind::Secondary }, Size::Small, Some(Icon::History), format!("History ({})", hist.len()))
                        .focus_ring(fh, th, 6.)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tw.panel = if this.tw.panel == Panel::History { Panel::None } else { Panel::History };
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );
        match self.tw.panel {
            Panel::Explain if game.is_some() => out.push(self.render_tweaks_explain(th, &s, cx)),
            Panel::History => out.push(self.render_tweaks_history(th, &hist, cx)),
            _ => {}
        }
        let cmd = match game {
            None => "quiver tweaks list --global".to_owned(),
            Some(_) => format!("quiver tweaks list {}", logic::cli_scope(&scope, profile.as_deref())),
        };
        out.push(div().flex().child(self.cli_line("tw-cli", th, cmd, cx)).into_any_element());
        out
    }

    /// Room for a Last session column next to the control.
    fn tweaks_wide(&self, game: Option<Gid>) -> bool {
        match game {
            Some(_) => self.pane_bp() >= crate::ui::responsive::Bp::Large,
            None => self.bp() >= crate::ui::responsive::Bp::Large,
        }
    }

    /// One tweak: label, chips, help, where the value comes from, what the next launch does;
    /// its control; the last session.
    #[allow(clippy::too_many_arguments)]
    fn tweak_row(
        &mut self,
        th: &Theme,
        game: Option<Gid>,
        s: &SurfaceState,
        oid: &str,
        st: &OptionState,
        staged: &BTreeMap<RowKey, Edit>,
        hist: &[OptionsRevision],
        profile: Option<&str>,
        wide: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key: RowKey = (s.key.clone(), oid.to_owned());
        let edit = staged.get(&key).cloned();
        let draft = ol::draft_value(st, edit.as_ref());
        let here = logic::set_here(st);
        let t = logic::row(st).cloned().unwrap_or_default();
        let label = st.def.label_or(oid).to_owned();

        // Left: what it is, where it can work, where its value comes from, the plan.
        let mut meta: Vec<AnyElement> = Vec::new();
        let (src, who) = logic::source_text(st, profile);
        if let Some(w) = &who {
            meta.push(who_badge(th, w).into_any_element());
        }
        let when_text = match &st.from {
            quiver_api::options::ValueSource::Scope { revision, .. } if here => when(hist, *revision).map(|w| format!(" · {w}")).unwrap_or_default(),
            _ => String::new(),
        };
        meta.push(div().child(format!("{src}{when_text}")).into_any_element());
        if let Some(o) = self.tw.outcomes.get(oid) {
            let (text, tone) = ol::outcome_text(&o.outcome);
            meta.push(tone_badge(th, text, tone).into_any_element());
        }
        let mut notes: Vec<AnyElement> = Vec::new();
        if let Some(o) = logic::overridden_text(&t, profile.or(self.tweaks_default_profile_name(game).as_deref())) {
            notes.push(div().flex().items_start().gap(px(6.)).text_color(th.warn).child(div().pt(px(2.)).flex_none().child(icon(Icon::TriangleAlert, 12., th.warn))).child(div().flex_1().min_w_0().child(o)).into_any_element());
        }
        if let Some(l) = t.planned.as_ref().and_then(|p| logic::planned_line(p, t.prefer)) {
            let fg = tone_colors(th, l.tone).1;
            let ic = match l.tone {
                Tone::Ok => Icon::CircleCheck,
                Tone::Accent => Icon::Layers,
                Tone::Bad => Icon::CircleX,
                Tone::Warn => Icon::TriangleAlert,
                Tone::Muted => Icon::Info,
            };
            let id = oid.to_owned();
            let text = div().flex_1().min_w_0().flex().flex_wrap().gap_x(px(6.)).child(div().child(l.text)).when_some(l.action, |d, (label, prefer)| {
                d.child(div().text_color(th.tx3).child("·")).child(
                    div().id(SharedString::from(format!("tw-prefer-{oid}"))).text_color(th.acc_hi).font_weight(MEDIUM).cursor_pointer().child(label).on_click(cx.listener(move |this, _, _, cx| {
                        this.tweaks_prefer(id.clone(), prefer, cx);
                        cx.notify();
                    })),
                )
            });
            notes.push(div().flex().items_start().gap(px(6.)).text_color(fg).child(div().pt(px(2.)).flex_none().child(icon(ic, 12., fg))).child(text).into_any_element());
        }
        if let Some(d) = &st.dormant {
            notes.push(div().text_color(th.warn).child(d.clone()).into_any_element());
        }
        let left = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(div().size(px(7.)).rounded_full().flex_none().bg(if edit.is_some() { th.warn } else if here { th.acc } else { gpui::transparent_black() }))
                    .child(div().text_size(px(14.)).font_weight(SEMIBOLD).child(label))
                    .children(t.chips.iter().map(|c| {
                        div().flex().flex_none().items_center().h(px(18.)).px(px(6.)).rounded(px(5.)).border_1().border_color(th.line2).text_size(px(10.5)).text_color(th.tx3).whitespace_nowrap().child(c.clone())
                    }))
                    .when(st.def.danger.is_some(), |d| d.child(tone_badge(th, "DANGER", Tone::Bad)))
                    .when(st.def.level != Level::Basic, |d| d.child(div().text_size(px(11.)).text_color(th.tx4).child(format!("{:?}", st.def.level).to_lowercase()))),
            )
            .when_some(st.def.help.clone(), |d, h| d.child(div().text_size(px(12.)).text_color(th.tx3).child(h)))
            .child(div().flex().flex_wrap().items_center().gap(px(6.)).text_size(px(11.5)).text_color(th.tx3).children(meta))
            .when(!notes.is_empty(), |d| d.child(div().flex().flex_col().gap(px(2.)).text_size(px(12.)).children(notes)))
            .child(mono(11., th.tx4).child(oid.to_owned()));

        // Middle: the control, then undo or reset.
        let control = self.option_control(Form::Tweaks, th, s, &key, st, &draft, cx);
        let mut under: Vec<AnyElement> = Vec::new();
        if let Some(Edit::Set(v)) = &edit
            && let Err(why) = st.def.normalize(v)
        {
            under.push(div().text_color(th.bad).child(why).into_any_element());
        }
        let k2 = key.clone();
        if edit.is_some() {
            let was = logic::value_label(&st.def, &st.value);
            under.push(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().text_color(th.warn).child(match edit {
                        Some(Edit::Reset) => format!("Unsaved reset · was {was}"),
                        _ => format!("Unsaved · was {was}"),
                    }))
                    .child(div().id(SharedString::from(format!("tw-undo-{oid}"))).text_color(th.acc_hi).cursor_pointer().child("Undo").on_click(cx.listener(move |this, _, _, cx| {
                        this.tweaks_stage(k2.clone(), None, cx);
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        } else if here {
            under.push(
                div()
                    .id(SharedString::from(format!("tw-reset-{oid}")))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_color(th.acc_hi)
                    .cursor_pointer()
                    .child(icon(Icon::RotateCcw, 12., th.acc_hi))
                    .child(logic::reset_text(st))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tweaks_stage(k2.clone(), Some(Edit::Reset), cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let middle = div()
            .w(px(300.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(control)
            .when(!under.is_empty(), |d| d.child(div().flex().flex_col().gap(px(2.)).text_size(px(12.)).text_color(th.tx3).children(under)));

        // Right: the last session (game scopes).
        let last = game.map(|_| match &t.last {
            Some(e) => {
                let (b, tone, detail) = logic::effect_badge(e);
                div().flex().flex_col().gap(px(3.)).child(div().flex().child(tone_badge(th, b, tone))).when_some(detail, |d, x| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(x)))
            }
            None => div().text_size(px(11.5)).text_color(th.tx4).child("—"),
        });
        let focused = self.tw_focused(game, &TwTarget::Row(key.clone()));
        let row = div().flex().gap(px(20.)).py(px(13.)).when(focused, |d| d.bg(th.acc_soft).px(px(8.)).rounded(px(8.))).border_t_1().border_color(th.line1);
        if wide {
            row.items_start().child(left).child(middle).when_some(last, |d, l| d.child(div().w(px(170.)).flex_none().child(l))).into_any_element()
        } else {
            let last = last.map(|l| div().flex().items_center().gap(px(8.)).child(div().text_size(px(11.)).text_color(th.tx3).child("Last session")).child(l));
            row.flex_col().items_start().child(left).child(middle).children(last).into_any_element()
        }
    }

    /// The profile Play runs, by name (for notices on the game's scope).
    fn tweaks_default_profile_name(&self, game: Option<Gid>) -> Option<String> {
        let id = game?;
        let p = self.default_profile(id)?;
        self.gp.profiles().into_iter().find(|x| x.id == p).map(|x| x.name)
    }

    /// Explain next launch: the capabilities, the exact environment, per tweak the bindings
    /// considered.
    fn render_tweaks_explain(&mut self, th: &Theme, s: &SurfaceState, cx: &mut Context<Self>) -> AnyElement {
        let plan = match &self.tw.plan {
            Load::Ready(p) => p.clone(),
            Load::Failed(e) => return card(th).p(px(16.)).text_color(th.bad).child(format!("Couldn't explain the next launch: {e}")).into_any_element(),
            _ => return card(th).p(px(16.)).text_color(th.tx3).child("Working out the next launch…").into_any_element(),
        };
        let scope = self.tw.scope.clone().unwrap_or_default();
        let profile = self.tweaks_profile_name(&scope);
        let r = plan.revisions;
        let mono_block = |lines: Vec<String>| div().flex().flex_col().gap(px(2.)).p(px(10.)).rounded(px(8.)).bg(th.ink1).border_1().border_color(th.line1).children(lines.into_iter().map(|l| mono(11.5, th.tx1).child(l)));
        let mut body: Vec<AnyElement> = vec![
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child("Next launch"))
                .child(div().text_size(px(12.)).text_color(th.tx3).child(format!(
                    "{} · {} · global r{} · game r{} · profile r{} · route {}",
                    plan.profile.as_deref().map(|p| if p == quiver_api::mods::VANILLA { "Vanilla".to_owned() } else { profile.clone().unwrap_or_else(|| p.to_owned()) }).unwrap_or_else(|| "default profile".into()),
                    plan.caps.summary(),
                    r.global,
                    r.game,
                    r.profile,
                    if plan.caps.route.is_empty() { "quiver" } else { plan.caps.route.as_str() }
                )))
                .into_any_element(),
        ];
        for w in &plan.warnings {
            body.push(banner(th, Tone::Warn, Icon::TriangleAlert, w.clone()).into_any_element());
        }
        let env = logic::env_lines(&plan);
        body.push(sec(th, "Environment Tweaks add").pt(px(6.)).into_any_element());
        body.push(if env.is_empty() { div().text_size(px(12.)).text_color(th.tx3).child("Nothing: every tweak is at the game's choice here, or a mod implements it.").into_any_element() } else { mono_block(env).into_any_element() });
        if !plan.dxvk_config.is_empty() {
            body.push(sec(th, "Quiver's DXVK_CONFIG entries (after yours)").pt(px(6.)).into_any_element());
            body.push(mono_block(plan.dxvk_config.clone()).into_any_element());
        }
        if !plan.bound.is_empty() {
            body.push(sec(th, "Into mods' options").pt(px(6.)).into_any_element());
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(plan.bound.iter().map(|b| {
                        div().text_size(px(12.5)).child(format!("{} → {} {} = {} ({})", b.tweak, b.surface.rsplit('/').next().unwrap_or(&b.surface), b.option, quiver_api::options::scalar_text(&b.value), b.scope.label()))
                    }))
                    .into_any_element(),
            );
        }
        for (title, list) in [("Files", &plan.files), ("Notes", &plan.notes)] {
            if !list.is_empty() {
                body.push(sec(th, title).pt(px(6.)).into_any_element());
                body.push(div().flex().flex_col().gap(px(2.)).text_size(px(12.5)).text_color(th.tx2).children(list.iter().map(|l| div().child(l.clone()))).into_any_element());
            }
        }
        body.push(sec(th, "Per tweak: the mechanisms considered").pt(px(6.)).into_any_element());
        if plan.entries.is_empty() {
            body.push(div().text_size(px(12.)).text_color(th.tx3).child("No tweak is set for this launch.").into_any_element());
        }
        for e in &plan.entries {
            let label = s.options.get(&e.id).map(|st| st.def.label_or(&e.id).to_owned()).unwrap_or_else(|| e.label.clone());
            let line = logic::planned_line(&e.planned, Prefer::Provider);
            let (text, tone) = line.map(|l| (l.text, l.tone)).unwrap_or_else(|| ("game's choice: nothing written".into(), Tone::Muted));
            let from = match &e.from {
                quiver_api::options::ValueSource::Scope { scope, revision, .. } => format!("{} r{revision}", logic::scope_name(*scope, profile.as_deref())),
                _ => "default".into(),
            };
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                            .child(div().font_weight(SEMIBOLD).child(label))
                            .child(mono(11.5, th.tx2).child(e.display.clone()))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(from))
                            .child(div().text_size(px(12.)).text_color(tone_colors(th, tone).1).child(text)),
                    )
                    .children(e.considered.iter().map(|c| {
                        let (ic, color) = if c.chosen { (Icon::Check, th.ok) } else { (Icon::X, th.tx4) };
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .pl(px(4.))
                            .text_size(px(12.))
                            .child(icon(ic, 12., color))
                            .child(mono(11.5, if c.chosen { th.tx1 } else { th.tx3 }).child(c.mech.clone()))
                            .when(!c.why.is_empty(), |d| d.child(div().text_color(th.tx3).child(c.why.clone())))
                    }))
                    .into_any_element(),
            );
        }
        let cmd = match &scope {
            Scope::Profile { game, .. } => format!("quiver tweaks plan {} --profile {}", game.key(), profile.clone().map(|p| if p.contains(' ') { format!("'{p}'") } else { p }).unwrap_or_default()),
            s => format!("quiver tweaks plan {}", s.game().map(|g| g.key()).unwrap_or_default()),
        };
        body.push(div().pt(px(6.)).flex().child(self.cli_line("tw-plan-cli", th, cmd, cx)).into_any_element());
        card(th).px(px(16.)).py(px(12.)).flex().flex_col().gap(px(6.)).children(body).into_any_element()
    }

    /// The scope's revisions, newest first, with Restore.
    fn render_tweaks_history(&mut self, th: &Theme, hist: &[OptionsRevision], cx: &mut Context<Self>) -> AnyElement {
        let cur = hist.last().map(|r| r.revision);
        let pending = self.tw.restore;
        let rows: Vec<AnyElement> = hist
            .iter()
            .rev()
            .map(|r| {
                let n = r.revision;
                let at = when(hist, n).unwrap_or_default();
                let action: AnyElement = if Some(n) == cur {
                    div().text_size(px(11.5)).text_color(th.tx3).child("current").into_any_element()
                } else if pending == Some(n) {
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(button(("tw-restore-ok", n as usize), th, Kind::Primary, Size::Small, Some(Icon::Undo), format!("Restore r{n}")).on_click(cx.listener(move |this, _, _, cx| this.tweaks_restore(n, cx))))
                        .child(button(("tw-restore-no", n as usize), th, Kind::Ghost, Size::Small, None, "Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.tw.restore = None;
                            cx.notify();
                        })))
                        .into_any_element()
                } else {
                    button(("tw-restore", n as usize), th, Kind::Ghost, Size::Small, Some(Icon::Undo), "Restore")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.tw.restore = Some(n);
                            cx.notify();
                        }))
                        .into_any_element()
                };
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(mono(12., th.tx2).w(px(34.)).flex_none().child(format!("r{n}")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(px(11.5))
                                    .text_color(th.tx3)
                                    .child(who_badge(th, &r.source))
                                    .child(at)
                                    .when_some(r.message.clone(), |d, m| d.child(div().text_color(th.tx2).child(format!("\u{201c}{m}\u{201d}")))),
                            )
                            .children(r.changes.iter().map(|c| div().text_size(px(12.5)).child(c.clone()))),
                    )
                    .child(action)
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        card(th)
            .px(px(16.))
            .py(px(12.))
            .flex()
            .flex_col()
            .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).pb(px(6.)).child("History"))
            .when(empty, |d| d.child(div().text_size(px(12.5)).text_color(th.tx3).child("No revisions yet: nothing has been set at this scope.")))
            .children(rows)
            .into_any_element()
    }

    /// The unsaved bar over the bottom of the screen (Settings → Tweaks and the Tweaks tab).
    pub fn render_tweaks_bar(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let scope = self.tw.scope.clone()?;
        let game = scope.game().map(Gid);
        let on_screen = match game {
            None => self.nav.page == crate::nav::Page::Settings,
            Some(id) => self.nav.page == crate::nav::Page::Game(id) && self.nav.tab == Tab::Tweaks,
        };
        let staged = self.tw.staged_for(&scope);
        if !on_screen || staged.is_empty() {
            return None;
        }
        let n = staged.len();
        let labels: Vec<String> = staged.keys().map(|k| self.tw.option(&k.1).map(|st| st.def.label_or(&k.1).to_owned()).unwrap_or_else(|| k.1.clone())).collect();
        let save_label = match &scope {
            Scope::Global => "Save for every game".to_owned(),
            Scope::Game { .. } => format!("Save for {}", game.and_then(|g| self.game(g)).map(|g| crate::model::clean_name(&g.entry.name)).unwrap_or_else(|| "this game".into())),
            Scope::Profile { .. } => format!("Save to profile {}", self.tweaks_profile_name(&scope).unwrap_or_default()),
        };
        let busy = self.tw.saving;
        let note = if scope.kind() == ScopeKind::Global { "Every game Quiver launches gets it at its next launch." } else { "Tweaks apply at the next launch." };
        let save_focus = self.tw_focused(game, &TwTarget::Save);
        let discard_focus = self.tw_focused(game, &TwTarget::Discard);
        let inset = if game.is_some() { self.pane_bp().pad().min(24.) } else { 24. };
        let bar = div()
            .id("tweaks-pending")
            .depth_overlay()
            .absolute()
            .left(px(inset))
            .right(px(inset))
            .bottom(px(16.))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .pl(px(16.))
            .pr(px(12.))
            .py(px(10.))
            .rounded(px(12.))
            .bg(th.ink3)
            .border_1()
            .shadow(vec![soft_shadow(crate::theme::rgba(0x000000, 0.55), 14., 40.)])
            .occlude();
        let bar = match self.tw.danger.clone() {
            Some(lines) => bar
                .border_color(th.bad.opacity(0.7))
                .child(icon(Icon::TriangleAlert, 16., th.bad))
                .child(div().flex_1().min_w(px(200.)).flex().flex_col().gap(px(2.)).child(div().font_weight(SEMIBOLD).child("These tweaks can break games. Save anyway?")).children(lines.into_iter().map(|l| div().text_size(px(12.)).text_color(th.tx2).child(l))))
                .child(button("tw-danger-back", th, Kind::Ghost, Size::Small, None, "Back").on_click(cx.listener(|this, _, _, cx| {
                    this.tw.danger = None;
                    cx.notify();
                })))
                .child(button("tw-danger-save", th, Kind::Primary, Size::Small, Some(Icon::Check), "Save anyway").focus_ring(save_focus, th, 6.).on_click(cx.listener(|this, _, _, cx| this.tweaks_save(true, cx)))),
            None => bar
                .border_color(th.warn.opacity(0.6))
                .child(div().flex_none().size(px(8.)).rounded_full().bg(th.warn))
                .child(div().flex_none().font_weight(SEMIBOLD).child(format!("{n} unsaved tweak{}", if n == 1 { "" } else { "s" })))
                .child(div().min_w_0().flex_shrink_1().truncate().text_color(th.tx2).child(labels.join(", ")))
                .child(div().flex_1().min_w(px(120.)).text_size(px(12.)).text_color(th.tx3).child(note))
                .child(button("tw-discard", th, Kind::Ghost, Size::Small, Some(Icon::Undo), "Discard").focus_ring(discard_focus, th, 6.).on_click(cx.listener(|this, _, _, cx| {
                    this.tweaks_discard(cx);
                    cx.notify();
                })))
                .child(
                    button("tw-save", th, Kind::Primary, Size::Small, Some(Icon::Check), if busy { "Saving…".to_owned() } else { save_label })
                        .focus_ring(save_focus, th, 6.)
                        .on_click(cx.listener(|this, _, _, cx| this.tweaks_save(false, cx))),
                ),
        };
        Some(bar.into_any_element())
    }

    /// Demo mode: `QUIVER_DEMO_GLOBAL=tweaks=settings|game[,tweaks-game=3539][,tweaks-scope=ID]
    /// [,tweaks-advanced][,tweaks-explain][,tweaks-history][,tweaks-unsaved][,tweaks-scroll=PX]`
    /// opens a Tweaks screen (for screenshots).
    pub fn start_tweaks_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let flags: Vec<String> = std::env::var("QUIVER_DEMO_GLOBAL").map(|v| v.split(',').map(|s| s.trim().to_owned()).collect()).unwrap_or_default();
        let Some(screen) = flags.iter().find_map(|f| f.strip_prefix("tweaks=")).map(str::to_owned) else { return };
        let get = |k: &str| flags.iter().find_map(|f| f.strip_prefix(&format!("{k}="))).map(str::to_owned);
        let has = |k: &str| flags.iter().any(|f| f == k);
        let game: Gid = get("tweaks-game").and_then(|g| g.parse().ok()).unwrap_or(Gid::from(3539));
        let scope = get("tweaks-scope");
        let (advanced, explain, history, unsaved) = (has("tweaks-advanced"), has("tweaks-explain"), has("tweaks-history"), has("tweaks-unsaved"));
        let scroll: Option<f32> = get("tweaks-scroll").and_then(|v| v.parse().ok());
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                if this.update(cx, |this, _| !this.games.is_empty()).unwrap_or(true) {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if screen == "settings" {
                    this.nav.open_settings(crate::nav::SettingsSection::Tweaks);
                } else {
                    this.open_game(game, cx);
                    this.nav.select_tab(Tab::Tweaks);
                    this.tw.pick.insert(game, scope.clone());
                }
                if advanced {
                    this.tw.level = Level::Advanced;
                }
                this.tw.panel = if explain {
                    Panel::Explain
                } else if history {
                    Panel::History
                } else {
                    Panel::None
                };
                cx.notify();
            });
            cx.background_executor().timer(Duration::from_millis(1200)).await;
            let _ = this.update(cx, |this, cx| {
                if unsaved {
                    this.tweaks_stage((surface_key(), "sync.vsync".into()), Some(Edit::Set(serde_json::json!("off"))), cx);
                }
                if let Some(y) = scroll {
                    this.body_scroll.set_offset(gpui::point(px(0.), px(-y)));
                }
                cx.notify();
            });
        })
        .detach();
    }
}
