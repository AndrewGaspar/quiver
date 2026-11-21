//! Settings → Machines › Moonlight clients (design/REMOTE.md phase 1b): this machine's
//! GameStream server for Moonlight on TVs, phones and other PCs. On or off, where it listens
//! (every network, Tailscale only, or a list), the ports to allow, the PIN for a client
//! waiting to pair, and the paired clients.
//!
//! Every action is this machine's quiverd's (`Moonlight*` requests), so the CLI does the
//! same: `quiver remote moonlight status|enable|disable|pin|clients|unpair`.

use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::moonlight::{MoonlightClient, MoonlightStatus, is_tailnet_v6};
use quiver_api::{Event, Request};

use super::{SettingsAction, card_head, foot_cli, note_row};
use crate::app::Quiver;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::ui::{FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, card, icon, mono, pill};
use crate::{err_text, model::ago};

/// Where the server listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// Every address.
    All,
    /// The tailnet's addresses only (`tailscale0`).
    Tailscale,
    /// A list typed here.
    Custom,
}

impl Bind {
    fn of(bind: &[String]) -> Bind {
        match bind {
            [] => Bind::All,
            [one] if one == "tailscale0" => Bind::Tailscale,
            _ => Bind::Custom,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Bind::All => "All networks",
            Bind::Tailscale => "Tailscale only",
            Bind::Custom => "Custom…",
        }
    }
}

/// A focusable control in the Moonlight card.
#[derive(Clone, Debug, PartialEq)]
pub enum MoonlightAction {
    Toggle,
    Bind(Bind),
    CustomSave,
    Pin,
    /// First press asks, second unpairs.
    Unpair(String),
}

#[derive(Default)]
pub struct MoonlightUi {
    pub status: Option<Result<MoonlightStatus, String>>,
    /// The custom address list being edited.
    pub custom: Option<Entity<TextInput>>,
    /// The PIN field, while a client waits.
    pub pin: Option<Entity<TextInput>>,
    pub pin_error: Option<String>,
    pub busy: bool,
    pub confirm_unpair: Option<String>,
    /// Where to listen, chosen while it's off: used when it's turned on.
    pub chosen_bind: Option<Vec<String>>,
}

impl Quiver {
    fn ml_status(&self) -> Option<&MoonlightStatus> {
        self.mx.settings.moonlight.status.as_ref().and_then(|s| s.as_ref().ok())
    }

    fn ml_call<T, F>(&self, req: Request, cx: &mut Context<Self>, done: F)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    {
        let Some(local) = self.mx.local.clone().or_else(|| self.source.clone()) else { return };
        self.call_on(local, req, cx, done);
    }

    /// Ask this machine's quiverd for the Moonlight server's state.
    pub fn moonlight_reload(&mut self, cx: &mut Context<Self>) {
        self.ml_call::<MoonlightStatus, _>(Request::MoonlightStatus, cx, |this, r, cx| {
            let pending = r.as_ref().ok().and_then(|s| s.pending.clone());
            let ui = &mut this.mx.settings.moonlight;
            ui.status = Some(r.map_err(|e| err_text(&e)));
            match pending {
                Some(_) if ui.pin.is_none() => {
                    let input = cx.new(|cx| TextInput::new(cx, "1234", false).compact(true));
                    cx.observe(&input, |_, _, cx| cx.notify()).detach();
                    ui.pin = Some(input);
                }
                None => {
                    ui.pin = None;
                    ui.pin_error = None;
                }
                _ => {}
            }
            cx.notify();
        });
    }

    /// `Moonlight*` events from this machine's quiverd: a toast for a pairing request (from
    /// any page: the PIN is entered here), then a reload.
    pub fn moonlight_event(&mut self, e: &Event, cx: &mut Context<Self>) {
        match e {
            Event::MoonlightPairingRequested { device_name, address } => {
                self.toast_for(format!("Moonlight on {device_name} ({address}) wants to pair: enter its PIN in Settings → Machines").into(), false, 9000, cx);
            }
            Event::MoonlightPaired { name, .. } => self.show_toast(format!("Moonlight on {name} is paired"), cx),
            Event::MoonlightChanged => {}
            _ => return,
        }
        self.moonlight_reload(cx);
    }

