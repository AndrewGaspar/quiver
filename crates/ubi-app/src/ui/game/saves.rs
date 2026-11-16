//! Saves & progress: play time, the Ubisoft Connect level, save slots (this PC against the
//! cloud), backups with restore, player stats, rewards and achievement definitions.

use std::path::Path;
use std::sync::Arc;

use gpui::{AnyElement, Context, IntoElement, ObjectFit, ParentElement, Styled, StyledImage, div, img, prelude::*, px};
use ubi_api::info::{Achievement, Progress, SaveSlot, SavesReport, SlotState};

use super::page::{load_note, note, small};
use crate::app::Quiver;
use crate::model::{GameRow, clock, day, duration, hours};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::{BOLD, FocusExt, Icon, MEDIUM, PillKind, SEMIBOLD, badge, caption, card, card_header, icon, icon_button, mono, pill, progress};

/// An achievement's icon (a local file from the daemon's cache), or a trophy tile.
pub fn ach_icon(th: &Theme, a: &Achievement, size: f32) -> AnyElement {
    let tile = move |th: &Theme| {
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(size))
            .rounded(px(size * 0.22))
            .bg(th.ink3)
            .border_1()
            .border_color(th.line2)
            .child(icon(Icon::Trophy, size * 0.42, th.tx3))
    };
    match &a.image {
        Some(p) if Path::new(p).is_file() => {
            let th2 = th.clone();
            img(Arc::<Path>::from(Path::new(p)))
                .flex_none()
                .size(px(size))
                .rounded(px(size * 0.22))
                .object_fit(ObjectFit::Cover)
                .with_fallback(move || tile(&th2).into_any_element())
                .into_any_element()
        }
        _ => tile(th).into_any_element(),
    }
}

fn kb(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn when(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|t| format!("{} {}", day(t), clock(t))).unwrap_or_else(|| "—".into())
}

fn slot_state(th: &Theme, s: &SaveSlot) -> AnyElement {
    let (k, label) = match s.state {
        SlotState::InSync => (PillKind::Ok, "In sync"),
        SlotState::LocalOnly if s.pending_upload => (PillKind::Warn, "Not uploaded yet"),
        SlotState::LocalOnly => (PillKind::Muted, "Only on this PC"),
        SlotState::CloudOnly => (PillKind::Accent, "Only in the cloud"),
        SlotState::LocalNewer => (PillKind::Warn, "This PC is newer"),
        SlotState::CloudNewer => (PillKind::Accent, "Cloud is newer"),
        SlotState::Differs => (PillKind::Warn, "Differs"),
        SlotState::Unknown => (PillKind::Muted, "Not compared"),
    };
    pill(th, k, None, label).into_any_element()
}

