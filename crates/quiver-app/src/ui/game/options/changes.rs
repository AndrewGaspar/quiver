//! History and changes: a change the game (or a file edit) made, imported as a revision;
//! comparing the profile with its defaults or another profile; the revisions with their
//! actors (you, the CLI, an agent, in game, the game, a file edit), restore and "revert this
//! session"; and crash notes after live changes (never acted on automatically).

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::options::OptionsRevision;

use super::form::{sec, who_badge};
use super::logic::{self, Who};
use super::{Confirm, ModsView};
use crate::app::Quiver;
use crate::model::{GameRow, ago, clock, day};
use crate::theme::Theme;
use crate::ui::game::page::Load;
use crate::ui::{Icon, Kind, SEMIBOLD, Size, button, card, chip, icon, mono, pill, PillKind};

fn panel(th: &Theme) -> gpui::Div {
    card(th).p(px(16.)).gap(px(10.))
}

impl Quiver {
    pub(super) fn render_options_changes(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let profile = self.mods_profile();
        let name = self.mods_profile_name(&profile);
        let hist: Vec<OptionsRevision> = match &self.gp.opt.history {
            Load::Ready(h) => h.clone(),
            Load::Failed(e) => return card(th).p(px(20.)).text_color(th.bad).child(format!("Couldn't load the history: {e}")).into_any_element(),
            _ => return card(th).p(px(20.)).text_color(th.tx3).child("Loading history…").into_any_element(),
        };
        let mut left: Vec<AnyElement> = Vec::new();
        if let Some(card) = self.render_options_import(th, &hist, cx) {
            left.push(card);
        }
        left.push(self.render_options_compare(th, id, &profile, &name, cx));
        let right = self.render_options_history(th, id, &name, &hist, cx);
        crate::ui::responsive::columns(self.pane_bp(), 20., 460., div().flex().flex_col().gap(px(20.)).children(left), div().child(right)).into_any_element()
    }

