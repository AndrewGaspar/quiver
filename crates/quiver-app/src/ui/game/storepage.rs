//! The game page for other stores' games (a Steam game in local mode): what the store's
//! client knows, read-only (`Request::GameDetail`). Settings, saves and performance are
//! Quiver's own launches' features, so those tabs aren't shown (a Steam game has Mods, with
//! their Options, which `quiver steam-run` applies: `mods.rs`); Play asks the
//! store's client to start the game. A Steam game's "Quiver launch" card says whether it
//! starts through `quiver steam-run` (its tweaks, HUD and launch settings reach it only then)
//! and switches that (`steamhook.rs`).

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px, relative};
use quiver_api::Request;
use quiver_api::stores::{Achievement, GameDetail};

use super::page::{Load, kv, load_note, note, small};
use crate::app::Quiver;
use crate::err_text;
use crate::model::{GameRow, Gid, day, gb, hours};
use crate::theme::Theme;
use crate::ui::{Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, button, caption, card, card_header, icon, mono, pill, progress};

/// Achievements shown on the page before "… and N more".
const ACH_SHOWN: usize = 12;

impl Quiver {
    pub fn load_store_detail(&mut self, id: Gid, refresh: bool, cx: &mut Context<Self>) {
        if !refresh && !self.gp.store.is_idle() {
            return;
        }
        self.gp.store = Load::Loading;
        self.call::<GameDetail, _>(Request::GameDetail { game: id.0, refresh }, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.store = match r {
                    Ok(d) => Load::Ready(d),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    fn store_detail(&self, id: Gid) -> Option<&GameDetail> {
        self.gp.store.ready().filter(|_| self.gp.id == id)
    }

    /// The stats row under the hero for another store's game.
    pub(super) fn store_stats(&self, g: &GameRow) -> Vec<(&'static str, String)> {
        let label = g.entry.store().label();
        let c = g.entry.client.as_ref();
        // Steam's client records play time; Epic's comes with the page (Epic's own total).
        let pt = c.and_then(|c| c.playtime).or_else(|| self.store_detail(g.id()).and_then(|d| d.playtime));
        let ach = self.store_detail(g.id()).and_then(|d| d.achievements.as_ref()).map(|a| (a.unlocked, a.total)).or_else(|| c.and_then(|c| c.achievements).map(|a| (a.unlocked, a.total)));
        vec![
            ("Last played", g.last_played().map(day).unwrap_or_else(|| "Never".into())),
            ("Play time", pt.map(|p| p.total_seconds).filter(|s| *s > 0).map(hours).unwrap_or_else(|| "—".into())),
            ("Two weeks", pt.and_then(|p| p.two_weeks_seconds).map(hours).unwrap_or_else(|| "—".into())),
            ("Achievements", ach.filter(|(_, t)| *t > 0).map(|(u, t)| format!("{u} of {t}")).unwrap_or_else(|| "—".into())),
            ("Installed", c.map(|c| c.size_bytes).filter(|s| *s > 0).map(gb).unwrap_or_else(|| format!("Via {label}"))),
            ("Store", label.to_owned()),
        ]
    }

    pub(super) fn render_store_overview(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let label = g.entry.store().label();
        let d = self.store_detail(id).cloned();
        let two = self.pane_bp().two_columns();
        let mut left = div().flex().flex_col().flex_1().min_w_0().gap(px(20.));
        let mut right = div().flex().flex_col().when(two, |d| d.w(px(360.)).flex_none()).gap(px(20.));
        match &d {
            None => {
                left = left.child(card(th).children(load_note(th, &self.gp.store, "the game's details")));
            }
            Some(d) => {
                left = left.child(self.render_store_achievements(th, d)).when(!d.dlc.is_empty(), |l| l.child(render_dlc(th, d)));
                if !d.extras.is_empty() {
                    left = left.child(self.render_extras(th, id, d, cx));
                }
                let install = if g.installed() { self.render_quiver_install(th, g, d, cx) } else { render_store_install(th, d, label) };
                right = right.children(self.render_steam_hook_card(th, g, cx)).child(render_store_about(th, d, label)).child(install);
                if let Some(c) = &d.cloud {
                    let pending = if c.unsynced > 0 { format!(" · {} not synced yet", c.unsynced) } else { String::new() };
                    right = right.child(
                        card(th).child(card_header(th, format!("{label} Cloud"))).child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .p(px(16.))
                                .child(kv(th, "Files", div().child(format!("{} · {}{pending}", c.files, gb(c.bytes)))))
                                .child(kv(th, "Last upload", div().child(c.last_upload.map(day).unwrap_or_else(|| "—".into()))))
                                .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{label} syncs these before and after each session; Quiver only shows them."))),
                        ),
                    );
                }
            }
        }
        right = right.child(self.cli_line("st-cli", th, format!("quiver play {id}"), cx));
        div().flex().when(!two, |d| d.flex_col()).gap(px(20.)).child(left).child(right).into_any_element()
    }

    pub(super) fn render_store_achievements(&self, th: &Theme, d: &GameDetail) -> AnyElement {
        let label = d.game.store.label();
        // Epic's achievements aren't read yet (STORE-EPIC phase 3).
        if d.achievements.is_none() && !d.capabilities.achievements.available() && d.game.store != quiver_api::StoreId::Steam {
            return card(th).child(card_header(th, "Achievements")).child(note(th, format!("Quiver doesn't read {label} achievements yet."))).into_any_element();
        }
        if d.achievements.is_none() && d.game.store == quiver_api::StoreId::Epic {
            return card(th).child(card_header(th, "Achievements")).child(note(th, "Epic's store couldn't be read for this game's achievements just now.")).into_any_element();
        }
        let Some(a) = &d.achievements else {
            return card(th)
                .child(card_header(th, "Achievements"))
                .child(note(th, format!("{label} hasn't cached this game's achievements here. Starting it once in {label} fills them in.")))
                .into_any_element();
        };
        if a.total == 0 {
            return card(th).child(card_header(th, "Achievements")).child(note(th, "This game has no achievements.")).into_any_element();
        }
        // Unlocked first (newest first), then locked in the game's order.
        let mut list: Vec<&Achievement> = a.list.iter().collect();
        list.sort_by_key(|x| (!x.unlocked, std::cmp::Reverse(x.unlocked_at)));
        let more = list.len().saturating_sub(ACH_SHOWN);
        let frac = a.unlocked as f32 / a.total as f32;
        card(th)
            .child(
                card_header(th, "Achievements")
                    .child(div().flex_1())
                    .child(div().text_size(px(12.5)).text_color(th.tx2).child(format!("{} of {} · {:.0}%", a.unlocked, a.total, frac * 100.))),
            )
            .child(div().px(px(16.)).pt(px(14.)).child(progress(th, frac, None, 6., None)))
            .child(div().flex().flex_col().p(px(8.)).children(list.into_iter().take(ACH_SHOWN).enumerate().map(|(i, x)| {
                let hidden = x.hidden && !x.unlocked;
                div()
                    .id(("st-ach", i))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(8.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.))
                            .flex_none()
                            .rounded(px(7.))
                            .bg(if x.unlocked { th.acc.opacity(0.18) } else { th.ink4 })
                            .child(icon(Icon::Trophy, 15., if x.unlocked { th.acc } else { th.tx4 })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(13.)).font_weight(MEDIUM).truncate().text_color(if x.unlocked { th.tx1 } else { th.tx2 }).child(if hidden { "Hidden achievement".to_owned() } else { x.name.clone() }))
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(if hidden { "Details appear once it's unlocked".to_owned() } else { x.description.clone() })),
                    )
                    .child(div().flex_none().text_size(px(11.5)).text_color(th.tx3).child(match (x.unlocked, x.unlocked_at) {
                        (true, Some(t)) => day(t),
                        (true, None) => "Unlocked".into(),
                        (false, _) => String::new(),
                    }))
            })))
            .when(more > 0, |c| c.child(div().px(px(16.)).pb(px(14.)).text_size(px(12.)).text_color(th.tx3).child(format!("… and {more} more"))))
            .into_any_element()
    }
}