impl Quiver {
    pub(super) fn render_saves(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        // A Steam copy's saves are in Steam's compatibility folder; quiverd reads them there.
        let local = g.installed() || g.steam_only();
        if local && self.gp.id == id && (self.gp.saves.is_idle() || (!self.gp.saves_cloud && self.gp.saves.ready().is_some())) {
            self.load_saves(id, true, cx);
        }
        let progress = self.gp.progress.ready().filter(|_| self.gp.id == id).cloned();
        let top = div()
            .flex()
            .gap(px(16.))
            .child(self.saves_playtime(th, g).flex_1())
            .child(self.connect_card(th, progress.as_ref()).w(px(300.)).flex_none())
            .child(self.ach_summary(th, progress.as_ref()).w(px(300.)).flex_none());
        let slots: AnyElement = if !local {
            card(th).child(card_header(th, "Save slots")).child(note(th, "Install the game to see its saves on this PC; cloud slots sync on the first launch.")).into_any_element()
        } else {
            match self.gp.saves.ready().cloned() {
                Some(s) => div()
                    .flex()
                    .gap(px(16.))
                    .items_start()
                    .child(self.slots_card(th, g, &s, cx))
                    .child(self.backups_card(th, g, &s, cx))
                    .into_any_element(),
                None => card(th).child(card_header(th, "Save slots")).children(load_note(th, &self.gp.saves, "saves")).into_any_element(),
            }
        };
        let mut col = div().flex().flex_col().gap(px(18.)).child(top).child(slots);
        if g.installed() {
            col = col.child(self.render_stores_card(th, g, cx));
        }
        match &progress {
            Some(p) => {
                col = col.child(div().flex().gap(px(16.)).items_start().child(self.stats_card(th, p).flex_1()).child(self.rewards_card(th, p).flex_1()));
                if let Some(a) = &p.achievements {
                    col = col.child(self.ach_grid(th, a));
                }
                if !p.errors.is_empty() {
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(th.tx3)
                            .child(icon(Icon::Info, 12., th.tx3))
                            .child(format!("Not available from Ubisoft right now: {}", p.errors.join("; "))),
                    );
                }
            }
            None => {
                col = col.child(card(th).child(card_header(th, "Progress")).children(load_note(th, &self.gp.progress, "progression")));
            }
        }
        col.into_any_element()
    }

    fn saves_playtime(&self, th: &Theme, g: &GameRow) -> gpui::Div {
        let pt = g.playtime.clone().unwrap_or_default();
        let ubi = self.ubisoft_playtime(g.id());
        card(th)
            .p(px(16.))
            .gap(px(8.))
            .child(div().flex().items_center().child(caption("Play time", th.tx3)).child(div().flex_1()).child(match ubi {
                Some(_) => pill(th, PillKind::Ok, Some(Icon::Cloud), "Ubisoft total"),
                None => pill(th, PillKind::Muted, Some(Icon::HardDrive), "Local only"),
            }))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .child(div().text_size(px(24.)).font_weight(BOLD).child(match ubi {
                        Some((s, _)) => hours(s),
                        None => hours(pt.total_seconds),
                    }))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!(
                        "{}local {} in {} sessions",
                        if ubi.is_some() { "Ubisoft total · " } else { "" },
                        duration(pt.total_seconds),
                        pt.sessions.len()
                    ))),
            )
            .child(super::overview::weekly_bars(th, &pt.sessions, 56.))
    }

    fn connect_card(&self, th: &Theme, p: Option<&Progress>) -> gpui::Div {
        let mut c = card(th).p(px(16.)).gap(px(10.)).child(div().flex().items_center().child(caption("Ubisoft Connect", th.tx3)).child(div().flex_1()).child(pill(th, PillKind::Muted, Some(Icon::User), "account-wide")));
        let Some(cp) = p.and_then(|p| p.connect.clone()) else {
            return c.child(div().text_color(th.tx3).text_size(px(12.)).child(if p.is_some() { "Ubisoft didn't return a Connect level." } else { "Loading…" }));
        };
        let frac = match (cp.xp, cp.level_start_xp, cp.next_level_xp) {
            (Some(x), Some(a), Some(b)) if b > a => (x.saturating_sub(a)) as f32 / (b - a) as f32,
            _ => 0.,
        };
        c = c
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(46.))
                            .rounded(px(12.))
                            .bg(th.acc_soft)
                            .border_1()
                            .border_color(th.acc_line)
                            .text_size(px(18.))
                            .font_weight(BOLD)
                            .text_color(th.acc_hi)
                            .child(cp.level.map(|l| l.to_string()).unwrap_or_else(|| "—".into())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(SEMIBOLD).child(format!("Level {}", cp.level.unwrap_or(0))))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(match (cp.xp, cp.next_level_xp) {
                                (Some(x), Some(n)) => format!("{} XP to level {}", n.saturating_sub(x), cp.level.unwrap_or(0) + 1),
                                (Some(x), None) => format!("{x} XP"),
                                _ => String::new(),
                            })),
                    ),
            )
            .child(progress(th, frac.clamp(0., 1.), None, 6., None))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.5))
                    .child(icon(Icon::Coins, 14., th.warn))
                    .child(div().font_weight(SEMIBOLD).child(cp.units.map(|u| u.to_string()).unwrap_or_else(|| "—".into())))
                    .child(div().text_color(th.tx3).child("units to spend on rewards")),
            );
        c
    }

    fn ach_summary(&self, th: &Theme, p: Option<&Progress>) -> gpui::Div {
        let mut c = card(th).p(px(16.)).gap(px(10.)).child(div().flex().items_center().child(caption("Achievements", th.tx3)).child(div().flex_1()));
        let Some(a) = p.and_then(|p| p.achievements.clone()) else {
            return c.child(div().text_color(th.tx3).text_size(px(12.)).child(if p.is_some() { "This game has no Ubisoft achievements, or their set isn't available." } else { "Loading…" }));
        };
        c = c
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(div().text_size(px(24.)).font_weight(BOLD).child(a.total.to_string()))
                    .child(div().text_color(th.tx3).text_size(px(12.)).child(if a.unlocked_known { format!("{} unlocked", a.unlocked) } else { "defined for this game".into() })),
            )
            .child(div().flex().gap(px(6.)).children(a.items.iter().take(6).map(|x| ach_icon(th, x, 36.))))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.5))
                    .text_color(th.tx3)
                    .child(icon(Icon::Info, 12., th.tx3))
                    .child(div().flex_1().min_w_0().child(if a.unlocked_known { "From Ubisoft" } else { "Unlock status isn't available from Ubisoft yet" })),
            );
        c
    }

    fn slots_card(&mut self, th: &Theme, g: &GameRow, s: &SavesReport, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let source = match s.cloud_source.as_deref() {
            Some("cloudsave") => pill(th, PillKind::Ok, Some(Icon::Cloud), "Compared with the cloud"),
            Some("connect_record") => pill(th, PillKind::Accent, Some(Icon::Cloud), "From Connect's upload record"),
            _ if self.gp.saves_cloud => pill(th, PillKind::Muted, Some(Icon::HardDrive), "No cloud saves"),
            _ => pill(th, PillKind::Muted, Some(Icon::HardDrive), "This PC only"),
        };
        let dir = s.dir.clone().map(|d| d.to_string_lossy().into_owned());
        let account = s.account.clone().map(|a| self.account_name(&a));
        let title = match &s.store {
            Some(n) => format!("Save slots · {n}"),
            None => "Save slots".to_owned(),
        };
        let header = card_header(th, title)
            .child(source)
            .when_some(account, |d, a| d.child(div().text_size(px(12.)).text_color(th.tx3).child(format!("saves of {a}"))))
            .child(div().flex_1())
            .child(small("saves-refresh", th, Some(Icon::RefreshCw), "Check again").on_click(cx.listener(move |this, _, _, cx| {
                this.load_saves(id, true, cx);
                cx.notify();
            })))
            .when_some(dir, |d, dir| {
                d.child(icon_button("saves-dir", th, Icon::FolderOpen, true).size(px(26.)).on_click(cx.listener(move |this, _, _, cx| this.open_external(&dir, cx))))
            });
        let th_row = |cells: Vec<(&str, f32)>| {
            div().flex().items_center().gap(px(12.)).px(px(16.)).py(px(7.)).border_b_1().border_color(th.line1).children(cells.into_iter().map(|(c, w)| {
                let d = caption(c, th.tx3);
                if w > 0. { d.w(px(w)).flex_none() } else { d.flex_1() }
            }))
        };
        let rows: Vec<AnyElement> = s
            .slots
            .iter()
            .map(|sl| {
                let warn = matches!(sl.state, SlotState::LocalNewer | SlotState::Differs) || sl.pending_upload;
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(th.line1)
                    .when(warn, |d| d.bg(th.warn_soft.opacity(0.5)))
                    .child(
                        div()
                            .w(px(40.))
                            .flex_none()
                            .child(mono(12., th.tx1).font_weight(SEMIBOLD).flex().items_center().justify_center().size(px(24.)).rounded(px(6.)).bg(th.ink4).child(sl.slot.to_string())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(MEDIUM).child(sl.local.as_ref().map(|l| when(l.modified)).unwrap_or_else(|| "—".into())))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(sl.local.as_ref().map(|l| kb(l.size)).unwrap_or_else(|| "not on this PC".into()))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(MEDIUM).text_color(if sl.cloud.is_some() { th.tx1 } else { th.tx3 }).child(sl.cloud.as_ref().map(|c| when(c.modified)).unwrap_or_else(|| "—".into())))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).child(match &sl.cloud {
                                Some(c) => c.size.map(kb).unwrap_or_else(|| "?".into()),
                                None if self.gp.saves_cloud && s.cloud_source.is_some() => "not in the cloud".into(),
                                None => String::new(),
                            })),
                    )
                    .child(div().w(px(160.)).flex_none().flex().flex_col().items_start().gap(px(2.)).child(slot_state(th, sl)).when(sl.pending_upload, |d| {
                        d.child(div().text_size(px(11.)).text_color(th.tx3).child("uploads after next session"))
                    }).when(sl.state == SlotState::CloudNewer, |d| d.child(div().text_size(px(11.)).text_color(th.tx3).child("downloads at the next launch"))))
                    .into_any_element()
            })
            .collect();
        let mut c = card(th).flex_1().min_w_0().child(header);
        if let Some(e) = &s.cloud_error {
            c = c.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mx(px(16.))
                    .mt(px(10.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(th.warn_soft)
                    .text_color(th.warn)
                    .text_size(px(12.))
                    .child(icon(Icon::TriangleAlert, 14., th.warn))
                    .child(format!("Couldn't read the cloud: {e}")),
            );
        }
        c = c.child(th_row(vec![("Slot", 40.), ("This PC", 0.), ("Cloud", 0.), ("Status", 160.)]));
        if rows.is_empty() {
            c = c.child(note(th, "No saves yet. The game writes them here; with cloud saves on they're uploaded after each session."));
        }
        c = c.children(rows);
        let cloud_newer: Vec<String> = s.slots.iter().filter(|x| x.state == SlotState::CloudNewer).map(|x| x.slot.to_string()).collect();
        if !cloud_newer.is_empty() {
            c = c.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .m(px(14.))
                    .px(px(12.))
                    .py(px(9.))
                    .rounded(px(8.))
                    .bg(th.acc_soft)
                    .text_size(px(12.))
                    .text_color(th.acc_hi)
                    .child(icon(Icon::CloudDownload, 14., th.acc_hi))
                    .child(format!(
                        "Slot {} is newer in the cloud. The next launch downloads it and backs up this PC's copy first.",
                        cloud_newer.join(", ")
                    )),
            );
        } else {
            c = c.child(div().px(px(16.)).py(px(10.)).text_size(px(11.5)).text_color(th.tx3).child("Pulled before each launch and pushed after it exits; Quiver backs up before overwriting anything."));
        }
        if let Some(r) = &s.connect_record {
            c = c.child(
                div()
                    .px(px(16.))
                    .pb(px(12.))
                    .text_size(px(11.5))
                    .text_color(th.tx3)
                    .child(format!("Ubisoft Connect last uploaded from {} on {}", r.machine.clone().unwrap_or_else(|| "another PC".into()), when(r.modified))),
            );
        }
        c
    }

    fn backups_card(&mut self, th: &Theme, g: &GameRow, s: &SavesReport, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let total: u64 = s.backups.iter().flat_map(|b| b.slots.iter().map(|x| x.1)).sum();
        let rows: Vec<AnyElement> = s
            .backups
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let focused = self.focused(Target::Body(i));
                let backup = b.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(8.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(focused, |d| d.bg(th.acc_soft))
                    .child(div().size(px(7.)).rounded_full().flex_none().bg(if b.reason.as_deref().is_some_and(|r| r.starts_with("before restoring")) { th.who_gui } else { th.tx4 }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(div().font_weight(MEDIUM).text_size(px(12.5)).child(when(b.created)))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(format!(
                                "{} · {} slot{}",
                                b.reason.clone().unwrap_or_else(|| "backup".into()),
                                b.slots.len(),
                                if b.slots.len() == 1 { "" } else { "s" }
                            ))),
                    )
                    .child(
                        small(("backup-restore", i), th, Some(Icon::RotateCcw), "Restore")
                            .focus_ring(focused, th, 6.)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_dialog(super::page::Dialog::RestoreSaves { game: id, backup: backup.clone() });
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        card(th)
            .w(px(330.))
            .flex_none()
            .child(card_header(th, "Backups").child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{} kept · {}", s.backups.len(), kb(total)))))
            .when(rows.is_empty(), |d| d.child(note(th, "None yet. Quiver backs the save folder up before a cloud download or a restore replaces anything.")))
            .child(div().flex().flex_col().p(px(8.)).children(rows))
    }

    fn stats_card(&self, th: &Theme, p: &Progress) -> gpui::Div {
        let rows: Vec<AnyElement> = p
            .stats
            .iter()
            .map(|s| {
                let pct = (s.kind.as_deref() == Some("Percent") || s.value.as_deref().is_some_and(|v| v.ends_with('%'))).then(|| s.number.unwrap_or(0.) as f32 / 100.);
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(5.))
                    .text_size(px(12.5))
                    .child(div().flex_1().text_color(th.tx2).child(super::overview::stat_name(s)))
                    .when_some(pct, |d, f| d.child(progress(th, f, Some(80.), 4., None)))
                    .child(div().w(px(80.)).flex().justify_end().font_weight(SEMIBOLD).child(s.value.clone().unwrap_or_else(|| "—".into())))
                    .into_any_element()
            })
            .collect();
        card(th)
            .child(card_header(th, "Player stats").child(div().text_size(px(12.)).text_color(th.tx3).child("from Ubisoft")))
            .when(rows.is_empty(), |d| d.child(note(th, "Ubisoft has no player stats for this game.")))
            .child(div().flex().flex_col().px(px(16.)).py(px(8.)).children(rows))
    }

    fn rewards_card(&self, th: &Theme, p: &Progress) -> gpui::Div {
        let rows: Vec<AnyElement> = p
            .rewards
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let rarity = r.rarity.clone().unwrap_or_default();
                let tone = match rarity.as_str() {
                    "Legendary" => th.warn,
                    "Exotic" => th.who_agent,
                    "Epic" => th.acc_hi,
                    "Rare" => th.who_cli,
                    _ => th.tx3,
                };
                let (k, status) = match (r.owned, r.status.as_deref()) {
                    (true, _) | (_, Some("LimitReached")) => (PillKind::Ok, "Redeemed".to_string()),
                    (_, Some("Unlocked")) => (PillKind::Accent, if r.cost.unwrap_or(0) == 0 { "Free".into() } else { format!("{} units", r.cost.unwrap_or(0)) }),
                    (_, Some("Locked")) => (PillKind::Muted, "Locked".into()),
                    (_, s) => (PillKind::Muted, s.unwrap_or("—").to_owned()),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(9.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(div().flex().flex_none().items_center().justify_center().size(px(30.)).rounded(px(8.)).bg(tone.opacity(0.14)).child(icon(Icon::Award, 15., tone)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(MEDIUM).truncate().child(r.name.clone().unwrap_or_else(|| r.id.clone())))
                            .child(div().text_size(px(11.5)).text_color(tone).child(format!(
                                "{rarity}{}",
                                match (r.cost, r.redeemed_at) {
                                    (_, Some(t)) => format!(" · redeemed {}", day(t)),
                                    (Some(c), None) if c > 0 => format!(" · {c} units"),
                                    _ => String::new(),
                                }
                            ))),
                    )
                    .child(pill(th, k, None, status))
                    .into_any_element()
            })
            .collect();
        card(th)
            .child(card_header(th, "Rewards").child(div().text_size(px(12.)).text_color(th.tx3).child("Ubisoft Connect, for this game")))
            .when(rows.is_empty(), |d| d.child(note(th, "No Ubisoft Connect rewards for this game.")))
            .children(rows)
    }

    fn ach_grid(&self, th: &Theme, a: &ubi_api::info::Achievements) -> gpui::Div {
        let items: Vec<AnyElement> = a
            .items
            .iter()
            .map(|x| {
                div()
                    .w(px(300.))
                    .flex_grow()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .p(px(8.))
                    .rounded(px(10.))
                    .bg(th.ink1)
                    .border_1()
                    .border_color(th.line1)
                    .child(ach_icon(th, x, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(div().font_weight(SEMIBOLD).text_size(px(12.5)).truncate().child(x.name.clone().unwrap_or_else(|| x.id.clone())))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).line_height(px(15.)).line_clamp(2).child(x.description.clone().unwrap_or_default())),
                    )
                    .into_any_element()
            })
            .collect();
        card(th)
            .child(
                card_header(th, "Achievements")
                    .child(badge(th, a.total.to_string(), false))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(if a.unlocked_known { format!("{} unlocked", a.unlocked) } else { "Definitions from Ubisoft; unlock status isn't available yet, so none are marked".into() })),
            )
            .child(div().flex().flex_wrap().gap(px(8.)).p(px(14.)).children(items))
    }
}
