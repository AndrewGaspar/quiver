//! Accounts across machines in Settings › Accounts (design/REMOTE.md §6): where each account
//! is signed in and which machine is playing on it, and **Add to <machine>** for an Epic
//! account (Epic's one-time exchange code, minted on the machine that has the session and
//! redeemed on the other, which then has a session of its own).
//!
//! Presence always comes from this machine's quiverd (`AccountsRequest::Presence`), which asks
//! its paired servers and counts what paired clients told it; hand-overs go to it too, naming
//! the machines (`from`/`to`, `None` for this one). With a paired machine shown (the machine
//! switcher), the page is that machine's: its accounts, **Sign in on <machine>**, and its
//! accounts' other machines.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, px};
use quiver_api::Request;
use quiver_api::StoreId;
use quiver_api::accounts::{AccountPresence, AccountsRequest, HandoverResult, Presence};

use super::SettingsAction;
use crate::app::Quiver;
use crate::err_text;
use crate::machines::View;
use crate::theme::Theme;
use crate::ui::{Icon, Kind};

/// A focusable control for accounts across machines.
#[derive(Clone, Debug, PartialEq)]
pub enum PresenceAction {
    /// Give machine `to` (a paired machine's id; `None`: this PC) its own session of the
    /// Epic account shown, from the machine shown.
    AddEpicTo { account_id: String, to: Option<String> },
    /// Give the machine shown its own session of an Epic account another machine has
    /// (`from`: a paired machine's id; `None`: this PC).
    AddEpicFrom { account_id: String, from: Option<String> },
}

/// An Epic account the machine shown doesn't have, and a machine that does.
#[derive(Clone, Debug, PartialEq)]
pub struct EpicSource {
    pub account_id: String,
    pub name: String,
    /// A paired machine's id; `None`: this PC.
    pub from: Option<String>,
    pub from_name: String,
}

#[derive(Default)]
pub struct PresenceUi {
    pub presence: Option<Result<Presence, String>>,
    loading: bool,
    /// A hand-over in flight: the account id.
    pub busy: Option<String>,
}

impl Quiver {
    /// Ask this machine's quiverd where every account is (it asks the paired servers).
    pub fn load_presence(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.mx.local.clone().or_else(|| self.source.clone()) else { return };
        if self.ui.presence.loading {
            return;
        }
        self.ui.presence.loading = true;
        self.call_on::<Presence, _>(local, Request::Accounts(AccountsRequest::Presence { wait_ms: Some(3000) }), cx, |this, r, _| {
            this.ui.presence.loading = false;
            this.ui.presence.presence = Some(r.map_err(|e| err_text(&e)));
        });
    }

    /// Presence is read for the first time once Settings › Accounts or a game page needs it.
    pub fn presence_wanted(&mut self, cx: &mut Context<Self>) {
        if self.ui.presence.presence.is_none() {
            self.load_presence(cx);
        }
    }

    pub fn presence(&self) -> Option<&Presence> {
        self.ui.presence.presence.as_ref().and_then(|p| p.as_ref().ok())
    }

    /// The machine the page is about: its id (as presence lists machines) and name.
    pub fn page_machine(&self) -> (Option<String>, String) {
        match self.mx.shown() {
            Some(r) => (Some(r.id().to_owned()), r.name().to_owned()),
            None => (self.presence().and_then(|p| p.machines.iter().find(|m| m.here).map(|m| m.id.clone())), "This PC".into()),
        }
    }

    /// A paired machine is shown: the page is its accounts ("Sign in on Tower").
    pub fn accounts_elsewhere(&self) -> Option<String> {
        matches!(self.mx.view, View::Machine(_)).then(|| self.page_machine().1)
    }

    fn account_presence(&self, store: StoreId, account_id: &str) -> Option<&AccountPresence> {
        self.presence()?.account(store, account_id)
    }

