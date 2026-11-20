//! Drawing the Mods and Tweaks sections (the SightMods, SightDockedMods and SightDockedTweaks
//! mock-ups), and their hints. The model is [`super::mods`].
//!
//! - **Centred Mods**: three columns. The surface list ("In this session"); the picked
//!   surface's card (page tabs between LT and RT, the preset picker, its option rows, the
//!   Restart bar with Undo); and on the right the readouts ("DLSS right now"), "This session"
//!   (Revert this session, Session only) and "Locked in game".
//! - **Docked** (500 px): the same stacked: the surface on a line of its own (B, then ▲ ▼
//!   picks another), the page tabs, the rows, the bars and cards under them.
//! - **Tweaks**: "Saving to …", its pages and rows, the Restart bar.
//!
//! An option row: label, badge and help on the left; the control on the right (a slider with
//! − / + for numbers with a range, ‹ › for choices, a switch for toggles) with its result line
//! under it ([`mods::row_line`]).

use std::time::Instant;

use gpui::{AnyElement, Context, Div, IntoElement, ParentElement, SharedString, Stateful, Styled, div, prelude::*, px, relative};
use quiver_api::options::{OptionState, OptionType, SurfaceKind, SurfaceState};
use quiver_api::perf::Stat;
use quiver_ui::components::{FocusExt, MEDIUM, SEMIBOLD, caption, glyph, pill};
use quiver_ui::input::Input;
use quiver_ui::options::{fraction, short_value};
use quiver_ui::theme::Theme;
use serde_json::Value;

use super::mods::{self, Changed};
use crate::panel::hints::{Glyph, Hints};
use crate::panel::state::{Act, Row, Zone, adjustable};
use crate::panel::view::{MARK_PAD, Mark, PanelView, card, mark, switch, tone_color, tone_pill};

// ---- Hints -----------------------------------------------------------------------------------

fn act_label(a: Act, v: &PanelView) -> &'static str {
    match a {
        Act::UndoRestart { .. } => "Undo",
        Act::RevertSession => "Revert this session",
        Act::SessionOnly if v.log.session_only => "Save changes again",
        Act::SessionOnly => "Session only",
    }
}

/// Mods' and Tweaks' hints: adjust, reset (with the default and range on the right), pages,
/// and Y (pin; held on a live option, compare).
pub fn hints(v: &PanelView, h: &mut Hints) {
    let docked = v.docked();
    if v.nav.zone == Zone::Surfaces {
        h.add([Glyph::UpDown], "Choose").add([Glyph::A], "Open");
        return;
    }
    let rows = v.rows();
    let focused = v.nav.focused_row(&rows).and_then(|i| rows.get(i));
    match focused {
        Some(r @ (Row::Opt { .. } | Row::Tweak { .. })) => {
            if let Some(st) = v.option_state(r) {
                if adjustable(st) {
                    h.add([Glyph::LeftRight], if docked { "Change" } else { "Adjust" });
                }
                let compare = v.compare_target(r).map(|t| short_value(&st.def, &t));
                match (compare, docked) {
                    (Some(t), true) => h.add([Glyph::Y], format!("Hold: compare with {t}")),
                    (Some(t), false) => h.add([Glyph::Y], format!("Pin · hold: compare with {t}")),
                    (None, _) => h.add([Glyph::Y], "Pin to quick menu"),
                };
                if v.surface_of(r).and_then(|s| mods::reset_op(&s.key, "", st)).is_some() {
                    h.add([Glyph::X], if docked { "Reset" } else { "Reset to default" });
                }
                if let Some(d) = mods::range_detail(st) {
                    h.detail(d);
                }
            }
        }
        Some(Row::Preset { .. }) => {
            h.add([Glyph::LeftRight], "Preset");
        }
        Some(Row::Act(a)) => {
            h.add([Glyph::A], act_label(*a, v));
        }
        _ => {}
    }
    if pages_here(v) > 1 {
        h.add([Glyph::LT, Glyph::RT], "Pages");
    }
}

fn pages_here(v: &PanelView) -> usize {
    match v.nav.section {
        quiver_api::sight::PanelSection::Mods => mods::surface(&v.data, &v.nav).map_or(0, |s| mods::pages(s).len()),
        _ => v.data.tweaks.ready().map_or(0, |s| mods::pages(s).len()),
    }
}