fn render_store_about(th: &Theme, d: &GameDetail, label: &str) -> AnyElement {
    let row = |k: &str, v: Option<String>| v.filter(|v| !v.is_empty()).map(|v| kv(th, k, div().child(v)));
    card(th)
        .child(card_header(th, "About"))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(16.))
                .children(row("Developer", d.developer.clone()))
                .children(row("Publisher", d.publisher.clone()))
                .children(row("Released", d.release_date.map(|t| t.format("%b %-d, %Y").to_string())))
                .children(row("Account", d.account.clone().map(|a| if d.game.store == quiver_api::StoreId::Steam { format!("{a} · the {label} client's") } else { a })))
                .children(row("Plays as", d.linked.map(|u| format!("{u}: {} manages it, and Quiver plays it there once it's on your account", if u.is_ubisoft() { "Ubisoft Connect" } else { u.store.label() }))))
                .children(d.description.clone().map(|t| div().text_size(px(12.5)).line_height(px(18.)).text_color(th.tx2).child(t)))
                .children(d.store_url.clone().map(|u| kv(th, "Store page", mono(12., th.tx2).truncate().child(u))))
                .when(!d.unavailable.is_empty(), |c| c.child(div().text_size(px(11.5)).text_color(th.tx4).child(format!("Not available from {label}'s files: {}", d.unavailable.join(", "))))),
        )
        .into_any_element()
}

