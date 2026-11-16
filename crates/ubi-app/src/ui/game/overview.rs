//! Overview: play time (Ubisoft's total first, local sessions as the detail), sessions,
//! a progression summary, and the install's facts, locations and links.

use chrono::{Datelike, Duration as Span, Local};
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px, relative};
use ubi_api::PlaySession;
use ubi_api::exports::{ExportKind, ExportState};

use super::page::{kv, load_note, note};
use crate::app::Quiver;
use crate::model::{GameRow, clock, day, duration, gb, hours};
use crate::nav::{Tab, Target};
use crate::theme::Theme;
use crate::ui::{BOLD, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, button, caption, card, card_header, chip, icon, icon_button, mono, pill, progress};

/// Hours played per week for the last `weeks` weeks (oldest first), from local sessions.
pub fn weekly_hours(sessions: &[PlaySession], weeks: usize, now: chrono::DateTime<Local>) -> Vec<f32> {
    let this_monday = now.date_naive() - Span::days(now.weekday().num_days_from_monday() as i64);
    let mut out = vec![0f32; weeks];
    for s in sessions {
        let d = s.started.with_timezone(&Local).date_naive();
        let back = (this_monday - d).num_days();
        let w = if back <= 0 { 0 } else { (back + 6) / 7 } as usize;
        if w < weeks {
            out[weeks - 1 - w] += s.seconds as f32 / 3600.;
        }
    }
    out
}

/// Weekly bars, the current week in the accent.
pub fn weekly_bars(th: &Theme, sessions: &[PlaySession], height: f32) -> gpui::Div {
    const WEEKS: usize = 12;
    let now = Local::now();
    let vals = weekly_hours(sessions, WEEKS, now);
    let max = vals.iter().cloned().fold(1f32, f32::max).ceil();
    let first = now - Span::weeks(WEEKS as i64 - 1);
    if vals.iter().all(|v| *v == 0.) {
        return div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(height + 18.))
            .rounded(px(8.))
            .border_1()
            .border_dashed()
            .border_color(th.line2)
            .text_size(px(12.))
            .text_color(th.tx3)
            .child(format!("No sessions on this PC since {}", first.format("%b %-d")));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .justify_between()
                        .h(px(height))
                        .text_size(px(10.))
                        .text_color(th.tx3)
                        .child(format!("{max:.0} h"))
                        .child("0 h"),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(height))
                        .flex()
                        .items_end()
                        .gap(px(5.))
                        .border_b_1()
                        .border_color(th.line1)
                        .children(vals.iter().enumerate().map(|(i, v)| {
                            let last = i + 1 == vals.len();
                            let frac = (v / max).clamp(0., 1.);
                            div()
                                .flex_1()
                                .h(if *v > 0. { relative(frac.max(0.03)) } else { relative(0.015) })
                                .rounded_t(px(2.))
                                .bg(if last { th.acc } else if *v > 0. { th.acc.opacity(0.4) } else { th.ink4 })
                        })),
                ),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .pl(px(30.))
                .text_size(px(10.))
                .text_color(th.tx3)
                .child(first.format("%b %-d").to_string())
                .child(div().text_color(th.tx2).child("This week")),
        )
}