// ---- Pieces ----------------------------------------------------------------------------------

/// A small bordered button for row `i` (Undo, Revert this session, Session only): A or a click.
fn button(v: &PanelView, th: &Theme, i: usize, label: &str, focused: bool, cx: &mut Context<PanelView>) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("row-{i}")))
        .relative()
        .flex_none()
        .h(px(30.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(th.line2)
        .text_size(px(13.))
        .font_weight(SEMIBOLD)
        .text_color(th.tx1)
        .when(focused, |d| d.bg(th.ink4))
        .cursor_pointer()
        .hover(|d| d.bg(th.ink3))
        .focus_ring(focused, th, 8.)
        .when(focused, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
        .child(label.to_owned())
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            this.input(Input::Activate, cx);
        }))
}

/// A square arrow button that steps row `i` (`dir` −1 / +1).
fn arrow(th: &Theme, id: String, g: &'static str, i: usize, dir: i32, size: f32, cx: &mut Context<PanelView>) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex_none()
        .size(px(size))
        .rounded(px(8.))
        .border_1()
        .border_color(th.line2)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.))
        .font_weight(SEMIBOLD)
        .text_color(th.tx2)
        .cursor_pointer()
        .hover(|d| d.bg(th.ink4))
        .child(g)
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            if let Some(r) = rows.get(i).cloned() {
                this.adjust(r, dir, cx);
            }
            cx.notify();
        }))
}

/// The control for an option: a switch, a slider with − / +, or ‹ value ›.
fn control(th: &Theme, i: usize, st: &OptionState, val: &Value, docked: bool, cx: &mut Context<PanelView>) -> Div {
    let def = &st.def;
    let text = short_value(def, val);
    if !adjustable(st) {
        return div().flex().justify_end().text_size(px(14.)).text_color(th.tx3).child(text);
    }
    let changed = !quiver_api::options::same_value(val, &st.default);
    let arr = if docked { 26. } else { 30. };
    match def.ty {
        OptionType::Bool => {
            let on = val.as_bool().unwrap_or(false);
            div().flex().justify_end().child(
                div()
                    .id(SharedString::from(format!("sw-{i}")))
                    .cursor_pointer()
                    .child(switch(th, on))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let rows = this.rows();
                        this.nav.focus_row(&rows, i);
                        if let Some(r) = rows.get(i).cloned() {
                            this.adjust(r, 0, cx);
                        }
                        cx.notify();
                    })),
            )
        }
        OptionType::Int | OptionType::Float if def.min.is_some() && def.max.is_some() => {
            let f = fraction(def, val).unwrap_or(0.0);
            let track = div()
                .flex_1()
                .h(px(6.))
                .rounded(px(3.))
                .bg(th.ink4)
                .relative()
                .child(div().absolute().left_0().top_0().h(px(6.)).rounded(px(3.)).bg(th.acc).w(relative(f)))
                .child(div().absolute().top(px(-6.)).left(relative(f)).ml(px(-9.)).size(px(18.)).rounded(px(9.)).bg(th.tx1).border(px(3.)).border_color(th.acc));
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(arrow(th, format!("dec-{i}"), "−", i, -1, arr, cx))
                .child(track)
                .child(arrow(th, format!("inc-{i}"), "+", i, 1, arr, cx))
                .child(div().min_w(px(if docked { 44. } else { 64. })).flex().justify_end().text_size(px(15.)).font_weight(SEMIBOLD).text_color(if changed { th.tx1 } else { th.tx2 }).child(text))
        }
        _ => div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(arrow(th, format!("prev-{i}"), "‹", i, -1, arr, cx))
            .child(div().flex_1().flex().justify_center().text_size(px(15.)).font_weight(SEMIBOLD).text_color(if changed { th.tx1 } else { th.tx2 }).child(text))
            .child(arrow(th, format!("next-{i}"), "›", i, 1, arr, cx)),
    }
}

/// Who answers for a surface's changes, for result lines ("DLSS", "Torchbearer"; nobody
/// for Quiver's own Tweaks).
fn who(s: &SurfaceState) -> String {
    if s.kind == SurfaceKind::Platform { String::new() } else { s.title.clone() }
}

