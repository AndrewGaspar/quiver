//! Settings → Machines (design/REMOTE.md §3, §5): this machine's name and remote on/off,
//! pairing another machine with a code shown here, adding a machine by address (typed or
//! found on the LAN) and its code, and the paired machines: rename, revoke, wake, show.
//!
//! Every action is this machine's quiverd's (`Remote*` requests), so the CLI does the same:
//! `quiver remote status|enable|disable|pair|add|peers|rename|revoke|wake|discover`.

use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::remote::{Discovered, PairCode, PeerInfo, PeerState, RemoteStatus, Role};

use super::{SettingsAction, card_head, foot_cli, head_row, note_row};
use crate::app::Quiver;
use crate::machines::{Link, View};
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{BOLD, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, card, display, icon, mono, pill};
use crate::{err_text, model::ago};

/// A focusable control in Settings → Machines.
#[derive(Clone, Debug, PartialEq)]
pub enum MachineAction {
    /// Turn remote on or off on this machine.
    Remote,
    /// Rename this machine (`None`) or a paired one (its id): opens the field.
    Rename(Option<String>),
    RenameSave,
    RenameCancel,
    /// Show a pairing code here, for another machine to type.
    PairHere,
    PairHereEnd,
    /// Open the Add form (and look for machines on the LAN).
    Add,
    /// Look on the LAN again.
    Discover,
    /// Put a found machine's address in the form.
    Pick(usize),
    AddSubmit,
    AddCancel,
    /// Show this paired machine's library (the switcher's entry).
    Show(String),
    Wake(String),
    /// First press asks, second revokes.
    Revoke(String),
}

pub struct AddForm {
    pub addr: Entity<TextInput>,
    pub code: Entity<TextInput>,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Default)]
pub struct MachinesSettings {
    pub add: Option<AddForm>,
    pub found: Option<Result<Vec<Discovered>, String>>,
    pub searching: bool,
    /// Renaming this machine (`None`) or a paired one, and the field.
    pub rename: Option<(Option<String>, Entity<TextInput>)>,
    /// The code shown here while this machine's pairing window is open.
    pub code: Option<PairCode>,
    pub confirm_revoke: Option<String>,

}

fn state_color(th: &Theme, s: PeerState) -> gpui::Hsla {
    match s {
        PeerState::Online => th.ok,
        PeerState::Asleep => th.warn,
        PeerState::Offline => th.tx4,
    }
}

impl Quiver {
    fn here_status(&self) -> Option<&RemoteStatus> {
        self.mx.here.as_ref().and_then(|h| h.as_ref().ok())
    }

    /// The section's controls, in focus order.
    pub(super) fn machines_actions(&self) -> Vec<SettingsAction> {
        let ms = &self.mx.settings;
        let mut v = vec![MachineAction::Add];
        let listening = self.here_status().is_some_and(|s| s.listening.is_some());
        match &ms.rename {
            Some((None, _)) => v.extend([MachineAction::RenameSave, MachineAction::RenameCancel]),
            _ => v.push(MachineAction::Rename(None)),
        }
        v.push(MachineAction::Remote);
        if listening {
            v.push(if ms.code.is_some() { MachineAction::PairHereEnd } else { MachineAction::PairHere });
        }
        if ms.add.is_some() {
            v.push(MachineAction::Discover);
            if let Some(Ok(found)) = &ms.found {
                v.extend((0..found.len()).map(MachineAction::Pick));
            }
            v.extend([MachineAction::AddSubmit, MachineAction::AddCancel]);
        }
        for p in self.here_status().map(|s| s.peers.clone()).unwrap_or_default() {
            if p.role == Role::Server {
                if self.mx.remote(&p.id).is_some_and(|r| r.up()) {
                    v.push(MachineAction::Show(p.id.clone()));
                }
                if p.mac.is_some() && self.mx.remote(&p.id).is_none_or(|r| !r.up()) {
                    v.push(MachineAction::Wake(p.id.clone()));
                }
            }
            match &ms.rename {
                Some((Some(id), _)) if *id == p.id => v.extend([MachineAction::RenameSave, MachineAction::RenameCancel]),
                _ => v.push(MachineAction::Rename(Some(p.id.clone()))),
            }
            v.push(MachineAction::Revoke(p.id.clone()));
        }
        v.into_iter().map(SettingsAction::Machine).collect()
    }