    /// The card's controls, in focus order.
    pub(super) fn moonlight_actions(&self) -> Vec<SettingsAction> {
        let Some(s) = self.ml_status() else { return Vec::new() };
        let ui = &self.mx.settings.moonlight;
        let mut v = vec![MoonlightAction::Toggle, MoonlightAction::Bind(Bind::All), MoonlightAction::Bind(Bind::Tailscale), MoonlightAction::Bind(Bind::Custom)];
        if ui.custom.is_some() {
            v.push(MoonlightAction::CustomSave);
        }
        if s.pending.is_some() {
            v.push(MoonlightAction::Pin);
        }
        v.extend(s.clients.iter().map(|c| MoonlightAction::Unpair(c.fingerprint.clone())));
        v.into_iter().map(SettingsAction::Moonlight).collect()
    }

    fn ml_enable(&mut self, bind: Option<Vec<String>>, cx: &mut Context<Self>) {
        let req = Request::MoonlightEnable { bind, codecs: None };
        self.ml_call::<MoonlightStatus, _>(req, cx, |this, r, cx| match r {
            Ok(_) => {
                this.mx.settings.moonlight.custom = None;
                this.moonlight_reload(cx);
            }
            Err(e) => {
                this.show_error(&e, cx);
                this.moonlight_reload(cx);
            }
        });
    }