/// One option row, two columns (centred) or stacked (docked).
fn option_row(v: &PanelView, th: &Theme, i: usize, row: &Row, focused: bool, docked: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let (Some(st), Some(s), Some(key), Some(val)) = (v.option_state(row), v.surface_of(row), PanelView::row_key(row), v.shown_value(row)) else { return div().into_any_element() };
    let tweaks = matches!(row, Row::Tweak { .. });
    let change: Option<&Changed> = v.log.get(tweaks, &s.key, &key.1);
    let line = mods::row_line(st, v.saving.get(&key), change, &who(s), Instant::now());
    let (badge, tone) = mods::badge(st, s);
    let label = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .min_w_0()
        .child(div().text_size(px(15.)).font_weight(SEMIBOLD).text_color(th.tx1).truncate().child(st.def.label_or(&key.1).to_owned()))
        .child(pill(th, tone_pill(tone), None, badge));
    let help = st.def.help.clone().map(|h| div().text_size(px(if docked { 12.5 } else { 13. })).text_color(th.tx2).line_clamp(2).child(h));
    let line_el = line.map(|(t, tone)| div().text_size(px(12.)).font_weight(SEMIBOLD).text_color(tone_color(th, tone)).child(t));
    let shell = div()
        .id(SharedString::from(format!("row-{i}")))
        .relative()
        .rounded(px(10.))
        .border_1()
        .border_color(if focused { th.acc_hi } else { gpui::transparent_black() })
        .when(focused, |d| d.bg(th.acc.opacity(0.08)))
        .cursor_pointer()
        .focus_ring(focused, th, 10.)
        .when(focused, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            cx.notify();
        }));
    let ctl = control(th, i, st, &val, docked, cx);
    if docked {
        return shell
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(14.))
            .py(px(12.))
            .child(div().flex().items_center().gap(px(10.)).child(div().flex_1().min_w_0().child(label)).child(div().w(px(210.)).flex_none().child(ctl)))
            .when_some(help, |d, h| d.child(h))
            .when_some(line_el, |d, l| d.child(l))
            .into_any_element();
    }
    shell
        .flex()
        .items_center()
        .gap(px(20.))
        .px(px(18.))
        .py(px(14.))
        .child(div().flex_1().min_w_0().flex().flex_col().gap(px(3.)).child(label).when_some(help, |d, h| d.child(h)))
        .child(div().w(px(380.)).flex_none().flex().flex_col().gap(px(6.)).child(ctl).when_some(line_el, |d, l| d.child(div().flex().justify_end().child(l))))
        .into_any_element()
}

/// The page tabs between LT and RT, and the preset picker.
fn page_bar(v: &PanelView, th: &Theme, s: &SurfaceState, rows: &[Row], focused: Option<usize>, docked: bool, cx: &mut Context<PanelView>) -> Div {
    let pages = mods::pages(s);
    let cur = v.nav.pages.page(&s.key, pages.len());
    let key = s.key.clone();
    let tabs = pages.iter().enumerate().map(|(n, p)| {
        let on = n == cur;
        let key = key.clone();
        div()
            .id(SharedString::from(format!("page-{n}")))
            .h(px(30.))
            .px(px(12.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .text_size(px(13.))
            .font_weight(SEMIBOLD)
            .text_color(if on { th.tx1 } else { th.tx2 })
            .when(on, |d| d.bg(th.ink4))
            .cursor_pointer()
            .hover(|d| d.bg(th.ink3))
            .child(p.title.clone())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.pages.set_page(&key, n);
                this.nav.zone = Zone::Body;
                this.nav.set_focus_index(0);
                cx.notify();
            }))
    }).collect::<Vec<_>>();
    let preset = rows
        .iter()
        .position(|r| matches!(r, Row::Preset { .. }))
        .map(|i| div().flex().flex_none().items_center().gap(px(8.)).child(div().text_size(px(12.)).text_color(th.tx3).child("Preset")).child(preset_picker(v, th, i, s, focused == Some(i), cx)));
    let tabs = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(if docked { 4. } else { 8. }))
        .when(pages.len() > 1, |d| d.child(glyph(th, "LT", false)).children(tabs).child(glyph(th, "RT", false)))
        .when(pages.len() == 1, |d| d.child(div().text_size(px(13.)).font_weight(SEMIBOLD).text_color(th.tx1).child(pages[0].title.clone())));
    if docked {
        // A narrow column: the preset on a line of its own.
        return div().flex().flex_col().gap(px(8.)).child(tabs).when_some(preset, |d, p| d.child(p));
    }
    div().flex().flex_none().items_center().gap(px(8.)).child(tabs).child(div().flex_1()).when_some(preset, |d, p| d.child(p))
}

