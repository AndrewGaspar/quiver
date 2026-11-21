//! Settings → About › System setup (design/INSTALL.md §5): the pieces of Quiver that need root,
//! each with its state and a plain reason, and **Set up…**, which opens a terminal running
//! `quiver system setup`. The terminal shows the script and asks before `sudo` runs it; the
//! app never asks for a password itself.
//!
//! The check is `quiver_api::system::check` on this machine (files, polkit, systemctl; no
//! root, nothing changed), run off the UI thread. The Moonlight card uses the same report for
//! its firewall notice.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::system::{Options, Report, State};

use super::{SettingsAction, card_head, foot_cli, note_row};
use crate::app::Quiver;
use crate::theme::Theme;
use crate::ui::{Icon, Kind, MEDIUM, PillKind, SEMIBOLD, card, command_line, icon, pill};

#[derive(Clone, Debug, PartialEq)]
pub enum SystemAction {
    /// Open a terminal running `quiver system setup` (with `--firewall lan,tailnet` when a
    /// firewall piece is what's missing).
    SetUp,
    Recheck,
}

#[derive(Default)]
pub struct SystemUi {
    pub report: Option<Report>,
    pub loading: bool,
    pub checked: Option<Instant>,
    /// A terminal we opened is still running setup.
    pub terminal_open: bool,
}

/// The `quiver` next to this app, else the one on PATH.
fn quiver_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("quiver")))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "quiver".into())
}