fn render_store_install(th: &Theme, d: &GameDetail, label: &str) -> AnyElement {
    let Some(i) = &d.install else {
        // An Epic game: Quiver installs it itself from the next build on.
        if d.game.store == quiver_api::StoreId::Epic && d.linked.is_none() {
            return card(th)
                .child(card_header(th, "Install").child(div().flex_1()).child(pill(th, PillKind::Muted, None, "Not installed")))
                .child(div().p(px(16.)).text_size(px(12.)).text_color(th.tx3).child("Installing and playing Epic games comes in the next Quiver build."))
                .into_any_element();
        }
        return div().into_any_element();
    };
    let state = if !i.complete {
        pill(th, PillKind::Warn, Some(Icon::Download), format!("Downloading in {label}"))
    } else if i.update_pending {
        pill(th, PillKind::Warn, Some(Icon::ArrowDownToLine), format!("Update waiting in {label}"))
    } else {
        pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Up to date")
    };
    card(th)
        .child(card_header(th, "Install").child(div().flex_1()).child(state))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(16.))
                .child(kv(th, "Size", div().child(gb(i.size_bytes))))
                .child(kv(th, "Folder", mono(12., th.tx2).child(crate::model::tilde(&i.dir))))
                .when(!i.platforms.is_empty(), |c| c.child(kv(th, "Platforms", div().child(i.platforms.join(", ")))))
                .children(i.build_id.clone().map(|b| kv(th, "Build", mono(12., th.tx2).child(b))))
                .children(i.launch_url.clone().map(|u| kv(th, "Plays with", mono(12., th.tx2).child(u))))
                .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{label} installs, updates and starts it, and keeps its licence, overlay and play time."))),
        )
        .into_any_element()
}