    /// The newest revision, when the game, a file edit or a mod's own menu made it: what
    /// changed, that Quiver kept it (the last writer wins), and an undo.
    fn render_options_import(&mut self, th: &Theme, hist: &[OptionsRevision], cx: &mut Context<Self>) -> Option<AnyElement> {
        let last = hist.last()?;
        let w = logic::who(&last.source);
        if !w.is_import() || self.gp.opt.dismissed.contains(&last.revision) {
            return None;
        }
        let prev = hist.iter().rev().nth(1).map(|r| r.revision).unwrap_or(0);
        let (title, body) = match &w {
            Who::Game => (
                format!("The game changed its own settings ({})", clock(last.at)),
                "Its own menu rewrote a file Quiver writes at launch. Quiver kept the new values in this profile (the last writer wins), so they stay from now on. Undo puts back what you had.",
            ),
            Who::File => (
                format!("A file was edited outside Quiver ({})", clock(last.at)),
                "Someone edited a file Quiver renders for this profile. Quiver kept the edit as a revision so it survives the next launch. Undo puts back what you had.",
            ),
            _ => (
                format!("The mod changed its settings in game ({})", clock(last.at)),
                "The mod's own menu reported these changes; Quiver saved them to this profile. Undo puts back what you had.",
            ),
        };
        let lines: Vec<AnyElement> = last
            .changes
            .iter()
            .map(|c| {
                let (what, ft) = logic::split_change(c);
                match ft {
                    Some((from, to)) => div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(div().text_color(th.tx2).child(what))
                        .child(div().text_color(th.bad).child(from))
                        .child(div().text_color(th.tx3).child("→"))
                        .child(div().text_color(th.ok).child(to))
                        .into_any_element(),
                    None => div().text_color(th.tx2).child(what).into_any_element(),
                }
            })
            .collect();
        let rev = last.revision;
        Some(
            panel(th)
                .border_color(th.warn.opacity(0.45))
                .child(sec(th, "Changed outside Quiver").text_color(th.warn))
                .child(div().flex().items_center().gap(px(8.)).child(div().text_size(px(15.)).font_weight(SEMIBOLD).child(title)).child(who_badge(th, &last.source)))
                .child(div().text_size(px(12.5)).text_color(th.tx2).child(body))
                .child(mono(12., th.tx1).flex().flex_col().gap(px(3.)).px(px(12.)).py(px(10.)).rounded(px(8.)).bg(th.ink1).border_1().border_color(th.line1).children(lines))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button("imp-keep", th, Kind::Primary, Size::Small, Some(Icon::Check), format!("Keep (r{rev})")).on_click(cx.listener(move |this, _, _, cx| {
                            this.gp.opt.dismissed.push(rev);
                            cx.notify();
                        })))
                        .child(button("imp-undo", th, Kind::Secondary, Size::Small, Some(Icon::Undo), format!("Undo: back to r{prev}")).on_click(cx.listener(move |this, _, _, cx| {
                            this.gp.opt.dismissed.push(rev);
                            this.options_restore(prev, cx);
                        }))),
                )
                .into_any_element(),
        )
    }

    /// This profile against its defaults or another profile (`OptionsDiff`).
    fn render_options_compare(&mut self, th: &Theme, id: u32, profile: &str, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let against = if self.gp.opt.compare.is_empty() { "default".to_owned() } else { self.gp.opt.compare.clone() };
        let mut choices: Vec<(String, String)> = vec![("default".into(), "Defaults".into())];
        if profile != quiver_api::mods::VANILLA {
            choices.push((quiver_api::mods::VANILLA.into(), "Vanilla".into()));
        }
        for p in self.gp.profiles() {
            if p.id != profile {
                choices.push((p.id.clone(), p.name.clone()));
            }
        }
        let other_name = choices.iter().find(|c| c.0 == against).map(|c| c.1.clone()).unwrap_or_else(|| against.clone());
        let picker = div().flex().flex_wrap().gap(px(6.)).children(choices.into_iter().enumerate().map(|(i, (key, label))| {
            let on = key == against;
            chip(th, None, label, on).id(("cmp", i)).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                this.gp.opt.compare = key.clone();
                this.gp.opt.diff = Load::Idle;
                cx.notify();
            }))
        }));
        let body: AnyElement = match &self.gp.opt.diff {
            Load::Ready((a, rows)) if *a == against => {
                if rows.is_empty() {
                    div().py(px(8.)).text_color(th.tx3).child(format!("No differences: {name} and {other_name} have the same values.")).into_any_element()
                } else {
                    let head = |t: String| div().text_size(px(11.5)).font_weight(SEMIBOLD).text_color(th.tx3).child(t);
                    let mut grid = vec![
                        div().flex().gap(px(12.)).pb(px(6.)).child(head("Option".into()).flex_1()).child(head(name.to_owned()).w(px(150.))).child(head(other_name.clone()).w(px(150.))).into_any_element(),
                    ];
                    for r in rows {
                        let label = r.label.clone().unwrap_or_else(|| r.id.clone());
                        grid.push(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .py(px(7.))
                                .border_t_1()
                                .border_color(th.line1)
                                .text_size(px(12.5))
                                .child(div().flex_1().min_w_0().flex().flex_col().child(div().truncate().child(format!("{}: {label}", r.title))).child(mono(10.5, th.tx4).child(r.id.clone())))
                                .child(div().w(px(150.)).text_color(th.acc_hi).font_weight(SEMIBOLD).truncate().child(r.value.clone()))
                                .child(div().w(px(150.)).text_color(th.tx2).truncate().child(r.against.clone()))
                                .into_any_element(),
                        );
                    }
                    div().flex().flex_col().children(grid).into_any_element()
                }
            }
            Load::Failed(e) => div().text_color(th.bad).child(e.clone()).into_any_element(),
            _ => div().text_color(th.tx3).child("Comparing…").into_any_element(),
        };
        if !matches!(&self.gp.opt.diff, Load::Ready((a, _)) if *a == against) && !matches!(self.gp.opt.diff, Load::Loading) {
            self.load_option_diff(id, cx);
        }
        panel(th)
            .child(sec(th, "Compare profiles"))
            .child(div().text_size(px(12.5)).text_color(th.tx2).child(format!("Where {name}'s options differ from:")))
            .child(picker)
            .child(body)
            .child(self.cli_line("cmp-cli", th, self.options_cli(id, &format!("diff --against {}", crate::ui::game::page::sh_quote(&against))).to_string(), cx))
            .into_any_element()
    }

    fn render_options_history(&mut self, th: &Theme, id: u32, name: &str, hist: &[OptionsRevision], cx: &mut Context<Self>) -> AnyElement {
        let cur = hist.last().map(|r| r.revision);
        let running_session = self.gp.opt.loaded_surfaces().iter().find_map(|s| s.live.session.clone());
        let confirm = self.gp.opt.confirm.clone();
        let rows: Vec<AnyElement> = hist
            .iter()
            .rev()
            .take(40)
            .enumerate()
            .map(|(i, r)| {
                let is_cur = Some(r.revision) == cur;
                let w = logic::who(&r.source);
                let title = r.message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| r.changes.first().map(|c| logic::split_change(c).0).unwrap_or_else(|| "Changed".into()));
                let changes: Vec<String> = r.changes.iter().map(|c| match logic::split_change(c) {
                    (what, Some((from, to))) => format!("{} {from} → {to}", what.split(' ').next_back().unwrap_or(&what)),
                    (what, None) => what,
                }).collect();
                let rev = r.revision;
                let session = r.session.clone().filter(|s| Some(s) != running_session.as_ref());
                let asking = confirm == Some(Confirm::Restore(rev));
                let actions: AnyElement = if is_cur {
                    pill(th, PillKind::Accent, None, "Current").into_any_element()
                } else if asking {
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(button(("rev-cancel", i), th, Kind::Ghost, Size::Small, None, "Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.gp.opt.confirm = None;
                            cx.notify();
                        })))
                        .child(button(("rev-yes", i), th, Kind::Primary, Size::Small, Some(Icon::RotateCcw), format!("Restore r{rev}")).on_click(cx.listener(move |this, _, _, cx| this.options_restore(rev, cx))))
                        .into_any_element()
                } else {
                    button(("rev-restore", i), th, Kind::Secondary, Size::Small, Some(Icon::RotateCcw), "Restore")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.gp.opt.confirm = Some(Confirm::Restore(rev));
                            cx.notify();
                        }))
                        .into_any_element()
                };
                let revert = session.filter(|_| matches!(w, Who::InGame) || r.message.as_deref().is_some_and(|m| m.starts_with("Changed in game"))).map(|s| {
                    let asking = confirm == Some(Confirm::RevertSession(s.clone()));
                    let s2 = s.clone();
                    if asking {
                        button(("rev-sess-yes", i), th, Kind::Primary, Size::Small, Some(Icon::Undo), "Revert the session").on_click(cx.listener(move |this, _, _, cx| this.options_revert_session(s2.clone(), cx))).into_any_element()
                    } else {
                        button(("rev-sess", i), th, Kind::Ghost, Size::Small, Some(Icon::Undo), "Revert session").on_click(cx.listener(move |this, _, _, cx| {
                            this.gp.opt.confirm = Some(Confirm::RevertSession(s.clone()));
                            cx.notify();
                        })).into_any_element()
                    }
                });
                let note = match &w {
                    Who::Agent(a) => Some(format!("Made by the agent {a}{}", r.message.as_ref().map(|m| format!(": \u{201c}{m}\u{201d}")).unwrap_or_default())),
                    Who::Game => Some("Imported: the game wrote its own file".into()),
                    Who::File => Some("Imported: a hand edit of the file".into()),
                    Who::Mod(m) => Some(format!("Reported by {m}'s own menu")),
                    _ => None,
                };
                let when = format!("{} {}", day(r.at), clock(r.at));
                div()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .py(px(11.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(matches!(w, Who::Agent(_)), |d| d.border_l_2().border_color(th.who_agent).pl(px(10.)))
                    .child(mono(12.5, if is_cur { th.acc_hi } else { th.tx2 }).w(px(36.)).flex_none().font_weight(SEMIBOLD).child(format!("r{rev}")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).child(title)).child(who_badge(th, &r.source)))
                            .when(!changes.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(th.tx2).child(changes.join(" · "))))
                            .when_some(note, |d, n| d.child(div().text_size(px(11.5)).text_color(if matches!(w, Who::Agent(_)) { th.who_agent } else { th.tx3 }).child(n)))
                            .child(div().text_size(px(11.)).text_color(th.tx3).child(format!("{when} · {}{}", ago(r.at), r.session.as_ref().map(|s| format!(" · session {s}")).unwrap_or_default()))),
                    )
                    .child(div().flex().flex_col().items_end().gap(px(6.)).child(actions).children(revert))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        // Crash notes from recent sessions (newest first).
        let crashes: Vec<AnyElement> = self
            .gp
            .opt
            .records
            .ready()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| s.crash.clone().map(|c| (s, c)))
            .take(3)
            .enumerate()
            .map(|(i, (s, c))| {
                let reverted = hist.iter().any(|r| r.message.as_deref().is_some_and(|m| m.contains(&s.session) && m.starts_with("reverted")));
                let when = s.ended.map(|t| format!("{} {}", day(t), clock(t))).unwrap_or_default();
                let sid = s.session.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(8.))
                    .bg(th.bad.opacity(0.08))
                    .border_1()
                    .border_color(th.bad.opacity(0.3))
                    .text_size(px(12.5))
                    .child(div().flex().items_center().gap(px(8.)).child(icon(Icon::TriangleAlert, 14., th.bad)).child(div().font_weight(SEMIBOLD).child(format!("Crash {} s after a live change · {when}", c.seconds_after))))
                    .child(div().text_color(th.tx2).child(c.message.clone()))
                    .child(div().flex().items_center().gap(px(8.)).child(if reverted {
                        div().text_color(th.tx3).child("You reverted that session's changes.").into_any_element()
                    } else {
                        button(("crash-revert", i), th, Kind::Secondary, Size::Small, Some(Icon::Undo), "Revert that session").on_click(cx.listener(move |this, _, _, cx| this.options_revert_session(sid.clone(), cx))).into_any_element()
                    }).child(div().text_size(px(11.)).text_color(th.tx3).child("Quiver never reverts on its own.")))
                    .into_any_element()
            })
            .collect();
        panel(th)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(sec(th, format!("History · profile {name}")))
                    .child(div().flex_1())
                    .child(button("hist-options", th, Kind::Ghost, Size::Small, Some(Icon::SlidersHorizontal), "Options").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.view = ModsView::Options;
                        cx.notify();
                    }))),
            )
            .child(div().text_size(px(12.)).text_color(th.tx3).child("Every change is a revision, with who made it: you, the CLI, an agent, the game or a file edit. Restoring makes a new one."))
            .when(empty, |d| d.child(div().py(px(10.)).text_color(th.tx3).child("No option changes yet: everything has the mods' defaults.")))
            .child(div().flex().flex_col().children(rows))
            .children(crashes)
            .child(self.cli_line("hist-cli", th, self.options_cli(id, "history").to_string(), cx))
            .into_any_element()
    }
}
