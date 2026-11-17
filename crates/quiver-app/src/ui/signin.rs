//! The screens before the library: connecting, "can't reach quiverd", and sign-in (email and
//! password, then the two-step code), which also adds another account to a running app.

use gpui::{AnyElement, Context, Focusable, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::global::{CancelSignin, NextField, SubmitSignin};
use super::settings::brand_mark;
use super::{BOLD, Icon, Kind, MEDIUM, SEMIBOLD, Size, art_fill, button, command_line, display, icon, mono};
use crate::app::{Conn, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::theme::{Theme, rgba};

impl Quiver {
    /// Connecting, or the daemon couldn't be reached (`error`).
    pub fn render_splash(&mut self, th: &Theme, error: Option<String>, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let brand = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(brand_mark(th, 30.))
            .child(display(26., FontWeight::EXTRA_BOLD, th.tx1).child("Quiver"));
        let body: AnyElement = match error {
            None => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_color(th.tx3)
                .child(icon(Icon::LoaderCircle, 15., th.tx3))
                .child(if self.demo { "Loading demo data…" } else { "Connecting to quiverd…" })
                .into_any_element(),
            Some(e) => {
                let socket = quiver_api::socket_path();
                let log = quiver_api::client::daemon_log();
                let kv = |k: &str, v: String| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .text_size(px(12.5))
                        .child(div().w(px(70.)).flex_none().text_color(th.tx3).child(k.to_owned()))
                        .child(mono(12., th.tx2).flex_1().min_w_0().truncate().child(v))
                };
                super::card(th)
                    .w(px(self.dialog_width(520.)))
                    .p(px(24.))
                    .gap(px(16.))
                    .shadow(vec![super::soft_shadow(rgba(0x000000, 0.5), 24., 70.)])
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .flex_none()
                                    .size(px(38.))
                                    .rounded(px(10.))
                                    .bg(th.bad_soft)
                                    .child(icon(Icon::PlugZap, 18., th.bad)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(div().text_size(px(17.)).font_weight(BOLD).child("Can't reach quiverd"))
                                    .child(div().text_size(px(12.5)).text_color(th.tx3).child(
                                        "Quiver's background service runs installs, launches and your Ubisoft session. The app starts it when it isn't running, but it didn't answer.",
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .bg(th.bad_soft)
                            .text_size(px(12.5))
                            .text_color(th.bad)
                            .child(e),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(kv("Socket", crate::model::tilde(&socket)))
                            .child(kv("Log", crate::model::tilde(&log))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                button("retry", th, Kind::Primary, Size::Medium, Some(Icon::RefreshCw), "Try again")
                                    .on_click(cx.listener(|this, _, _, cx| this.retry_connect(cx))),
                            )
                            .child(div().flex_1())
                            .child(command_line(th, "quiver status").w(px(200.))),
                    )
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(22.))
            .bg(th.ink0)
            .child(brand)
            .child(body)
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(th.tx4)
                    .child("Quiver is a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft."),
            )
    }

    /// Sign in (first account, a session that ended, or another account).
    pub fn render_login(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Conn::Login(form) = &self.conn else { return div() };
        let two_factor = form.needs_code;
        let busy = form.busy;
        let error = form.error.clone();
        let code = form.code.read(cx).text().to_owned();
        let (email_in, password_in, code_in) = (form.email.clone(), form.password.clone(), form.code.clone());
        let adding = self.ui.login_adding && self.status.as_ref().is_some_and(|s| s.signed_in.is_some());
        let accounts: Vec<String> = self.status.as_ref().map(|s| s.accounts.iter().map(|a| a.name.clone()).collect()).unwrap_or_default();
        let email = self.ui.login_email.clone();

        // Steps: Account › Two-step verification › Library.
        let step = |n: usize, label: &str, state: u8| {
            // 0 done, 1 current, 2 to come
            let (fg, weight) = match state {
                0 => (th.ok, MEDIUM),
                1 => (th.acc_hi, SEMIBOLD),
                _ => (th.tx3, MEDIUM),
            };
            let mark = match state {
                0 => icon(Icon::CircleCheck, 15., th.ok).into_any_element(),
                1 => div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .rounded_full()
                    .bg(th.acc)
                    .text_color(th.acc_ink)
                    .text_size(px(10.))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .child(n.to_string())
                    .into_any_element(),
                _ => div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(th.tx4)
                    .text_size(px(10.))
                    .child(n.to_string())
                    .into_any_element(),
            };
            div().flex().flex_none().items_center().gap(px(6.)).text_size(px(12.)).font_weight(weight).text_color(fg).child(mark).child(label.to_owned())
        };
        let rule = || div().flex_1().h(px(1.)).bg(th.line2);
        let steps = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(step(1, "Account", if two_factor { 0 } else { 1 }))
            .child(rule())
            .child(step(2, "Two-step verification", if two_factor { 1 } else { 2 }))
            .child(rule())
            .child(step(3, "Library", 2));

        let label = |t: &str| div().text_size(px(12.)).font_weight(MEDIUM).text_color(th.tx2).child(t.to_owned());
        let mut col = div().flex().flex_col().gap(px(18.)).child(steps);
        if two_factor {
            let who = email.clone().map(|e| mask_email(&e));
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().text_size(px(21.)).font_weight(BOLD).child("Enter your verification code"))
                    .child(div().text_color(th.tx2).text_size(px(13.)).line_height(px(19.)).child(if who.is_some() {
                        "Open your authenticator app and enter the 6-digit code."
                    } else {
                        "A sign-in is waiting for its code. Open your authenticator app and enter the 6-digit code."
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(12.5))
                            .when_some(who.clone(), |d, w| {
                                d.child(icon(Icon::User, 13., th.tx3))
                                    .child(div().font_weight(SEMIBOLD).text_color(th.tx1).child(w))
                                    .child(div().text_color(th.tx4).child("·"))
                            })
                            .child(
                                div()
                                    .id("not-you")
                                    .text_color(th.acc_hi)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .on_click(cx.listener(|this, _, _, cx| this.restart_signin(cx)))
                                    .child(if email.is_some() { "Not you?" } else { "Start over" }),
                            ),
                    ),
            );
            // Six boxes over the real (invisible) field, which keeps focus and input.
            let digits: Vec<char> = code.chars().filter(|c| !c.is_whitespace()).take(6).collect();
            let cur = digits.len().min(5);
            let boxes = div().flex().justify_between().children((0..6).map(|i| {
                let on = i == cur && !busy;
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(52.))
                    .h(px(62.))
                    .rounded(px(10.))
                    .bg(th.ink1)
                    .border_1()
                    .border_color(if on { th.acc } else { th.line2 })
                    .when(on, |d| d.border_2())
                    .when(on, |d| d.shadow(vec![gpui::BoxShadow {
                        color: th.acc_soft,
                        offset: gpui::point(px(0.), px(0.)),
                        blur_radius: px(0.),
                        spread_radius: px(4.),
                        inset: false,
                    }]))
                    .font_family(crate::assets::MONO_FONT)
                    .text_size(px(26.))
                    .font_weight(SEMIBOLD)
                    .child(match digits.get(i) {
                        Some(c) => div().child(c.to_string()).into_any_element(),
                        None if on => div().w(px(2.)).h(px(28.)).bg(th.acc).into_any_element(),
                        None => div().into_any_element(),
                    })
            }));
            col = col.child(
                div()
                    .relative()
                    .child(boxes)
                    .child(div().absolute().inset_0().opacity(0.).overflow_hidden().child(code_in.clone())),
            );
        } else {
            col = col
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(div().text_size(px(21.)).font_weight(BOLD).child(if adding { "Add a Ubisoft account" } else { "Sign in with your Ubisoft account" }))
                        .child(div().text_color(th.tx2).text_size(px(13.)).child(if adding {
                            format!("Its games join this library next to {}'s. Shared games keep launching with your default account.", accounts.join(" and "))
                        } else {
                            "Quiver signs in to Ubisoft the way Ubisoft Connect does, then shows every game you own.".to_owned()
                        })),
                )
                .child(div().flex().flex_col().gap(px(6.)).child(label("Email")).child(email_in.clone()))
                .child(div().flex().flex_col().gap(px(6.)).child(label("Password")).child(password_in.clone()));
        }
        if let Some(e) = error {
            col = col.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(9.))
                    .rounded(px(8.))
                    .bg(th.bad_soft)
                    .text_size(px(12.5))
                    .text_color(th.bad)
                    .child(div().pt(px(1.)).child(icon(Icon::CircleX, 14., th.bad)))
                    .child(e),
            );
        }
        let (label_text, ready) = match (busy, two_factor) {
            (true, true) => ("Verifying…", false),
            (true, false) => ("Signing in…", false),
            (false, true) => ("Verify", code.trim().len() == 6),
            (false, false) => ("Continue", true),
        };
        col = col.child(
            button("login", th, Kind::Primary, Size::Large, None, label_text)
                .w_full()
                .font_weight(BOLD)
                .when(!ready, |d| d.opacity(0.55))
                .on_click(cx.listener(|this, _, _, cx| this.submit_signin(cx))),
        );
        if !two_factor {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .text_color(th.tx3)
                    .child(icon(Icon::KeyRound, 13., th.tx3))
                    .child(div().flex_1().min_w_0().child("Your password goes to Ubisoft and is never stored. The session ticket goes to the system keyring.")),
            );
        }

        let card_w = self.dialog_width(480.);
        // Narrow windows: less padding, so the fields keep their width.
        let card_px = if card_w < 480. { 18. } else { 30. };
        let card = div()
            .w(px(card_w))
            .flex()
            .flex_col()
            .px(px(card_px))
            .py(px(28.))
            .rounded(px(14.))
            .bg(th.ink2.opacity(0.96))
            .border_1()
            .border_color(th.line1)
            .shadow(vec![super::soft_shadow(th.acc.opacity(0.10), 0., 180.), super::soft_shadow(rgba(0x000000, 0.6), 30., 80.)])
            .child(col);

        let center = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(18.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(brand_mark(th, 30.))
                    .child(display(22., FontWeight::EXTRA_BOLD, th.tx1).child("Quiver")),
            )
            .when_some(self.welcome_signin_banner(th), |d, b| d.child(b))
            .when(adding, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .w(px(card_w))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(10.))
                        .bg(th.acc_soft)
                        .border_1()
                        .border_color(th.acc_line)
                        .text_size(px(12.5))
                        .text_color(th.acc_hi)
                        .child(icon(Icon::Users, 14., th.acc_hi))
                        .child(div().flex_1().min_w_0().child(format!("Signed in: {}. Adding another account; the first stays the default.", accounts.join(", ")))),
                )
            })
            .when(!adding && !accounts.is_empty() && self.status.as_ref().is_some_and(|s| s.signed_in.is_none()), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .w(px(card_w))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(10.))
                        .bg(th.warn_soft)
                        .text_size(px(12.5))
                        .text_color(th.warn)
                        .child(icon(Icon::ClockAlert, 14., th.warn))
                        .child(div().flex_1().min_w_0().child("The Ubisoft session ended. Sign in again to continue.")),
                )
            })
            .child(card)
            .child(
                div()
                    .max_w(px(card_w))
                    .text_center()
                    .text_size(px(11.5))
                    .text_color(th.tx3)
                    .child("Quiver is a third-party launcher for your Ubisoft library. Not affiliated with or endorsed by Ubisoft."),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .max_w(px(card_w))
                    .items_center()
                    .gap(px(10.))
                    .child(command_line(th, "quiver login").w(px(230.)))
                    .child(div().text_size(px(11.5)).text_color(th.tx4).child("the same sign-in, from a terminal")),
            );

        let mut root = div()
            .id("signin")
            .relative()
            .size_full()
            .bg(th.ink0)
            .on_action(cx.listener(|this, _: &SubmitSignin, _, cx| this.submit_signin(cx)))
            .on_action(cx.listener(|this, _: &CancelSignin, _, cx| this.cancel_add_account(cx)))
            .on_action(cx.listener(move |_, _: &NextField, window, cx| {
                // Tab: email <-> password.
                let e = email_in.read(cx).focus_handle(cx);
                let p = password_in.read(cx).focus_handle(cx);
                if e.is_focused(window) { window.focus(&p, cx) } else { window.focus(&e, cx) }
            }));
        if let Some(m) = self.render_signin_mosaic(th, cx) {
            root = root.child(m);
        }
        // Signing in is optional (the library works without Ubisoft), so there is always a
        // way back.
        let root = root
            .child(center)
            .child(
                div().absolute().top(px(18.)).left(px(18.)).child(
                    button("signin-back", th, Kind::Secondary, Size::Medium, Some(Icon::ArrowLeft), "Back to Quiver")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_add_account(cx))),
                ),
            )
            .child(div().absolute().top(px(26.)).right(px(24.)).flex().items_center().gap(px(6.)).child(super::kbd(th, "Esc")).child(
                div().text_size(px(12.)).text_color(th.tx3).child("Cancel"),
            ));
        div().size_full().child(root)
    }

    /// The library's art, dimmed, behind the card (only once there is a library: adding an
    /// account or signing in again).
    pub fn render_signin_mosaic(&mut self, th: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.games.is_empty() {
            return None;
        }
        let games: Vec<crate::model::GameRow> = self.games.iter().cycle().take(40).cloned().collect();
        let cols = 10usize;
        let w = (self.viewport.0 + 80.) / cols as f32;
        let h = w * 280. / 240.;
        let tiles: Vec<AnyElement> = games
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let art = self.art_for(g, ArtSlot::Tile, Variant::Tile, cx);
                let (r, c) = (i / cols, i % cols);
                div()
                    .absolute()
                    .left(px(c as f32 * w - 40.))
                    .top(px(r as f32 * h - 40.))
                    .w(px(w - 14.))
                    .h(px(h - 14.))
                    .child(art_fill(th, &art, "", 10., false))
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .child(div().absolute().inset_0().opacity(0.45).children(tiles))
                .child(div().absolute().inset_0().bg(gpui::linear_gradient(
                    180.,
                    gpui::linear_color_stop(th.ink0.opacity(0.86), 0.),
                    gpui::linear_color_stop(th.ink0.opacity(0.95), 1.),
                )))
                .into_any_element(),
        )
    }
}

/// "a•••@g•••.com", like Ubisoft shows it.
pub fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((user, domain)) => {
            let first = |s: &str| s.chars().next().map(|c| c.to_string()).unwrap_or_default();
            let (host, tld) = domain.rsplit_once('.').unwrap_or((domain, ""));
            let tld = if tld.is_empty() { String::new() } else { format!(".{tld}") };
            format!("{}•••@{}•••{tld}", first(user), first(host))
        }
        None => e.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::mask_email;

    #[test]
    fn masks_email() {
        assert_eq!(mask_email("andrew@gmail.com"), "a•••@g•••.com");
        assert_eq!(mask_email("x@host"), "x•••@h•••");
        assert_eq!(mask_email("plain"), "plain");
    }
}
