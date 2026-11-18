//! Settings › Accounts › Epic Games (design/STORE-EPIC.md §5, §9.6): Epic accounts, and
//! signing in. "Sign in to Epic" opens Epic's page in Quiver's sign-in window
//! (`quiver-webview`), which hands back the code Epic's page ends on; quiverd exchanges
//! it. Without that window (no WebKitGTK), or when asked, it falls back to the browser and
//! a field for pasting the code.

use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::epic::webview::{self, Outcome};
use quiver_api::epic::{EpicAccount, EpicRequest, LoginStart};
use quiver_api::stores::AccountState;

use super::{SettingsAction, account_colors, card_head, head_row, note_row, short_id};
use crate::app::Quiver;
use crate::model::initials;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{Icon, Kind, PillKind, SEMIBOLD, avatar, card, mono, pill};
use crate::{err_text, on_tokio};

/// A focusable Epic control in Settings › Accounts.
#[derive(Clone, Debug, PartialEq)]
pub enum EpicAction {
    /// Open Epic's sign-in page in the sign-in window.
    SignIn,
    /// Open Epic's sign-in page in the browser and show the code field.
    PasteInstead,
    /// Send the pasted code.
    Submit,
    Cancel,
    /// Open the sign-in page again (the code works once, for a few minutes).
    Reopen,
    MakeDefault(usize),
    /// First press asks (the button says so), second signs out.
    SignOut(usize),
}

#[derive(Default)]
pub struct EpicUi {
    pub accounts: Option<Result<Vec<EpicAccount>, String>>,
    /// Signing in by pasting: the page opened, and the code field.
    pub signing_in: Option<(String, Entity<TextInput>)>,
    /// The sign-in window is open.
    pub window_open: bool,
    /// How the last sign-in window ended, when it didn't sign in.
    pub note: Option<String>,
    pub busy: bool,
    /// The account whose Sign out was pressed once.
    pub confirm_sign_out: Option<String>,
}

impl Quiver {
    pub fn load_epic_accounts(&mut self, cx: &mut Context<Self>) {
        self.call::<Vec<EpicAccount>, _>(Request::Epic(EpicRequest::Accounts), cx, |this, r, _| {
            this.ui.epic.accounts = Some(r.map_err(|e| err_text(&e)));
        });
    }

    fn epic_accounts(&self) -> Vec<EpicAccount> {
        match &self.ui.epic.accounts {
            Some(Ok(v)) => v.clone(),
            _ => Vec::new(),
        }
    }

    /// The Epic controls, in focus order (after Ubisoft's).
    pub(super) fn epic_actions(&self) -> Vec<SettingsAction> {
        let mut v = Vec::new();
        if self.ui.epic.signing_in.is_some() {
            v.extend([EpicAction::Submit, EpicAction::Reopen, EpicAction::Cancel]);
        } else {
            if !self.ui.epic.window_open {
                v.push(EpicAction::SignIn);
            }
            if self.ui.epic.window_open || self.ui.epic.note.is_some() {
                v.push(EpicAction::PasteInstead);
            }
        }
        for (i, a) in self.epic_accounts().iter().enumerate() {
            if !a.default {
                v.push(EpicAction::MakeDefault(i));
            }
            v.push(EpicAction::SignOut(i));
        }
        v.into_iter().map(SettingsAction::Epic).collect()
    }

