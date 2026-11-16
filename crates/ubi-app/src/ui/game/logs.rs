//! Logs: the per-launch logs in `<prefix>/ubi-run/` (the bridge's own log, the game's
//! Proton output and the bridge's Proton output), read from disk, in a scrolling
//! monospace view with copy.

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px, uniform_list};

use super::page::{LOG_FILES, Load, note, small};
use crate::app::Quiver;
use crate::model::{GameRow, ago, tilde};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, MEDIUM, card, icon, mono};

impl Quiver {
    pub(super) fn render_logs(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if !g.installed() {
            return card(th).child(note(th, "Not installed: a game's launch logs appear here after it runs.")).into_any_element();
        }
        if self.gp.id == id && self.gp.log.is_idle() {
            self.load_log(id, cx);
        }
        let sel = self.gp.log_file.min(LOG_FILES.len() - 1);
        let tabs = div().flex().gap(px(6.)).children(LOG_FILES.iter().enumerate().map(|(i, (file, label))| {
            let on = i == sel;
            let focused = self.focused(Target::Body(i));
            div()
                .id(("log-file", i))
                .flex()
                .items_center()
                .gap(px(7.))
                .h(px(30.))
                .px(px(12.))
                .rounded(px(8.))
                .bg(if on { th.acc_soft } else { th.ink2 })
                .border_1()
                .border_color(if on { th.acc_line } else { th.line2 })
                .text_color(if on { th.acc_hi } else { th.tx2 })
                .cursor_pointer()
                .focus_ring(focused, th, 8.)
                .child(icon(Icon::FileText, 13., if on { th.acc_hi } else { th.tx3 }))
                .child(div().font_weight(MEDIUM).child(label.to_string()))
                .child(mono(11., th.tx3).child(file.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.gp.log_file = i;
                    this.load_log(id, cx);
                    cx.notify();
                }))
        }));
        let (meta, lines): (String, Option<std::sync::Arc<Vec<SharedString>>>) = match &self.gp.log {
            Load::Ready(t) => (
                format!(
                    "{} · {} lines · {}{}",
                    tilde(&t.path),
                    t.lines.len(),
                    if t.bytes >= 1_048_576 { format!("{:.1} MB", t.bytes as f64 / 1_048_576.) } else { format!("{:.1} KB", t.bytes as f64 / 1024.) },
                    t.modified.map(|m| format!(" · written {}", ago(m))).unwrap_or_default()
                ),
                Some(t.lines.clone()),
            ),
            Load::Failed(e) => (e.clone(), None),
            _ => ("Reading…".into(), None),
        };
        let height = (self.viewport.1 - 52. - 112. - 40. - 48. - 92.).max(260.);
        let view: AnyElement = match lines {
            Some(lines) if !lines.is_empty() => {
                let n = lines.len();
                let (tx2, tx3, warn, bad) = (th.tx2, th.tx3, th.warn, th.bad);
                let width = (n.to_string().len() as f32) * 7.5 + 8.;
                uniform_list("log-lines", n, move |range, _, _| {
                    range
                        .map(|i| {
                            let l = &lines[i];
                            let lower = l.to_ascii_lowercase();
                            let color = if lower.contains("error") || lower.contains(":err:") || lower.contains("panic") {
                                bad
                            } else if lower.contains("warn") {
                                warn
                            } else if lower.contains("fixme") {
                                tx3
                            } else {
                                tx2
                            };
                            div()
                                .flex()
                                .gap(px(12.))
                                .h(px(18.))
                                .px(px(12.))
                                .child(div().w(px(width)).flex_none().text_color(tx3.opacity(0.6)).child((i + 1).to_string()))
                                .child(div().text_color(color).whitespace_nowrap().child(l.clone()))
                        })
                        .collect()
                })
                .h(px(height))
                .py(px(8.))
                .into_any_element()
            }
            Some(_) => div().h(px(height)).child(note(th, "The log is empty.")).into_any_element(),
            None => div()
                .h(px(height))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_color(th.tx3)
                .child(icon(Icon::ScrollText, 22., th.tx4))
                .child(if matches!(self.gp.log, Load::Failed(_)) { "No log yet: it's written the next time the game launches." } else { "Reading…" })
                .into_any_element(),
        };
        let copy_text = match &self.gp.log {
            Load::Ready(t) => Some(t.lines.iter().map(|l| l.as_ref()).collect::<Vec<&str>>().join("\n")),
            _ => None,
        };
        let dir = g.entry.installed.as_ref().map(|i| i.prefix.join("ubi-run").to_string_lossy().into_owned()).unwrap_or_default();
        let truncated = matches!(&self.gp.log, Load::Ready(t) if t.truncated);
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(tabs)
                    .child(div().flex_1())
                    .child(small("log-refresh", th, Some(Icon::RefreshCw), "Reload").on_click(cx.listener(move |this, _, _, cx| {
                        this.load_log(id, cx);
                        cx.notify();
                    })))
                    .when_some(copy_text, |d, t| {
                        d.child(small("log-copy", th, Some(Icon::Copy), "Copy").on_click(cx.listener(move |this, _, _, cx| this.copy_text(t.clone(), "the log", cx))))
                    })
                    .child(small("log-open", th, Some(Icon::FolderOpen), "Open folder").on_click(cx.listener(move |this, _, _, cx| this.open_external(&dir, cx)))),
            )
            .child(
                card(th)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(14.))
                            .py(px(8.))
                            .border_b_1()
                            .border_color(th.line1)
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .child(mono(11.5, th.tx3).flex_1().min_w_0().truncate().child(meta))
                            .when(truncated, |d| d.child(div().text_color(th.warn).child("showing the last 512 KB")))
                    )
                    .child(div().bg(th.ink0).font_family(crate::assets::MONO_FONT).text_size(px(11.5)).child(view)),
            )
            .into_any_element()
    }
}