fn preset_picker(v: &PanelView, th: &Theme, i: usize, s: &SurfaceState, focused: bool, cx: &mut Context<PanelView>) -> Stateful<Div> {
    let pick = v.preset_drafts.get(&s.key).copied().or_else(|| mods::preset_now(s));
    let label = pick.and_then(|p| s.presets.get(p)).map(|p| p.label.clone()).unwrap_or_else(|| "Custom".into());
    div()
        .id(SharedString::from(format!("row-{i}")))
        .relative()
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(30.))
        .px(px(6.))
        .rounded(px(8.))
        .border_1()
        .border_color(th.line2)
        .when(focused, |d| d.bg(th.ink4))
        .focus_ring(focused, th, 8.)
        .when(focused, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
        .child(arrow(th, format!("pp-{i}"), "‹", i, -1, 22., cx))
        .child(div().px(px(4.)).text_size(px(13.)).font_weight(SEMIBOLD).text_color(th.tx1).child(label))
        .child(arrow(th, format!("pn-{i}"), "›", i, 1, 22., cx))
}

/// The option rows (every row, so the list's children line up with the rows for scrolling;
/// the preset picker and the buttons are drawn elsewhere).
fn row_list(v: &PanelView, th: &Theme, rows: &[Row], focused: Option<usize>, docked: bool, cx: &mut Context<PanelView>) -> Vec<AnyElement> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| match r {
            Row::Opt { .. } | Row::Tweak { .. } => option_row(v, th, i, r, focused == Some(i), docked, cx),
            Row::Heading { title, .. } => div().px(px(14.)).pt(px(12.)).pb(px(2.)).child(caption(title, th.tx3)).into_any_element(),
            Row::Preset { .. } | Row::Act(_) => div().into_any_element(),
            _ => v.render_row(th, i, r, focused == Some(i), cx),
        })
        .collect()
}

/// "Render scale 0.50 → 0.58 is saved and applies next launch." with Undo.
fn restart_bar(v: &PanelView, th: &Theme, rows: &[Row], focused: Option<usize>, tweaks: bool, cx: &mut Context<PanelView>) -> Option<Div> {
    let i = rows.iter().position(|r| *r == Row::Act(Act::UndoRestart { tweaks }))?;
    let w = v.log.waiting(tweaks);
    let what = |c: &Changed| format!("{} {} → {}", c.label, c.first_text, c.after_text);
    let text = match w.as_slice() {
        [one] if tweaks => format!("1 change waits for the next launch: {}", what(one)),
        [one] => format!("{} is saved and applies next launch.", what(one)),
        many => format!("{} changes wait for the next launch: {}", many.len(), many.iter().map(|c| what(c)).collect::<Vec<_>>().join(", ")),
    };
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .rounded(px(10.))
            .bg(if tweaks { th.warn_soft } else { th.ink3 })
            .text_size(px(13.))
            .text_color(if tweaks { th.warn } else { th.tx1 })
            .when(!tweaks, |d| d.child(pill(th, quiver_ui::components::PillKind::Muted, None, "RESTART")))
            .child(div().flex_1().min_w_0().child(text))
            .child(button(v, th, i, "Undo", focused == Some(i), cx)),
    )
}

/// The buttons of "This session" that are there now.
fn session_buttons(v: &PanelView, th: &Theme, rows: &[Row], focused: Option<usize>, cx: &mut Context<PanelView>) -> Vec<Stateful<Div>> {
    [Act::RevertSession, Act::SessionOnly]
        .into_iter()
        .filter_map(|a| rows.iter().position(|r| *r == Row::Act(a)).map(|i| button(v, th, i, act_label(a, v), focused == Some(i), cx)))
        .collect()
}

fn profile_name(v: &PanelView) -> String {
    v.data.session.ready().and_then(|s| s.profile_name.clone()).unwrap_or_else(|| "Vanilla".into())
}

