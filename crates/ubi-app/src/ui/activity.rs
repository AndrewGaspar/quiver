//! Activity: everything the app, the CLI, agents and the daemon did, newest first, with who
//! did it. Filters by source (LT / RT) and game (Y on an entry). Kept fresh by events and,
//! while the page is open, a poll (there is no "activity appended" event yet).

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};
use gpui::{AnyElement, Context, FontWeight, HighlightStyle, IntoElement, ParentElement, StyledText, Styled, div, prelude::*, px};
use ubi_api::ActivityEntry;

use super::global::SourceFilter;
use super::{FocusExt, Icon, MEDIUM, PillKind, SEMIBOLD, art_fill, caption, card, command_line, display, glyph, icon, mono, pill};
use crate::app::{InputMode, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{Actor, actor, actor_label, clean_name, clock, day, short_name};
use crate::nav::{Page, Target, Zone};
use crate::theme::Theme;

/// Day headings and rows, as the list's children.
enum Item {
    Day(String),
    Row(usize),
}

/// The list's children for `rows` (newest first): a heading whenever the day changes.
fn items(rows: &[&ActivityEntry]) -> Vec<Item> {
    let mut out = vec![];
    let mut last: Option<chrono::NaiveDate> = None;
    for (i, a) in rows.iter().enumerate() {
        let d = a.at.with_timezone(&Local).date_naive();
        if last != Some(d) {
            out.push(Item::Day(day(a.at)));
            last = Some(d);
        }
        out.push(Item::Row(i));
    }
    out
}

/// "install started" -> "Install started".
fn sentence(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

impl Quiver {
    /// Activity entries that pass the filters, newest first.
    pub fn activity_rows(&self) -> Vec<&ActivityEntry> {
        let f = self.ui.source_filter();
        self.activity
            .iter()
            .filter(|a| f.matches(&a.source))
            .filter(|a| self.ui.activity_game.is_none() || a.product_id == self.ui.activity_game)
            .collect()
    }

    /// A on an entry: its game.
    pub fn activity_open(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(id) = self.activity_rows().get(i).and_then(|a| a.product_id) {
            self.open_game(id, cx);
        }
    }

    /// Keep the focused entry (or the section list) in view.
    pub fn scroll_global_focus(&mut self) {
        match (self.nav.page, self.nav.zone) {
            (Page::Activity, Zone::Body) => {
                let rows = self.activity_rows();
                let want = self.nav.body_index();
                // Children of the scroll view: the header, then the list's items.
                if let Some(ix) = items(&rows).iter().position(|it| matches!(it, Item::Row(i) if *i == want)) {
                    let ix = if want == 0 { 0 } else { ix + 1 };
                    self.body_scroll.scroll_to_item(ix);
                }
            }
            (Page::Settings, Zone::Side) => self.body_scroll.set_offset(gpui::point(px(0.), px(0.))),
            _ => {}
        }
    }

    pub fn render_activity(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        let filter = self.ui.source_filter();
        let rows: Vec<ActivityEntry> = self.activity_rows().into_iter().cloned().collect();
        let row_refs: Vec<&ActivityEntry> = rows.iter().collect();

        // Source filter: segmented, with counts (of the game filter's entries).
        let in_game: Vec<&ActivityEntry> =
            self.activity.iter().filter(|a| self.ui.activity_game.is_none() || a.product_id == self.ui.activity_game).collect();
        let seg = div()
            .flex()
            .flex_none()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line2)
            .when(pad, |d| d.child(div().flex().items_center().px(px(4.)).child(glyph(th, "LT", true))))
            .children(SourceFilter::ALL.into_iter().map(|f| {
                let on = f == filter;
                let n = in_game.iter().filter(|a| f.matches(&a.source)).count();
                let fg = if on { th.tx1 } else { th.tx2 };
                div()
                    .id(("src", f as usize))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(fg)
                    .cursor_pointer()
                    .when(on, |d| d.bg(th.ink5))
                    .when(!on, |d| d.hover(|s| s.bg(th.ink3)))
                    .when_some(f.icon(), |d, i| d.child(icon(i, 12., fg)))
                    .child(f.label())
                    .child(div().text_color(if on { th.tx2 } else { th.tx4 }).child(n.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_activity_source(f);
                        cx.notify();
                    }))
            }))
            .when(pad, |d| d.child(div().flex().items_center().px(px(4.)).child(glyph(th, "RT", true))));

        // Game filter: a button that opens a list of the games in the log.
        let game_label = match self.ui.activity_game {
            Some(id) => self.game_name(id),
            None => "All games".into(),
        };
        let game_button = div()
            .id("game-filter")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(if self.ui.activity_game.is_some() { th.acc_soft } else { th.ink4 })
            .border_1()
            .border_color(if self.ui.activity_game.is_some() { th.acc_line } else { th.line2 })
            .text_size(px(12.5))
            .font_weight(MEDIUM)
            .text_color(if self.ui.activity_game.is_some() { th.acc_hi } else { th.tx1 })
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ui.activity_games_open = !this.ui.activity_games_open;
                cx.notify();
            }))
            .child(icon(Icon::LibraryBig, 13., th.tx3))
            .child(game_label)
            .child(icon(Icon::ChevronDown, 13., th.tx3))
            .when(pad, |d| d.child(glyph(th, "Y", true)));
        let clear = self.ui.activity_game.is_some().then(|| {
            div()
                .id("game-filter-clear")
                .flex()
                .items_center()
                .gap(px(4.))
                .text_size(px(12.))
                .text_color(th.tx3)
                .cursor_pointer()
                .hover(|s| s.text_color(th.tx1))
                .child(icon(Icon::X, 12., th.tx3))
                .child("Clear")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ui.activity_game = None;
                    cx.notify();
                }))
        });

        let checked = self.ui.activity_checked.map(|t| t.elapsed().as_secs());
        let live = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.))
            .text_size(px(12.))
            .text_color(th.tx3)
            .child(div().size(px(7.)).rounded_full().bg(if self.ui.link_up() { th.ok } else { th.warn }))
            .child(match checked {
                Some(s) if s < 2 => "Live · just checked".to_owned(),
                Some(s) => format!("Live · checked {s} s ago"),
                None => "Live".to_owned(),
            });

        let header = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .pt(px(24.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(14.))
                    .child(display(22., FontWeight::BOLD, th.tx1).child("Activity"))
                    .child(div().text_color(th.tx3).child("Everything that changed, and who changed it"))
                    .child(div().flex_1())
                    .child(live),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(seg)
                    .child(game_button)
                    .when_some(clear, |d, c| d.child(c))
                    .child(div().flex_1())
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!(
                        "{} of {} entries",
                        rows.len(),
                        self.activity.len()
                    ))),
            );

        // The list.
        let mut children: Vec<AnyElement> = vec![header.into_any_element()];
        let list_items = items(&row_refs);
        let n_items = list_items.len();
        for (k, it) in list_items.into_iter().enumerate() {
            let first = k == 0;
            let last = k + 1 == n_items;
            let el = match it {
                Item::Day(label) => div()
                    .px(px(18.))
                    .pt(px(if first { 14. } else { 18. }))
                    .pb(px(8.))
                    .bg(th.ink2)
                    .border_x_1()
                    .border_color(th.line1)
                    .when(first, |d| d.border_t_1().rounded_t(px(12.)))
                    .child(caption(&label, th.tx3))
                    .into_any_element(),
                Item::Row(i) => self.render_activity_row(th, rows[i].clone(), i, last, cx),
            };
            children.push(el);
        }
        if rows.is_empty() {
            children.push(
                card(th)
                    .items_center()
                    .gap(px(6.))
                    .py(px(36.))
                    .child(icon(Icon::Activity, 20., th.tx4))
                    .child(div().text_color(th.tx2).child(if self.activity.is_empty() {
                        "Nothing yet. Installs, launches, settings changes and sign-ins show up here."
                    } else {
                        "Nothing from this source yet."
                    }))
                    .into_any_element(),
            );
        }

        let main = div()
            .id("page")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .px(px(28.))
            .pb(px(28.))
            .children(children);

        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .child(main)
            .child(self.render_activity_side(th))
            .when(self.ui.activity_games_open, |d| d.child(self.render_game_filter_menu(th, cx)))
    }

    fn render_activity_row(&mut self, th: &Theme, a: ActivityEntry, i: usize, last: bool, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focused(Target::Body(i));
        let (color, i_) = match actor(&a.source) {
            Actor::App => (th.who_gui, Icon::MousePointer),
            Actor::Cli => (th.who_cli, Icon::SquareTerminal),
            Actor::Agent => (th.who_agent, Icon::Bot),
            Actor::System => (th.who_sys, Icon::Cog),
        };
        let g = a.product_id.and_then(|id| self.game(id)).cloned();
        let thumb: AnyElement = match &g {
            Some(g) => {
                let art = self.art_for(g, ArtSlot::Tile, Variant::Tile, cx);
                div().w(px(20.)).h(px(24.)).flex_none().child(art_fill(th, &art, "", 3., false)).into_any_element()
            }
            None => div().w(px(20.)).flex().justify_center().child(icon(Icon::Cog, 13., th.tx4)).into_any_element(),
        };
        // The action, with the game's name in it set in bold.
        let mut text = sentence(&clean_name(&a.action));
        let mut highlights = vec![];
        if let Some(g) = &g {
            let full = clean_name(&g.entry.name);
            // "issued launch context for 3539": name the game instead of its id.
            if !text.contains(&full) {
                text = replace_word(&text, &g.id().to_string(), &full);
            }
            if let Some(pos) = text.find(&full) {
                highlights.push((pos..pos + full.len(), HighlightStyle { font_weight: Some(FontWeight::SEMIBOLD), color: Some(th.tx1), ..Default::default() }));
            }
        }
        let named = !highlights.is_empty();
        let action = StyledText::new(text).with_highlights(highlights);
        let game_suffix = g.as_ref().filter(|_| !named).map(|g| short_name(&g.entry.name));
        let pid = a.product_id;
        let open = pid.is_some();
        div()
            .id(("act", i))
            .relative()
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(18.))
            .py(px(11.))
            .bg(if focused { th.ink3 } else { th.ink2 })
            .border_x_1()
            .border_t_1()
            .border_color(th.line1)
            .when(last, |d| d.border_b_1().rounded_b(px(12.)))
            .when(focused, |d| d.child(div().absolute().left(px(0.)).top(px(0.)).bottom(px(0.)).w(px(3.)).bg(color)))
            .focus_ring(focused && self.mode != InputMode::Pointer, th, 4.)
            .when(open, |d| d.cursor_pointer().hover(|s| s.bg(th.ink3)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, Target::Body(i));
                if let Some(id) = pid {
                    this.open_game(id, cx);
                }
                cx.notify();
            }))
            .child(mono(12., th.tx3).w(px(48.)).flex_none().child(clock(a.at)))
            .child(
                div().w(px(150.)).flex_none().flex().child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .px(px(7.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(color.opacity(0.13))
                        .text_size(px(11.5))
                        .font_weight(SEMIBOLD)
                        .text_color(color)
                        .max_w(px(150.))
                        .child(icon(i_, 12., color))
                        .child(div().truncate().child(actor_label(&a.source))),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(10.))
                    .child(thumb)
                    .child(div().min_w_0().truncate().text_color(th.tx2).child(action))
                    .when_some(game_suffix, |d, n| {
                        d.child(div().flex_none().text_color(th.tx4).child("·")).child(div().flex_none().font_weight(SEMIBOLD).child(n))
                    }),
            )
            .child(mono(11., th.tx4).flex_none().child(ago_short(a.at)))
            .when(open, |d| d.child(icon(Icon::ChevronRight, 14., if focused { th.tx2 } else { th.tx4 })))
            .into_any_element()
    }

    fn render_activity_side(&self, th: &Theme) -> impl IntoElement + use<> {
        // Counts per source in what's loaded.
        let mut by: BTreeMap<&'static str, (usize, gpui::Hsla)> = BTreeMap::new();
        for a in &self.activity {
            let (k, c) = match actor(&a.source) {
                Actor::App => ("1 App", th.who_gui),
                Actor::Cli => ("2 CLI", th.who_cli),
                Actor::Agent => ("3 Agents", th.who_agent),
                Actor::System => ("4 System", th.who_sys),
            };
            by.entry(k).or_insert((0, c)).0 += 1;
        }
        let max = by.values().map(|v| v.0).max().unwrap_or(1).max(1);
        let mut agents: BTreeMap<&str, usize> = BTreeMap::new();
        for a in self.activity.iter().filter(|a| actor(&a.source) == Actor::Agent) {
            *agents.entry(a.source.as_str()).or_default() += 1;
        }
        let since = self.activity.last().map(|a| crate::model::day(a.at)).unwrap_or_default();
        let cli = "quiver activity -n 100";
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(290.))
            .h_full()
            .gap(px(18.))
            .px(px(20.))
            .pt(px(24.))
            .bg(th.ink0)
            .border_l_1()
            .border_color(th.line1)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(caption("Who did what", th.tx3))
                    .children(by.into_iter().map(|(k, (n, c))| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .text_size(px(12.))
                            .child(div().w(px(56.)).text_color(th.tx2).child(k[2..].to_owned()))
                            .child(div().flex_1().child(super::progress(th, n as f32 / max as f32, None, 6., Some(c))))
                            .child(mono(11.5, th.tx3).w(px(26.)).flex().justify_end().child(n.to_string()))
                    }))
                    .when(!since.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(th.tx4).child(format!("Since {since}, {} entries", self.activity.len())))),
            )
            .child(div().h(px(1.)).bg(th.line1))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(caption("Agents seen", th.tx3))
                    .when(agents.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(th.tx3).child("None yet")))
                    .children(agents.into_iter().map(|(name, n)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(12.5))
                            .child(div().size(px(7.)).rounded_full().bg(th.who_agent))
                            .child(div().flex_1().font_weight(SEMIBOLD).child(name.to_owned()))
                            .child(mono(11.5, th.tx3).child(n.to_string()))
                    }))
                    .child(div().text_size(px(11.5)).line_height(px(16.)).text_color(th.tx3).child(
                        "Any client whose --source isn't gui or cli counts as an agent, labelled with that name.",
                    )),
            )
            .child(div().h(px(1.)).bg(th.line1))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().flex().items_center().gap(px(8.)).child(caption("Approvals", th.tx3)).child(pill(th, PillKind::Future, None, "Not wired yet")))
                    .child(div().text_size(px(12.)).line_height(px(17.)).text_color(th.tx3).child(
                        "Agent requests that need your OK will wait here, with Allow and Deny. Restoring saves already asks.",
                    )),
            )
            .child(div().h(px(1.)).bg(th.line1))
            .child(div().flex().flex_col().gap(px(8.)).child(caption("From a terminal", th.tx3)).child(command_line(th, cli)))
            .child(div().flex_1())
            .child(
                div()
                    .pb(px(16.))
                    .text_size(px(11.5))
                    .text_color(th.tx4)
                    .child(if self.mode == InputMode::Pad { "LT/RT source · Y only this game · A open" } else { "Click an entry to open its game" }),
            )
    }

    fn render_game_filter_menu(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Games in the log, most entries first.
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for a in &self.activity {
            if let Some(id) = a.product_id {
                *counts.entry(id).or_default() += 1;
            }
        }
        let mut games: Vec<(u32, usize)> = counts.into_iter().collect();
        games.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| self.game_name(a.0).cmp(&self.game_name(b.0))));
        let current = self.ui.activity_game;
        let mut rows: Vec<AnyElement> = vec![];
        let all_on = current.is_none();
        rows.push(
            div()
                .id("gf-all")
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .h(px(32.))
                .rounded(px(7.))
                .cursor_pointer()
                .when(all_on, |d| d.bg(th.acc_soft))
                .hover(|s| s.bg(th.ink3))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ui.activity_game = None;
                    this.ui.activity_games_open = false;
                    cx.notify();
                }))
                .child(div().w(px(18.)).child(icon(Icon::LibraryBig, 13., th.tx3)))
                .child(div().flex_1().font_weight(if all_on { SEMIBOLD } else { MEDIUM }).child("All games"))
                .child(mono(11.5, th.tx3).child(self.activity.len().to_string()))
                .into_any_element(),
        );
        for (id, n) in games.into_iter().take(12) {
            let on = current == Some(id);
            let g = self.game(id).cloned();
            let art = match &g {
                Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
                None => crate::art::ArtState::Failed,
            };
            rows.push(
                div()
                    .id(("gf", id as usize))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .h(px(34.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(th.acc_soft))
                    .hover(|s| s.bg(th.ink3))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ui.activity_game = Some(id);
                        this.ui.activity_games_open = false;
                        this.nav.focus(&this.layout, Target::Body(0));
                        cx.notify();
                    }))
                    .child(div().w(px(18.)).h(px(21.)).flex_none().child(art_fill(th, &art, "", 3., false)))
                    .child(div().flex_1().min_w_0().truncate().font_weight(if on { SEMIBOLD } else { MEDIUM }).child(self.game_name(id)))
                    .child(mono(11.5, th.tx3).child(n.to_string()))
                    .into_any_element(),
            );
        }
        div()
            .id("gf-scrim")
            .absolute()
            .inset_0()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ui.activity_games_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("gf-menu")
                    .absolute()
                    .top(px(108.))
                    .left(px(28. + 436.))
                    .w(px(300.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .p(px(6.))
                    .rounded(px(12.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line2)
                    .shadow(vec![super::soft_shadow(crate::theme::rgba(0x000000, 0.55), 18., 50.)])
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .children(rows),
            )
            .into_any_element()
    }
}

