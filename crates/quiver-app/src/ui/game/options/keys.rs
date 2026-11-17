//! Keys in play for a profile (MOD-CONFIG §7, `OptionsKeys`): the function keys and who uses
//! them, conflicts (shown, never resolved by Quiver), and every binding with where to change it.

use std::collections::BTreeMap;

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::options::{KeyBinding, KeysReport};

use super::form::sec;
use super::logic;
use crate::app::Quiver;
use crate::model::GameRow;
use crate::theme::Theme;
use crate::ui::game::page::Load;
use crate::ui::{Icon, Kind, MEDIUM, SEMIBOLD, Size, button, card, icon, kbd};

/// A stable colour per owner (Sight, DLSS, MangoHud, …).
fn owner_color(th: &Theme, owners: &[String], owner: &str) -> gpui::Hsla {
    let i = owners.iter().position(|o| o == owner).unwrap_or(0);
    [th.who_gui, th.who_cli, th.warn, th.who_agent, th.who_sys, th.ok][i % 6]
}

fn read_by(b: &KeyBinding) -> String {
    match b.kind.as_str() {
        "option" => "The mod, in game".into(),
        "runtime" => "Quiver's in-game overlay".into(),
        "overlay" => "Sight (the in-game overlay)".into(),
        _ => format!("{}, in game", b.owner),
    }
}

fn chord(th: &Theme, key: &str) -> gpui::Div {
    div().flex().items_center().gap(px(3.)).children(key.split('+').map(|p| kbd(th, p.to_owned())))
}