fn session_words(v: &PanelView) -> (String, String) {
    let n = v.log.count(false);
    let head = match n {
        0 => "Nothing changed in game yet".to_owned(),
        1 => "1 option changed in game".to_owned(),
        n => format!("{n} options changed in game"),
    };
    let sub = if v.log.session_only {
        "Session only: the game has them, the profile doesn't; they go when the game closes.".to_owned()
    } else {
        format!("Saved to profile {} as you change them.", profile_name(v))
    };
    (head, sub)
}

/// "DLSS right now": the surface's readouts and their live values.
fn readouts(v: &PanelView, th: &Theme, s: &SurfaceState) -> Option<Div> {
    if s.kind == SurfaceKind::Platform {
        return None;
    }
    let list = v.data.readouts.get(&s.key).cloned().unwrap_or_default();
    let value = |id: &str| {
        let m = v.data.metrics.get(id)?;
        let pv = m.get(&Stat::Last).or_else(|| m.values().next())?;
        Some(if pv.unit.is_empty() { pv.text.clone() } else { format!("{} {}", pv.text, pv.unit) })
    };
    let rows = list.iter().map(|(id, label)| {
        div()
            .flex()
            .justify_between()
            .gap(px(12.))
            .py(px(9.))
            .border_t_1()
            .border_color(th.line1)
            .text_size(px(14.))
            .child(div().text_color(th.tx2).child(label.clone()))
            .child(div().text_color(th.tx1).child(value(id).unwrap_or_else(|| "—".into())))
    });
    Some(
        card(th)
            .p(px(18.))
            .flex()
            .flex_col()
            .child(div().pb(px(10.)).child(caption(&format!("{} right now", s.title), th.tx3)))
            .children(rows)
            .when(list.is_empty(), |d| {
                d.child(div().text_size(px(12.)).text_color(th.tx3).child(if s.kind == SurfaceKind::Mod {
                    "No readouts: this mod doesn't publish any (they come through the Quiver SDK)."
                } else {
                    "No readouts for this surface."
                }))
            }),
    )
}

fn locked_card(th: &Theme, s: &SurfaceState) -> Option<Div> {
    let l = mods::locked(s);
    if l.is_empty() {
        return None;
    }
    Some(
        card(th).p(px(18.)).flex().flex_col().gap(px(6.)).child(div().pb(px(4.)).child(caption("Locked in game", th.tx3))).children(
            l.into_iter().map(|(label, why)| div().text_size(px(14.)).text_color(th.tx2).child(format!("{label} · marked dangerous by the mod ({why}). Change it in the app."))),
        ),
    )
}

/// The profile's mods that have no option surface ("Skip intros has no options.").
fn optionless(v: &PanelView, list: &[SurfaceState]) -> Vec<String> {
    let Some(s) = v.data.session.ready() else { return Vec::new() };
    s.mods
        .iter()
        .map(|m| {
            let name = m.split(" (").next().unwrap_or(m);
            let name = match name.rsplit_once(' ') {
                Some((a, b)) if b.starts_with('v') && b[1..].chars().next().is_some_and(|c| c.is_ascii_digit()) => a,
                _ => name,
            };
            name.trim().to_owned()
        })
        .filter(|name| !name.is_empty() && !list.iter().any(|x| x.title.eq_ignore_ascii_case(name) || x.mod_id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(name))))
        .collect()
}

/// The centred layout's surface list.
fn surface_list(v: &PanelView, th: &Theme, list: &[SurfaceState], cx: &mut Context<PanelView>) -> Div {
    let cur = v.nav.pages.surface.min(list.len().saturating_sub(1));
    let in_list = v.nav.zone == Zone::Surfaces && v.nav.confirm.is_none();
    let none = optionless(v, list);
    card(th)
        .w(px(270.))
        .flex_none()
        .h_full()
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().px(px(8.)).pt(px(8.)).pb(px(6.)).child(caption("In this session", th.tx3)))
        .children(list.iter().enumerate().map(|(j, s)| {
            let on = j == cur;
            div()
                .id(SharedString::from(format!("surface-{j}")))
                .relative()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .when(on, |d| d.bg(th.acc.opacity(0.14)).border_l_2().border_color(th.acc))
                .cursor_pointer()
                .hover(|d| d.bg(th.ink3))
                .focus_ring(on && in_list, th, 10.)
                .when(on && in_list, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
                .child(div().text_size(px(15.)).font_weight(MEDIUM).text_color(th.tx1).truncate().child(mods::surface_title(s)))
                .child(div().text_size(px(12.)).text_color(th.tx3).truncate().child(mods::surface_where(s)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav.pages.surface = j;
                    this.nav.zone = Zone::Surfaces;
                    cx.notify();
                }))
        }))
        .child(div().flex_1())
        .children(none.into_iter().map(|n| div().px(px(8.)).pb(px(6.)).text_size(px(12.)).text_color(th.tx3).child(format!("{n} has no options."))))
}

