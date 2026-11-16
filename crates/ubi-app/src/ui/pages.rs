//! Full-width pages (Downloads, Activity) and the screens before the library (connecting,
//! sign in).

use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::{
    BOLD, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_fill, button, caption, card, display, icon, pill,
    progress,
};
use crate::app::{Conn, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{Actor, actor, actor_label, ago, gb, job_fraction};
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
                Some(g) => self.art_for(g, ArtSlot::Tile, Variant::Original, cx),
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
                pill(th, PillKind::Accent, Some(Icon::Download), format!("{:.0}%", f * 100.))
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
                    .focus_if(focused, th)
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

    pub fn render_activity(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let entries = self.activity.clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, a) in entries.iter().enumerate() {
            let focused = self.focused(Target::Body(i));
            let (color, i_) = match actor(&a.source) {
                Actor::App => (th.who_gui, Icon::MousePointer),
                Actor::Cli => (th.who_cli, Icon::SquareTerminal),
                Actor::Agent => (th.who_agent, Icon::Bot),
                Actor::System => (th.who_sys, Icon::Cog),
            };
            let game = a.product_id.and_then(|id| self.game(id)).map(|g| g.entry.name.clone());
            let pid = a.product_id;
            rows.push(
                div()
                    .id(("act", i))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(14.))
                    .py(px(11.))
                    .rounded(px(10.))
                    .border_b_1()
                    .border_color(th.line1)
                    .when(focused, |d| d.bg(th.ink3).focus_if(true, th))
                    .hover(|s| s.bg(th.ink2))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(id) = pid {
                            this.open_game(id, cx);
                            cx.notify();
                        }
                    }))
                    .child(div().w(px(96.)).flex_none().text_size(px(12.)).text_color(th.tx3).child(ago(a.at)))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .w(px(140.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .px(px(6.))
                                    .py(px(2.))
                                    .rounded(px(6.))
                                    .bg(color.opacity(0.13))
                                    .text_size(px(11.5))
                                    .font_weight(SEMIBOLD)
                                    .text_color(color)
                                    .child(icon(i_, 12., color))
                                    .child(actor_label(&a.source)),
                            ),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(a.action.clone()))
                    .when_some(game, |d, g| d.child(div().text_color(th.tx2).text_size(px(12.5)).child(g)))
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(div().text_color(th.tx3).child("No activity yet.").into_any_element());
        }
        self.page_shell(th, "Activity", "What the app, the CLI, agents and the daemon did".into(), rows)
    }

    pub fn render_splash(&mut self, th: &Theme, error: Option<String>, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(display(28., FontWeight::EXTRA_BOLD, th.tx1).child("Quiver"))
            .child(match &error {
                None => div()
                    .text_color(th.tx3)
                    .child(if self.demo { "Loading demo data…" } else { "Connecting to quiverd…" })
                    .into_any_element(),
                Some(e) => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.))
                    .max_w(px(520.))
                    .child(div().text_color(th.bad).text_center().child(format!("Could not reach quiverd: {e}")))
                    .child(
                        button("retry", th, Kind::Primary, Size::Medium, Some(Icon::RefreshCw), "Try again")
                            .on_click(cx.listener(|this, _, _, cx| this.retry_connect(cx))),
                    )
                    .into_any_element(),
            })
    }

    pub fn render_login(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Conn::Login(form) = &self.conn else { return div() };
        let two_factor = form.needs_code;
        let label = match (form.busy, two_factor) {
            (true, _) => "Signing in…",
            (false, true) => "Verify",
            (false, false) => "Sign in",
        };
        let field = |e: gpui::AnyView| div().child(e);
        let mut col = card(th)
            .w(px(380.))
            .p(px(24.))
            .gap(px(12.))
            .child(display(22., FontWeight::BOLD, th.tx1).child("Sign in to Ubisoft"))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(th.tx3)
                    .child("Quiver keeps the session in your keyring. It is not affiliated with Ubisoft."),
            );
        if two_factor {
            col = col
                .child(div().text_color(th.tx2).child("Enter the code from your authenticator app."))
                .child(field(form.code.clone().into()));
        } else {
            col = col
                .child(caption("Email", th.tx3))
                .child(field(form.email.clone().into()))
                .child(caption("Password", th.tx3))
                .child(field(form.password.clone().into()));
        }
        if let Some(e) = &form.error {
            col = col.child(div().text_color(th.bad).child(e.clone()));
        }
        let busy = form.busy;
        col = col.child(
            button("login", th, Kind::Primary, Size::Medium, None, label)
                .w_full()
                .font_weight(BOLD)
                .when(busy, |d| d.opacity(0.6))
                .on_click(cx.listener(|this, _, _, cx| this.submit_login(cx))),
        );
        let _ = MEDIUM;
        div().size_full().flex().items_center().justify_center().child(col)
    }
}
