//! The command palette (Ctrl K / Start): type to filter commands and games. Every entry is
//! one daemon call or a place in the app, and shows the `quiver …` command that does the
//! same from a terminal.

use quiver_ui::depth::DepthExt;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};

use super::downloads::DlCmd;
use super::global::{GlobalCmd, SourceFilter};
use super::{Icon, MEDIUM, PillKind, SEMIBOLD, art_fill, caption, glyph, icon, kbd, mono, pill, soft_shadow};
use crate::app::{Cmd, Command, InputMode, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{Gid, GameRow, clean_name, short_name};
use crate::nav::{Page, Section, SettingsSection, Tab, Target};
use crate::theme::{Theme, rgba};

/// Most entries shown at once (the list doesn't scroll).
const MAX_ENTRIES: usize = 11;
const MAX_GAMES: usize = 5;

/// A palette row: the command, the group heading it sits under, and for games the art.
#[derive(Clone)]
pub struct Entry {
    pub group: String,
    pub cmd: Command,
    pub game: Option<Gid>,
}

fn cmd(icon: Icon, label: impl Into<String>, detail: Option<String>, cli: Option<String>, action: Cmd) -> Command {
    Command {
        icon,
        label: label.into().into(),
        detail: detail.map(Into::into),
        cli: cli.map(Into::into),
        action,
        future: false,
    }
}

/// How well `query` matches `hay` (both lowercase): every word must appear; words that
/// start a word in `hay`, or all of it, count more. `None`: no match.
pub fn score(query: &str, hay: &str) -> Option<u32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        let pos = hay.find(word)?;
        let starts_word = pos == 0 || !hay.as_bytes()[pos - 1].is_ascii_alphanumeric();
        total += if pos == 0 { 6 } else if starts_word { 4 } else { 1 };
        if word.len() >= 3 {
            total += 1;
        }
    }
    Some(total)
}

impl Quiver {
    /// What the palette lists right now, in display order (`palette_commands` is the same
    /// list without the grouping, for the layout and for running entry `i`).
    pub fn palette_entries(&self) -> Vec<Entry> {
        let query = self.ui.palette_query.trim().to_lowercase();
        if query.is_empty() { self.palette_default() } else { self.palette_search(&query) }
    }

    pub fn global_palette_commands(&self) -> Vec<Command> {
        self.palette_entries().into_iter().map(|e| e.cmd).collect()
    }

    /// The game's main action as a command: Stop, Install, Update or Play.
    fn game_primary(&self, g: &GameRow) -> Command {
        let id = g.id();
        let name = short_name(&g.entry.name);
        if g.running() {
            cmd(Icon::Square, format!("Stop {name}"), None, Some(format!("quiver stop {id}")), Cmd::Primary(id))
        } else if g.steam_only() {
            cmd(Icon::Play, format!("Play {name}"), Some("Starts from Steam".into()), Some(format!("quiver play {id}")), Cmd::Primary(id))
        } else if !g.installed() {
            cmd(Icon::Download, format!("Install {name}"), None, Some(format!("quiver install {id}")), Cmd::Primary(id))
        } else if g.entry.update_available {
            cmd(Icon::ArrowDownToLine, format!("Update {name}"), None, Some(format!("quiver install {id}")), Cmd::Primary(id))
        } else {
            cmd(Icon::Play, format!("Play {name}"), None, Some(format!("quiver play {id}")), Cmd::Primary(id))
        }
    }

    fn game_open(&self, g: &GameRow) -> Command {
        let detail = g.tile_meta();
        cmd(Icon::Info, format!("Go to {}", clean_name(&g.entry.name)), Some(detail), Some(format!("quiver info {}", g.id())), Cmd::Open(g.id()))
    }