/// The docked layout's surface line: the surface, the profile, how it reaches the game.
fn surface_line(v: &PanelView, th: &Theme, list: &[SurfaceState], s: &SurfaceState, cx: &mut Context<PanelView>) -> Stateful<Div> {
    let in_list = v.nav.zone == Zone::Surfaces && v.nav.confirm.is_none();
    let cur = v.nav.pages.surface.min(list.len().saturating_sub(1));
    let live = mods::reaches_game(s);
    div()
        .id("surface-line")
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .mx(px(8.))
        .mb(px(8.))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(8.))
        .text_size(px(12.5))
        .text_color(th.tx2)
        .when(in_list, |d| d.bg(th.ink3).child(mark(&v.marks, Mark::Focus, MARK_PAD)))
        .focus_ring(in_list, th, 8.)
        .cursor_pointer()
        .when(list.len() > 1, |d| d.child(glyph(th, if in_list { "▲ ▼" } else { "B" }, !in_list)))
        .child(div().font_weight(SEMIBOLD).text_color(th.tx1).child(mods::surface_title(s)))
        .child(div().truncate().child(format!("· profile {}", profile_name(v))))
        .when(list.len() > 1, |d| d.child(div().text_color(th.tx3).child(format!("{}/{}", cur + 1, list.len()))))
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .text_color(if live { th.ok } else { th.tx3 })
                .child(div().size(px(7.)).rounded(px(4.)).bg(if live { th.ok } else { th.tx4 }))
                .child(if s.live.channel == quiver_api::options::LiveChannel::Sdk && s.live.connected { "SDK connected".to_owned() } else { mods::surface_where(s) }),
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.nav.zone = Zone::Surfaces;
            cx.notify();
        }))
}

/// The scrolling list of rows (and, docked, the bars and cards after them).
fn scroller(v: &PanelView, children: Vec<AnyElement>) -> Stateful<Div> {
    div().id("body").flex_1().min_h_0().overflow_y_scroll().track_scroll(&v.scroll).flex().flex_col().gap(px(2.)).children(children)
}

// ---- Mods ------------------------------------------------------------------------------------

