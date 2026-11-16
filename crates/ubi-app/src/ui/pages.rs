//! The Downloads page. (Activity is in `activity.rs`; connecting and sign-in in
//! `signin.rs`.)

use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::{FocusExt, Icon, PillKind, SEMIBOLD, art_fill, card, display, icon, pill, progress};
use crate::app::Quiver;
use crate::art::{ArtSlot, Variant};
use crate::model::{gb, job_fraction};
use crate::nav::Target;
use crate::theme::Theme;

impl Quiver {
    fn page_shell(&self, th: &Theme, title: &str, sub: String, rows: Vec<AnyElement>) -> impl IntoElement + use<> {
        let _ = th;
        div()
            .id("page")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .px(px(Self::GUTTER))
            .pb(px(40.))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(14.))
                    .pt(px(28.))
                    .pb(px(18.))
                    .child(display(22., FontWeight::BOLD, th.tx1).child(title.to_owned()))
                    .child(div().text_color(th.tx3).child(sub)),
            )
            .children(rows)
    }

    pub fn render_downloads(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let jobs = self.download_jobs();
        let active = jobs.iter().filter(|j| !j.done).count();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, j) in jobs.iter().enumerate() {
            let focused = self.focused(Target::Body(i));
            let g = self.game(j.product_id).cloned();
            let thumb = match &g {
                Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Tile, cx),
                None => crate::art::ArtState::Failed,
            };
            let queued = j.status.starts_with("Queued");
            let f = job_fraction(j);
            let state = if let Some(e) = &j.error {
                pill(th, PillKind::Bad, Some(Icon::CircleX), format!("Failed: {e}"))
            } else if j.done {
                pill(th, PillKind::Ok, Some(Icon::CircleCheck), j.status.clone())
            } else if queued {
                pill(th, PillKind::Muted, None, "Queued")
            } else {
                pill(th, PillKind::Accent, Some(Icon::Download), format!("{:.0}%", (f * 100.).floor()))
            };
            let id = j.product_id;
            rows.push(
                card(th)
                    .id(("job", j.id as usize))
                    .flex_row()
                    .items_center()
                    .gap(px(16.))
                    .p(px(14.))
                    .mb(px(10.))
                    .cursor_pointer()
                    .focus_glow(focused, th, 12.)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_game(id, cx)))
                    .child(div().w(px(48.)).h(px(56.)).flex_none().child(art_fill(th, &thumb, "", 6., false)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(div().font_weight(SEMIBOLD).text_size(px(14.)).child(j.name.clone()))
                                    .child(state),
                            )
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(if j.total > 0 {
                                format!("{} · {} of {}", j.status, gb(j.downloaded), gb(j.total))
                            } else {
                                j.status.clone()
                            }))
                            .when(!j.done && !queued && j.total > 0, |d| d.child(progress(th, f, None, 6., None))),
                    )
                    .child(icon(Icon::ChevronRight, 16., th.tx3))
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                card(th)
                    .p(px(28.))
                    .items_center()
                    .text_color(th.tx3)
                    .child("Nothing downloading. Installs and updates you start show up here.")
                    .into_any_element(),
            );
        }
        let sub = if active > 0 { format!("{active} active or queued") } else { "Idle".into() };
        self.page_shell(th, "Downloads", sub, rows)
    }
}