/// `text` with `word` (a whole word) replaced by `with`.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find(word) {
        let before = rest[..pos].chars().last();
        let after = rest[pos + word.len()..].chars().next();
        let whole = !before.is_some_and(|c| c.is_alphanumeric()) && !after.is_some_and(|c| c.is_alphanumeric());
        out.push_str(&rest[..pos]);
        out.push_str(if whole { with } else { word });
        rest = &rest[pos + word.len()..];
    }
    out.push_str(rest);
    out
}

/// "4m", "3h", "2d" for the row's right edge.
fn ago_short(t: DateTime<Utc>) -> String {
    let m = (Utc::now() - t).num_minutes().max(0);
    match m {
        0 => "now".into(),
        1..=59 => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        _ => format!("{}d", m / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn entry(mins: i64) -> ActivityEntry {
        ActivityEntry { at: Utc::now() - Duration::minutes(mins), source: "gui".into(), action: "x".into(), product_id: None }
    }

    #[test]
    fn day_headings_split_rows() {
        let a = [entry(1), entry(2), entry(60 * 24 * 3), entry(60 * 24 * 3 + 5)];
        let refs: Vec<&ActivityEntry> = a.iter().collect();
        let it = items(&refs);
        // Today: heading + 2 rows (unless the test runs right after midnight); then another day.
        let headings = it.iter().filter(|i| matches!(i, Item::Day(_))).count();
        assert!((2..=3).contains(&headings));
        assert_eq!(it.iter().filter(|i| matches!(i, Item::Row(_))).count(), 4);
        assert_eq!(sentence("install started"), "Install started");
        assert_eq!(replace_word("context for 3539 as x", "3539", "AC Origins"), "context for AC Origins as x");
        assert_eq!(replace_word("r35390 3539", "3539", "Y"), "r35390 Y");
        assert_eq!(ago_short(Utc::now() - Duration::minutes(90)), "1h");
    }
}