    /// Without a query: the focused game's actions, what's running, and where to go.
    fn palette_default(&self) -> Vec<Entry> {
        let mut out = vec![];
        let focused = self.nav.focused_game(&self.layout).and_then(|id| self.game(id));
        if let Some(g) = focused {
            let group = clean_name(&g.entry.name);
            let mut push = |c: Command| out.push(Entry { group: group.clone(), cmd: c, game: Some(g.id()) });
            push(self.game_primary(g));
            if !matches!(self.nav.page, Page::Game(_)) {
                push(self.game_open(g));
            }
            if g.installed() {
                push(cmd(
                    Icon::SlidersHorizontal,
                    "Launch options and history",
                    None,
                    Some(format!("quiver history {}", g.id())),
                    Cmd::Tab(g.id(), Tab::LaunchOptions),
                ));
            }
        }
        for s in &self.ui.sessions {
            if focused.is_some_and(|g| g.id() == s.product_id) {
                continue;
            }
            let name = self.game_name(Gid::from(s.product_id));
            out.push(Entry {
                group: "Now playing".into(),
                cmd: cmd(
                    Icon::Gamepad,
                    format!("Go to {name}"),
                    Some(if s.reported { "Playing now · reported to Ubisoft".into() } else { "Playing now".into() }),
                    Some("quiver sessions".into()),
                    Cmd::Open(Gid::from(s.product_id)),
                ),
                game: Some(Gid::from(s.product_id)),
            });
        }
        let go = |icon: Icon, label: &str, cli: Option<&str>, action: Cmd| Entry {
            group: "Go to".into(),
            cmd: cmd(icon, label, None, cli.map(str::to_owned), action),
            game: None,
        };
        out.push(go(Icon::LibraryBig, "Library", Some("quiver library"), Cmd::Go(Section::Library)));
        out.push(go(Icon::Download, "Downloads", Some("quiver status"), Cmd::Go(Section::Downloads)));
        out.push(go(Icon::Activity, "Activity", Some("quiver activity"), Cmd::Go(Section::Activity)));
        out.push(go(Icon::Settings, "Settings", None, Cmd::Settings(self.nav.settings)));
        out.push(Entry {
            group: "Everywhere".into(),
            cmd: cmd(
                Icon::RefreshCw,
                "Refresh library",
                Some("Ask Ubisoft for ownership and updates again".into()),
                Some("quiver library --refresh".into()),
                Cmd::Refresh,
            ),
            game: None,
        });
        out.push(Entry {
            group: "Everywhere".into(),
            cmd: cmd(Icon::Users, if self.ubisoft_signed_in() { "Add a Ubisoft account…" } else { "Sign in to Ubisoft…" }, Some("Its games join this library".into()), Some("quiver login".into()), Cmd::Global(GlobalCmd::AddAccount)),
            game: None,
        });
        out.truncate(MAX_ENTRIES);
        out
    }