pub fn mods_body(v: &PanelView, th: &Theme, docked: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let rows = v.rows();
    let focused = v.nav.focused_row(&rows).filter(|_| v.nav.confirm.is_none());
    let (Some(list), Some(s)) = (v.data.options.ready(), mods::surface(&v.data, &v.nav)) else {
        // Loading, failed, or nothing to show: the note.
        let notes = rows.iter().enumerate().map(|(i, r)| v.render_row(th, i, r, false, cx)).collect::<Vec<_>>();
        let body = div().flex().flex_col().gap(px(8.)).p(px(if docked { 16. } else { 24. })).children(notes);
        return if docked { body.into_any_element() } else { card(th).w_full().child(body).into_any_element() };
    };
    let list_rows = row_list(v, th, &rows, focused, docked, cx);
    let bar = restart_bar(v, th, &rows, focused, false, cx);
    let buttons = session_buttons(v, th, &rows, focused, cx);
    let (head, sub) = session_words(v);
    if docked {
        let session_bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(10.))
            .bg(th.ink3)
            .text_size(px(12.5))
            .text_color(th.tx2)
            .child(div().w_full().child(format!("{head} · {sub}")))
            .children(buttons);
        let mut tail: Vec<AnyElement> = Vec::new();
        tail.push(div().h(px(8.)).into_any_element());
        if let Some(b) = bar {
            tail.push(b.mx(px(6.)).into_any_element());
        }
        tail.push(session_bar.mx(px(6.)).mt(px(6.)).into_any_element());
        if let Some(r) = readouts(v, th, s) {
            tail.push(r.mx(px(6.)).mt(px(8.)).into_any_element());
        }
        if let Some(l) = locked_card(th, s) {
            tail.push(l.mx(px(6.)).mt(px(8.)).mb(px(8.)).into_any_element());
        }
        let mut children = list_rows;
        children.extend(tail);
        return div()
            .flex()
            .flex_col()
            .size_full()
            .child(surface_line(v, th, list, s, cx))
            .child(div().px(px(12.)).py(px(8.)).border_t_1().border_b_1().border_color(th.line1).child(page_bar(v, th, s, &rows, focused, true, cx)))
            .child(scroller(v, children).px(px(8.)).py(px(8.)))
            .into_any_element();
    }
    let main = card(th)
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .child(div().px(px(16.)).py(px(14.)).border_b_1().border_color(th.line1).child(page_bar(v, th, s, &rows, focused, false, cx)))
        .child(scroller(v, list_rows).p(px(10.)))
        .when_some(bar, |d, b| d.child(b.m(px(10.))));
    let session_card = card(th)
        .p(px(18.))
        .flex()
        .flex_col()
        .child(div().pb(px(10.)).child(caption("This session", th.tx3)))
        .child(div().text_size(px(15.)).text_color(th.tx1).pb(px(4.)).child(head))
        .child(div().text_size(px(12.)).text_color(th.tx3).pb(px(14.)).child(sub))
        .child(div().flex().flex_wrap().gap(px(8.)).children(buttons));
    let right = div()
        .w(px(330.))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .gap(px(20.))
        .when_some(readouts(v, th, s), |d, r| d.child(r))
        .child(session_card)
        .when_some(locked_card(th, s), |d, l| d.child(l));
    div().size_full().flex().gap(px(20.)).child(surface_list(v, th, list, cx)).child(main).child(right).into_any_element()
}

// ---- Tweaks ----------------------------------------------------------------------------------

pub fn tweaks_body(v: &PanelView, th: &Theme, docked: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let rows = v.rows();
    let focused = v.nav.focused_row(&rows).filter(|_| v.nav.confirm.is_none());
    let Some(s) = v.data.tweaks.ready() else {
        let notes = rows.iter().enumerate().map(|(i, r)| v.render_row(th, i, r, false, cx)).collect::<Vec<_>>();
        let body = div().flex().flex_col().gap(px(8.)).p(px(if docked { 16. } else { 24. })).children(notes);
        return if docked { body.into_any_element() } else { card(th).w_full().child(body).into_any_element() };
    };
    let saving_to = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .text_size(px(12.5))
        .text_color(th.tx2)
        .child("Saving to")
        .child(div().px(px(10.)).h(px(24.)).flex().items_center().rounded(px(12.)).bg(th.acc.opacity(0.16)).text_color(th.tx1).font_weight(SEMIBOLD).child(v.data.tweak_scope_name.clone()))
        .child(div().text_color(th.tx3).child("· most apply when the game next starts"));
    let list_rows = row_list(v, th, &rows, focused, docked, cx);
    let bar = restart_bar(v, th, &rows, focused, true, cx);
    if docked {
        let mut children = list_rows;
        if let Some(b) = bar {
            children.push(div().h(px(8.)).into_any_element());
            children.push(b.mx(px(6.)).into_any_element());
        }
        return div()
            .flex()
            .flex_col()
            .size_full()
            .child(div().px(px(16.)).pb(px(10.)).child(saving_to))
            .child(div().px(px(12.)).py(px(8.)).border_t_1().border_b_1().border_color(th.line1).child(page_bar(v, th, s, &rows, focused, true, cx)))
            .child(scroller(v, children).px(px(8.)).py(px(8.)))
            .into_any_element();
    }
    card(th)
        .size_full()
        .flex()
        .flex_col()
        .child(div().px(px(16.)).pt(px(14.)).child(saving_to))
        .child(div().px(px(16.)).py(px(12.)).border_b_1().border_color(th.line1).child(page_bar(v, th, s, &rows, focused, false, cx)))
        .child(scroller(v, list_rows).p(px(10.)))
        .when_some(bar, |d, b| d.child(b.m(px(10.))))
        .into_any_element()
}
