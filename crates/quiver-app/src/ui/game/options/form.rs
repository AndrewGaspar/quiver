//! The Options view: the profile's surfaces and the selected one's pages (left), the page as
//! a form generated from the schema (middle), what the profile changes (right), and the
//! unsaved bar under the pane. Rows show the apply mode, who set the value and, after a
//! save, the daemon's outcome for it.

use std::collections::BTreeMap;

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px, relative};
use quiver_api::options::{Level, Live, OptionState, OptionType, OptionsRevision, SurfaceKind, SurfaceState, ValueSource, same_value, scalar_text};
use serde_json::Value;

use super::logic::{self, Edit, RowKey, Tone, Who};
use super::{Confirm, ModsView};
use crate::app::Quiver;
use crate::model::{GameRow, clock, day};
use crate::theme::Theme;
use crate::ui::game::page::{Load, toggle};
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, SEMIBOLD, Size, button, caption, chip, icon, mono, soft_shadow};

/// A small uppercase badge (`LIVE`, `RESTART`, `EXP`).
pub(super) fn tone_badge(th: &Theme, label: impl Into<SharedString>, tone: Tone) -> gpui::Div {
    let (bg, fg) = tone_colors(th, tone);
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.))
        .px(px(6.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::BOLD)
        .whitespace_nowrap()
        .child(label.into())
}

pub(super) fn tone_colors(th: &Theme, tone: Tone) -> (gpui::Hsla, gpui::Hsla) {
    match tone {
        Tone::Ok => (th.ok_soft, th.ok),
        Tone::Warn => (th.warn_soft, th.warn),
        Tone::Bad => (th.bad_soft, th.bad),
        Tone::Muted => (th.ink4, th.tx2),
        Tone::Accent => (th.acc_soft, th.acc_hi),
    }
}

pub(super) fn who_color(th: &Theme, w: &Who) -> gpui::Hsla {
    match w {
        Who::App => th.who_gui,
        Who::Cli | Who::InGame => th.who_cli,
        Who::Agent(_) => th.who_agent,
        Who::System => th.who_sys,
        Who::Game | Who::File | Who::Mod(_) => th.warn,
    }
}

/// Who made a change, as a coloured badge ("app", "in game", "agent aco-dlss").
pub(super) fn who_badge(th: &Theme, source: &str) -> gpui::Div {
    let w = logic::who(source);
    let c = who_color(th, &w);
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.))
        .px(px(6.))
        .rounded(px(5.))
        .bg(c.opacity(0.16))
        .text_color(c)
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::BOLD)
        .whitespace_nowrap()
        .gap(px(4.))
        .when(matches!(w, Who::Agent(_)), |d| d.child(icon(Icon::Bot, 11., c)))
        .child(w.badge())
}

/// A section caption in the mock-ups' style (uppercase, tracked).
pub(super) fn sec(th: &Theme, label: impl Into<String>) -> gpui::Div {
    caption(&label.into(), th.tx3)
}

/// A banner across the form (running game, a change from elsewhere, a crash).
pub(super) fn banner(th: &Theme, tone: Tone, ic: Icon, text: impl Into<SharedString>) -> gpui::Div {
    let (_, fg) = tone_colors(th, tone);
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .py(px(9.))
        .rounded(px(8.))
        .bg(fg.opacity(0.08))
        .border_1()
        .border_color(fg.opacity(0.3))
        .text_size(px(12.5))
        .text_color(th.tx1)
        .child(icon(ic, 14., fg))
        .child(div().flex_1().min_w_0().child(text.into()))
}

/// When a revision happened, for provenance lines ("Nov 14", "today 21:16").
pub(super) fn when(hist: &[OptionsRevision], revision: u32) -> Option<String> {
    let r = hist.iter().find(|r| r.revision == revision)?;
    let d = day(r.at);
    Some(if d == "Today" || d == "Yesterday" { format!("{} {}", d.to_lowercase(), clock(r.at)) } else { d })
}

impl Quiver {
    pub(super) fn render_options_form(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let profile = self.mods_profile();
        let surfaces = match &self.gp.opt.surfaces {
            Load::Ready(s) => s.clone(),
            Load::Failed(e) => return crate::ui::card(th).p(px(20.)).text_color(th.bad).child(format!("Couldn't load options: {e}")).into_any_element(),
            _ => return crate::ui::card(th).p(px(20.)).text_color(th.tx3).child("Loading options…").into_any_element(),
        };
        let nav = self.render_options_nav(th, g, &surfaces, cx);
        let Some(s) = self.gp.opt.surface.as_ref().and_then(|k| surfaces.iter().find(|s| &s.key == k)).cloned() else {
            let empty = crate::ui::card(th)
                .p(px(20.))
                .gap(px(8.))
                .child(div().font_weight(SEMIBOLD).child("No options for this profile"))
                .child(div().text_color(th.tx3).child(
                    "None of its mods ships a settings description, and Quiver's catalog has none for them. Pick a mod on the left to guess one from its file.",
                ));
            return div().flex().gap(px(16.)).child(nav).child(div().flex_1().child(empty)).into_any_element();
        };
        let form = self.render_surface_page(th, id, &profile, &s, cx);
        let side = self.render_profile_changes(th, id, &profile, &surfaces, cx);
        let bp = self.pane_bp();
        let main = if bp.two_columns() {
            div().flex().items_start().gap(px(16.)).child(nav).child(div().flex_1().min_w_0().child(form)).into_any_element()
        } else {
            div().flex().flex_col().gap(px(12.)).child(nav).child(form).into_any_element()
        };
        // Room for the unsaved bar over the bottom of the form.
        let pad = if self.gp.opt.staged_for(id, &profile).is_empty() { 0. } else { 80. };
        if bp >= crate::ui::responsive::Bp::Large {
            div().flex().items_start().gap(px(20.)).pb(px(pad)).child(div().flex_1().min_w_0().child(main)).child(div().w(px(300.)).flex_none().child(side)).into_any_element()
        } else {
            div().flex().flex_col().gap(px(16.)).pb(px(pad)).child(main).child(side).into_any_element()
        }
    }