    /// Every command the palette knows, with the words it can be found by.
    fn palette_catalog(&self) -> Vec<(Command, String)> {
        let mut v: Vec<(Command, String)> = vec![];
        let mut add = |c: Command, words: &str| {
            let hay = format!("{} {} {}", c.label, c.detail.clone().unwrap_or_default(), words).to_lowercase();
            v.push((c, hay));
        };
        add(cmd(Icon::LibraryBig, "Go to Library", None, Some("quiver library".into()), Cmd::Go(Section::Library)), "home games grid");
        add(cmd(Icon::Download, "Go to Downloads", None, Some("quiver status".into()), Cmd::Go(Section::Downloads)), "installs queue jobs updates");
        add(cmd(Icon::Activity, "Go to Activity", None, Some("quiver activity".into()), Cmd::Go(Section::Activity)), "log history audit who changed");
        for (f, words) in [
            (SourceFilter::Agent, "agents tokens claude bots automation"),
            (SourceFilter::Cli, "terminal command line"),
            (SourceFilter::App, "gui window"),
            (SourceFilter::System, "daemon automatic"),
        ] {
            add(
                cmd(
                    Icon::Activity,
                    format!("Activity from {}", if f == SourceFilter::Cli { "the CLI".into() } else { f.label().to_lowercase() }),
                    None,
                    Some("quiver activity".into()),
                    Cmd::Global(GlobalCmd::ActivityFrom(f)),
                ),
                words,
            );
        }
        for s in SettingsSection::ALL {
            let (i, cli, words) = match s {
                SettingsSection::Accounts => (Icon::Users, Some("quiver account list"), "ubisoft sign in out login logout default owners who owns"),
                SettingsSection::Library => (Icon::HardDrive, Some("quiver library"), "storage folders paths disk compatibility prefixes proton cache"),
                SettingsSection::Compatibility => (Icon::Cpu, Some("quiver protons"), "proton wine ge runtime builds"),
                SettingsSection::Performance => (Icon::Activity, Some("quiver hud status"), "hud fps overlay mangohud frame rate latency performance"),
                SettingsSection::Tweaks => (Icon::SlidersHorizontal, Some("quiver tweaks"), "tweaks dlss frame rate cap vsync latency reflex anisotropic frame generation nvidia override"),
                SettingsSection::Steam => (Icon::Gamepad, Some("quiver export list"), "shortcuts desktop entries exports restart steam user"),
                SettingsSection::Appearance => (Icon::Palette, None, "theme omarchy colors controller gamepad hyprpad hints"),
                SettingsSection::Display => (Icon::Monitor, None, "3d stereo depth xreal glasses headset side by side sbs"),
                SettingsSection::Automation => (Icon::Bot, Some("quiver status"), "cli agents permissions policy socket daemon quiverd"),
                SettingsSection::About => (Icon::Info, Some("quiver status"), "version license credits ubisoft links"),
            };
            add(cmd(i, format!("Settings › {}", s.label()), None, cli.map(str::to_owned), Cmd::Settings(s)), &format!("preferences options {words}"));
        }
        add(
            cmd(Icon::RefreshCw, "Refresh library", Some("Ask Ubisoft for ownership and updates again".into()), Some("quiver library --refresh".into()), Cmd::Refresh),
            "reload sync",
        );
        add(
            cmd(Icon::Users, if self.ubisoft_signed_in() { "Add a Ubisoft account…" } else { "Sign in to Ubisoft…" }, Some("Its games join this library".into()), Some("quiver login".into()), Cmd::Global(GlobalCmd::AddAccount)),
            "sign in login another second family",
        );
        if let Some(s) = &self.status {
            for (i, a) in s.accounts.iter().enumerate() {
                if !a.default {
                    add(
                        cmd(Icon::Star, format!("Make {} the default account", a.name), Some("Launches games every account owns".into()), Some(format!("quiver account default {}", a.name)), Cmd::Global(GlobalCmd::MakeDefault(i))),
                        "account switch",
                    );
                }
                add(
                    cmd(Icon::LogOut, format!("Sign out {}", a.name), Some(if a.default { "The default account".into() } else { "Asks first".into() }), Some(format!("quiver logout {}", a.name)), Cmd::Global(GlobalCmd::SignOut(i))),
                    "account logout remove",
                );
            }
        }
        add(cmd(Icon::Cpu, "Rescan Proton builds", None, Some("quiver protons".into()), Cmd::Global(GlobalCmd::RescanProtons)), "proton runtime compatibility");
        if let Some(Ok(steam)) = &self.ui.steam {
            if !steam.staged.is_empty() && steam.running {
                add(
                    cmd(Icon::RefreshCw, "Restart Steam to apply staged changes…", Some("Asks first; Quiver never restarts Steam on its own".into()), Some("quiver steam restart".into()), Cmd::Global(GlobalCmd::SteamRestart)),
                    "steam shortcuts restart staged launch options",
                );
            }
            if !steam.staged.is_empty() && !steam.running {
                add(
                    cmd(Icon::Gamepad, "Write staged Steam changes", Some(format!("{} waiting; Steam is closed", steam.staged.len())), Some("quiver steam apply".into()), Cmd::Global(GlobalCmd::SteamApply)),
                    "steam shortcuts apply",
                );
            }
        }
        // Ubisoft games installed through Steam: start them through Quiver, or undo that.
        for g in self.steam_copies() {
            let name = crate::model::clean_name(&g.name);
            match super::steamgames::control(&g) {
                Some(true) => add(
                    cmd(Icon::Gamepad, format!("Start {name} from Steam through Quiver"), Some("Instead of Ubisoft Connect; undo any time".into()), Some(format!("quiver steam patch {}", g.app_id)), Cmd::Global(GlobalCmd::SteamPatch(g.app_id))),
                    "steam connect patch launch options ubisoft",
                ),
                Some(false) => add(
                    cmd(Icon::Undo, format!("Start {name} from Steam through Ubisoft Connect again"), None, Some(format!("quiver steam unpatch {}", g.app_id)), Cmd::Global(GlobalCmd::SteamUnpatch(g.app_id))),
                    "steam connect unpatch undo launch options ubisoft",
                ),
                None => {}
            }
        }
        for s in &self.ui.sessions {
            let name = self.game_name(Gid::from(s.product_id));
            add(cmd(Icon::Gamepad, "Now playing", Some(name.clone()), Some("quiver sessions".into()), Cmd::Global(GlobalCmd::NowPlaying)), "running session play time");
            if s.holders.iter().any(|h| h == "launcher") {
                add(cmd(Icon::Square, format!("Stop {name}"), None, Some(format!("quiver stop {}", s.product_id)), Cmd::Global(GlobalCmd::Stop(Gid::from(s.product_id)))), "quit kill end running");
            }
        }
        // Per game: found by the game's name plus what they do ("origins proton").
        for g in &self.games {
            let id = g.id();
            let name = short_name(&g.entry.name);
            let words = format!("{} {name} {id}", clean_name(&g.entry.name));
            if g.installed() {
                add(
                    cmd(Icon::SlidersHorizontal, format!("{name}: launch options and history"), Some("Proton, wrappers, environment, revisions".into()), Some(format!("quiver history {id}")), Cmd::Tab(id, Tab::LaunchOptions)),
                    &format!("{words} proton settings config wrappers env dll args revisions restore"),
                );
                add(
                    cmd(Icon::Gamepad, format!("Add {name} to Steam"), Some("A non-Steam shortcut with its art; staged while Steam runs".into()), Some(format!("quiver export steam {id}")), Cmd::Global(GlobalCmd::ExportSteam(id))),
                    &format!("{words} steam shortcut export"),
                );
                add(
                    cmd(Icon::Monitor, format!("Add {name} to the app launcher"), Some(".desktop entry".into()), Some(format!("quiver export desktop {id}")), Cmd::Global(GlobalCmd::ExportDesktop(id))),
                    &format!("{words} desktop entry launcher shortcut export"),
                );
                add(
                    cmd(
                        Icon::Trash,
                        format!("Uninstall {name}…"),
                        Some("Shows the folder and size first; the compatibility folder only if you ask".into()),
                        Some(format!("quiver uninstall {id}")),
                        Cmd::Downloads(DlCmd::Uninstall(id)),
                    ),
                    &format!("{words} uninstall remove delete free space"),
                );
            }
            add(
                cmd(Icon::Activity, format!("Activity for {name}"), None, Some("quiver activity".into()), Cmd::Global(GlobalCmd::ActivityGame(id))),
                &format!("{words} activity log history who changed"),
            );
        }
        v
    }