    fn local_call<T, F>(&self, req: Request, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let Some(local) = self.mx.local.clone().or_else(|| self.source.clone()) else { return };
        self.call_on(local, req, cx, done);
    }

    pub(crate) fn machine_action(&mut self, a: MachineAction, cx: &mut Context<Self>) {
        if !matches!(a, MachineAction::Revoke(_)) {
            self.mx.settings.confirm_revoke = None;
        }
        match a {
            MachineAction::Remote => {
                let on = self.here_status().is_some_and(|s| s.enabled);
                let req = if on { Request::RemoteDisable } else { Request::RemoteEnable { listen: None } };
                self.local_call::<serde_json::Value, _>(req, cx, move |this, r, cx| match r {
                    Ok(_) => {
                        this.show_toast(if on { "Remote is off: nothing listens" } else { "Remote is on: paired machines can reach this one" }, cx);
                        this.mx.settings.code = None;
                        this.machines_reload(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
            MachineAction::Rename(target) => {
                let current = match &target {
                    None => self.mx.here_name(),
                    Some(id) => self.here_status().and_then(|s| s.peers.iter().find(|p| p.id == *id)).map(|p| p.name.clone()).unwrap_or_default(),
                };
                let input = cx.new(|cx| {
                    let mut t = TextInput::new(cx, "Name", false).compact(false);
                    t.set_text(current.clone(), cx);
                    t
                });
                cx.observe(&input, |_, _, cx| cx.notify()).detach();
                self.mx.settings.rename = Some((target, input));
            }
            MachineAction::RenameCancel => self.mx.settings.rename = None,
            MachineAction::RenameSave => {
                let Some((target, input)) = self.mx.settings.rename.clone() else { return };
                let name = input.read(cx).text().trim().to_owned();
                if name.is_empty() {
                    return self.show_toast("Type a name first", cx);
                }
                self.local_call::<serde_json::Value, _>(Request::RemoteRename { id: target, name: name.clone() }, cx, move |this, r, cx| match r {
                    Ok(_) => {
                        this.mx.settings.rename = None;
                        this.show_toast(format!("Renamed to {name}"), cx);
                        this.machines_reload(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
            MachineAction::PairHere => {
                self.local_call::<PairCode, _>(Request::RemotePairBegin, cx, |this, r, cx| match r {
                    Ok(code) => {
                        this.mx.settings.code = Some(code);
                        this.machines_reload(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
            MachineAction::PairHereEnd => {
                self.mx.settings.code = None;
                self.local_call::<(), _>(Request::RemotePairEnd, cx, |this, _, cx| this.machines_reload(cx));
            }
            MachineAction::Add => {
                if self.mx.settings.add.is_none() {
                    let addr = cx.new(|cx| TextInput::new(cx, "tower, 192.168.1.40 or a Tailscale name", false).compact(true));
                    let code = cx.new(|cx| TextInput::new(cx, "123456", false).compact(true));
                    cx.observe(&addr, |_, _, cx| cx.notify()).detach();
                    cx.observe(&code, |_, _, cx| cx.notify()).detach();
                    self.mx.settings.add = Some(AddForm { addr, code, error: None, busy: false });
                }
                self.machine_action(MachineAction::Discover, cx);
            }
            MachineAction::Discover => {
                self.mx.settings.searching = true;
                self.local_call::<Vec<Discovered>, _>(Request::RemoteDiscover { wait_ms: Some(1500) }, cx, |this, r, _| {
                    this.mx.settings.searching = false;
                    this.mx.settings.found = Some(r.map_err(|e| err_text(&e)));
                });
            }
            MachineAction::Pick(i) => {
                let Some(Ok(found)) = &self.mx.settings.found else { return };
                let Some(d) = found.get(i) else { return };
                let addr = d.addrs.first().cloned().unwrap_or_default();
                if let Some(f) = &self.mx.settings.add {
                    f.addr.update(cx, |t, cx| t.set_text(addr.clone(), cx));
                }
            }
            MachineAction::AddCancel => {
                self.mx.settings.add = None;
                self.mx.settings.found = None;
            }
            MachineAction::AddSubmit => {
                let Some(f) = &mut self.mx.settings.add else { return };
                if f.busy {
                    return;
                }
                let addr = f.addr.read(cx).text().trim().to_owned();
                let code = f.code.read(cx).text().trim().to_owned();
                if addr.is_empty() || code.is_empty() {
                    f.error = Some("Type the machine's address and the code it shows (`quiver remote pair` there).".into());
                    return;
                }
                f.busy = true;
                f.error = None;
                self.local_call::<PeerInfo, _>(Request::RemotePairWith { addr, code }, cx, |this, r, cx| {
                    let Some(f) = &mut this.mx.settings.add else { return };
                    f.busy = false;
                    match r {
                        Ok(p) => {
                            this.mx.settings.add = None;
                            this.mx.settings.found = None;
                            this.show_toast(format!("Paired with {}. Its games are joining the library.", p.name), cx);
                            this.machines_reload(cx);
                        }
                        Err(e) => f.error = Some(err_text(&e)),
                    }
                });
            }
            MachineAction::Show(id) => self.set_view(View::Machine(id), cx),
            MachineAction::Wake(id) => self.machine_wake(&id, cx),
            MachineAction::Revoke(id) => {
                if self.mx.settings.confirm_revoke.as_deref() != Some(id.as_str()) {
                    self.mx.settings.confirm_revoke = Some(id);
                    return;
                }
                self.mx.settings.confirm_revoke = None;
                self.local_call::<Vec<PeerInfo>, _>(Request::RemoteRevoke { id }, cx, |this, r, cx| match r {
                    Ok(gone) => {
                        let name = gone.first().map(|p| p.name.clone()).unwrap_or_default();
                        this.show_toast(format!("Revoked {name}: it can't reach this machine, nor this one it"), cx);
                        this.machines_reload(cx);
                    }
                    Err(e) => this.show_error(&e, cx),
                });
            }
        }
    }

    /// This machine's pairing window closed (`Event::PairingEnded`).
    pub fn machines_pairing_ended(&mut self, e: &quiver_api::Event, cx: &mut Context<Self>) {
        let quiver_api::Event::PairingEnded { outcome, name, .. } = e else { return };
        let shown = self.mx.settings.code.take().is_some();
        match outcome.as_str() {
            "paired" => self.show_toast(format!("Paired with {}", name.clone().unwrap_or_default()), cx),
            "expired" if shown => self.show_toast("The pairing code expired", cx),
            "failed" if shown => self.toast_for("Three wrong codes: pairing closed".into(), true, 7000, cx),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn m_button(&self, th: &Theme, actions: &[SettingsAction], a: MachineAction, kind: Kind, i: Option<Icon>, label: impl Into<gpui::SharedString>, cx: &mut Context<Self>) -> AnyElement {
        self.action_button(th, actions, SettingsAction::Machine(a), kind, i, label, cx)
    }

    pub(super) fn settings_machines(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let add = self.m_button(th, actions, MachineAction::Add, Kind::Secondary, Some(Icon::Plus), "Add machine", cx);
        let mut out = vec![head_row(
            th,
            "Machines",
            "Pair a machine with a GPU and its library joins this one: install there, and stream its games here.",
            Some(add),
        )];
        match &self.mx.here {
            None => out.push(card(th).p(px(16.)).text_color(th.tx3).child("Reading this machine's remote state…").into_any_element()),
            Some(Err(e)) => out.push(card(th).p(px(16.)).text_color(th.bad).child(format!("Could not read the remote state: {e}")).into_any_element()),
            Some(Ok(_)) => {}
        }
        if let Some(s) = self.here_status().cloned() {
            out.push(self.machines_this(th, actions, &s, cx));
        }
        if self.mx.settings.add.is_some() {
            out.push(self.machines_add_card(th, actions, cx));
        }
        let peers = self.here_status().map(|s| s.peers.clone()).unwrap_or_default();
        if !peers.is_empty() {
            let mut list = card(th).child(card_head(th, "Paired machines", &format!("{}", peers.len()), None));
            for p in &peers {
                list = list.child(self.machines_peer_row(th, actions, p, cx));
            }
            out.push(list.into_any_element());
        } else if self.here_status().is_some() {
            out.push(
                card(th)
                    .p(px(18.))
                    .text_color(th.tx2)
                    .child("No machine is paired yet. On the server, run `quiver remote enable` and `quiver remote pair`, then Add machine here with the code it prints.")
                    .into_any_element(),
            );
        }
        out
    }

    fn machines_this(&mut self, th: &Theme, actions: &[SettingsAction], s: &RemoteStatus, cx: &mut Context<Self>) -> AnyElement {
        let renaming = matches!(&self.mx.settings.rename, Some((None, _)));
        let rename_input = self.mx.settings.rename.as_ref().filter(|(t, _)| t.is_none()).map(|(_, i)| i.clone());
        let on = s.enabled;
        let focused = self.action_focused(actions, &SettingsAction::Machine(MachineAction::Remote));
        let ix = actions.iter().position(|a| *a == SettingsAction::Machine(MachineAction::Remote)).unwrap_or(usize::MAX);
        let toggle_row = div()
            .id("mx-remote")
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .py(px(12.))
            .border_t_1()
            .border_color(th.line1)
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .focus_ring(focused, th, 8.)
                    .child(crate::ui::game::toggle(th, on))
                    .child(div().font_weight(MEDIUM).child(if on { "Remote is on" } else { "Remote is off" })),
            )
            .child(div().flex_1().min_w(px(200.)).text_size(px(12.)).text_color(th.tx3).child(match (&s.listening, &s.error) {
                (Some(a), _) => format!("Listening on {a}{}. Paired machines only; reach it over the LAN or a Tailscale/WireGuard address, never a forwarded port.", if s.discoverable { ", seen on the LAN" } else { "" }),
                (None, Some(e)) if on => format!("Not listening: {e}"),
                _ => "Nothing listens. Turn it on to pair this machine and use it from another.".to_owned(),
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, crate::nav::Target::Body(ix));
                this.machine_action(MachineAction::Remote, cx);
                cx.notify();
            }));
        let name_line = if let Some(input) = rename_input {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(div().w(px(240.)).child(input))
                .child(self.m_button(th, actions, MachineAction::RenameSave, Kind::Primary, Some(Icon::Check), "Save", cx))
                .child(self.m_button(th, actions, MachineAction::RenameCancel, Kind::Ghost, None, "Cancel", cx))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .child(display(20., BOLD, th.tx1).child(s.name.clone()))
                .child(mono(11.5, th.tx3).child(s.id.clone()))
                .child(self.m_button(th, actions, MachineAction::Rename(None), Kind::Ghost, Some(Icon::Pencil), "Rename", cx))
                .into_any_element()
        };
        let pairing = if let Some(code) = self.mx.settings.code.clone() {
            let digits = if code.code.len() == 6 { format!("{} {}", &code.code[..3], &code.code[3..]) } else { code.code.clone() };
            let left = (code.expires_at - chrono::Utc::now()).num_seconds().max(0);
            Some(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(16.))
                    .px(px(16.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(th.line1)
                    .bg(th.acc_soft)
                    .child(display(30., BOLD, th.acc_hi).child(digits))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(220.))
                            .gap(px(3.))
                            .child(div().font_weight(SEMIBOLD).child(format!("Type this on the other machine · {}:{:02} left", left / 60, left % 60)))
                            .child(div().text_size(px(12.)).text_color(th.tx2).child(format!(
                                "Settings → Machines → Add there, with {} as the address. Three wrong codes end it.",
                                code.addrs.first().cloned().unwrap_or_else(|| s.name.clone())
                            ))),
                    )
                    .child(self.m_button(th, actions, MachineAction::PairHereEnd, Kind::Ghost, None, "Stop pairing", cx))
                    .into_any_element(),
            )
        } else if s.listening.is_some() {
            Some(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(div().flex_1().min_w(px(200.)).text_size(px(12.5)).text_color(th.tx2).child("Let another machine use this one: show a code here and type it there."))
                    .child(self.m_button(th, actions, MachineAction::PairHere, Kind::Secondary, Some(Icon::KeyRound), "Pair a machine", cx))
                    .into_any_element(),
            )
        } else {
            None
        };
        card(th)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .p(px(16.))
                    .child(
                        div().flex().items_center().justify_center().size(px(40.)).rounded(px(10.)).bg(th.ink4).child(icon(Icon::Laptop, 20., th.acc_hi)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(name_line)
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(if renaming { "Paired machines see this name from their next pairing." } else { "This machine" })),
                    ),
            )
            .child(toggle_row)
            .children(pairing)
            .into_any_element()
    }

    fn machines_add_card(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> AnyElement {
        let ms = &self.mx.settings;
        let (addr, code, error, busy) = match &ms.add {
            Some(f) => (f.addr.clone(), f.code.clone(), f.error.clone(), f.busy),
            None => return div().into_any_element(),
        };
        let found = ms.found.clone();
        let searching = ms.searching;
        let paired: Vec<String> = self.here_status().map(|s| s.peers.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        let mut found_row = div().flex().flex_wrap().items_center().gap(px(8.)).px(px(16.)).pb(px(12.));
        match &found {
            Some(Ok(list)) if !list.is_empty() => {
                for (i, d) in list.iter().enumerate() {
                    let is_paired = paired.contains(&d.id);
                    let label = if is_paired { format!("{} · paired", d.name) } else { format!("{} · Pair…", d.name) };
                    found_row = found_row.child(self.m_button(th, actions, MachineAction::Pick(i), Kind::Secondary, Some(Icon::Server), label, cx));
                }
            }
            Some(Ok(_)) => found_row = found_row.child(div().text_size(px(12.)).text_color(th.tx3).child("No Quiver machine answered on the LAN. Type its address instead.")),
            Some(Err(e)) => found_row = found_row.child(div().text_size(px(12.)).text_color(th.warn).child(format!("Couldn't look on the LAN: {e}"))),
            None => {}
        }
        found_row = found_row.child(self.m_button(th, actions, MachineAction::Discover, Kind::Ghost, Some(Icon::RefreshCw), if searching { "Looking…" } else { "Look again" }, cx));
        let field = |label: &str, input: Entity<TextInput>, w: f32| {
            div()
                .flex()
                .flex_col()
                .gap(px(5.))
                .child(div().text_size(px(12.)).font_weight(MEDIUM).text_color(th.tx2).child(label.to_owned()))
                .child(div().w(px(w)).max_w(gpui::relative(1.)).child(input))
        };
        card(th)
            .child(card_head(th, "Add a machine", "on the LAN, or by address", None))
            .child(found_row)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_end()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(field("Address", addr, 300.))
                    .child(field("Code", code, 120.))
                    .child(self.m_button(th, actions, MachineAction::AddSubmit, Kind::Primary, Some(Icon::Check), if busy { "Pairing…" } else { "Pair" }, cx))
                    .child(self.m_button(th, actions, MachineAction::AddCancel, Kind::Ghost, None, "Cancel", cx)),
            )
            .when_some(error, |d, e| d.child(div().px(px(16.)).pb(px(12.)).text_size(px(12.5)).text_color(th.bad).child(e)))
            .child(note_row(
                th,
                "The code is checked without ever being sent (SPAKE2), and each machine then pins the other's key. On the other machine: quiver remote pair",
            ))
            .child(foot_cli(th, "quiver remote add tower 418207", "the same from a terminal"))
            .into_any_element()
    }

    fn machines_peer_row(&mut self, th: &Theme, actions: &[SettingsAction], p: &PeerInfo, cx: &mut Context<Self>) -> AnyElement {
        let remote = self.mx.remote(&p.id);
        let state = remote.map(|r| r.state()).unwrap_or(p.state);
        let link_note = remote.and_then(|r| match &r.link {
            Link::Down(e) if r.peer.state == PeerState::Online => Some(e.clone()),
            _ => None,
        });
        let activity = remote.map(|r| r.activity()).unwrap_or_default();
        let games = remote.map(|r| r.library.iter().filter(|e| e.on_disk()).count()).unwrap_or(0);
        let (role_pill, what) = match p.role {
            Role::Server => ("Server", if games > 0 { format!("{games} games installed there") } else { "streams to this machine".into() }),
            Role::Client => ("Client", "It uses this machine's library".to_owned()),
        };
        let status = match (p.role, state) {
            (Role::Client, PeerState::Online) => format!("Connected now ({})", p.connections),
            (_, PeerState::Online) => "Online".to_owned(),
            (_, PeerState::Asleep) => "Asleep · Wake-on-LAN can wake it".to_owned(),
            (_, PeerState::Offline) => "Offline".to_owned(),
        };
        let seen = p.last_seen.map(|t| format!("seen {}", ago(t))).unwrap_or_else(|| "never seen".into());
        let renaming = self.mx.settings.rename.as_ref().filter(|(t, _)| t.as_deref() == Some(p.id.as_str())).map(|(_, i)| i.clone());
        let asking = self.mx.settings.confirm_revoke.as_deref() == Some(p.id.as_str());
        let mut buttons = div().flex().flex_wrap().flex_none().items_center().gap(px(6.));
        if p.role == Role::Server {
            if remote.is_some_and(|r| r.up()) {
                buttons = buttons.child(self.m_button(th, actions, MachineAction::Show(p.id.clone()), Kind::Secondary, Some(Icon::LibraryBig), "Show its library", cx));
            }
            if p.mac.is_some() && remote.is_none_or(|r| !r.up()) {
                buttons = buttons.child(self.m_button(th, actions, MachineAction::Wake(p.id.clone()), Kind::Ghost, Some(Icon::Power), "Wake", cx));
            }
        }
        if renaming.is_some() {
            buttons = buttons
                .child(self.m_button(th, actions, MachineAction::RenameSave, Kind::Primary, Some(Icon::Check), "Save", cx))
                .child(self.m_button(th, actions, MachineAction::RenameCancel, Kind::Ghost, None, "Cancel", cx));
        } else {
            buttons = buttons.child(self.m_button(th, actions, MachineAction::Rename(Some(p.id.clone())), Kind::Ghost, Some(Icon::Pencil), "Rename", cx));
        }
        buttons = buttons.child(self.m_button(
            th,
            actions,
            MachineAction::Revoke(p.id.clone()),
            if asking { Kind::Secondary } else { Kind::Ghost },
            Some(Icon::Trash),
            if asking { "Revoke?" } else { "Revoke" },
            cx,
        ));
        let title: AnyElement = match renaming {
            Some(input) => div().w(px(220.)).child(input).into_any_element(),
            None => div().font_weight(SEMIBOLD).text_size(px(14.5)).child(p.name.clone()).into_any_element(),
        };
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap(px(14.))
            .px(px(16.))
            .py(px(14.))
            .border_t_1()
            .border_color(th.line1)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(36.))
                    .rounded(px(9.))
                    .bg(th.ink4)
                    .child(icon(if p.role == Role::Server { Icon::Server } else { Icon::Laptop }, 17., th.tx2)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(220.))
                    .gap(px(3.))
                    .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(title).child(pill(th, PillKind::Muted, None, role_pill)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(th.tx3)
                            .child(mono(11.5, th.tx3).child(p.id.clone()))
                            .when_some(p.addr.clone(), |d, a| d.child("·").child(mono(11.5, th.tx3).child(a)))
                            .child("·")
                            .child(what),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(7.))
                            .mt(px(4.))
                            .text_size(px(12.))
                            .child(div().size(px(7.)).rounded_full().bg(state_color(th, state)))
                            .child(div().text_color(th.tx2).child(status))
                            .child(div().text_color(th.tx4).child(format!("· {seen}")))
                            .when(!activity.is_empty(), |d| d.child(pill(th, PillKind::Accent, Some(Icon::Download), activity.clone()))),
                    )
                    .when_some(link_note, |d, e| d.child(div().text_size(px(12.)).text_color(th.warn).child(format!("Its quiverd didn't answer this app: {e}")))),
            )
            .child(buttons)
            .into_any_element()
    }
}