    /// Surfaces of the profile's stack (and mods without options), then the selected one's pages.
    fn render_options_nav(&mut self, th: &Theme, g: &GameRow, surfaces: &[SurfaceState], cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let profile = self.mods_profile();
        let staged = self.gp.opt.staged_for(id, &profile);
        let sel = self.gp.opt.surface.clone();
        let page = self.gp.opt.page.clone();
        let stack: Vec<quiver_api::mods::ResolvedMod> =
            self.gp.mods.ready().and_then(|m| m.profiles.iter().find(|p| p.profile.id == profile)).map(|p| p.resolved.iter().rev().cloned().collect()).unwrap_or_default();
        let item = |key: String, label: String, right: Option<AnyElement>, on: bool| {
            div()
                .id(SharedString::from(key))
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(8.))
                .text_size(px(13.))
                .font_weight(MEDIUM)
                .text_color(if on { th.tx1 } else { th.tx2 })
                .when(on, |d| d.bg(th.ink3))
                .cursor_pointer()
                .hover(|s| s.bg(th.ink3))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .children(right)
        };
        let count = |n: usize| (n > 0).then(|| div().px(px(7.)).rounded_full().bg(th.acc).text_color(th.acc_ink).text_size(px(11.)).font_weight(gpui::FontWeight::BOLD).child(n.to_string()).into_any_element());
        let mut col: Vec<AnyElement> = vec![sec(th, "Stack").px(px(10.)).pb(px(2.)).into_any_element()];
        let mut seen = Vec::new();
        for r in &stack {
            let surf = surfaces.iter().find(|s| s.mod_id.as_deref() == Some(r.mod_id.as_str()));
            match surf {
                Some(s) => {
                    seen.push(s.key.clone());
                    let n = logic::changed_on(s, &s.options.keys().cloned().collect::<Vec<_>>(), &staged);
                    let label = if r.versions > 1 { format!("{} · {}", s.title, r.label) } else { s.title.clone() };
                    let key = s.key.clone();
                    let on = sel.as_deref() == Some(key.as_str()) && self.gp.opt.view == ModsView::Options;
                    col.push(
                        item(format!("nav-s-{key}"), label, count(n), on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.options_jump(&key, None);
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
                None => {
                    let m = r.mod_id.clone();
                    let on = self.gp.opt.view == ModsView::Source && self.gp.opt.source_mod.as_deref() == Some(m.as_str());
                    col.push(
                        item(format!("nav-m-{m}"), r.name.clone(), Some(div().text_size(px(11.5)).text_color(th.tx3).child("no options").into_any_element()), on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.options_open_source(m.clone());
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        // Mod surfaces outside the stack (fixes on vanilla), then game and runtime ones.
        let rest: Vec<&SurfaceState> = surfaces.iter().filter(|s| !seen.contains(&s.key)).collect();
        let (mods, other): (Vec<&SurfaceState>, Vec<&SurfaceState>) = rest.into_iter().partition(|s| s.kind == SurfaceKind::Mod);
        if stack.is_empty() && mods.is_empty() {
            col.push(div().px(px(10.)).text_size(px(12.)).text_color(th.tx3).child("No mods in this profile").into_any_element());
        }
        for s in mods {
            let n = logic::changed_on(s, &s.options.keys().cloned().collect::<Vec<_>>(), &staged);
            let key = s.key.clone();
            let on = sel.as_deref() == Some(key.as_str());
            col.push(item(format!("nav-s-{key}"), s.title.clone(), count(n), on).on_click(cx.listener(move |this, _, _, cx| {
                this.options_jump(&key, None);
                cx.notify();
            })).into_any_element());
        }
        if !other.is_empty() {
            col.push(sec(th, "Game and runtime").px(px(10.)).pt(px(12.)).pb(px(2.)).into_any_element());
            for s in other {
                let n = logic::changed_on(s, &s.options.keys().cloned().collect::<Vec<_>>(), &staged);
                let key = s.key.clone();
                let on = sel.as_deref() == Some(key.as_str());
                let file = s.files.first().map(|f| f.def.path.rsplit('/').next().unwrap_or(&f.def.path).to_owned());
                let right = if n > 0 { count(n) } else { file.map(|f| div().text_size(px(11.5)).text_color(th.tx3).child(f).into_any_element()) };
                col.push(item(format!("nav-s-{key}"), s.title.clone(), right, on).on_click(cx.listener(move |this, _, _, cx| {
                    this.options_jump(&key, None);
                    cx.notify();
                })).into_any_element());
            }
        }
        // The selected surface's pages.
        if let Some(s) = sel.as_ref().and_then(|k| surfaces.iter().find(|s| &s.key == k)) {
            col.push(sec(th, format!("{} pages", s.title)).px(px(10.)).pt(px(16.)).pb(px(2.)).into_any_element());
            let level = self.gp.opt.level;
            let mut pages: Vec<(String, String, usize)> = s
                .pages
                .iter()
                .filter(|p| p.groups.iter().flat_map(|g| &g.options).any(|o| s.options.get(o).is_some_and(|x| logic::at_level(&x.def, level))))
                .map(|p| (p.id.clone(), p.title.clone(), logic::changed_on(s, &logic::page_ids(s, &p.id), &staged)))
                .collect();
            if !s.raw.is_empty() {
                pages.push(("other".into(), "Other keys".into(), s.raw.iter().filter(|r| r.changed.is_some()).count()));
            }
            if !s.dormant.is_empty() {
                pages.push(("dormant".into(), format!("Not used by {}", s.version_label.clone().unwrap_or_else(|| "this version".into())), 0));
            }
            for (pid, title, n) in pages {
                let on = page.as_deref() == Some(pid.as_str());
                let p2 = pid.clone();
                col.push(item(format!("nav-p-{pid}"), title, count(n), on).on_click(cx.listener(move |this, _, _, cx| {
                    this.gp.opt.page = Some(p2.clone());
                    this.gp.opt.picker = None;
                    cx.notify();
                })).into_any_element());
            }
        }
        if self.pane_bp().two_columns() {
            div().w(px(210.)).flex_none().flex().flex_col().gap(px(2.)).children(col).into_any_element()
        } else {
            div().flex().flex_wrap().gap(px(4.)).children(col).into_any_element()
        }
    }

    pub(super) fn options_open_source(&mut self, mod_id: String) {
        if self.gp.opt.source_mod.as_deref() != Some(mod_id.as_str()) {
            self.gp.opt.schema = Load::Idle;
            self.gp.opt.files = Load::Idle;
            self.gp.opt.guess = Load::Idle;
        }
        self.gp.opt.source_mod = Some(mod_id);
        self.gp.opt.view = ModsView::Source;
    }

    fn render_surface_page(&mut self, th: &Theme, id: u32, profile: &str, s: &SurfaceState, cx: &mut Context<Self>) -> AnyElement {
        let staged = self.gp.opt.staged_for(id, profile);
        let level = self.gp.opt.level;
        let page_id = self.gp.opt.page.clone().unwrap_or_default();
        let page = s.pages.iter().find(|p| p.id == page_id).cloned();
        let title = match page_id.as_str() {
            "other" => "Other keys".to_owned(),
            "dormant" => "Not used by this version".to_owned(),
            _ => page.as_ref().map(|p| p.title.clone()).unwrap_or_else(|| s.title.clone()),
        };
        let query = self.gp.opt.search.as_ref().map(|e| e.read(cx).text().to_owned()).unwrap_or_default();
        let searching = !query.trim().is_empty();
        let mut body: Vec<AnyElement> = Vec::new();

        // Header: page, where the schema is from, Source, level.
        let source_mod = s.mod_id.clone();
        let from = match (&s.version_label, s.kind) {
            (Some(v), SurfaceKind::Mod) => format!("{} · {}", s.schema_note, v),
            _ => s.schema_note.clone(),
        };
        let levels = div().flex().items_center().gap(px(2.)).p(px(2.)).rounded_full().bg(th.ink3).border_1().border_color(th.line2).children(
            [(Level::Basic, "Basic"), (Level::Advanced, "Advanced"), (Level::Debug, "Debug")].into_iter().map(|(l, label)| {
                let on = level == l;
                div()
                    .id(SharedString::from(format!("lvl-{label}")))
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
                        this.gp.opt.level = l;
                        cx.notify();
                    }))
            }),
        );
        if self.gp.opt.search.is_none() {
            let e = self.new_search_input(cx);
            self.gp.opt.search = Some(e);
        }
        let search = self.gp.opt.search.clone();
        let changed_only = self.gp.opt.changed_only;
        body.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::BOLD).child(if searching { format!("Search · {}", s.title) } else { title.clone() }))
                .child(div().text_size(px(12.5)).text_color(th.tx3).child(format!("from {from}")))
                .when_some(source_mod, |d, m| {
                    d.child(
                        button("opt-source", th, Kind::Ghost, Size::Small, Some(Icon::FileText), "Source").on_click(cx.listener(move |this, _, _, cx| {
                            this.options_open_source(m.clone());
                            cx.notify();
                        })),
                    )
                })
                .child(div().flex_1())
                .child(levels)
                .into_any_element(),
        );
        body.push(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(div().w(px(260.)).flex_shrink_1().min_w(px(140.)).children(search))
                .child(
                    chip(th, Some(Icon::ListFilter), "Changed only", changed_only).id("opt-changed-only").cursor_pointer().on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.changed_only = !this.gp.opt.changed_only;
                        cx.notify();
                    })),
                )
                .when_some(s.help.clone(), |d, h| d.child(div().flex_1().min_w(px(200.)).text_size(px(12.)).text_color(th.tx3).truncate().child(h)))
                .into_any_element(),
        );

        // Banners: the running game, a change from elsewhere, a crash, the last save.
        for b in self.options_banners(th, s, cx) {
            body.push(b);
        }

        // Presets on this page.
        let page_opts: Vec<String> = page.as_ref().map(|p| p.groups.iter().flat_map(|g| g.options.clone()).collect()).unwrap_or_default();
        let presets: Vec<quiver_api::options::PresetInfo> = s.presets.iter().filter(|p| p.set.keys().any(|k| page_opts.contains(k))).cloned().collect();
        if !presets.is_empty() && !searching {
            let key = s.key.clone();
            body.push(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(12.5)).text_color(th.tx3).child("Presets"))
                    .children(presets.into_iter().enumerate().map(|(i, p)| {
                        let k = key.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(chip(th, Some(Icon::Sparkles), p.label.clone(), false).id(("preset", i)).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                for (oid, v) in &p.set {
                                    this.options_stage((k.clone(), oid.clone()), Some(Edit::Set(v.clone())), cx);
                                }
                                this.gp.opt.preset_note = Some(format!("Preset: {}", p.label));
                                cx.notify();
                            })))
                            .when_some(p.help.clone(), |d, h| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(h)))
                    }))
                    .into_any_element(),
            );
        }

        // The rows.
        let hist = self.gp.opt.history.ready().cloned().unwrap_or_default();
        let groups: Vec<(String, Vec<String>)> = if searching {
            // Every page of every surface: matches, grouped by page.
            s.pages.iter().map(|p| (p.title.clone(), p.groups.iter().flat_map(|g| g.options.clone()).collect())).collect()
        } else {
            match page_id.as_str() {
                "other" | "dormant" => Vec::new(),
                _ => page.map(|p| p.groups.iter().map(|g| (g.title.clone(), g.options.clone())).collect()).unwrap_or_default(),
            }
        };
        let (mut hidden_level, mut hidden_when, mut shown) = (0usize, 0usize, 0usize);
        self.gp.opt.pad_rows.clear();
        for (gi, (gtitle, ids)) in groups.into_iter().enumerate() {
            let mut rows: Vec<AnyElement> = Vec::new();
            for oid in ids {
                let Some(st) = s.options.get(&oid) else { continue };
                let key = (s.key.clone(), oid.clone());
                if !logic::at_level(&st.def, level) {
                    hidden_level += 1;
                    continue;
                }
                if !logic::visible_with(s, &staged, &st.def) {
                    hidden_when += 1;
                    continue;
                }
                if changed_only && !logic::is_changed(st) && !staged.contains_key(&key) {
                    continue;
                }
                if searching && !logic::matches_search(&query, &oid, st) {
                    continue;
                }
                rows.push(self.option_row(th, s, &oid, st, &staged, &hist, cx));
                self.gp.opt.pad_rows.push(key);
            }
            if rows.is_empty() {
                continue;
            }
            shown += rows.len();
            if !gtitle.is_empty() {
                body.push(sec(th, gtitle).pt(px(if gi == 0 { 4. } else { 14. })).into_any_element());
            }
            body.push(div().flex().flex_col().children(rows).into_any_element());
        }
        match page_id.as_str() {
            "other" if !searching => body.push(self.render_raw_keys(th, s, &staged, cx)),
            "dormant" if !searching => body.push(self.render_dormant(th, s)),
            _ => {}
        }
        if shown == 0 && !matches!(page_id.as_str(), "other" | "dormant") {
            body.push(
                div()
                    .py(px(18.))
                    .text_color(th.tx3)
                    .child(if searching {
                        "Nothing on this surface matches.".to_owned()
                    } else if changed_only {
                        "This profile doesn't change anything on this page.".to_owned()
                    } else {
                        "Nothing at this level on this page.".to_owned()
                    })
                    .into_any_element(),
            );
        }
        if hidden_level + hidden_when > 0 {
            let mut parts = Vec::new();
            if hidden_level > 0 {
                parts.push(format!("{hidden_level} more at a higher level"));
            }
            if hidden_when > 0 {
                parts.push(format!("{hidden_when} hidden until their condition holds"));
            }
            body.push(div().pt(px(10.)).text_size(px(12.)).text_color(th.tx3).child(parts.join(" · ")).into_any_element());
        }
        body.push(self.cli_line("opt-cli", th, self.options_cli(id, match level { Level::Basic => "list", Level::Advanced => "list --advanced", Level::Debug => "list --all" }).to_string(), cx).mt(px(14.)).into_any_element());
        div().flex().flex_col().gap(px(8.)).children(body).into_any_element()
    }

    fn new_search_input(&self, cx: &mut Context<Self>) -> gpui::Entity<crate::text_input::TextInput> {
        let e = cx.new(|cx| crate::text_input::TextInput::new(cx, "Search options…", false).compact(false));
        cx.observe(&e, |_, _, cx| cx.notify()).detach();
        e
    }

    fn options_banners(&mut self, th: &Theme, s: &SurfaceState, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let g = self.gp.id;
        let name = self.game(g).map(|g| crate::model::clean_name(&g.entry.name)).unwrap_or_default();
        if s.live.session.is_some() {
            let text = match (s.live.channel, s.live.connected) {
                (quiver_api::options::LiveChannel::Sdk, true) => format!("{name} is running with this profile and {} is connected: live options apply at once; the others wait for the moment shown.", s.title),
                (quiver_api::options::LiveChannel::File, true) => format!("{name} is running with this profile. Live options apply when {} re-reads its file (about once a second); the others wait for the moment shown.", s.title),
                (quiver_api::options::LiveChannel::File, false) => format!("{name} is starting with this profile; live changes reach it once its process is known."),
                _ => format!("{name} is running with this profile, but {} has no live channel: changes are saved and apply at the next launch.", s.title),
            };
            out.push(banner(th, Tone::Ok, Icon::Play, text).into_any_element());
        }
        if let Some(n) = self.gp.opt.notice.clone() {
            out.push(
                banner(th, Tone::Accent, Icon::RefreshCw, format!("Updated while you were here: {n}"))
                    .child(button("opt-notice-x", th, Kind::Ghost, Size::Small, Some(Icon::X), "").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.notice = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some((pid, prof, note)) = self.gp.opt.crash.clone()
            && pid == g
            && self.gp.opt.of.as_ref().is_some_and(|(_, p)| *p == prof)
        {
            let msg = note.crash.as_ref().map(|c| c.message.clone()).unwrap_or_else(|| "The game exited abnormally right after a live change.".into());
            let sid = note.session.clone();
            out.push(
                banner(th, Tone::Bad, Icon::TriangleAlert, msg)
                    .child(button("opt-crash-revert", th, Kind::Primary, Size::Small, Some(Icon::Undo), "Revert this session").on_click(cx.listener(move |this, _, _, cx| this.options_revert_session(sid.clone(), cx))))
                    .child(button("opt-crash-x", th, Kind::Ghost, Size::Small, None, "Keep").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.crash = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some(r) = self.gp.opt.result.clone() {
            match &r.error {
                Some(e) => out.push(banner(th, Tone::Bad, Icon::CircleX, format!("Not saved: {e}")).into_any_element()),
                None => {
                    let mut text = r.summary.clone();
                    if let Some(l) = r.live.first() {
                        text.push_str(&format!(". {l}"));
                    }
                    let tone = if r.summary.contains("rejected") { Tone::Warn } else { Tone::Ok };
                    out.push(banner(th, tone, Icon::CircleCheck, text).into_any_element());
                    for w in &r.warnings {
                        out.push(banner(th, Tone::Warn, Icon::TriangleAlert, w.clone()).into_any_element());
                    }
                }
            }
        }
        out
    }

    /// One option: label, help, badges and provenance on the left; its control on the right.
    #[allow(clippy::too_many_arguments)]
    fn option_row(&mut self, th: &Theme, s: &SurfaceState, oid: &str, st: &OptionState, staged: &BTreeMap<RowKey, Edit>, hist: &[OptionsRevision], cx: &mut Context<Self>) -> AnyElement {
        let key: RowKey = (s.key.clone(), oid.to_owned());
        let edit = staged.get(&key).cloned();
        let draft = logic::draft_value(st, edit.as_ref());
        let changed = logic::is_changed(st);
        let (badge, tone) = logic::apply_badge(&st.def);
        let label = st.def.label_or(oid).to_owned();

        // Left: what it is and where its value comes from.
        let mut meta: Vec<AnyElement> = Vec::new();
        match &st.from {
            ValueSource::Profile { revision, source } => {
                meta.push(who_badge(th, source).into_any_element());
                let w = when(hist, *revision).map(|t| format!(" · {t}")).unwrap_or_default();
                meta.push(div().child(format!("r{revision}{w}")).into_any_element());
            }
            ValueSource::Session { source } => {
                meta.push(who_badge(th, source).into_any_element());
                meta.push(div().child("this session only, not saved").into_any_element());
            }
            ValueSource::ModPatch { mod_id } => meta.push(div().child(format!("set by {mod_id}'s config patch; yours overrides it")).into_any_element()),
            ValueSource::Default => {}
        }
        if let Some(o) = self.gp.opt.outcomes.get(&key) {
            let (t, tone) = logic::outcome_text(&o.outcome);
            meta.push(tone_badge(th, t, tone).into_any_element());
        } else if s.live.session.is_some()
            && let Some(p) = logic::pending_text(&st.pending)
        {
            meta.push(tone_badge(th, p, Tone::Warn).into_any_element());
        }
        if let Some((v, by)) = self.gp.opt.in_game.get(&key)
            && !same_value(v, &st.value)
        {
            meta.push(div().text_color(th.who_cli).child(format!("In game now: {} ({}, not saved)", st.def.display(v), logic::who(by).badge())).into_any_element());
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
                    .child(div().size(px(7.)).rounded_full().flex_none().bg(if edit.is_some() { th.warn } else if changed { th.acc } else { gpui::transparent_black() }))
                    .child(div().text_size(px(14.)).font_weight(SEMIBOLD).child(label.clone()))
                    .child(tone_badge(th, badge, tone))
                    .when(st.def.experimental, |d| d.child(tone_badge(th, "EXPERIMENTAL", Tone::Accent)))
                    .when(st.def.danger.is_some(), |d| d.child(tone_badge(th, "DANGER", Tone::Bad)))
                    .when(st.def.level != Level::Basic, |d| d.child(div().text_size(px(11.)).text_color(th.tx4).child(format!("{:?}", st.def.level).to_lowercase()))),
            )
            .when_some(st.def.help.clone(), |d, h| d.child(div().text_size(px(12.5)).text_color(th.tx2).child(h)))
            .when_some(st.def.danger.clone(), |d, h| d.child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(th.bad).child(icon(Icon::TriangleAlert, 12., th.bad)).child(h)))
            .when_some(st.dormant.clone(), |d, h| d.child(div().text_size(px(12.)).text_color(th.warn).child(format!("Your value is kept but not used: {h}"))))
            .when_some(st.invalid.clone(), |d, h| d.child(div().text_size(px(12.)).text_color(th.warn).child(format!("The file's value doesn't fit: {h}"))))
            .when(!meta.is_empty(), |d| d.child(div().flex().flex_wrap().items_center().gap(px(6.)).text_size(px(11.5)).text_color(th.tx3).children(meta)))
            .child(mono(11., th.tx4).child(oid.to_owned()));

        // Right: the control, then what resetting or undoing does.
        let control = self.option_control(th, s, &key, st, &draft, cx);
        let mut under: Vec<AnyElement> = Vec::new();
        if let Some(Edit::Set(v)) = &edit
            && let Err(why) = st.def.normalize(v)
        {
            under.push(div().text_color(th.bad).child(why).into_any_element());
        }
        let k2 = key.clone();
        if edit.is_some() {
            under.push(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().text_color(th.warn).child(format!("Unsaved · was {}", logic::short_value(&st.def, &st.value))))
                    .child(div().id(SharedString::from(format!("undo-{oid}"))).text_color(th.acc_hi).cursor_pointer().child("Undo").on_click(cx.listener(move |this, _, _, cx| {
                        this.options_stage(k2.clone(), None, cx);
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        } else if changed {
            under.push(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().child(format!("Default: {}", logic::short_value(&st.def, &st.default))))
                    .child(div().id(SharedString::from(format!("reset-{oid}"))).text_color(th.acc_hi).cursor_pointer().child("Reset").on_click(cx.listener(move |this, _, _, cx| {
                        this.options_stage(k2.clone(), Some(Edit::Reset), cx);
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        let right = div()
            .w(px(300.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(control)
            .when(!under.is_empty(), |d| d.child(div().flex().flex_col().gap(px(2.)).text_size(px(12.)).text_color(th.tx3).children(under)));
        let stacked = self.pane_bp() < crate::ui::responsive::Bp::Large;
        let pad_i = self.options_body_base() + super::ModsView::TABS.len() + self.gp.opt.pad_rows.len();
        let focused = self.focused(crate::nav::Target::Body(pad_i));
        div()
            .flex()
            .when(stacked, |d| d.flex_col().items_start())
            .when(!stacked, |d| d.items_center())
            .gap(px(20.))
            .py(px(13.))
            .when(focused, |d| d.bg(th.acc_soft).px(px(8.)).rounded(px(8.)))
            .border_t_1()
            .border_color(th.line1)
            .child(left)
            .child(right)
            .into_any_element()
    }

    /// The control for one option's type.
    fn option_control(&mut self, th: &Theme, s: &SurfaceState, key: &RowKey, st: &OptionState, draft: &Value, cx: &mut Context<Self>) -> AnyElement {
        let id_base = format!("{}-{}", key.0, key.1);
        let def = st.def.clone();
        match def.ty {
            OptionType::Bool => {
                let on = draft.as_bool().unwrap_or(false);
                let k = key.clone();
                div()
                    .flex()
                    .justify_end()
                    .child(div().id(SharedString::from(format!("b-{id_base}"))).cursor_pointer().child(toggle(th, on)).on_click(cx.listener(move |this, _, _, cx| {
                        this.options_stage(k.clone(), Some(Edit::Set(Value::Bool(!on))), cx);
                        cx.notify();
                    })))
                    .into_any_element()
            }
            OptionType::Enum => self.enum_control(th, key, st, draft, cx),
            OptionType::Int | OptionType::Float => self.number_control(th, key, st, draft, cx),
            OptionType::Key => self.key_control(th, s, key, st, draft, cx),
            OptionType::List => self.list_control(th, key, st, draft, cx),
            OptionType::String | OptionType::Path | OptionType::Color => {
                let text = def.to_text(draft);
                let input = self.option_input(key, &text, if def.ty == OptionType::Path { "Path" } else { "Text" }, cx);
                let swatch = (def.ty == OptionType::Color).then(|| parse_hex(&text)).flatten();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .when_some(swatch, |d, c| d.child(div().size(px(22.)).rounded(px(5.)).border_1().border_color(th.line2).bg(c)))
                    .child(div().flex_1().child(input))
                    .into_any_element()
            }
        }
    }

    fn enum_control(&mut self, th: &Theme, key: &RowKey, st: &OptionState, draft: &Value, cx: &mut Context<Self>) -> AnyElement {
        let level = self.gp.opt.level;
        let values: Vec<quiver_api::options::EnumValue> =
            st.def.values.iter().filter(|e| e.level.is_none_or(|l| l <= level) || same_value(&e.value, draft) || same_value(&e.value, &st.value)).cloned().collect();
        let label_of = |e: &quiver_api::options::EnumValue| e.label.clone().unwrap_or_else(|| scalar_text(&e.value));
        let short = values.len() <= 3 && values.iter().all(|e| label_of(e).chars().count() <= 14);
        let id_base = format!("{}-{}", key.0, key.1);
        if short {
            return div()
                .flex()
                .justify_end()
                .gap(px(2.))
                .p(px(2.))
                .rounded(px(9.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line2)
                .children(values.into_iter().enumerate().map(|(i, e)| {
                    let on = same_value(&e.value, draft);
                    let k = key.clone();
                    let v = e.value.clone();
                    div()
                        .id(SharedString::from(format!("e-{id_base}-{i}")))
                        .flex_1()
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(8.))
                        .rounded(px(7.))
                        .text_size(px(12.5))
                        .font_weight(if on { SEMIBOLD } else { MEDIUM })
                        .text_color(if on { th.acc_ink } else { th.tx2 })
                        .when(on, |d| d.bg(th.acc))
                        .cursor_pointer()
                        .child(label_of(&e))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.options_stage(k.clone(), Some(Edit::Set(v.clone())), cx);
                            cx.notify();
                        }))
                }))
                .into_any_element();
        }
        let open = self.gp.opt.picker.as_ref() == Some(key);
        let current = values.iter().find(|e| same_value(&e.value, draft)).map(label_of).unwrap_or_else(|| scalar_text(draft));
        let k = key.clone();
        let btn = div()
            .id(SharedString::from(format!("e-{id_base}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(34.))
            .px(px(12.))
            .rounded(px(8.))
            .bg(th.ink1)
            .border_1()
            .border_color(if open { th.acc } else { th.line2 })
            .cursor_pointer()
            .hover(|s| s.border_color(th.acc_line))
            .child(div().flex_1().min_w_0().truncate().font_weight(MEDIUM).child(current))
            .child(icon(if open { Icon::ChevronUp } else { Icon::ChevronDown }, 14., th.tx3))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.gp.opt.picker = if this.gp.opt.picker.as_ref() == Some(&k) { None } else { Some(k.clone()) };
                cx.notify();
            }));
        let list = open.then(|| {
            div().flex().flex_col().p(px(4.)).rounded(px(9.)).bg(th.ink2).border_1().border_color(th.line2).shadow(vec![soft_shadow(crate::theme::rgba(0x000000, 0.4), 8., 24.)]).children(
                values.into_iter().enumerate().map(|(i, e)| {
                    let on = same_value(&e.value, draft);
                    let k = key.clone();
                    let v = e.value.clone();
                    div()
                        .id(SharedString::from(format!("eo-{id_base}-{i}")))
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|s| s.bg(th.ink3))
                        .child(div().w(px(14.)).pt(px(2.)).children(on.then(|| icon(Icon::Check, 13., th.acc_hi))))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).child(label_of(&e)))
                                .when_some(e.help.clone(), |d, h| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(h))),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.options_stage(k.clone(), Some(Edit::Set(v.clone())), cx);
                            this.gp.opt.picker = None;
                            cx.notify();
                        }))
                }),
            )
        });
        div().flex().flex_col().gap(px(4.)).child(btn).children(list).into_any_element()
    }

    fn number_control(&mut self, th: &Theme, key: &RowKey, st: &OptionState, draft: &Value, cx: &mut Context<Self>) -> AnyElement {
        let def = st.def.clone();
        let id_base = format!("{}-{}", key.0, key.1);
        let text = def.to_text(draft);
        let input = self.option_input(key, &text, "", cx);
        let step = |dir: f64, this: &mut Self, cx: &mut Context<Self>| {
            let _ = this;
            let k = key.clone();
            let d = def.clone();
            let v = draft.clone();
            div()
                .id(SharedString::from(format!("n{}-{id_base}", if dir > 0. { "+" } else { "-" })))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded(px(7.))
                .bg(th.ink3)
                .border_1()
                .border_color(th.line2)
                .cursor_pointer()
                .hover(|s| s.bg(th.ink4))
                .child(icon(if dir > 0. { Icon::Plus } else { Icon::Minus }, 13., th.tx2))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let n = logic::step_number(&d, &v, dir);
                    this.options_stage(k.clone(), Some(Edit::Set(n)), cx);
                    cx.notify();
                }))
        };
        let minus = step(-1., self, cx);
        let plus = step(1., self, cx);
        let track = logic::fraction(&def, draft).map(|f| {
            div()
                .flex_1()
                .min_w(px(40.))
                .h(px(5.))
                .rounded(px(3.))
                .bg(th.ink4)
                .relative()
                .child(div().absolute().left_0().top_0().h(px(5.)).rounded(px(3.)).bg(th.acc).w(relative(f)))
                .child(div().absolute().top(px(-5.5)).left(relative(f)).ml(px(-8.)).size(px(16.)).rounded_full().bg(th.tx1))
        });
        let has_track = track.is_some();
        let unit = def.unit.clone();
        let chips: Vec<(Value, String)> = def.special.iter().map(|m| (m.value.clone(), m.label.clone())).chain(def.marks.iter().map(|m| (m.value.clone(), format!("{} {}", m.label, scalar_text(&m.value))))).collect();
        let k = key.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(minus)
                    .children(track)
                    .when(!has_track, |d| d.child(div().flex_1()))
                    .child(plus)
                    .child(div().w(px(76.)).flex_none().child(input))
                    .when_some(unit, |d, u| d.child(div().text_size(px(12.)).text_color(th.tx3).child(u))),
            )
            .when(!chips.is_empty(), |d| {
                d.child(div().flex().flex_wrap().gap(px(4.)).children(chips.into_iter().enumerate().map(|(i, (v, label))| {
                    let on = same_value(&v, draft);
                    let k = k.clone();
                    div()
                        .id(SharedString::from(format!("m-{id_base}-{i}")))
                        .px(px(7.))
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .text_size(px(11.))
                        .bg(if on { th.acc_soft } else { th.ink3 })
                        .text_color(if on { th.acc_hi } else { th.tx3 })
                        .cursor_pointer()
                        .hover(|s| s.text_color(th.tx1))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.options_stage(k.clone(), Some(Edit::Set(v.clone())), cx);
                            cx.notify();
                        }))
                })))
            })
            .into_any_element()
    }

    fn key_control(&mut self, th: &Theme, s: &SurfaceState, key: &RowKey, st: &OptionState, draft: &Value, cx: &mut Context<Self>) -> AnyElement {
        let def = st.def.clone();
        let id_base = format!("{}-{}", key.0, key.1);
        let none = def.none.clone().unwrap_or_else(|| "none".into());
        let cur = scalar_text(draft);
        let is_none = cur.eq_ignore_ascii_case(&none) || cur.is_empty();
        let open = self.gp.opt.picker.as_ref() == Some(key);
        let keys = self.gp.opt.keys.ready().cloned();
        // Who else uses a key (normalized names compare case-insensitively).
        let others = |k: &str| -> Vec<String> {
            keys.as_ref()
                .map(|r| r.bindings.iter().filter(|b| b.key.eq_ignore_ascii_case(k) && !(b.surface == s.key && b.id == key.1)).map(|b| format!("{}: {}", b.owner, b.label)).collect())
                .unwrap_or_default()
        };
        let clash = if is_none { Vec::new() } else { others(&cur) };
        let k = key.clone();
        let head = div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(if clash.is_empty() { th.line2 } else { th.bad })
                    .bg(th.ink1)
                    .font_weight(SEMIBOLD)
                    .text_color(if is_none { th.tx3 } else { th.tx1 })
                    .child(if is_none { "No key".to_owned() } else { cur.clone() }),
            )
            .child(button(SharedString::from(format!("k-{id_base}")), th, Kind::Secondary, Size::Small, None, if open { "Done" } else { "Change…" }).on_click(cx.listener(move |this, _, _, cx| {
                this.gp.opt.picker = if this.gp.opt.picker.as_ref() == Some(&k) { None } else { Some(k.clone()) };
                cx.notify();
            })));
        let warn = (!clash.is_empty()).then(|| div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(th.bad).child(icon(Icon::TriangleAlert, 12., th.bad)).child(format!("Also used by {}", clash.join(", "))));
        let picker = open.then(|| match logic::allowed_keys(&def) {
            Some(list) => {
                let mut items: Vec<(String, String)> = list.into_iter().map(|k| (k.clone(), k)).collect();
                items.push((none.clone(), "No key".into()));
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .p(px(6.))
                    .rounded(px(9.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line2)
                    .children(items.into_iter().enumerate().map(|(i, (v, label))| {
                        let on = v.eq_ignore_ascii_case(&cur);
                        let used = others(&v);
                        let k = key.clone();
                        let tip = used.join(", ");
                        div()
                            .id(SharedString::from(format!("kp-{id_base}-{i}")))
                            .min_w(px(40.))
                            .h(px(26.))
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(if on { th.acc } else if used.is_empty() { th.line2 } else { th.bad.opacity(0.6) })
                            .bg(if on { th.acc_soft } else { th.ink1 })
                            .text_size(px(12.))
                            .font_weight(MEDIUM)
                            .text_color(if used.is_empty() { th.tx1 } else { th.bad })
                            .cursor_pointer()
                            .hover(|s| s.border_color(th.acc_line))
                            .child(label)
                            .when(!tip.is_empty(), |d| d.tooltip(crate::ui::responsive::tip(th, tip.clone())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.options_stage(k.clone(), Some(Edit::Set(Value::String(v.clone()))), cx);
                                this.gp.opt.picker = None;
                                cx.notify();
                            }))
                    }))
                    .into_any_element()
            }
            None => {
                let input = self.option_input(key, &cur, "Key, e.g. F9 or Shift+Home", cx);
                div().child(input).into_any_element()
            }
        });
        div().flex().flex_col().gap(px(6.)).child(head).children(warn).children(picker).into_any_element()
    }

    fn list_control(&mut self, th: &Theme, key: &RowKey, st: &OptionState, draft: &Value, cx: &mut Context<Self>) -> AnyElement {
        let id_base = format!("{}-{}", key.0, key.1);
        let items: Vec<Value> = draft.as_array().cloned().unwrap_or_default();
        let add_key = (key.0.clone(), format!("+{}", key.1));
        let add = self.option_input(&add_key, "", "Add an item", cx);
        let item_def = st.def.item.as_deref().cloned().unwrap_or_default();
        let chips = items.iter().enumerate().map(|(i, v)| {
            let k = key.clone();
            let mut rest = items.clone();
            rest.remove(i);
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(24.))
                .pl(px(8.))
                .pr(px(4.))
                .rounded_full()
                .bg(th.ink3)
                .border_1()
                .border_color(th.line2)
                .child(mono(11.5, th.tx1).child(item_def.to_text(v)))
                .child(div().id(SharedString::from(format!("l-{id_base}-{i}"))).cursor_pointer().child(icon(Icon::X, 11., th.tx3)).on_click(cx.listener(move |this, _, _, cx| {
                    this.options_stage(k.clone(), Some(Edit::Set(Value::Array(rest.clone()))), cx);
                    cx.notify();
                })))
                .into_any_element()
        });
        let chips: Vec<AnyElement> = chips.collect();
        let k = key.clone();
        let input = add.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(if chips.is_empty() { div().text_size(px(12.)).text_color(th.tx3).child("Empty").into_any_element() } else { div().flex().flex_wrap().gap(px(4.)).children(chips).into_any_element() })
            .child(div().flex().items_center().gap(px(6.)).child(div().flex_1().child(add)).child(button(SharedString::from(format!("la-{id_base}")), th, Kind::Secondary, Size::Small, Some(Icon::Plus), "Add").on_click(
                cx.listener(move |this, _, _, cx| {
                    let t = input.read(cx).text().trim().to_owned();
                    if t.is_empty() {
                        return;
                    }
                    let mut v = items.clone();
                    v.push(item_def.parse_text(&t).unwrap_or(Value::String(t)));
                    this.options_stage(k.clone(), Some(Edit::Set(Value::Array(v))), cx);
                    input.update(cx, |i, cx| i.set_text("", cx));
                    cx.notify();
                }),
            )))
            .into_any_element()
    }

    /// "Other keys": keys of the surface's files its schema doesn't describe (guessed types).
    fn render_raw_keys(&mut self, th: &Theme, s: &SurfaceState, staged: &BTreeMap<RowKey, Edit>, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = s
            .raw
            .iter()
            .map(|r| {
                let key = (s.key.clone(), format!("raw:{}", r.binding));
                let edit = staged.get(&key);
                let text = match edit {
                    Some(Edit::Set(v)) => scalar_text(v),
                    _ => r.text.clone(),
                };
                let input = self.option_input(&key, &text, "", cx);
                let ty = format!("{:?}", r.guess.ty).to_uppercase();
                let outcome = self.gp.opt.outcomes.get(&(s.key.clone(), r.binding.clone())).map(|o| logic::outcome_text(&o.outcome));
                div()
                    .flex()
                    .items_center()
                    .gap(px(20.))
                    .py(px(11.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).child(r.guess.label.clone().unwrap_or_else(|| r.binding.clone()))).child(tone_badge(th, format!("GUESS · {ty}"), Tone::Warn)))
                            .when_some(r.guess.help.clone(), |d, h| d.child(div().text_size(px(12.)).text_color(th.tx3).child(format!("\u{201c}{h}\u{201d}"))))
                            .child(mono(11., th.tx4).child(r.binding.clone()))
                            .when_some(outcome, |d, (t, tone)| d.child(tone_badge(th, t, tone))),
                    )
                    .child(div().w(px(200.)).flex_none().child(input))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .child(div().pb(px(8.)).text_size(px(12.)).text_color(th.tx3).child("Keys the description doesn't cover. Types are guessed from the values; changes apply at the next launch and every value round-trips exactly."))
            .children(rows)
            .into_any_element()
    }

    fn render_dormant(&self, th: &Theme, s: &SurfaceState) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .child(div().pb(px(8.)).text_size(px(12.)).text_color(th.tx3).child("Values this profile set that the running version doesn't take. They're kept, not applied; pinning a version that has them brings them back."))
            .children(s.dormant.iter().map(|d| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(mono(12., th.tx2).w(px(200.)).child(d.id.clone()))
                    .child(mono(12., th.tx1).w(px(100.)).child(scalar_text(&d.value)))
                    .child(div().flex_1().text_size(px(12.)).text_color(th.warn).child(d.reason.clone()))
            }))
            .into_any_element()
    }

    /// What the profile changes, across its surfaces, newest first (MOD-CONFIG: "What this
    /// profile changes").
    fn render_profile_changes(&mut self, th: &Theme, id: u32, profile: &str, surfaces: &[SurfaceState], cx: &mut Context<Self>) -> AnyElement {
        let staged = self.gp.opt.staged_for(id, profile);
        let hist = self.gp.opt.history.ready().cloned().unwrap_or_default();
        let name = self.mods_profile_name(profile);
        let cur = hist.last().map(|r| r.revision).unwrap_or(0);
        struct Line {
            key: RowKey,
            label: String,
            from: String,
            to: String,
            source: Option<String>,
            revision: u32,
            staged: bool,
            pending: Option<String>,
        }
        let mut lines: Vec<Line> = Vec::new();
        for s in surfaces {
            for (oid, st) in &s.options {
                let key = (s.key.clone(), oid.clone());
                let edit = staged.get(&key);
                if !logic::is_changed(st) && edit.is_none() {
                    continue;
                }
                let to = logic::short_value(&st.def, &logic::draft_value(st, edit));
                let from = logic::short_value(&st.def, &st.default);
                let (source, revision) = match &st.from {
                    ValueSource::Profile { revision, source } => (Some(source.clone()), *revision),
                    ValueSource::Session { source } => (Some(source.clone()), u32::MAX),
                    _ => (None, 0),
                };
                let label = if s.kind == SurfaceKind::Mod { st.def.label_or(oid).to_owned() } else { format!("{}: {}", s.title, st.def.label_or(oid)) };
                let pending = (s.live.session.is_some()).then(|| logic::pending_text(&st.pending)).flatten().map(|_| match st.def.apply {
                    quiver_api::options::Apply::At => format!("AT {}", logic::short_moment(st.def.at.as_deref().unwrap_or(""))).to_uppercase(),
                    _ => "NEXT LAUNCH".into(),
                });
                lines.push(Line { key, label, from, to, source, revision, staged: edit.is_some(), pending });
            }
        }
        lines.sort_by(|a, b| b.staged.cmp(&a.staged).then(b.revision.cmp(&a.revision)));
        let saved = lines.iter().filter(|l| !l.staged || surfaces.iter().any(|s| s.key == l.key.0 && s.options.get(&l.key.1).is_some_and(logic::is_changed))).count();
        let rows: Vec<AnyElement> = lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let k = l.key.clone();
                let t = when(&hist, l.revision);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(th.line1)
                    .text_size(px(12.5))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(div().font_weight(SEMIBOLD).child(l.label))
                            .when_some(l.pending, |d, p| d.child(tone_badge(th, p, Tone::Muted))),
                    )
                    .child(div().min_w_0().text_color(th.tx2).child(format!("{} → {}", l.from, l.to)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .when(l.staged, |d| d.child(tone_badge(th, "UNSAVED", Tone::Warn)))
                            .when_some(l.source.clone().filter(|_| !l.staged), |d, s| d.child(who_badge(th, &s)))
                            .when(!l.staged, |d| {
                                d.child(match (l.revision, &t) {
                                    (u32::MAX, _) => "this session".to_owned(),
                                    (r, Some(t)) => format!("r{r} · {t}"),
                                    (r, None) => format!("r{r}"),
                                })
                            })
                            .child(div().flex_1())
                            .when(!l.staged, |d| {
                                d.child(div().id(("chg-reset", i)).text_color(th.acc_hi).cursor_pointer().child("Reset").on_click(cx.listener(move |this, _, _, cx| {
                                    this.options_stage(k.clone(), Some(Edit::Reset), cx);
                                    cx.notify();
                                })))
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .p(px(16.))
            .rounded(px(12.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line1)
            .child(sec(th, format!("Profile {name} changes")))
            .child(div().pt(px(4.)).pb(px(8.)).text_size(px(12.5)).text_color(th.tx2).child(if empty {
                "Nothing: every option has its default.".to_owned()
            } else {
                format!("{saved} option{} differ from the defaults{}", if saved == 1 { "" } else { "s" }, if cur > 0 { format!(" · r{cur}") } else { String::new() })
            }))
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .pt(px(12.))
                    .child(button("chg-compare", th, Kind::Secondary, Size::Small, Some(Icon::GitCompare), "Compare with Vanilla").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.compare = quiver_api::mods::VANILLA.into();
                        this.gp.opt.diff = Load::Idle;
                        this.gp.opt.view = ModsView::Changes;
                        cx.notify();
                    })))
                    .child(button("chg-history", th, Kind::Secondary, Size::Small, Some(Icon::History), "History").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.view = ModsView::Changes;
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }

    /// The unsaved bar over the bottom of the Mods tab (any view), like the launch options'.
    pub(in crate::ui::game) fn render_options_bar(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = g.id();
        let (gid, profile) = self.gp.opt.of.clone()?;
        if gid != id {
            return None;
        }
        let staged = self.gp.opt.staged_for(id, &profile);
        if staged.is_empty() {
            return None;
        }
        let name = self.mods_profile_name(&profile);
        let labels: Vec<String> = staged.keys().map(|k| self.gp.opt.surface(&k.0).and_then(|s| s.options.get(&k.1)).map(|st| st.def.label_or(&k.1).to_owned()).unwrap_or_else(|| k.1.clone())).collect();
        let running = self.gp.opt.loaded_surfaces().iter().any(|s| s.live.session.is_some());
        // What saving will do, from the schema (the outcomes say what really happened).
        let (mut live, mut later) = (0, 0);
        for k in staged.keys() {
            let s = self.gp.opt.surface(&k.0);
            let live_ok = s.is_some_and(|s| s.live.session.is_some() && s.live.channel != quiver_api::options::LiveChannel::None);
            match s.and_then(|s| s.options.get(&k.1)).map(|st| st.def.apply) {
                Some(quiver_api::options::Apply::Live) if live_ok => live += 1,
                _ => later += 1,
            }
        }
        let note = match (running, live, later) {
            (false, _, _) => "Saving keeps them for the next launch.".to_owned(),
            (true, l, 0) => format!("Saving applies {} live to the running game.", if l == 1 { "it" } else { "them" }),
            (true, 0, _) => "The game is running; these apply at the moment each option says.".to_owned(),
            (true, l, n) => format!("Saving applies {l} live; {n} wait for the moment shown."),
        };
        let n = staged.len();
        let busy = self.gp.opt.saving;
        let pb = self.pane_bp();
        let inset = pb.pad().min(24.);
        let confirm = self.gp.opt.confirm.clone();
        let rows = if self.gp.opt.view == ModsView::Options { self.gp.opt.pad_rows.len() } else { 0 };
        let bar_i = self.options_body_base() + super::ModsView::TABS.len() + rows;
        let save_focus = self.focused(crate::nav::Target::Body(bar_i));
        let discard_focus = self.focused(crate::nav::Target::Body(bar_i + 1));
        let bar = div()
            .id("options-pending")
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
        let bar = match confirm {
            Some(Confirm::Danger { lines, live }) => bar
                .border_color(th.bad.opacity(0.7))
                .child(icon(Icon::TriangleAlert, 16., th.bad))
                .child(div().flex_1().min_w(px(200.)).flex().flex_col().gap(px(2.)).child(div().font_weight(SEMIBOLD).child("These options can break the game. Save anyway?")).children(lines.into_iter().map(|l| div().text_size(px(12.)).text_color(th.tx2).child(l))))
                .child(button("opt-danger-back", th, Kind::Ghost, Size::Small, None, "Back").on_click(cx.listener(|this, _, _, cx| {
                    this.gp.opt.confirm = None;
                    cx.notify();
                })))
                .child(button("opt-danger-save", th, Kind::Primary, Size::Small, Some(Icon::Check), "Save anyway").focus_ring(save_focus, th, 6.).on_click(cx.listener(move |this, _, _, cx| this.options_save(live, true, cx)))),
            _ => bar
                .border_color(th.warn.opacity(0.6))
                .child(div().flex_none().size(px(8.)).rounded_full().bg(th.warn))
                .child(div().flex_none().font_weight(SEMIBOLD).child(format!("{n} unsaved change{}", if n == 1 { "" } else { "s" })))
                .child(div().min_w_0().flex_shrink_1().truncate().text_color(th.tx2).child(labels.join(", ")))
                .child(div().flex_1().min_w(px(120.)).text_size(px(12.)).text_color(th.tx3).child(note))
                .child(button("opt-discard", th, Kind::Ghost, Size::Small, Some(Icon::Undo), "Discard").focus_ring(discard_focus, th, 6.).on_click(cx.listener(|this, _, _, cx| {
                    this.options_discard(cx);
                    cx.notify();
                })))
                .when(running && live > 0, |d| {
                    d.child(button("opt-save-later", th, Kind::Secondary, Size::Small, None, "Save for next launch").on_click(cx.listener(|this, _, _, cx| this.options_save(Live::No, false, cx))))
                })
                .child(
                    button("opt-save", th, Kind::Primary, Size::Small, Some(Icon::Check), if busy { "Saving…".to_owned() } else { format!("Save to profile {name}") })
                        .focus_ring(save_focus, th, 6.)
                        .on_click(cx.listener(|this, _, _, cx| this.options_save(Live::IfRunning, false, cx))),
                ),
        };
        Some(bar.into_any_element())
    }
}

fn parse_hex(t: &str) -> Option<gpui::Hsla> {
    let h = t.trim().strip_prefix('#')?;
    let v = u32::from_str_radix(h.get(..6)?, 16).ok()?;
    Some(crate::theme::rgba(v, 1.0))
}