    fn palette_search(&self, query: &str) -> Vec<Entry> {
        // Games: by full name, short name and product id.
        let mut games: Vec<(u32, &GameRow)> = self
            .games
            .iter()
            .filter_map(|g| {
                let hay = format!("{} {} {}", clean_name(&g.entry.name), short_name(&g.entry.name), g.id()).to_lowercase();
                score(query, &hay).map(|s| (s + if g.installed() { 1 } else { 0 }, g))
            })
            .collect();
        games.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.entry.name.cmp(&b.1.entry.name)));
        // Per-game commands only for the games the query names best, by any of its words
        // ("origins proton": AC Origins, then its launch options).
        let mut named: Vec<(usize, &GameRow)> = self
            .games
            .iter()
            .filter_map(|g| {
                let hay = format!("{} {} {}", clean_name(&g.entry.name), short_name(&g.entry.name), g.id()).to_lowercase();
                let n = query.split_whitespace().filter(|w| w.len() >= 2 && hay.contains(w)).count();
                (n > 0).then_some((n, g))
            })
            .collect();
        named.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.installed().cmp(&a.1.installed())));
        let top: Vec<Gid> = named.iter().take(2).map(|(_, g)| g.id()).collect();
        let per_game = |c: &Command| match c.action {
            Cmd::Tab(id, _) => Some(id),
            Cmd::Global(GlobalCmd::ExportSteam(id) | GlobalCmd::ExportDesktop(id) | GlobalCmd::ActivityGame(id)) => Some(id),
            Cmd::Downloads(DlCmd::Uninstall(id)) => Some(id),
            _ => None,
        };
        let mut commands: Vec<(u32, Command)> = self
            .palette_catalog()
            .into_iter()
            .filter(|(c, _)| per_game(c).is_none_or(|id| top.contains(&id)))
            .filter_map(|(c, hay)| score(query, &hay).map(|s| (s, c)))
            .collect();
        commands.sort_by(|a, b| b.0.cmp(&a.0));

        let mut game_entries = vec![];
        for (i, (_, g)) in games.iter().take(MAX_GAMES).enumerate() {
            // The best match gets its main action too ("Play AC Origins").
            if i == 0 {
                game_entries.push(Entry { group: "Games".into(), cmd: self.game_primary(g), game: Some(g.id()) });
            }
            game_entries.push(Entry { group: "Games".into(), cmd: self.game_open(g), game: Some(g.id()) });
        }
        let command_entries: Vec<Entry> =
            commands.into_iter().map(|(_, c)| Entry { group: "Commands".into(), cmd: c, game: None }).collect();
        let games_first = match (games.first(), self.palette_catalog_best(query)) {
            (Some((gs, _)), Some(cs)) => *gs >= cs,
            (Some(_), None) => true,
            _ => false,
        };
        let mut out = if games_first {
            let n = game_entries.len().min(MAX_ENTRIES.saturating_sub(command_entries.len().min(4)));
            let mut v: Vec<Entry> = game_entries.into_iter().take(n).collect();
            v.extend(command_entries);
            v
        } else {
            let n = command_entries.len().min(MAX_ENTRIES.saturating_sub(game_entries.len().min(4)));
            let mut v: Vec<Entry> = command_entries.into_iter().take(n).collect();
            v.extend(game_entries);
            v
        };
        out.truncate(MAX_ENTRIES);
        out
    }

    fn palette_catalog_best(&self, query: &str) -> Option<u32> {
        self.palette_catalog().into_iter().filter_map(|(_, hay)| score(query, &hay)).max()
    }

    /// The palette overlay.
    pub fn render_palette(&mut self, th: &Theme, selected: usize, cx: &mut Context<Self>) -> AnyElement {
        let pad = self.mode == InputMode::Pad;
        let entries = self.palette_entries();
        let width = self.dialog_width(720.);
        let left = ((self.viewport.0 - width) / 2.).max(16.);
        let context = self.nav.focused_game(&self.layout).and_then(|id| self.game(id)).map(|g| short_name(&g.entry.name));
        let query_empty = self.ui.palette_query.trim().is_empty();

        let mut list: Vec<AnyElement> = vec![];
        let mut last_group: Option<String> = None;
        for (i, e) in entries.iter().enumerate() {
            if last_group.as_deref() != Some(e.group.as_str()) {
                list.push(
                    div()
                        .px(px(20.))
                        .pt(px(if i == 0 { 6. } else { 10. }))
                        .pb(px(6.))
                        .child(caption(&e.group, th.tx3))
                        .into_any_element(),
                );
                last_group = Some(e.group.clone());
            }
            list.push(self.render_palette_row(th, e, i, i == selected, cx));
        }
        if entries.is_empty() {
            list.push(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.))
                    .py(px(28.))
                    .child(icon(Icon::SearchX, 20., th.tx4))
                    .child(div().text_color(th.tx2).child(format!("Nothing matches “{}”", self.ui.palette_query.trim())))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child("Try a game's name, its product id, or words like proton, account, steam"))
                    .into_any_element(),
            );
        }

        let field: AnyElement = match &self.ui.palette_input {
            Some(input) => div().flex_1().min_w_0().child(input.clone()).into_any_element(),
            None => div().flex_1().text_size(px(15.)).text_color(th.tx3).child("Type a command or a game").into_any_element(),
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .py(px(13.))
            .border_b_1()
            .border_color(th.line1)
            .child(icon(Icon::Search, 16., th.tx3))
            .child(field)
            .when_some(context.filter(|_| query_empty), |d, c| d.child(pill(th, PillKind::Muted, None, c)))
            .when(!query_empty, |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("{} result{}", entries.len(), if entries.len() == 1 { "" } else { "s" }))));

        let hint = |k: AnyElement, label: &str| div().flex().items_center().gap(px(6.)).child(k).child(label.to_owned());
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(16.))
            .gap_y(px(6.))
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(th.line1)
            .bg(th.ink1)
            .rounded_b(px(14.))
            .text_size(px(11.5))
            .text_color(th.tx3)
            .when(!pad, |d| {
                d.child(hint(kbd(th, "↑↓").into_any_element(), "Move"))
                    .child(hint(kbd(th, "↵").into_any_element(), "Run"))
                    .child(hint(kbd(th, "Ctrl C").into_any_element(), "Copy as quiver command"))
                    .child(hint(kbd(th, "Esc").into_any_element(), "Close"))
            })
            .when(pad, |d| {
                d.child(hint(glyph(th, "A", false).into_any_element(), "Run"))
                    .child(hint(glyph(th, "B", false).into_any_element(), "Close"))
                    .child(hint(glyph(th, "≡", false).into_any_element(), "Close"))
            })
            .child(div().flex_1())
            .child("Every entry is a daemon call");

        div()
            .id("scrim")
            .absolute()
            .inset_0()
            .bg(th.scrim)
            .on_click(cx.listener(|this, _, _, cx| {
                this.nav.close_overlay();
                cx.notify();
            }))
            .child(
                div()
                    .id("palette")
                    .depth_overlay()
                    .absolute()
                    .top(px(92.))
                    .left(px(left))
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .rounded(px(14.))
                    .bg(th.ink2)
                    .border_1()
                    .border_color(th.line2)
                    .shadow(vec![soft_shadow(rgba(0x000000, 0.7), 30., 90.)])
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(header)
                    .child(div().flex().flex_col().py(px(8.)).children(list))
                    .child(footer),
            )
            .into_any_element()
    }

    fn render_palette_row(&mut self, th: &Theme, e: &Entry, i: usize, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = &e.cmd;
        let fg = if on { th.acc_hi } else { th.tx3 };
        let thumb: Option<AnyElement> = e.game.filter(|_| matches!(c.action, Cmd::Open(_))).and_then(|id| {
            let g = self.game(id)?.clone();
            let art = self.art_for(&g, ArtSlot::Tile, Variant::Tile, cx);
            Some(div().w(px(18.)).h(px(21.)).flex_none().child(art_fill(th, &art, "", 3., false)).into_any_element())
        });
        div()
            .id(("pal", i))
            .flex()
            .items_center()
            .gap(px(12.))
            .mx(px(6.))
            .px(px(14.))
            .py(px(if c.detail.is_some() { 7. } else { 9. }))
            .rounded(px(8.))
            .when(on, |d| d.bg(th.acc_soft))
            .when(!on, |d| d.hover(|s| s.bg(th.ink3)))
            // Stereo: the selected entry is the palette's focus.
            .stereo_focused(on)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.click(Target::PaletteItem(i), cx)))
            .child(match thumb {
                Some(t) => t,
                None => div().w(px(18.)).flex().justify_center().child(icon(c.icon, 14., fg)).into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).truncate().child(c.label.clone()))
                    .when_some(c.detail.clone(), |d, s| d.child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(s))),
            )
            .when(c.future, |d| d.child(pill(th, PillKind::Future, None, "Not yet")))
            .when_some(c.cli.clone(), |d, cli| {
                d.child(mono(11., if on { th.who_cli } else { th.tx4 }).max_w(px(280.)).truncate().child(cli))
            })
            .when(on, |d| d.child(kbd(th, "↵")))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn scoring() {
        assert!(score("orig", "assassin's creed origins ac origins 3539").is_some());
        assert!(score("ac orig", "assassin's creed origins ac origins 3539").is_some());
        assert!(score("odyssey", "assassin's creed origins").is_none());
        assert!(score("3539", "assassin's creed origins ac origins 3539").is_some());
        // A word start beats the middle of a word.
        assert!(score("pro", "settings › compatibility proton") > score("pro", "reproduce"));
        // All words must match.
        assert!(score("sign family", "sign out family").is_some());
        assert!(score("sign bob", "sign out family").is_none());
    }
}