    pub(crate) fn moonlight_action(&mut self, a: MoonlightAction, cx: &mut Context<Self>) {
        if !matches!(a, MoonlightAction::Unpair(_)) {
            self.mx.settings.moonlight.confirm_unpair = None;
        }
        let Some(s) = self.ml_status().cloned() else { return };
        match a {
            MoonlightAction::Toggle => {
                if s.enabled {
                    self.ml_call::<serde_json::Value, _>(Request::MoonlightDisable, cx, |this, r, cx| {
                        match r {
                            Ok(_) => this.show_toast("Moonlight is off: nothing listens for it", cx),
                            Err(e) => this.show_error(&e, cx),
                        }
                        this.moonlight_reload(cx);
                    });
                } else {
                    self.show_toast("Moonlight is on: add this machine in Moonlight by its address", cx);
                    let bind = self.mx.settings.moonlight.chosen_bind.take();
                    self.ml_enable(bind, cx);
                }
            }
            MoonlightAction::Bind(Bind::Custom) => {
                if self.mx.settings.moonlight.custom.is_none() {
                    let current = s.bind.join(", ");
                    let input = cx.new(|cx| {
                        let mut t = TextInput::new(cx, "100.101.12.7, fd7a:115c:a1e0::7 or an interface", false).compact(true);
                        t.set_text(current.clone(), cx);
                        t
                    });
                    cx.observe(&input, |_, _, cx| cx.notify()).detach();
                    self.mx.settings.moonlight.custom = Some(input);
                }
            }
            MoonlightAction::Bind(b) => {
                self.mx.settings.moonlight.custom = None;
                let bind = if b == Bind::Tailscale { vec!["tailscale0".to_owned()] } else { Vec::new() };
                if s.enabled {
                    self.ml_enable(Some(bind), cx);
                } else {
                    // Off: nothing listens until it's turned on, with this choice.
                    self.mx.settings.moonlight.chosen_bind = Some(bind);
                }
            }
            MoonlightAction::CustomSave => {
                let Some(input) = self.mx.settings.moonlight.custom.clone() else { return };
                let list: Vec<String> = input.read(cx).text().split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
                if list.is_empty() {
                    return self.show_toast("Type an address or an interface name", cx);
                }
                if s.enabled {
                    self.ml_enable(Some(list), cx);
                } else {
                    self.mx.settings.moonlight.chosen_bind = Some(list);
                    self.mx.settings.moonlight.custom = None;
                }
            }
            MoonlightAction::Pin => {
                let ui = &mut self.mx.settings.moonlight;
                let Some(input) = ui.pin.clone() else { return };
                if ui.busy {
                    return;
                }
                let pin = input.read(cx).text().trim().to_owned();
                if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
                    ui.pin_error = Some("Type the 4-digit PIN Moonlight shows".into());
                    return;
                }
                ui.busy = true;
                ui.pin_error = None;
                self.ml_call::<MoonlightClient, _>(Request::MoonlightPin { pin }, cx, |this, r, cx| {
                    this.mx.settings.moonlight.busy = false;
                    match r {
                        Ok(_) => this.mx.settings.moonlight.pin = None,
                        Err(e) => this.mx.settings.moonlight.pin_error = Some(err_text(&e)),
                    }
                    this.moonlight_reload(cx);
                });
            }
            MoonlightAction::Unpair(fp) => {
                if self.mx.settings.moonlight.confirm_unpair.as_deref() != Some(fp.as_str()) {
                    self.mx.settings.moonlight.confirm_unpair = Some(fp);
                    return;
                }
                self.mx.settings.moonlight.confirm_unpair = None;
                self.ml_call::<MoonlightClient, _>(Request::MoonlightUnpair { fingerprint: fp }, cx, |this, r, cx| {
                    match r {
                        Ok(c) => this.show_toast(format!("Unpaired {}: it can't stream from this machine any more", c.name), cx),
                        Err(e) => this.show_error(&e, cx),
                    }
                    this.moonlight_reload(cx);
                });
            }
        }
    }

    fn ml_button(&self, th: &Theme, actions: &[SettingsAction], a: MoonlightAction, kind: Kind, i: Option<Icon>, label: impl Into<gpui::SharedString>, cx: &mut Context<Self>) -> AnyElement {
        self.action_button(th, actions, SettingsAction::Moonlight(a), kind, i, label, cx)
    }

    pub(super) fn settings_moonlight(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = match &self.mx.settings.moonlight.status {
            None => return None,
            Some(Err(e)) => {
                return Some(card(th).p(px(16.)).text_color(th.bad).child(format!("Could not read the Moonlight server's state: {e}")).into_any_element());
            }
            Some(Ok(s)) => s.clone(),
        };
        let ui = &self.mx.settings.moonlight;
        let (custom, pin, pin_error, busy, asking) = (ui.custom.clone(), ui.pin.clone(), ui.pin_error.clone(), ui.busy, ui.confirm_unpair.clone());
        let bind = match (&custom, &ui.chosen_bind) {
            (Some(_), _) => Bind::Custom,
            (None, Some(chosen)) if !s.enabled => Bind::of(chosen),
            _ => Bind::of(&s.bind),
        };

        // On/off.
        let focused = self.action_focused(actions, &SettingsAction::Moonlight(MoonlightAction::Toggle));
        let ix = actions.iter().position(|a| *a == SettingsAction::Moonlight(MoonlightAction::Toggle)).unwrap_or(usize::MAX);
        let state = match (s.enabled, s.listening.is_empty(), &s.error) {
            (true, false, _) => format!("Listening on {} as \"{}\". Paired clients only; add this machine in Moonlight by its address or Tailscale name.", s.listening.join(", "), s.name),
            (true, true, Some(e)) => format!("Not listening: {e}"),
            (true, true, None) => "Starting…".to_owned(),
            (false, ..) => "Off: nothing listens for Moonlight. Turn it on to stream to a TV, phone or another PC with Moonlight.".to_owned(),
        };
        let toggle_row = div()
            .id("ml-toggle")
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
                    .child(crate::ui::game::toggle(th, s.enabled))
                    .child(div().font_weight(MEDIUM).child(if s.enabled { "Moonlight is on" } else { "Moonlight is off" })),
            )
            .child(div().flex_1().min_w(px(200.)).text_size(px(12.)).text_color(if s.error.is_some() && s.enabled { th.warn } else { th.tx3 }).child(state))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, crate::nav::Target::Body(ix));
                this.moonlight_action(MoonlightAction::Toggle, cx);
                cx.notify();
            }));

        // Where it listens.
        let mut bind_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(12.))
            .border_t_1()
            .border_color(th.line1)
            .child(div().w(px(90.)).text_size(px(12.)).font_weight(MEDIUM).text_color(th.tx2).child("Listen on"));
        for b in [Bind::All, Bind::Tailscale, Bind::Custom] {
            let kind = if b == bind { Kind::Primary } else { Kind::Secondary };
            bind_row = bind_row.child(self.ml_button(th, actions, MoonlightAction::Bind(b), kind, (b == bind).then_some(Icon::Check), b.label(), cx));
        }
        let custom_row = custom.map(|input| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .px(px(16.))
                .pb(px(12.))
                .child(div().w(px(90.)))
                .child(div().w(px(360.)).max_w(gpui::relative(1.)).child(input))
                .child(self.ml_button(th, actions, MoonlightAction::CustomSave, Kind::Primary, Some(Icon::Check), "Save", cx))
                .into_any_element()
        });

        // The ports, for firewalls and Tailscale ACLs.
        let list = |proto: &str| s.ports.iter().filter(|p| p.protocol == proto).map(|p| p.port.to_string()).collect::<Vec<_>>().join(", ");
        let ports_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(th.line1)
            .child(div().w(px(90.)).text_size(px(12.)).font_weight(MEDIUM).text_color(th.tx2).child("Ports"))
            .child(mono(12., th.tx2).child(format!("TCP {} · UDP {}", list("tcp"), list("udp"))))
            .child(div().text_size(px(11.5)).text_color(th.tx4).child("allow these in a firewall or Tailscale ACL; never forward them"));

        // A client waiting for its PIN.
        let pending = s.pending.clone().zip(pin).map(|(p, input)| {
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .px(px(16.))
                .py(px(14.))
                .border_t_1()
                .border_color(th.line1)
                .bg(th.acc_soft)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(icon(Icon::KeyRound, 16., th.acc_hi))
                        .child(div().font_weight(SEMIBOLD).child(format!("{} wants to pair", p.device_name)))
                        .child(mono(11.5, th.tx3).child(p.address.clone())),
                )
                .child(div().text_size(px(12.)).text_color(th.tx2).child("Enter the PIN Moonlight shows on that device. Only pair a device you're holding: whoever's request you approve gets in."))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(div().w(px(110.)).child(input))
                        .child(self.ml_button(th, actions, MoonlightAction::Pin, Kind::Primary, Some(Icon::Check), if busy { "Pairing…" } else { "Pair" }, cx)),
                )
                .when_some(pin_error, |d, e| d.child(div().text_size(px(12.5)).text_color(th.bad).child(e)))
                .into_any_element()
        });

        // Paired clients.
        let mut clients = div().flex().flex_col();
        for c in &s.clients {
            let confirm = asking.as_deref() == Some(c.fingerprint.as_str());
            let short = c.fingerprint.chars().take(16).collect::<String>();
            clients = clients.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(div().flex().items_center().justify_center().size(px(32.)).rounded(px(8.)).bg(th.ink4).child(icon(Icon::Cast, 15., th.tx2)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(200.))
                            .gap(px(2.))
                            .child(div().flex().items_center().gap(px(8.)).child(div().font_weight(SEMIBOLD).child(c.name.clone())).child(pill(th, PillKind::Muted, None, "Moonlight")))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(6.))
                                    .text_size(px(12.))
                                    .text_color(th.tx3)
                                    .child(mono(11.5, th.tx3).child(short))
                                    .child("·")
                                    .child(format!("paired {} from {}", ago(c.paired_at), c.address)),
                            ),
                    )
                    .child(self.ml_button(
                        th,
                        actions,
                        MoonlightAction::Unpair(c.fingerprint.clone()),
                        if confirm { Kind::Secondary } else { Kind::Ghost },
                        Some(Icon::Trash),
                        if confirm { "Unpair?" } else { "Unpair" },
                        cx,
                    )),
            );
        }
        let none = s.clients.is_empty().then(|| {
            div()
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(th.line1)
                .text_size(px(12.5))
                .text_color(th.tx3)
                .child("No Moonlight client is paired. In Moonlight, add this machine by its address; it shows a PIN to enter here.")
        });
        let streaming = s.stream.clone().map(|st| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .text_size(px(12.5))
                .child(div().size(px(7.)).rounded_full().bg(if st.streaming { th.ok } else { th.tx4 }))
                .child(div().text_color(th.tx2).child(format!("{} {} to {}", if st.streaming { "Streaming" } else { "Launched" }, st.app, st.client)))
        });

        let tailnet = bind == Bind::Tailscale || s.listening.iter().any(|a| is_tailnet_v6(a));
        let tip = if tailnet {
            "Over a Tailscale IPv6 address (fd7a:…), set Moonlight's video packet size to 1024 (Settings → Advanced): Moonlight treats those as local and sends 1392-byte packets, too big for Tailscale's 1280 MTU, and the server then has to send smaller packets without error correction. A 100.x address needs nothing."
        } else {
            "Moonlight pairs with a 4-digit PIN, which is weaker than Quiver's own pairing: a paired Moonlight client can stream, but never manage this machine."
        };
        Some(
            card(th)
                .child(card_head(th, "Moonlight clients", "TVs, phones and other PCs with Moonlight", None))
                .child(toggle_row)
                .child(bind_row)
                .children(custom_row)
                .child(ports_row)
                .children(pending)
                .children(streaming)
                .child(clients)
                .children(none)
                .child(note_row(th, tip))
                .child(foot_cli(th, "quiver remote moonlight pin 1234", "enter a PIN from a terminal, here or with --machine"))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_choice() {
        assert_eq!(Bind::of(&[]), Bind::All);
        assert_eq!(Bind::of(&["tailscale0".into()]), Bind::Tailscale);
        assert_eq!(Bind::of(&["100.64.1.2".into()]), Bind::Custom);
        assert_eq!(Bind::of(&["tailscale0".into(), "lo".into()]), Bind::Custom);
    }
}