    /// "Also on Tower · playing Origins there", from the shown machine's point of view.
    pub fn presence_line(&self, store: StoreId, account_id: &str) -> Option<(String, bool)> {
        let p = self.account_presence(store, account_id)?;
        let (shown, _) = self.page_machine();
        let others: Vec<_> = p.machines.iter().filter(|m| Some(&m.id) != shown.as_ref()).collect();
        let label = |m: &quiver_api::accounts::SignedInOn| if m.here { "This PC".to_owned() } else { m.machine.clone() };
        if let Some(m) = others.iter().find(|m| m.playing.is_some()) {
            let g = m.playing.as_ref().map(|g| g.name.clone()).unwrap_or_default();
            let mut line = format!("Playing {g} on {}", label(m));
            if store == StoreId::Ubisoft {
                line.push_str(" · its games won't start here until that ends");
            }
            return Some((line, true));
        }
        if others.is_empty() {
            return Some(("Only on this machine".to_owned(), false));
        }
        Some((format!("Also signed in on {}", others.iter().map(|m| label(m)).collect::<Vec<_>>().join(", ")), false))
    }

    /// For the game page's launch account: the other machine playing on it, if one is.
    pub fn playing_elsewhere(&self, store: StoreId, account_name: &str) -> Option<String> {
        let p = self.presence()?.accounts.iter().find(|a| a.store == store && a.name == account_name)?;
        let (shown, _) = self.page_machine();
        let m = p.machines.iter().find(|m| m.playing.is_some() && Some(&m.id) != shown.as_ref())?;
        Some(if m.here { "This PC".to_owned() } else { m.machine.clone() })
    }

    /// Machines an Epic account could be added to: paired machines that are up, and this
    /// PC, that don't have it, other than the one shown. (`None`: this PC.)
    pub fn epic_targets(&self, account_id: &str) -> Vec<(Option<String>, String)> {
        let has: Vec<String> = self.account_presence(StoreId::Epic, account_id).map(|p| p.machines.iter().map(|m| m.id.clone()).collect()).unwrap_or_default();
        let (shown, _) = self.page_machine();
        let mut out = Vec::new();
        if matches!(self.mx.view, View::Machine(_)) {
            let here = self.presence().and_then(|p| p.machines.iter().find(|m| m.here).map(|m| m.id.clone()));
            if here.as_ref().is_none_or(|h| !has.contains(h)) {
                out.push((None, "This PC".to_owned()));
            }
        }
        for r in self.mx.remotes.iter().filter(|r| r.up()) {
            let id = r.id().to_owned();
            if Some(&id) != shown.as_ref() && !has.contains(&id) {
                out.push((Some(id), r.name().to_owned()));
            }
        }
        out
    }

    /// With a paired machine shown: Epic accounts other machines have and it doesn't, each
    /// with a machine to take it from (this PC first).
    pub fn epic_sources(&self) -> Vec<EpicSource> {
        let (Some(shown), _) = self.page_machine() else { return Vec::new() };
        if !matches!(self.mx.view, View::Machine(_)) {
            return Vec::new();
        }
        let Some(p) = self.presence() else { return Vec::new() };
        p.accounts
            .iter()
            .filter(|a| a.store == StoreId::Epic && !a.machines.iter().any(|m| m.id == shown))
            .filter_map(|a| {
                let m = a.machines.iter().find(|m| m.here).or_else(|| a.machines.iter().find(|m| self.mx.remote(&m.id).is_some_and(|r| r.up())))?;
                Some(EpicSource {
                    account_id: a.account_id.clone(),
                    name: a.name.clone(),
                    from: (!m.here).then(|| m.id.clone()),
                    from_name: if m.here { "This PC".to_owned() } else { m.machine.clone() },
                })
            })
            .collect()
    }