impl Quiver {
    /// The install card of a game Quiver installed from another store (Epic): its folder,
    /// size and build, with Verify and Uninstall.
    fn render_quiver_install(&self, th: &Theme, g: &GameRow, d: &GameDetail, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let label = g.entry.store().label();
        let dir = g.entry.installed.as_ref().map(|i| i.install_dir.clone()).or_else(|| d.install.as_ref().map(|i| i.dir.clone())).unwrap_or_default();
        let state = match &d.install {
            Some(i) if i.update_pending => pill(th, PillKind::Warn, Some(Icon::ArrowDownToLine), "Update available"),
            _ => pill(th, PillKind::Ok, Some(Icon::CircleCheck), "Installed"),
        };
        let verify = button("st-verify", th, Kind::Ghost, Size::Small, Some(Icon::ShieldCheck), "Verify…")
            .on_click(cx.listener(move |this, _, _, cx| this.run_dl(crate::ui::downloads::DlCmd::Verify(id), cx)));
        let uninstall = button("st-uninstall", th, Kind::Ghost, Size::Small, Some(Icon::Trash), "Uninstall…")
            .on_click(cx.listener(move |this, _, _, cx| this.run_dl(crate::ui::downloads::DlCmd::Uninstall(id), cx)));
        card(th)
            .child(card_header(th, "This install").child(div().flex_1()).child(state).child(verify).child(uninstall))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(16.))
                    .children(d.install.as_ref().filter(|i| i.size_bytes > 0).map(|i| kv(th, "Size", div().child(gb(i.size_bytes)))))
                    .child(kv(th, "Folder", mono(12., th.tx2).child(crate::model::tilde(&dir))))
                    .children(d.install.as_ref().and_then(|i| i.build_id.clone()).map(|b| kv(th, "Build", mono(12., th.tx2).child(b))))
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!("Quiver installed it from {label} and updates, verifies and plays it, in its own compatibility folder."))),
            )
            .into_any_element()
    }

    /// Digital extras (Epic: soundtracks, art books, wallpapers): their own downloads, in a
    /// folder next to the game's. Download, or Open once downloaded.
    fn render_extras(&self, th: &Theme, id: Gid, d: &GameDetail, cx: &mut Context<Self>) -> AnyElement {
        let done = d.extras.iter().filter(|x| x.installed.is_some()).count();
        let rows = d.extras.iter().enumerate().map(|(i, x)| {
            let action = match (&x.installed, &x.dir) {
                (Some(_), Some(dir)) => {
                    let dir = dir.to_string_lossy().into_owned();
                    Some(small(("st-extra-open", i), th, Some(Icon::FolderOpen), "Open").on_click(cx.listener(move |this, _, _, cx| this.open_external(&dir, cx))).into_any_element())
                }
                (None, _) if x.installable => {
                    let extra = x.id.clone();
                    Some(
                        small(("st-extra-get", i), th, Some(Icon::Download), "Download")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let req = Request::Epic(quiver_api::epic::EpicRequest::ExtraInstall { game: id.0, extra: extra.clone() });
                                this.call::<u64, _>(req, cx, move |this, r, cx| match r {
                                    Ok(_) => this.show_toast("Downloading the extra next to the game's folder (see Downloads)", cx),
                                    Err(e) => this.show_error(&e, cx),
                                });
                            }))
                            .into_any_element(),
                    )
                }
                _ => None,
            };
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .py(px(6.))
                .when(i > 0, |r| r.border_t_1().border_color(th.line1))
                .child(icon(if x.installed.is_some() { Icon::CircleCheck } else { Icon::Package }, 14., if x.installed.is_some() { th.ok } else { th.tx4 }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().text_size(px(12.5)).font_weight(MEDIUM).child(x.name.clone()))
                        .children(x.dir.as_ref().filter(|_| x.installed.is_some()).map(|dir| mono(11., th.tx3).truncate().child(crate::model::tilde(dir))))
                        .children(x.note.clone().filter(|_| x.installed.is_none()).map(|n| div().text_size(px(11.5)).text_color(th.tx3).child(n))),
                )
                .children(action)
        });
        card(th)
            .child(card_header(th, "Extras").child(div().flex_1()).child(caption(&format!("{} owned · {done} downloaded", d.extras.len()), th.tx3)))
            .child(div().flex().flex_col().p(px(8.)).children(rows))
            .into_any_element()
    }
}

fn render_dlc(th: &Theme, d: &GameDetail) -> AnyElement {
    let installed = d.dlc.iter().filter(|x| x.installed).count();
    let owned = d.dlc.iter().filter(|x| x.owned == Some(true)).count();
    // Stores that say what's owned (Epic): owned first, and the count in the header.
    let mut list: Vec<&quiver_api::stores::Dlc> = d.dlc.iter().collect();
    list.sort_by_key(|x| (!x.installed, x.owned != Some(true)));
    let summary = if d.dlc.iter().any(|x| x.owned.is_some()) {
        format!("{} listed · {owned} owned · {installed} installed", d.dlc.len())
    } else {
        format!("{} listed · {installed} installed", d.dlc.len())
    };
    card(th)
        .child(card_header(th, "DLC").child(div().flex_1()).child(caption(&summary, th.tx3)))
        .child(div().flex().flex_col().p(px(8.)).children(list.into_iter().take(10).enumerate().map(|(i, x)| {
            let mine = x.installed || x.owned == Some(true);
            div()
                .id(("st-dlc", i))
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .py(px(6.))
                .child(icon(if mine { Icon::CircleCheck } else { Icon::Package }, 14., if mine { th.ok } else { th.tx4 }))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.5)).font_weight(if mine { SEMIBOLD } else { MEDIUM }).child(x.name.clone().unwrap_or_else(|| x.game.to_string())))
                .when(x.owned == Some(true) && !x.installed, |r| r.child(caption("Owned", th.tx3)))
        })))
        .when(d.dlc.len() > 10, |c| c.child(div().px(px(16.)).pb(px(12.)).w(relative(1.)).text_size(px(12.)).text_color(th.tx3).child(format!("… and {} more", d.dlc.len() - 10))))
        .into_any_element()
}