/// How to run `argv` in a new terminal window: `xdg-terminal-exec`, else `$TERMINAL`, else the
/// first known terminal installed.
pub fn terminal_argv(argv: &[String], have: &dyn Fn(&str) -> bool, terminal_env: Option<&str>) -> Option<Vec<String>> {
    let with = |pre: &[&str]| -> Vec<String> { pre.iter().map(|s| s.to_string()).chain(argv.iter().cloned()).collect() };
    if have("xdg-terminal-exec") {
        return Some(with(&["xdg-terminal-exec"]));
    }
    if let Some(t) = terminal_env.filter(|t| !t.is_empty() && have(t)) {
        return Some(with(&[t, "-e"]));
    }
    let known: [(&str, &[&str]); 8] = [
        ("kitty", &["kitty"]),
        ("alacritty", &["alacritty", "-e"]),
        ("ghostty", &["ghostty", "-e"]),
        ("foot", &["foot"]),
        ("wezterm", &["wezterm", "start", "--"]),
        ("gnome-terminal", &["gnome-terminal", "--"]),
        ("konsole", &["konsole", "-e"]),
        ("xterm", &["xterm", "-e"]),
    ];
    known.iter().find(|(t, _)| have(t)).map(|(_, pre)| with(pre))
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

/// The command the terminal runs: setup, then a pause so its last lines stay readable.
pub fn setup_command(quiver: &str, firewall: bool) -> Vec<String> {
    let flags = if firewall { " --firewall lan,tailnet" } else { "" };
    let q = quiver_api::system::script::sh_quote(quiver);
    vec!["sh".into(), "-c".into(), format!("{q} system setup{flags}; echo; printf 'Press Enter to close this window. '; read _")]
}

/// A plain-words state for the pill.
fn pill_of(s: State) -> (PillKind, &'static str) {
    match s {
        State::Ok => (PillKind::Ok, "OK"),
        State::Missing => (PillKind::Warn, "Missing"),
        State::Outdated => (PillKind::Warn, "Outdated"),
        State::NotNeeded => (PillKind::Muted, "Not needed here"),
        State::Unknown => (PillKind::Muted, "Can't tell"),
        State::Conflict => (PillKind::Bad, "Needs a look"),
    }
}

/// Demo mode's report: a laptop whose pad rules are in, with the uinput rule and remote
/// play's firewall rules missing.
pub fn demo_report() -> Report {
    use quiver_api::system::firewall::Kind;
    use quiver_api::system::{Item, Piece};
    let piece = |id: &str, title: &str, state: State, summary: &str, unlocks: &str, reason: &str, fix: Option<&str>| Piece {
        id: id.into(),
        title: title.into(),
        state,
        summary: summary.into(),
        unlocks: unlocks.into(),
        reason: reason.into(),
        items: Vec::<Item>::new(),
        fix: fix.map(str::to_owned),
    };
    let mut r = Report {
        user: "demo".into(),
        firewall: Kind::Ufw,
        scopes: vec!["192.168.1.0/24 (wlan0)".into(), "the tailnet (tailscale0)".into()],
        contrib: Some("~/.local/lib/quiver/contrib".into()),
        contrib_problem: None,
        pieces: vec![
            piece("lab-pads", "Lab controllers", State::Ok, "installed (v1 in /etc/udev/rules.d, by hand; group quiver-lab-pad)", "game controllers in lab instances", "Lab controllers need a udev rule and a group so a lab instance's virtual pads are seen only inside its container, never by your desktop, Steam or your games.", None),
            piece("stream-pads", "Stream controllers", State::Ok, "installed (v1 in /etc/udev/rules.d, by hand; group quiver-stream-pad)", "game controllers in streams from this machine", "Stream controllers need a udev rule and a group so games can open them while your desktop can't.", None),
            piece(
                "headless-uinput",
                "Controllers with nobody seated",
                State::Missing,
                "72-quiver-uinput.rules: not installed; group quiver-uinput: doesn't exist",
                "stream controllers while nobody is signed in at this machine",
                "With nobody signed in at the machine there is no seat to give Quiver /dev/uinput, which makes the pads. A udev rule gives it a group only the stream host's helper gets.",
                Some("quiver system setup"),
            ),
            piece("firewall-profiles", "Firewall profiles", State::Ok, "ufw: quiver v1 in /etc/ufw/applications.d", "firewall rules by name", "ufw reads application profiles from root's folders.", None),
            piece(
                "firewall-moonlight",
                "Firewall: Moonlight",
                State::Missing,
                "ufw: the tailnet (tailscale0): TCP 47984, 47989, 48010; UDP 47998–48000 not allowed yet",
                "Moonlight clients reaching this machine",
                "The firewall blocks Moonlight's ports until a rule allows them from your LAN and tailnet.",
                Some("quiver system setup --firewall lan,tailnet"),
            ),
            piece("firewall-remote", "Firewall: remote play", State::Ok, "ufw: allowed by Quiver's rules (tagged quiver)", "your other Quiver machines reaching this one", "The firewall blocks remote play's ports until a rule allows them.", None),
            piece("linger", "Start at boot", State::NotNeeded, "quiverd starts when you sign in (quiver system setup --at-boot starts it at boot)", "quiverd at boot, before anyone signs in", "Lingering starts your user manager at boot.", None),
        ],
    };
    if let Some(p) = r.pieces.iter_mut().find(|p| p.id == "firewall-moonlight") {
        // The demo's Moonlight listens on Tailscale only, so only the tailnet counts.
        p.items = vec![Item { what: "the tailnet (tailscale0)".into(), state: State::Missing, detail: "TCP 47984, 47989, 48010; UDP 47998–48000 not allowed yet".into() }];
    }
    r
}

impl Quiver {
    /// Look at the machine again when the page shows it and the last look is old.
    pub(crate) fn system_wanted(&mut self, cx: &mut Context<Self>) {
        let ui = &self.ui.system;
        if ui.loading || ui.checked.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return;
        }
        self.system_reload(cx);
    }

    pub(crate) fn system_reload(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            self.ui.system.report = Some(demo_report());
            self.ui.system.checked = Some(Instant::now());
            return;
        }
        self.ui.system.loading = true;
        let task = cx.background_executor().spawn(async move { quiver_api::system::check(&quiver_api::system::Facts::gather(), &Options::default()).report });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                let ui = &mut this.ui.system;
                ui.report = Some(r);
                ui.loading = false;
                ui.checked = Some(Instant::now());
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn system_actions(&self) -> Vec<SettingsAction> {
        if self.ui.system.report.is_none() {
            return Vec::new();
        }
        vec![SettingsAction::System(SystemAction::SetUp), SettingsAction::System(SystemAction::Recheck)]
    }

    pub(crate) fn system_action(&mut self, a: SystemAction, cx: &mut Context<Self>) {
        match a {
            SystemAction::Recheck => {
                self.ui.system.checked = None;
                self.system_reload(cx);
                self.show_toast("Checking this machine's system setup", cx);
            }
            SystemAction::SetUp => {
                let firewall = self.ui.system.report.as_ref().is_some_and(|r| r.attention().any(|p| p.id.starts_with("firewall-")));
                let cmd = setup_command(&quiver_exe(), firewall);
                if self.demo {
                    return self.show_toast(format!("Would open a terminal: {}", cmd[2]), cx);
                }
                let terminal = std::env::var("TERMINAL").ok();
                let Some(argv) = terminal_argv(&cmd, &on_path, terminal.as_deref()) else {
                    return self.show_toast("No terminal found (xdg-terminal-exec or $TERMINAL): run quiver system setup in one", cx);
                };
                match std::process::Command::new(&argv[0]).args(&argv[1..]).stdin(std::process::Stdio::null()).spawn() {
                    Ok(mut child) => {
                        self.ui.system.terminal_open = true;
                        self.show_toast("Opened a terminal: it shows the script, then asks before sudo runs it", cx);
                        // Look again once it's closed (polled: no thread sits in wait()).
                        cx.spawn(async move |this, cx| {
                            while matches!(child.try_wait(), Ok(None)) {
                                cx.background_executor().timer(Duration::from_secs(1)).await;
                            }
                            let _ = this.update(cx, |this, cx| {
                                this.ui.system.terminal_open = false;
                                this.system_reload(cx);
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    Err(e) => self.show_toast(format!("Couldn't open a terminal ({}): {e}", argv[0]), cx),
                }
            }
        }
    }

    pub(super) fn settings_system(&mut self, th: &Theme, actions: &[SettingsAction], cx: &mut Context<Self>) -> AnyElement {
        self.system_wanted(cx);
        let ui = &self.ui.system;
        let Some(r) = ui.report.clone() else {
            return card(th).child(card_head(th, "System setup", "the parts of Quiver that need root", None)).child(note_row(th, "Checking this machine…")).into_any_element();
        };
        let missing = r.attention().count();
        let sub = match missing {
            0 => "everything this machine uses is set up".to_owned(),
            1 => "1 piece needs a one-time root step".to_owned(),
            n => format!("{n} pieces need a one-time root step"),
        };
        let set_up = self.action_button(
            th,
            actions,
            SettingsAction::System(SystemAction::SetUp),
            if missing > 0 { Kind::Primary } else { Kind::Secondary },
            Some(Icon::SquareTerminal),
            if ui.terminal_open { "Setting up…" } else { "Set up…" },
            cx,
        );
        let recheck = self.action_button(th, actions, SettingsAction::System(SystemAction::Recheck), Kind::Ghost, Some(Icon::RefreshCw), if ui.loading { "Checking…" } else { "Check again" }, cx);
        let head = card_head(th, "System setup", &sub, Some(div().flex().gap(px(8.)).child(recheck).child(set_up).into_any_element()));
        let rows = r.pieces.iter().map(|p| {
            let (kind, word) = pill_of(p.state);
            let lead = match p.state {
                State::Ok | State::NotNeeded => p.summary.clone(),
                _ => p.reason.clone(),
            };
            div()
                .flex()
                .flex_wrap()
                .items_start()
                .gap(px(12.))
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(th.line1)
                .child(div().w(px(124.)).flex_none().flex().child(pill(th, kind, None, word)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(220.))
                        .gap(px(3.))
                        .child(div().font_weight(SEMIBOLD).text_size(px(13.)).child(p.title.clone()))
                        .when(!p.unlocks.is_empty() && p.state != State::NotNeeded, |d| d.child(div().text_size(px(12.)).text_color(th.tx2).child(format!("For {}.", p.unlocks))))
                        .child(div().text_size(px(12.)).line_height(px(16.)).text_color(th.tx3).child(lead))
                        .when(p.state.attention() || p.state == State::Unknown, |d| d.child(div().text_size(px(11.5)).line_height(px(15.)).text_color(th.tx4).child(p.summary.clone()))),
                )
                .into_any_element()
        });
        let source = r.contrib_problem.clone().map(|p| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .child(icon(Icon::TriangleAlert, 14., th.warn))
                .child(div().text_size(px(12.)).text_color(th.warn).child(p))
        });
        card(th)
            .child(head)
            .children(rows)
            .children(source)
            .child(note_row(
                th,
                "Set up… opens a terminal running quiver system setup: it shows the script, saves it, and asks before sudo runs it. Quiver never asks for your password, and agents never run it.",
            ))
            .child(foot_cli(th, "quiver system setup --check", "the same list in a terminal"))
            .into_any_element()
    }

    /// One line for the Moonlight card when the firewall may block it.
    pub(crate) fn moonlight_firewall_notice(&mut self, th: &Theme, enabled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !enabled {
            return None;
        }
        self.system_wanted(cx);
        let r = self.ui.system.report.as_ref()?;
        let p = r.piece("firewall-moonlight")?;
        let text = match p.state {
            State::Missing => format!("{} may block Moonlight from {}.", r.firewall.name(), p.items.iter().filter(|i| i.state == State::Missing).map(|i| i.what.clone()).collect::<Vec<_>>().join(", ").trim().to_owned()),
            State::Unknown if p.fix.is_some() => format!("{} is on, and Quiver can't tell without root whether it allows Moonlight.", r.firewall.name()),
            _ => return None,
        };
        let text = if text.ends_with(" from .") { format!("{} may block Moonlight.", r.firewall.name()) } else { text };
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .bg(th.warn_soft)
                .child(icon(Icon::TriangleAlert, 14., th.warn))
                .child(div().text_size(px(12.5)).font_weight(MEDIUM).text_color(th.tx1).child(text))
                .child(command_line(th, "quiver system setup --firewall lan,tailnet").min_w(px(200.)).max_w(px(380.)))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals() {
        let cmd = setup_command("/home/p/.local/bin/quiver", true);
        assert_eq!(cmd[2], "/home/p/.local/bin/quiver system setup --firewall lan,tailnet; echo; printf 'Press Enter to close this window. '; read _");
        let only = |names: &'static [&'static str]| move |t: &str| names.contains(&t);
        assert_eq!(terminal_argv(&cmd, &only(&["xdg-terminal-exec", "kitty"]), Some("foot")).unwrap()[0], "xdg-terminal-exec");
        assert_eq!(terminal_argv(&cmd, &only(&["foot", "kitty"]), Some("foot")).unwrap()[..2], ["foot", "-e"]);
        assert_eq!(terminal_argv(&cmd, &only(&["alacritty"]), None).unwrap()[..3], ["alacritty", "-e", "sh"]);
        assert!(terminal_argv(&cmd, &only(&[]), None).is_none());
        assert!(setup_command("quiver", false)[2].starts_with("quiver system setup; echo"));
    }

    #[test]
    fn the_demo_report_has_each_kind() {
        let r = demo_report();
        assert!(r.attention().count() >= 2);
        assert!(r.pieces.iter().any(|p| p.state == State::NotNeeded) && r.pieces.iter().any(|p| p.state == State::Ok));
    }
}