impl Quiver {
    pub(super) fn render_options_keys(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let r: KeysReport = match &self.gp.opt.keys {
            Load::Ready(k) => k.clone(),
            Load::Failed(e) => return card(th).p(px(20.)).text_color(th.bad).child(format!("Couldn't list the keys: {e}")).into_any_element(),
            _ => return card(th).p(px(20.)).text_color(th.tx3).child("Listing keys…").into_any_element(),
        };
        let mut owners: Vec<String> = Vec::new();
        for b in &r.bindings {
            if !owners.contains(&b.owner) {
                owners.push(b.owner.clone());
            }
        }
        let in_conflict: Vec<usize> = r.conflicts.iter().flat_map(|c| c.bindings.iter().copied()).collect();

        // The function keys.
        let strip = logic::function_keys(&r);
        let keys: Vec<AnyElement> = strip
            .into_iter()
            .map(|(k, on)| {
                let bad = on.iter().any(|i| in_conflict.contains(i));
                let first = on.first().map(|i| &r.bindings[*i]);
                let color = first.map(|b| owner_color(th, &owners, &b.owner));
                let lines: Vec<AnyElement> = on
                    .iter()
                    .map(|i| {
                        let b = &r.bindings[*i];
                        let c = owner_color(th, &owners, &b.owner);
                        div()
                            .text_size(px(11.))
                            .line_height(px(14.))
                            .text_color(if bad { th.bad } else { c })
                            .child(format!("{}{}: {}", logic::chord_prefix(&b.key), b.owner, short_label(&b.label)))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex_1()
                    .min_w(px(64.))
                    .h(px(84.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(8.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if bad { th.bad } else { color.map(|c| c.opacity(0.6)).unwrap_or(th.line2) })
                    .bg(if bad { th.bad.opacity(0.1) } else { th.ink1 })
                    .child(div().font_weight(gpui::FontWeight::BOLD).text_size(px(13.)).child(k))
                    .children(lines)
                    .into_any_element()
            })
            .collect();
        let legend = div().flex().flex_wrap().gap(px(6.)).children(owners.iter().map(|o| {
            let c = owner_color(th, &owners, o);
            div().px(px(8.)).h(px(20.)).flex().items_center().rounded(px(5.)).bg(c.opacity(0.16)).text_color(c).text_size(px(11.)).font_weight(gpui::FontWeight::BOLD).child(o.clone())
        }));
        let strip_card = card(th)
            .p(px(16.))
            .gap(px(12.))
            .child(div().flex().flex_wrap().items_center().gap(px(10.)).child(sec(th, "Function keys")).child(div().flex_1()).child(legend))
            .child(div().flex().flex_wrap().gap(px(6.)).children(keys));

        // Conflicts.
        let conflicts: Vec<AnyElement> = r
            .conflicts
            .iter()
            .enumerate()
            .map(|(ci, c)| {
                let bs: Vec<&KeyBinding> = c.bindings.iter().map(|i| &r.bindings[*i]).collect();
                let who: Vec<String> = bs.iter().map(|b| format!("{} ({})", b.label, b.owner)).collect();
                let raws: Vec<String> = bs.iter().filter(|b| b.raw != b.key).map(|b| format!("{}: {}", b.owner, b.raw)).collect();
                let fixes: Vec<AnyElement> = bs
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.kind == "option" && !b.surface.is_empty())
                    .map(|(j, b)| {
                        let (s, o) = (b.surface.clone(), b.id.clone());
                        button(SharedString::from(format!("kc-{ci}-{j}")), th, Kind::Primary, Size::Small, Some(Icon::Keyboard), format!("Change {}'s key…", b.owner))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.options_jump(&s, Some(&o));
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(chord(th, &c.key)).child(div().text_size(px(14.)).font_weight(SEMIBOLD).child(who.join(" and "))))
                    .when(!raws.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(th.tx3).child(format!("As each spells it: {}", raws.join(" · ")))))
                    .when(!fixes.is_empty(), |d| d.child(div().flex().flex_wrap().gap(px(8.)).children(fixes)))
                    .into_any_element()
            })
            .collect();
        let n = conflicts.len();
        let conflict_card = (n > 0).then(|| {
            card(th)
                .p(px(16.))
                .gap(px(12.))
                .border_color(th.bad.opacity(0.45))
                .child(sec(th, format!("{n} conflict{}", if n == 1 { "" } else { "s" })).text_color(th.bad))
                .children(conflicts)
                .child(div().text_size(px(12.)).text_color(th.tx3).child("Both programs see the key, so one press does both. Quiver shows conflicts and never moves anyone's keys by itself."))
        });

        // Every binding.
        let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, b) in r.bindings.iter().enumerate() {
            by_key.entry(b.key.clone()).or_default().push(i);
        }
        let head = |t: &str, w: Option<f32>| {
            let d = div().text_size(px(11.5)).font_weight(SEMIBOLD).text_color(th.tx3).child(t.to_owned());
            match w {
                Some(w) => d.w(px(w)).flex_none(),
                None => d.flex_1().min_w_0(),
            }
        };
        let mut rows: Vec<AnyElement> = vec![div()
            .flex()
            .gap(px(12.))
            .pb(px(6.))
            .child(head("Key", Some(150.)))
            .child(head("Action", None))
            .child(head("Owner", Some(110.)))
            .child(head("Read by", Some(180.)))
            .child(head("Change it", Some(200.)))
            .into_any_element()];
        for (i, b) in r.bindings.iter().enumerate() {
            let bad = in_conflict.contains(&i);
            let c = owner_color(th, &owners, &b.owner);
            let change: AnyElement = if b.kind == "option" && !b.surface.is_empty() {
                let (s, o) = (b.surface.clone(), b.id.clone());
                let page = self.gp.opt.surface(&s).and_then(|x| logic::page_of(x, &o).and_then(|p| x.pages.iter().find(|q| q.id == p).map(|q| format!("{} options › {}", x.title, q.title))));
                div()
                    .id(("kb-change", i))
                    .text_color(th.acc_hi)
                    .cursor_pointer()
                    .child(page.unwrap_or_else(|| format!("{} options", b.owner)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.options_jump(&s, Some(&o));
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                div().text_color(th.tx3).child(match b.kind.as_str() {
                    "runtime" | "overlay" => "Settings › Performance HUD",
                    _ => "Its own settings",
                }).into_any_element()
            };
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(th.line1)
                    .text_size(px(12.5))
                    .text_color(if bad { th.bad } else { th.tx1 })
                    .child(div().w(px(150.)).flex_none().child(chord(th, &b.key)))
                    .child(div().flex_1().min_w_0().font_weight(MEDIUM).child(b.label.clone()))
                    .child(div().w(px(110.)).flex_none().child(div().px(px(6.)).h(px(18.)).flex().items_center().rounded(px(5.)).bg(c.opacity(0.16)).text_color(c).text_size(px(11.)).font_weight(gpui::FontWeight::BOLD).child(b.owner.clone())))
                    .child(div().w(px(180.)).flex_none().text_color(th.tx2).child(read_by(b)))
                    .child(div().w(px(200.)).flex_none().child(change))
                    .into_any_element(),
            );
        }
        let table = card(th)
            .p(px(16.))
            .child(div().flex().flex_col().children(rows))
            .child(div().pt(px(12.)).text_size(px(12.)).text_color(th.tx3).child("Mods' keys come from their options; Quiver's own and well-known tools' keys are built in. Desktop shortcuts that never reach the game aren't listed."))
            .child(self.cli_line("keys-cli", th, self.options_cli(id, "keys").to_string(), cx).mt(px(10.)));
        let _ = icon;
        div().flex().flex_col().gap(px(20.)).child(strip_card).children(conflict_card).child(table).into_any_element()
    }
}

/// A binding's label for the narrow key boxes.
fn short_label(l: &str) -> String {
    let l = l.trim_start_matches("Show or hide ").trim_start_matches("Toggle ");
    let mut s: String = l.chars().take(22).collect();
    if l.chars().count() > 22 {
        s.push('…');
    }
    s
}
