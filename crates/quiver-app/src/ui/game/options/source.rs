//! Where a surface's description (schema) came from: the mod's author, Quiver's catalog, a
//! local one, or a guess (MOD-CONFIG §1 "Where schemas come from"), with its lint findings
//! and text. For a mod without one: guess it from one of its files ("infer from file") and
//! keep the guess as a local description.

use crate::model::Gid;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::options::{OptionsRequest, Schema, SchemaInfo, SchemaSource, Severity};
use quiver_api::Request;

use super::form::{sec, tone_badge};
use super::logic::Tone;
use super::ModsView;
use crate::app::Quiver;
use crate::err_text;
use crate::model::GameRow;
use crate::theme::Theme;
use crate::ui::game::page::Load;
use crate::ui::{Icon, Kind, SEMIBOLD, Size, button, card, chip, icon, mono};

fn source_words(s: SchemaSource) -> (&'static str, &'static str, Tone) {
    match s {
        SchemaSource::Author => ("Mod author", "Shipped with this version of the mod (quiver-options.toml), or carried forward from an earlier version.", Tone::Ok),
        SchemaSource::Catalog => ("Quiver catalog", "Quiver's audited description (options/ in Quiver's repository), matched to this version's files.", Tone::Accent),
        SchemaSource::Local => ("Local", "Written on this machine, by you or an agent (quiver mods … options schema --set), and revisioned.", Tone::Warn),
        SchemaSource::Inferred => ("Guessed", "Guessed from the file's values and comments; types and ranges may be wrong.", Tone::Warn),
    }
}

impl Quiver {
    pub(super) fn render_options_source(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let Some(m) = self.gp.opt.source_mod.clone() else {
            self.gp.opt.view = ModsView::Options;
            return div().into_any_element();
        };
        let name = self.gp.mods.ready().and_then(|s| s.all_mods().into_iter().find(|r| r.id == m)).map(|r| r.name).unwrap_or_else(|| m.clone());
        let back = button("src-back", th, Kind::Ghost, Size::Small, Some(Icon::ArrowLeft), "Options").on_click(cx.listener(|this, _, _, cx| {
            this.gp.opt.view = ModsView::Options;
            cx.notify();
        }));
        let head = div().flex().items_center().gap(px(10.)).child(back).child(div().text_size(px(17.)).font_weight(gpui::FontWeight::BOLD).child(format!("{name} · where its options come from")));
        let left: AnyElement = match self.gp.opt.schema.clone() {
            Load::Ready((sm, info)) if sm == m => self.render_schema_info(th, id, &name, &info, cx),
            Load::Failed(e) => self.render_no_schema(th, id, &m, &name, &e, cx),
            _ => card(th).p(px(20.)).text_color(th.tx3).child("Looking for a description…").into_any_element(),
        };
        let right = self.render_guess(th, id, &m, &name, cx);
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(head)
            .child(crate::ui::responsive::columns(self.pane_bp(), 20., 460., div().child(left), div().child(right)))
            .into_any_element()
    }

    fn render_schema_info(&mut self, th: &Theme, id: Gid, name: &str, info: &SchemaInfo, cx: &mut Context<Self>) -> AnyElement {
        let (label, what, tone) = source_words(info.source);
        let order = [SchemaSource::Author, SchemaSource::Catalog, SchemaSource::Local, SchemaSource::Inferred];
        let chain = div().flex().flex_wrap().items_center().gap(px(6.)).children(order.into_iter().enumerate().flat_map(|(i, s)| {
            let on = s == info.source;
            let (l, _, _) = source_words(s);
            let mut v: Vec<AnyElement> = Vec::new();
            if i > 0 {
                v.push(icon(Icon::ChevronRight, 12., th.tx4).into_any_element());
            }
            v.push(chip(th, None, l, on).into_any_element());
            v
        }));
        let lint: Vec<AnyElement> = info
            .lint
            .iter()
            .map(|f| {
                let (ic, c) = match f.severity {
                    Severity::Error => (Icon::CircleX, th.bad),
                    Severity::Warning => (Icon::TriangleAlert, th.warn),
                    Severity::Note => (Icon::Info, th.tx3),
                };
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .child(icon(ic, 13., c))
                    .child(div().flex_1().min_w_0().when(!f.at.is_empty(), |d| d.child(mono(11.5, th.tx3).child(f.at.clone()))).child(div().text_color(th.tx2).child(f.message.clone())))
                    .into_any_element()
            })
            .collect();
        let n_opts = info.schema.options.len();
        let files: Vec<String> = info.schema.files.values().map(|f| f.path.clone()).collect();
        let text = info.text.clone().unwrap_or_default();
        let show = self.gp.opt.show_text;
        let shown: String = if show { text.clone() } else { text.lines().take(18).collect::<Vec<_>>().join("\n") };
        let more = text.lines().count() > 18;
        card(th)
            .p(px(16.))
            .gap(px(12.))
            .child(div().flex().items_center().gap(px(8.)).child(tone_badge(th, label.to_uppercase(), tone)).child(div().font_weight(SEMIBOLD).child(info.note.clone())))
            .child(div().text_size(px(12.5)).text_color(th.tx2).child(what))
            .child(div().text_size(px(12.5)).text_color(th.tx2).child(format!(
                "{n_opts} option{} for {} {} · {}",
                if n_opts == 1 { "" } else { "s" },
                name,
                info.version_label,
                if files.is_empty() { "no files".to_owned() } else { files.join(", ") }
            )))
            .child(sec(th, "Precedence"))
            .child(chain)
            .child(div().text_size(px(12.)).text_color(th.tx3).child("The first that exists describes the whole surface; keys it doesn't describe show as Other keys."))
            .when(!lint.is_empty(), |d| d.child(sec(th, "Checks")).children(lint))
            .when(!text.is_empty(), |d| {
                d.child(sec(th, "The description"))
                    .child(mono(11.5, th.tx2).px(px(12.)).py(px(10.)).rounded(px(8.)).bg(th.ink1).border_1().border_color(th.line1).whitespace_normal().children(shown.lines().map(|l| div().min_h(px(15.)).child(l.to_owned())).collect::<Vec<_>>()))
                    .when(more, |d| {
                        d.child(button("src-more", th, Kind::Ghost, Size::Small, Some(if show { Icon::ChevronUp } else { Icon::ChevronDown }), if show { "Show less" } else { "Show all" }).on_click(cx.listener(|this, _, _, cx| {
                            this.gp.opt.show_text = !this.gp.opt.show_text;
                            cx.notify();
                        })))
                    })
            })
            .child(self.cli_line("src-cli", th, format!("quiver mods {id} options schema {} --lint", crate::ui::game::page::sh_quote(name)), cx))
            .into_any_element()
    }