    pub(super) fn presence_action(&mut self, a: PresenceAction, cx: &mut Context<Self>) {
        let (account_id, from, to) = match a {
            PresenceAction::AddEpicTo { account_id, to } => (account_id, self.mx.shown().map(|r| r.id().to_owned()), to),
            PresenceAction::AddEpicFrom { account_id, from } => (account_id, from, self.mx.shown().map(|r| r.id().to_owned())),
        };
        // This machine's quiverd does it (it is paired with every machine shown), naming
        // the machines; the code goes from one to the other inside their pinned TLS.
        let Some(local) = self.mx.local.clone().or_else(|| self.source.clone()) else { return };
        if self.ui.presence.busy.is_some() {
            return;
        }
        self.ui.presence.busy = Some(account_id.clone());
        let req = Request::Accounts(AccountsRequest::Handover { store: StoreId::Epic, account: Some(account_id), from, to });
        self.call_on::<HandoverResult, _>(local, req, cx, |this, r, cx| {
            this.ui.presence.busy = None;
            match r {
                Ok(r) => {
                    let to = if r.to == this.mx.here_name() { "This PC".to_owned() } else { r.to.clone() };
                    this.show_toast(format!("{to} now has its own Epic session for {}", r.name), cx);
                    this.load_presence(cx);
                    this.load_epic_accounts(cx);
                }
                Err(e) => this.show_error(&e, cx),
            }
        });
    }

    /// An Epic account card's presence line and **Add to …** buttons.
    pub(super) fn epic_presence_row(&mut self, th: &Theme, actions: &[SettingsAction], account_id: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.presence_line(StoreId::Epic, account_id);
        let targets = self.epic_targets(account_id);
        if line.is_none() && targets.is_empty() {
            return None;
        }
        let busy = self.ui.presence.busy.as_deref() == Some(account_id);
        let mut row = div().flex().flex_wrap().items_center().gap(px(6.)).mt(px(6.));
        if let Some((text, warn)) = line {
            row = row.child(presence_text(th, text, warn));
        }
        for (to, name) in targets {
            let a = SettingsAction::Presence(PresenceAction::AddEpicTo { account_id: account_id.to_owned(), to });
            let label = if busy { "Adding…".to_owned() } else { format!("Add to {name}") };
            row = row.child(self.action_button(th, actions, a, Kind::Ghost, Some(Icon::Plus), label, cx));
        }
        Some(row.into_any_element())
    }

    /// **Add <account> from <machine>** buttons for the machine shown.
    pub(super) fn epic_sources_row(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Option<AnyElement> {
        let sources = self.epic_sources();
        if sources.is_empty() {
            return None;
        }
        let mut row = div().flex().flex_wrap().items_center().gap(px(8.)).px(px(16.)).py(px(12.));
        row = row.child(div().text_size(px(12.5)).text_color(th.tx2).child("Or give it a session of its own from another machine (Epic's one-time code; no password):"));
        for s in sources {
            let busy = self.ui.presence.busy.as_deref() == Some(s.account_id.as_str());
            let a = SettingsAction::Presence(PresenceAction::AddEpicFrom { account_id: s.account_id.clone(), from: s.from.clone() });
            let label = if busy { "Adding…".to_owned() } else { format!("Add {} from {}", s.name, s.from_name) };
            row = row.child(self.action_button(th, actions, a, Kind::Secondary, Some(Icon::Plus), label, cx));
        }
        Some(crate::ui::card(th).child(row).into_any_element())
    }

    /// The Epic presence actions for the account cards, in focus order.
    pub(super) fn presence_actions(&self, account_id: &str) -> Vec<SettingsAction> {
        self.epic_targets(account_id)
            .into_iter()
            .map(|(to, _)| SettingsAction::Presence(PresenceAction::AddEpicTo { account_id: account_id.to_owned(), to }))
            .collect()
    }
}

/// A presence line: a dot (amber when another machine is playing) and the text.
pub fn presence_text(th: &Theme, text: String, playing: bool) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(7.))
        .text_size(px(12.))
        .child(crate::ui::icon(Icon::Server, 12., if playing { th.warn } else { th.tx3 }))
        .child(div().text_color(if playing { th.warn } else { th.tx2 }).child(text))
        .into_any_element()
}