    pub(super) fn epic_action(&mut self, a: EpicAction, cx: &mut Context<Self>) {
        if !matches!(a, EpicAction::SignOut(_)) {
            self.ui.epic.confirm_sign_out = None;
        }
        match a {
            EpicAction::SignIn => {
                if self.ui.epic.window_open {
                    return;
                }
                self.ui.epic.note = None;
                self.call::<LoginStart, _>(Request::Epic(EpicRequest::LoginBegin), cx, |this, r, cx| match r {
                    Ok(s) => this.epic_window(s.url, cx),
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            EpicAction::PasteInstead | EpicAction::Reopen => {
                let reopen = a == EpicAction::Reopen;
                self.ui.epic.note = None;
                self.call::<LoginStart, _>(Request::Epic(EpicRequest::LoginBegin), cx, move |this, r, cx| match r {
                    Ok(s) => this.epic_paste(s.url, reopen, cx),
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            EpicAction::Submit => {
                let Some((_, input)) = &self.ui.epic.signing_in else { return };
                let code = input.read(cx).text().trim().to_owned();
                if code.is_empty() {
                    self.show_toast("Paste the code from Epic's page first", cx);
                    return;
                }
                self.epic_submit(code, cx);
            }
            EpicAction::Cancel => self.ui.epic.signing_in = None,
            EpicAction::MakeDefault(i) => {
                let Some(a) = self.epic_accounts().get(i).cloned() else { return };
                self.call::<EpicAccount, _>(Request::Epic(EpicRequest::AccountDefault { account: a.account_id }), cx, |this, r, cx| match r {
                    Ok(a) => {
                        this.show_toast(format!("{} is now the default Epic account", a.display_name), cx);
                        this.load_epic_accounts(cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
            EpicAction::SignOut(i) => {
                let Some(a) = self.epic_accounts().get(i).cloned() else { return };
                if self.ui.epic.confirm_sign_out.as_deref() != Some(a.account_id.as_str()) {
                    self.ui.epic.confirm_sign_out = Some(a.account_id);
                    return;
                }
                self.ui.epic.confirm_sign_out = None;
                self.call::<EpicAccount, _>(Request::Epic(EpicRequest::Logout { account: Some(a.account_id) }), cx, |this, r, cx| match r {
                    Ok(a) => {
                        this.show_toast(format!("Signed {} out of Epic", a.display_name), cx);
                        this.load_epic_accounts(cx);
                        this.load_library(false, cx);
                    }
                    Err(e) => this.show_toast(err_text(&e), cx),
                });
            }
        }
    }

    /// Sign in in the sign-in window (off the UI thread); the code goes straight to
    /// quiverd. No window here: the browser and the code field instead.
    fn epic_window(&mut self, url: String, cx: &mut Context<Self>) {
        self.ui.epic.window_open = true;
        let u = url.clone();
        let rx = on_tokio(async move { webview::sign_in(&u, webview::DEFAULT_TIMEOUT).await });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or_else(|_| Outcome::Failed("cancelled".into()));
            this.update(cx, |this, cx| {
                this.ui.epic.window_open = false;
                match outcome {
                    Outcome::Code(code) => this.epic_submit(code.into_inner(), cx),
                    Outcome::Cancelled => this.ui.epic.note = Some("Sign-in cancelled: the window was closed.".into()),
                    Outcome::TimedOut => this.ui.epic.note = Some("The sign-in window timed out. Sign in again when you're ready.".into()),
                    Outcome::Failed(e) => this.ui.epic.note = Some(format!("The sign-in window failed: {e}")),
                    Outcome::Unavailable(why) => {
                        this.show_toast(format!("The sign-in window isn't available ({why}); install webkit2gtk-4.1 for it. Sign in in your browser instead."), cx);
                        this.epic_paste(url, false, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the page in the browser and show the code field.
    fn epic_paste(&mut self, url: String, reopen: bool, cx: &mut Context<Self>) {
        cx.open_url(&url);
        if !reopen || self.ui.epic.signing_in.is_none() {
            let input = cx.new(|cx| TextInput::new(cx, "Paste the code (or the whole page) here", false).compact(true));
            cx.observe(&input, |_, _, cx| cx.notify()).detach();
            self.ui.epic.signing_in = Some((url, input));
        }
        self.show_toast("Epic's sign-in page is open in your browser", cx);
    }

    fn epic_submit(&mut self, code: String, cx: &mut Context<Self>) {
        if self.ui.epic.busy {
            return;
        }
        self.ui.epic.busy = true;
        self.call::<EpicAccount, _>(Request::Epic(EpicRequest::LoginComplete { code }), cx, |this, r, cx| {
            this.ui.epic.busy = false;
            match r {
                Ok(a) => {
                    this.ui.epic.signing_in = None;
                    this.ui.epic.note = None;
                    this.show_toast(format!("Signed in to Epic as {}. Its games are joining the library.", a.display_name), cx);
                    this.load_epic_accounts(cx);
                    this.load_library(false, cx);
                }
                Err(e) => {
                    let e = err_text(&e);
                    this.ui.epic.note = Some(e.clone());
                    this.show_toast(e, cx);
                }
            }
        });
    }

    pub(super) fn settings_epic(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let accounts = self.epic_accounts();
        let signing_in = self.ui.epic.signing_in.clone();
        let window_open = self.ui.epic.window_open;
        let sign_in = (signing_in.is_none() && !window_open).then(|| {
            self.action_button(
                th,
                actions,
                SettingsAction::Epic(EpicAction::SignIn),
                if accounts.is_empty() { Kind::Primary } else { Kind::Secondary },
                Some(Icon::Plus),
                if accounts.is_empty() { "Sign in to Epic" } else { "Add Epic account" },
                cx,
            )
        });
        let mut out = vec![head_row(th, "Epic Games", "Epic games show in the library with an Epic badge. Installing and playing them comes next.", sign_in)];
        if let Some(Err(e)) = &self.ui.epic.accounts {
            out.push(card(th).p(px(16.)).text_color(th.bad).child(format!("Could not read Epic accounts: {e}")).into_any_element());
        }
        if let Some((_, input)) = signing_in {
            out.push(self.epic_sign_in_card(th, actions, input, cx));
        } else if window_open || self.ui.epic.note.is_some() {
            let (sub, text) = if window_open {
                (
                    "in Quiver's sign-in window",
                    "Sign in on Epic's page in the window that just opened (password, two-factor code, captcha: all on Epic's site). Quiver takes the code when Epic shows it, and the window keeps no cookies.".to_owned(),
                )
            } else {
                ("didn't finish", self.ui.epic.note.clone().unwrap_or_default())
            };
            let busy = self.ui.epic.busy;
            out.push(
                card(th)
                    .child(card_head(th, "Sign in to Epic", sub, None))
                    .child(div().px(px(16.)).pb(px(12.)).text_size(px(12.5)).text_color(th.tx2).child(if busy { "Signing in…".to_owned() } else { text }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                            .px(px(16.))
                            .py(px(12.))
                            .border_t_1()
                            .border_color(th.line1)
                            .child(self.action_button(th, actions, SettingsAction::Epic(EpicAction::PasteInstead), Kind::Ghost, Some(Icon::ExternalLink), "Paste a code instead", cx)),
                    )
                    .into_any_element(),
            );
        } else if accounts.is_empty() && matches!(self.ui.epic.accounts, Some(Ok(_))) {
            out.push(card(th).p(px(18.)).text_color(th.tx2).child("No Epic account is signed in. Sign in on Epic's own page, in Quiver's sign-in window; Quiver never sees your password.").into_any_element());
        }
        let games = |id: &str| self.games.iter().filter(|g| g.entry.owned_by.iter().any(|o| o == id) || g.entry.records.iter().any(|r| r.owned_by.iter().any(|o| o == id))).count();
        let rows: Vec<(EpicAccount, usize)> = accounts.iter().map(|a| (a.clone(), games(&a.account_id))).collect();
        for (i, (a, n)) in rows.into_iter().enumerate() {
            let (from, to) = account_colors(i + 2);
            let ended = a.state != AccountState::SignedIn;
            let mut buttons = div().flex().flex_none().items_center().gap(px(6.));
            if !a.default {
                buttons = buttons.child(self.action_button(th, actions, SettingsAction::Epic(EpicAction::MakeDefault(i)), Kind::Ghost, Some(Icon::Star), "Make default", cx));
            }
            let asking = self.ui.epic.confirm_sign_out.as_deref() == Some(a.account_id.as_str());
            buttons = buttons.child(self.action_button(
                th,
                actions,
                SettingsAction::Epic(EpicAction::SignOut(i)),
                if asking { Kind::Secondary } else { Kind::Ghost },
                Some(Icon::LogOut),
                if asking { "Sign out?" } else { "Sign out" },
                cx,
            ));
            let status = if ended {
                "Epic ended this session · sign in again to read its library".to_owned()
            } else if a.default {
                "Default Epic account".to_owned()
            } else {
                "Signed in".to_owned()
            };
            let linked = a.ubisoft.as_ref().map(|u| format!("Linked Ubisoft account: {}", u.name.clone().unwrap_or_else(|| short_id(&u.id))));
            out.push(
                card(th)
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .gap(px(14.))
                    .p(px(16.))
                    .child(avatar(&initials(&a.display_name), from, to, th.ink2).size(px(40.)).text_size(px(14.)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(200.))
                            .gap(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().font_weight(SEMIBOLD).text_size(px(14.5)).child(a.display_name.clone()))
                                    .child(pill(th, PillKind::Muted, None, "Epic"))
                                    .when(a.default, |d| d.child(pill(th, PillKind::Accent, None, "Default")))
                                    .when(ended, |d| d.child(pill(th, PillKind::Warn, None, "Sign in again"))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(px(12.))
                                    .text_color(th.tx3)
                                    .child(mono(11.5, th.tx3).child(short_id(&a.account_id)))
                                    .child("·")
                                    .child(format!("{n} game{}", if n == 1 { "" } else { "s" })),
                            )
                            .when_some(linked, |d, l| d.child(div().text_size(px(12.)).text_color(th.tx3).child(l)))
                            .child(div().mt(px(4.)).text_size(px(12.)).text_color(if ended { th.warn } else { th.tx2 }).child(status)),
                    )
                    .child(buttons)
                    .into_any_element(),
            );
        }
        out
    }

    fn epic_sign_in_card(&mut self, th: &Theme, actions: &[SettingsAction], input: Entity<TextInput>, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.ui.epic.busy;
        let step = |n: &str, text: &str| {
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .text_size(px(12.5))
                .child(div().w(px(18.)).flex_none().font_weight(SEMIBOLD).text_color(th.acc).child(n.to_owned()))
                .child(div().flex_1().min_w_0().text_color(th.tx2).child(text.to_owned()))
        };
        card(th)
            .child(card_head(th, "Sign in to Epic", "in your browser, on Epic's page", None))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(16.))
                    .pb(px(12.))
                    .child(step("1", "Sign in on the Epic page that just opened (password, two-factor code, captcha: all on Epic's site)."))
                    .child(step("2", "It ends on a page of text with \"authorizationCode\" in it. Copy the code, or the whole page."))
                    .child(step("3", "Paste it below. The code works once, for a few minutes; if it's refused, open the page again.")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(div().flex_1().min_w(px(240.)).child(input))
                    .child(self.action_button(th, actions, SettingsAction::Epic(EpicAction::Submit), Kind::Primary, Some(Icon::Check), if busy { "Signing in…" } else { "Sign in" }, cx))
                    .child(self.action_button(th, actions, SettingsAction::Epic(EpicAction::Reopen), Kind::Ghost, Some(Icon::ExternalLink), "Open the page again", cx))
                    .child(self.action_button(th, actions, SettingsAction::Epic(EpicAction::Cancel), Kind::Ghost, None, "Cancel", cx)),
            )
            .child(note_row(
                th,
                "Quiver signs in as Epic's own launcher, as legendary and Heroic do, and keeps its own session in your keyring. It never reads another launcher's session. CLI: quiver epic login",
            ))
            .into_any_element()
    }
}