impl Quiver {
    pub(super) fn render_overview(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let left = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(20.))
            .child(self.render_playtime_card(th, g, cx))
            .child(self.render_sessions(th, g))
            .child(self.render_progress_summary(th, g, cx));
        let right = div()
            .flex()
            .flex_col()
            .w(px(360.))
            .flex_none()
            .gap(px(20.))
            .child(self.render_install_card(th, g, cx))
            .when(g.installed(), |d| d.child(self.render_locations(th, g, cx)))
            .when_some(self.render_links(th, g, cx), |d, l| d.child(l))
            .child({
                let cmd = match (g.installed(), self.gp.play_profile(id)) {
                    (false, _) if g.steam_only() => format!("quiver play {id}"),
                    (false, _) => format!("quiver install {id}"),
                    (true, None) => format!("quiver play {id}"),
                    (true, Some(p)) => format!("quiver play {id} --profile-id {p}"),
                };
                self.cli_line("ov-cli", th, cmd, cx)
            });
        div().flex().gap(px(20.)).child(left).child(right).into_any_element()
    }

    fn render_playtime_card(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let pt = g.playtime.clone().unwrap_or_default();
        let ubi = self.ubisoft_playtime(id);
        let pending = self.gp.id == id && matches!(self.gp.progress, super::page::Load::Loading);
        let headline = match ubi {
            Some((s, _)) => hours(s),
            None if pt.total_seconds > 0 => hours(pt.total_seconds),
            None => "—".into(),
        };
        let source = match ubi {
            Some((_, src)) => format!("Ubisoft total, from {src}"),
            None if pending => "Asking Ubisoft for its total…".into(),
            None => "Local total; Ubisoft has no record for this game".into(),
        };
        let local = if pt.sessions.is_empty() {
            "No local sessions yet".to_string()
        } else {
            format!("Local: {} in {} session{}", duration(pt.total_seconds), pt.sessions.len(), if pt.sessions.len() == 1 { "" } else { "s" })
        };
        let week: u64 = {
            let now = Local::now();
            let monday = now.date_naive() - Span::days(now.weekday().num_days_from_monday() as i64);
            pt.sessions.iter().filter(|s| s.started.with_timezone(&Local).date_naive() >= monday).map(|s| s.seconds).sum()
        };
        let _ = cx;
        card(th)
            .p(px(16.))
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(caption("Play time", th.tx3))
                    .child(div().flex_1())
                    .child(match ubi {
                        Some(_) => pill(th, PillKind::Ok, Some(Icon::Cloud), "Ubisoft"),
                        None => pill(th, PillKind::Muted, Some(Icon::HardDrive), "Local"),
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(14.))
                    .child(div().text_size(px(30.)).line_height(px(32.)).font_weight(BOLD).child(headline))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .pb(px(3.))
                            .child(div().text_size(px(12.)).text_color(th.tx2).child(source))
                            .child(div().text_size(px(12.)).text_color(th.tx3).child(local)),
                    ),
            )
            .child(super::overview::weekly_bars(th, &pt.sessions, 64.))
            .child(div().text_size(px(11.5)).text_color(th.tx3).child(format!(
                "Weekly detail from sessions on this PC{}",
                if week > 0 { format!(" · {} this week", duration(week)) } else { String::new() }
            )))
    }

    fn profile_label(&self, id: u32, p: Option<&str>) -> String {
        let Some(p) = p else { return "Vanilla".into() };
        if self.gp.id == id
            && let Some(m) = self.gp.mods.ready()
            && let Some(x) = m.profiles.iter().chain(m.deleted.iter()).find(|x| x.profile.id == p)
        {
            return x.profile.name.clone();
        }
        p.to_owned()
    }

    fn render_sessions(&self, th: &Theme, g: &GameRow) -> impl IntoElement + use<> {
        let id = g.id();
        let pt = g.playtime.clone().unwrap_or_default();
        let since = pt.sessions.iter().map(|s| s.started).min();
        let rows: Vec<AnyElement> = pt
            .sessions
            .iter()
            .take(8)
            .enumerate()
            .map(|(i, s)| {
                let focused = self.focused(Target::Body(i));
                let prof = self.profile_label(id, s.profile.as_deref());
                let modded = s.profile.is_some();
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(focused, |d| d.bg(th.acc_soft))
                    .child(
                        div()
                            .w(px(130.))
                            .flex()
                            .flex_col()
                            .child(div().font_weight(MEDIUM).child(day(s.started)))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("{} – {}", clock(s.started), clock(s.ended)))),
                    )
                    .child(div().w(px(84.)).font_weight(SEMIBOLD).child(duration(s.seconds)))
                    .child(chip(th, Some(Icon::Layers), prof, modded).h(px(22.)))
                    .child(div().flex_1())
                    .child(
                        mono(11.5, th.tx3).child(match (s.profile_revision, s.settings_revision) {
                            (Some(pr), r) => format!("launch options r{} · profile r{pr}", r.max(1)),
                            (None, r) => format!("launch options r{}", r.max(1)),
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        let more = pt.sessions.len().saturating_sub(8);
        card(th)
            .child(
                card_header(th, "Sessions")
                    .child(div().text_color(th.tx3).text_size(px(12.)).child(match since {
                        Some(t) => format!("Tracked on this PC since {}", day(t)),
                        None => "Tracked on this PC".into(),
                    }))
                    .child(div().flex_1()),
            )
            .when(rows.is_empty(), |d| d.child(note(th, "Play this game and its sessions show up here, with the launch options and mod profile each one used.")))
            .children(rows)
            .when(more > 0, |d| d.child(div().px(px(16.)).py(px(8.)).border_t_1().border_color(th.line1).text_size(px(12.)).text_color(th.tx3).child(format!("{more} older sessions"))))
    }

    fn render_progress_summary(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let header = card_header(th, "Progression").child(div().flex_1()).child(
            button("ov-to-saves", th, Kind::Ghost, Size::Small, None, "Saves & progress")
                .child(icon(Icon::ChevronRight, 14., th.tx2))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.nav.select_tab(Tab::Saves);
                    cx.notify();
                })),
        );
        let mut c = card(th).child(header);
        if self.gp.id != id {
            return c.child(note(th, "Loading…"));
        }
        let p = match self.gp.progress.ready() {
            Some(p) => p.clone(),
            None => return c.children(load_note(th, &self.gp.progress, "progression")),
        };
        let mut body = div().flex().flex_col().gap(px(14.)).p(px(16.));
        // Ubisoft Connect level and units (account-wide).
        if let Some(cp) = &p.connect {
            let frac = match (cp.xp, cp.level_start_xp, cp.next_level_xp) {
                (Some(x), Some(a), Some(b)) if b > a => ((x.saturating_sub(a)) as f32 / (b - a) as f32).clamp(0., 1.),
                _ => 0.,
            };
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(40.))
                            .rounded(px(10.))
                            .bg(th.acc_soft)
                            .text_color(th.acc_hi)
                            .font_weight(BOLD)
                            .child(cp.level.map(|l| l.to_string()).unwrap_or_else(|| "—".into())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(5.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(div().font_weight(SEMIBOLD).child("Ubisoft Connect level"))
                                    .child(div().text_color(th.tx3).text_size(px(12.)).child(match (cp.xp, cp.next_level_xp) {
                                        (Some(x), Some(n)) => format!("{x} / {n} XP"),
                                        _ => String::new(),
                                    })),
                            )
                            .child(progress(th, frac, None, 5., None)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(icon(Icon::Coins, 14., th.warn))
                            .child(div().font_weight(SEMIBOLD).child(cp.units.map(|u| u.to_string()).unwrap_or_else(|| "—".into())))
                            .child(div().text_color(th.tx3).text_size(px(12.)).child("units")),
                    ),
            );
        }
        if let Some(a) = &p.achievements {
            let icons: Vec<AnyElement> = a.items.iter().take(10).map(|x| super::saves::ach_icon(th, x, 36.)).collect();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(150.))
                            .child(div().flex().items_baseline().gap(px(5.)).child(div().text_size(px(20.)).font_weight(BOLD).child(a.total.to_string())).child(div().text_color(th.tx3).child("achievements")))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(if a.unlocked_known { format!("{} unlocked", a.unlocked) } else { "Unlock status not available yet".into() })),
                    )
                    .children(icons)
                    .when(a.items.len() > 10, |d| d.child(div().text_size(px(12.)).text_color(th.tx3).ml(px(4.)).child(format!("+ {}", a.items.len() - 10)))),
            );
        }
        if !p.stats.is_empty() {
            body = body.child(div().flex().gap(px(10.)).children(p.stats.iter().filter(|s| s.id != "TimePlayed").take(4).map(|s| {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(th.ink1)
                    .border_1()
                    .border_color(th.line1)
                    .child(caption(&stat_name(s), th.tx3).truncate())
                    .child(div().text_size(px(16.)).font_weight(SEMIBOLD).child(s.value.clone().unwrap_or_else(|| "—".into())))
            })));
        }
        if p.connect.is_none() && p.achievements.is_none() && p.stats.is_empty() {
            body = body.child(div().text_color(th.tx3).child("Ubisoft has no stats or achievements for this game."));
        }
        c = c.child(body);
        c
    }

    fn render_install_card(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let info = self.gp.info.ready().filter(|_| self.gp.id == id).cloned();
        let verify = button("verify", th, Kind::Ghost, Size::Small, Some(Icon::ShieldCheck), "Verify…")
            .on_click(cx.listener(move |this, _, _, cx| this.run_dl(crate::ui::downloads::DlCmd::Verify(id), cx)));
        let mut c = card(th).child(card_header(th, "This install").child(div().flex_1()).when(g.installed(), |d| d.child(verify)));
        let txt = |s: String| div().child(s).into_any_element();
        let muted = |s: String| div().text_color(th.tx3).child(s).into_any_element();
        let mut kvs = div().flex().flex_col().gap(px(8.)).p(px(16.));
        if let Some(game) = &g.entry.installed {
            let inst = info.as_ref().and_then(|i| i.install.clone());
            kvs = kvs.child(kv(
                th,
                "Version",
                div()
                    .flex()
                    .gap(px(6.))
                    .child(inst.as_ref().and_then(|i| i.version.clone()).unwrap_or_else(|| "—".into()))
                    .when_some(inst.as_ref().and_then(|i| i.manifest.clone()), |d, m| {
                        let short = if m.len() > 10 { format!("{}…{}", &m[..4], &m[m.len() - 4..]) } else { m };
                        d.child(mono(11.5, th.tx3).child(format!("manifest {short}")))
                    }),
            ));
            if let Some(i) = &inst
                && i.size_bytes > 0
            {
                kvs = kvs.child(kv(th, "Size", txt(gb(i.size_bytes))));
            }
            kvs = kvs.child(kv(th, "Executable", mono(12., th.tx1).truncate().child(game.exe.clone())));
            let langs = inst.as_ref().map(|i| i.languages.len()).unwrap_or(0);
            kvs = kvs.child(kv(
                th,
                "Language",
                txt(format!(
                    "{}{}",
                    game.language.clone().or_else(|| inst.as_ref().and_then(|i| i.language.clone())).unwrap_or_else(|| "Default".into()),
                    if langs > 1 { format!(" · {langs} available") } else { String::new() }
                )),
            ));
        } else if let Some(s) = g.steam().filter(|_| g.steam_only()) {
            let dir = s.install_dir.to_string_lossy().into_owned();
            let short = self.short_path(&s.install_dir);
            kvs = kvs
                .child(kv(th, "Installed", txt(format!("Through Steam (app {})", s.app_id))))
                .child(kv(
                    th,
                    "Folder",
                    div()
                        .id("ov-steam-dir")
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .min_w_0()
                        .cursor_pointer()
                        .child(mono(12., th.tx1).truncate().child(short))
                        .child(icon(Icon::FolderOpen, 12., th.tx3))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_external(&dir, cx))),
                ))
                .child(div().text_color(th.tx3).text_size(px(12.)).child(if g.entry.installable {
                    "Steam updates, verifies and uninstalls this copy. Quiver can also install its own."
                } else {
                    "Bought on Steam: Steam installs, updates, verifies and uninstalls it."
                }));
        } else {
            kvs = kvs.child(div().text_color(th.tx3).child("Not installed. Install puts it in its own compatibility folder; nothing is shared with other games."));
        }
        if let Some(i) = &info {
            if i.features.denuvo {
                kvs = kvs.child(kv(th, "DRM", txt("Denuvo, activated by quiverd".into())));
            }
            kvs = kvs.child(kv(th, "Cloud saves", if i.features.cloud_saves { txt("On · pull before, push after".into()) } else { muted("Not supported by this game".into()) }));
            if let Some(o) = &i.features.game_options {
                kvs = kvs.child(kv(th, "Game settings", txt(o.clone())));
            }
            let shown: Vec<_> = i.addons.iter().filter(|a| a.visible || a.owned).collect();
            if !shown.is_empty() {
                let owned = shown.iter().filter(|a| a.owned).count();
                kvs = kvs.child(kv(th, "Add-ons", txt(format!("{owned} of {} owned", shown.len()))));
            }
            if let Some(p) = &i.publisher {
                kvs = kvs.child(kv(th, "Publisher", txt(p.clone())));
            }
        }
        let owners: Vec<String> = g.entry.owned_by.iter().map(|o| self.account_name(o)).collect();
        if !owners.is_empty() {
            kvs = kvs.child(kv(th, "Owned by", txt(owners.join(", "))));
        }
        let (acct, over) = self.launch_account(id);
        kvs = kvs.child(kv(
            th,
            "Launch account",
            div()
                .id("ov-acct")
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .child(acct.clone())
                .child(div().text_color(th.tx3).text_size(px(11.5)).child(if over { "set for this game" } else { "default" }))
                .child(icon(Icon::Cloud, 12., th.tx3))
                .child(div().text_color(th.tx3).text_size(px(11.5)).child(format!("saves of {acct}")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.nav.select_tab(Tab::LaunchOptions);
                    cx.notify();
                })),
        ));
        if g.installed() {
            let export = |kind: ExportKind| -> AnyElement {
                match self.gp.export(id, kind) {
                    None => muted("Not created".into()),
                    Some(e) => {
                        let (col, label) = match e.state {
                            ExportState::Ok => (th.ok, if kind == ExportKind::Steam { "Shortcut added" } else { "Created" }),
                            ExportState::WaitingForSteam => (th.warn, "Waiting for a Steam restart"),
                            ExportState::Missing => (th.warn, "Deleted outside Quiver"),
                            ExportState::Changed => (th.warn, "Edited outside Quiver"),
                        };
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(div().text_color(col).child(label))
                            .child(div().text_color(th.tx3).child(format!("· {}", day(e.record.written_at))))
                            .into_any_element()
                    }
                }
            };
            kvs = kvs.child(kv(th, "Steam", export(ExportKind::Steam))).child(kv(th, "Desktop entry", export(ExportKind::Desktop)));
        }
        // Also installed through Steam: whether that copy starts through Quiver.
        if let Some(sc) = self.steam_copy(id) {
            let app_id = sc.app_id;
            let action = crate::ui::steamgames::control(&sc);
            let (col, label) = if sc.state.uses_quiver() { (th.ok, "Starts through Quiver") } else { (th.tx2, "Opens Connect first") };
            kvs = kvs.child(kv(
                th,
                "Steam copy",
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_color(col).child(label))
                    .when(matches!(sc.state, ubi_api::exports::PatchState::Waiting | ubi_api::exports::PatchState::WaitingUndo), |d| {
                        d.child(div().text_color(th.warn).child("· after Steam restarts"))
                    })
                    .when_some(action, |d, on| {
                        d.child(
                            div()
                                .id("ov-steam-copy")
                                .text_color(th.acc_hi)
                                .cursor_pointer()
                                .child(if on { "· Use Quiver" } else { "· Undo" })
                                .on_click(cx.listener(move |this, _, _, cx| this.steam_patch(app_id, !on, cx))),
                        )
                    })
                    .into_any_element(),
            ));
        }
        c = c.child(kvs);
        c
    }

    fn render_locations(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let locs = self.file_locations(g.id());
        card(th).child(card_header(th, "Locations")).child(
            div().flex().flex_col().gap(px(6.)).p(px(16.)).children(locs.into_iter().enumerate().map(|(i, (label, path, _))| {
                let p = path.to_string_lossy().into_owned();
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .child(div().w(px(84.)).flex_none().text_color(th.tx3).child(label.split(' ').next().unwrap_or(label).to_owned()))
                    .child(mono(12., th.tx2).flex_1().min_w_0().truncate().child(self.short_path(&path)))
                    .child(
                        icon_button(("ov-open", i), th, Icon::FolderOpen, false)
                            .size(px(26.))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_external(&p, cx))),
                    )
            })),
        )
    }

    fn render_links(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> Option<AnyElement> {
        let info = self.gp.info.ready().filter(|_| self.gp.id == g.id())?;
        if info.links.is_empty() {
            return None;
        }
        let links = info.links.clone();
        Some(
            card(th)
                .child(card_header(th, "Links"))
                .child(div().flex().flex_wrap().gap(px(6.)).p(px(16.)).children(links.into_iter().enumerate().map(|(i, l)| {
                    let (ic, label) = match l.kind.as_str() {
                        "homepage" => (Icon::Globe, "Homepage".to_string()),
                        "support" => (Icon::LifeBuoy, "Support".to_string()),
                        "forum" => (Icon::MessagesSquare, "Forums".to_string()),
                        "about" => (Icon::Info, "About".to_string()),
                        other => (Icon::ExternalLink, other[..1].to_uppercase() + &other[1..]),
                    };
                    let url = l.url.clone();
                    chip(th, Some(ic), label, false)
                        .id(("ov-link", i))
                        .cursor_pointer()
                        .hover(|s| s.bg(th.ink4))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_external(&url, cx)))
                })))
                .into_any_element(),
        )
    }
}

/// A stat's display name without Ubisoft's unit suffixes ("Game Progression (%)").
pub fn stat_name(s: &ubi_api::info::Stat) -> String {
    let n = s.name.clone().unwrap_or_else(|| s.id.clone());
    n.trim_end_matches(" (%)").trim_end_matches(" (m)").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn weekly_buckets() {
        let now = Local.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap(); // a Wednesday
        let s = |y, m, d, mins: u64| {
            let t = Local.with_ymd_and_hms(y, m, d, 20, 0, 0).unwrap().with_timezone(&Utc);
            PlaySession { started: t, ended: t, seconds: mins * 60, settings_revision: 0, profile: None, profile_revision: None }
        };
        let v = weekly_hours(&[s(2026, 11, 16, 60), s(2026, 11, 15, 120), s(2026, 11, 9, 30), s(2025, 1, 1, 600)], 4, now);
        assert_eq!(v, vec![0., 0., 2.5, 1.]);
    }
}
