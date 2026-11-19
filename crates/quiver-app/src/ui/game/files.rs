//! Files: where the game lives (install folder, prefix, save folder, launch logs), each
//! with an open-folder button, and the install's size, version and languages.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::page::{kv, note, small};
use crate::app::Quiver;
use crate::model::{GameRow, gb};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, MEDIUM, SEMIBOLD, card, card_header, icon, icon_button, mono};

impl Quiver {
    pub(super) fn render_files(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if !g.installed() {
            return card(th).child(card_header(th, "Files")).child(note(th, "Not installed. Installing creates the game's folder and its own compatibility folder.")).into_any_element();
        }
        // The save folder comes from the saves report (it depends on the launch account).
        if self.gp.id == id && self.gp.saves.is_idle() {
            self.load_saves(id, false, cx);
        }
        let locs = self.file_locations(id);
        let narrow = self.pane_bp().narrow();
        let rows: Vec<AnyElement> = locs
            .into_iter()
            .enumerate()
            .map(|(i, (label, path, what))| {
                let focused = self.focused(Target::Body(i));
                let p = path.to_string_lossy().into_owned();
                let p2 = p.clone();
                let ic = match i {
                    0 => Icon::HardDrive,
                    1 => Icon::Package,
                    2 => Icon::Cloud,
                    _ => Icon::ScrollText,
                };
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(14.))
                    .gap_y(px(8.))
                    .px(px(16.))
                    .py(px(12.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(focused, |d| d.bg(th.acc_soft))
                    .child(div().flex().flex_none().items_center().justify_center().size(px(34.)).rounded(px(9.)).bg(th.ink4).child(icon(ic, 16., th.tx2)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .when(narrow, |d| d.flex_col())
                                    .when(!narrow, |d| d.items_center().gap(px(8.)))
                                    .child(div().font_weight(SEMIBOLD).child(label))
                                    .child(div().text_size(px(11.5)).text_color(th.tx3).child(what)),
                            )
                            .child(mono(12., th.tx2).truncate().child(self.short_path(&path))),
                    )
                    .child(
                        icon_button(("files-copy", i), th, Icon::Copy, true)
                            .size(px(28.))
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_text(p2.clone(), "the path", cx))),
                    )
                    .child(
                        small(("files-open", i), th, Some(Icon::FolderOpen), "Open folder")
                            .focus_ring(focused, th, 6.)
                            .on_click(cx.listener(move |this, _, _, cx| this.open_external(&p, cx))),
                    )
                    .into_any_element()
            })
            .collect();
        let info = self.gp.info.ready().filter(|_| self.gp.id == id).cloned();
        let inst = info.as_ref().and_then(|i| i.install.clone());
        let game = g.entry.installed.clone();
        let txt = |s: String| div().child(s);
        let mut facts = div().flex().flex_col().gap(px(8.)).p(px(16.));
        if let Some(i) = &inst {
            facts = facts
                .child(kv(th, "Size", txt(if i.size_bytes > 0 { format!("{} for the installed language", gb(i.size_bytes)) } else { "—".into() })))
                .child(kv(th, "Version", txt(i.version.clone().unwrap_or_else(|| "—".into()))))
                .child(kv(th, "Manifest", mono(11.5, th.tx2).truncate().child(i.manifest.clone().unwrap_or_else(|| "—".into()))))
                .child(kv(th, "Language", txt(i.language.clone().unwrap_or_else(|| "Default".into()))))
                .child(kv(th, "Available", txt(if i.languages.is_empty() { "—".into() } else { i.languages.join(", ") })));
        } else {
            facts = facts.child(div().text_color(th.tx3).child("Loading install details…"));
        }
        if let Some(gm) = &game {
            facts = facts
                .child(kv(th, "Executable", mono(12., th.tx1).truncate().child(gm.exe.clone())))
                .child(kv(th, "Arguments", mono(12., th.tx2).truncate().child(gm.args.clone().unwrap_or_else(|| "—".into()))));
        }
        crate::ui::responsive::columns(
            self.pane_bp(),
            20.,
            380.,
            div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(card(th).child(card_header(th, "Locations").child(div().text_size(px(12.)).text_color(th.tx3).child("each game has its own compatibility folder; nothing is shared"))).children(rows))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .child(icon(Icon::Info, 12., th.tx3))
                            .child(div().flex_1().min_w_0().child("Profiles never write to the install folder: their mods and what the game writes there go to the profile's own layer.")),
                    ),
            div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(card(th).child(card_header(th, "Install").child(div().flex_1()).child(div().font_weight(MEDIUM).text_size(px(12.)).text_color(th.tx3).child(inst.as_ref().map(|i| gb(i.size_bytes)).unwrap_or_default()))).child(facts))
                    .child(self.cli_line("files-cli", th, format!("quiver info {id}"), cx)),
        )
        .into_any_element()
    }
}