    fn render_no_schema(&mut self, th: &Theme, id: Gid, m: &str, name: &str, why: &str, cx: &mut Context<Self>) -> AnyElement {
        let files = match &self.gp.opt.files {
            Load::Ready((fm, f)) if fm == m => f.clone(),
            _ => Vec::new(),
        };
        let picked = match &self.gp.opt.guess {
            Load::Ready((gm, file, _)) if gm == m => Some(file.clone()),
            _ => None,
        };
        let _ = why;
        let chips: Vec<AnyElement> = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let on = picked.as_deref() == Some(f.as_str());
                let (mm, ff) = (m.to_owned(), f.clone());
                chip(th, Some(Icon::FileText), f.clone(), on)
                    .id(("src-file", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.options_infer(id, mm.clone(), ff.clone(), cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        card(th)
            .p(px(16.))
            .gap(px(12.))
            .border_color(th.warn.opacity(0.4))
            .child(div().text_size(px(15.)).font_weight(SEMIBOLD).child(format!("{name} has no description of its options")))
            .child(div().text_size(px(12.5)).text_color(th.tx2).child(
                "It ships no quiver-options.toml, Quiver's catalog has none for it and none was written here. Quiver can guess one from one of its files: types, ranges and help come from the values and comments. Every value still round-trips exactly; nothing in the file is lost.",
            ))
            .child(sec(th, "Guess from a file"))
            .child(if chips.is_empty() {
                div().text_size(px(12.)).text_color(th.tx3).child(match &self.gp.opt.files {
                    Load::Loading => "Looking for settings files…".to_owned(),
                    Load::Failed(e) => format!("Couldn't list its files: {e}"),
                    _ => "It has no INI-like files Quiver can read.".to_owned(),
                }).into_any_element()
            } else {
                div().flex().flex_wrap().gap(px(6.)).children(chips).into_any_element()
            })
            .child(div().text_size(px(12.)).text_color(th.tx3).child("Or ask the agent that made the mod to describe it (quiver mods … options schema --set), so its options say when they apply and what their ranges are."))
            .child(self.cli_line("src-infer-cli", th, format!("quiver mods {id} options infer {} {}", crate::ui::game::page::sh_quote(name), picked.unwrap_or_else(|| "FILE".into())), cx))
            .into_any_element()
    }

    /// Ask quiverd to guess a schema from a mod's file.
    pub(super) fn options_infer(&mut self, id: Gid, m: String, file: String, cx: &mut Context<Self>) {
        self.gp.opt.guess = Load::Loading;
        let (m2, f2) = (m.clone(), file.clone());
        self.call::<String, _>(Request::Options(OptionsRequest::Infer { game: id.0.into(), mod_id: m, file }), cx, move |this, r, _| {
            if this.gp.opt.source_mod.as_deref() != Some(m2.as_str()) {
                return;
            }
            this.gp.opt.guess = match r {
                Ok(t) => Load::Ready((m2, f2, t)),
                Err(e) => {
                    this.gp.opt.guess_error = Some(err_text(&e));
                    Load::Failed(m2)
                }
            };
        });
    }

    /// The guess, as the form would show it, and its text; "Use this description" keeps it.
    fn render_guess(&mut self, th: &Theme, id: Gid, m: &str, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let (file, text) = match &self.gp.opt.guess {
            Load::Ready((gm, f, t)) if gm == m => (f.clone(), t.clone()),
            Load::Loading => return card(th).p(px(20.)).text_color(th.tx3).child("Guessing…").into_any_element(),
            Load::Failed(_) => {
                return card(th).p(px(20.)).text_color(th.bad).child(self.gp.opt.guess_error.clone().unwrap_or_else(|| "Couldn't guess a description.".into())).into_any_element();
            }
            _ => {
                return card(th)
                    .p(px(20.))
                    .gap(px(8.))
                    .child(sec(th, "A guess"))
                    .child(div().text_size(px(12.5)).text_color(th.tx3).child("Pick a file to see what Quiver would make of it. Nothing is saved until you keep it."))
                    .into_any_element();
            }
        };
        let schema: Option<Schema> = toml::from_str(&text).ok();
        let rows: Vec<AnyElement> = schema
            .as_ref()
            .map(|s| {
                let mut out: Vec<AnyElement> = Vec::new();
                for p in &s.pages {
                    out.push(sec(th, format!("[{}]", p.title)).pt(px(8.)).into_any_element());
                    for oid in &p.options {
                        let Some(d) = s.options.get(oid) else { continue };
                        let ty = match d.ty {
                            quiver_api::options::OptionType::Bool => "ON/OFF".to_owned(),
                            quiver_api::options::OptionType::Int => "NUMBER".to_owned(),
                            quiver_api::options::OptionType::Float => "DECIMAL".to_owned(),
                            other => format!("{other:?}").to_uppercase(),
                        };
                        out.push(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .py(px(9.))
                                .border_t_1()
                                .border_color(th.line1)
                                .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).child(d.label_or(oid).to_owned())).child(tone_badge(th, format!("GUESS · {ty}"), Tone::Warn)))
                                .child(div().text_size(px(12.)).text_color(th.tx3).child(d.help.clone().map(|h| format!("\u{201c}{h}\u{201d}")).unwrap_or_else(|| "No comment in the file.".into())))
                                .child(mono(11., th.tx4).child(oid.clone()))
                                .into_any_element(),
                        );
                    }
                }
                out
            })
            .unwrap_or_default();
        let shipped = crate::demo::detail::options_file_text(m, &file).filter(|_| self.demo);
        let (mm, tt) = (m.to_owned(), text.clone());
        card(th)
            .p(px(16.))
            .gap(px(10.))
            .child(div().flex().items_center().gap(px(8.)).child(div().text_size(px(15.)).font_weight(SEMIBOLD).child(format!("Guessed from {file}"))))
            .child(div().text_size(px(12.5)).text_color(th.tx2).child(format!(
                "{} option{} found. Apply timing is unknown, so every change is saved and applies at the next launch until a better description says otherwise.",
                schema.as_ref().map_or(0, |s| s.options.len()),
                if schema.as_ref().is_some_and(|s| s.options.len() == 1) { "" } else { "s" }
            )))
            .children(rows)
            .when_some(shipped, |d, t| d.child(sec(th, format!("{file} as shipped"))).child(mono(11.5, th.tx2).px(px(12.)).py(px(10.)).rounded(px(8.)).bg(th.ink1).border_1().border_color(th.line1).children(t.lines().map(|l| div().min_h(px(15.)).child(l.to_owned())).collect::<Vec<_>>())))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .pt(px(6.))
                    .child(button("src-keep", th, Kind::Primary, Size::Small, Some(Icon::Check), "Use this description").on_click(cx.listener(move |this, _, _, cx| this.options_keep_guess(id, mm.clone(), tt.clone(), cx))))
                    .child(button("src-drop", th, Kind::Ghost, Size::Small, None, "Discard").on_click(cx.listener(|this, _, _, cx| {
                        this.gp.opt.guess = Load::Idle;
                        cx.notify();
                    }))),
            )
            .child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("Kept as a local description of {name}: its options appear on the Options view, marked local, and you can replace it any time.")))
            .into_any_element()
    }

    fn options_keep_guess(&mut self, id: Gid, m: String, toml: String, cx: &mut Context<Self>) {
        self.call::<SchemaInfo, _>(
            Request::Options(OptionsRequest::SchemaSet { game: id.0.into(), mod_id: m.clone(), toml, message: Some("Guessed from the file in the app".into()) }),
            cx,
            move |this, r, cx| match r {
                Ok(info) => {
                    this.show_toast(format!("Kept a local description for {}: {} options", info.mod_id, info.schema.options.len()), cx);
                    this.gp.opt.guess = Load::Idle;
                    this.gp.opt.schema = Load::Ready((m.clone(), info.clone()));
                    this.options_reload(cx);
                    this.options_jump(&info.key, None);
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            },
        );
    }
}
