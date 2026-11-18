//! The uninstall dialog (a [`DlDialog`] beside install, update and verify): what goes
//! (the install folder and its size), whether the game's compatibility folder goes too
//! (off by default, with what that loses), and that Quiver's exports go with it.
//! Uninstall is refused while the game runs or downloads; the daemon checks the same.
//!
//! Games another store's client installs (Steam) aren't Quiver's to remove: the dialog
//! doesn't open for them and says who does it instead.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::exports::{ExportKind, ExportStatus};
use quiver_api::info::GameInfo;
use quiver_api::stores::GameDetail;
use quiver_api::{JobKind, Request, StoreId, UninstallResult};

use super::downloads::{DCtl, DlDialog};
use super::game::{checkbox, kv};
use super::{FocusExt, Icon, MEDIUM, icon, mono};
use crate::app::Quiver;
use crate::err_text;
use crate::model::{Gid, gb, tilde};
use crate::nav::Page;
use crate::theme::Theme;

/// What the uninstall dialog knows about the game beyond its library row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UninstallFacts {
    /// Bytes the install takes, as the daemon counts them.
    pub size: Option<u64>,
    /// The store's cloud keeps the game's saves: `None` when unknown.
    pub cloud_saves: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UninstallDialog {
    pub id: Gid,
    /// Also delete the compatibility folder (off until ticked).
    pub remove_prefix: bool,
    /// `None` while loading; an error keeps the dialog usable (the size is only shown).
    pub facts: Option<Result<UninstallFacts, String>>,
}

/// Why the game can't be uninstalled right now.
pub fn blocked(running: bool, busy: bool) -> Option<&'static str> {
    if running {
        Some("It's running. Quit the game first; Quiver doesn't uninstall a game while it runs.")
    } else if busy {
        Some("It's downloading or waiting in the queue. Remove it from Downloads first.")
    } else {
        None
    }
}

/// What deleting the compatibility folder loses, by store and cloud support.
pub fn prefix_loss(store: StoreId, cloud: Option<bool>) -> String {
    let base = "It holds the game's Windows settings and everything the game wrote to its Documents and AppData folders, which is often where its saves are.";
    let saves = match (store, cloud) {
        (StoreId::Ubisoft, Some(true)) => {
            "Ubisoft's cloud keeps this game's Default saves; saves of other save stores (profiles with saves of their own) exist only here and are lost."
        }
        (StoreId::Ubisoft, Some(false)) => "This game doesn't sync saves with Ubisoft's cloud: its saves exist only here and are lost.",
        (StoreId::Epic, Some(true)) => {
            "Quiver syncs this game's saves with Epic's cloud around each session; anything not synced yet, and anything the game keeps outside Epic's save folders, is lost."
        }
        (StoreId::Epic, Some(false)) => "Epic's cloud doesn't keep this game's saves: they exist only here and are lost.",
        _ => "Saves the store's cloud doesn't keep exist only here and are lost.",
    };
    format!("{base} {saves}")
}

/// What happens to the install folder: Ubisoft installs are deleted whole; Epic's
/// uninstall removes only the files its manifest lists.
pub fn folder_note(store: StoreId) -> &'static str {
    match store {
        StoreId::Epic => "Deletes the game's files from this folder. Files it didn't ship (mods or configs you added) stay, with the folder.",
        _ => "Deletes this folder and everything in it.",
    }
}

/// The exports that go with the game, from the export list.
pub fn exports_of(list: &[ExportStatus], id: Gid) -> (bool, bool) {
    let has = |k: ExportKind| list.iter().any(|e| e.record.game == id.0 && e.record.kind == k);
    (has(ExportKind::Desktop), has(ExportKind::Steam))
}

pub fn exports_note(desktop: bool, steam: bool) -> &'static str {
    match (desktop, steam) {
        (true, true) => "Its .desktop entry and Steam shortcut are removed too (a Steam shortcut leaves when Steam next restarts).",
        (true, false) => "Its .desktop entry is removed too.",
        (false, true) => "Its Steam shortcut is removed too (when Steam next restarts, if it's running).",
        (false, false) => "Mods, profiles, launch options and play history stay in Quiver for a reinstall.",
    }
}

impl Quiver {
    /// Open the uninstall dialog, or say why there is none.
    pub fn open_uninstall_dialog(&mut self, id: Gid, cx: &mut Context<Self>) {
        let Some(g) = self.game(id) else { return };
        let name = crate::model::clean_name(&g.entry.name);
        if !g.installed() {
            let msg = if g.steam_only() || g.client().is_some() {
                let label = if g.steam().is_some() { "Steam" } else { g.entry.store().label() };
                format!("{name} is installed through {label}, which uninstalls it")
            } else {
                format!("{name} isn't installed")
            };
            return self.show_toast(msg, cx);
        }
        self.nav.close_overlay();
        self.dl.dialog = Some(DlDialog::Uninstall(UninstallDialog { id, remove_prefix: false, facts: None }));
        // Destructive: Cancel has the focus.
        self.dl.dsel = Self::UNINSTALL_CONTROLS.iter().position(|c| *c == DCtl::Cancel).unwrap_or(0);
        self.load_uninstall_facts(id, cx);
    }

    fn set_uninstall_facts(&mut self, id: Gid, facts: Result<UninstallFacts, String>) {
        if let Some(DlDialog::Uninstall(d)) = &mut self.dl.dialog
            && d.id == id
        {
            d.facts = Some(facts);
        }
    }

    /// The install's size and whether its saves are in the cloud: the open page's, else
    /// asked for (Ubisoft: `GameInfo`; Epic: `GameDetail`).
    fn load_uninstall_facts(&mut self, id: Gid, cx: &mut Context<Self>) {
        let ubisoft = |i: &GameInfo| UninstallFacts { size: i.install.as_ref().map(|x| x.size_bytes).filter(|s| *s > 0), cloud_saves: Some(i.features.cloud_saves) };
        let store = |d: &GameDetail| UninstallFacts {
            size: d.install.as_ref().map(|x| x.size_bytes).filter(|s| *s > 0),
            cloud_saves: d.cloud.as_ref().map(|_| true),
        };
        let here = self.gp.id == id;
        if id.is_ubisoft() {
            if let Some(i) = self.gp.info.ready().filter(|_| here) {
                let f = ubisoft(i);
                return self.set_uninstall_facts(id, Ok(f));
            }
            self.call::<GameInfo, _>(Request::GameInfo { product_id: id.pid(), refresh: false }, cx, move |this, r, _| {
                this.set_uninstall_facts(id, r.map(|i| ubisoft(&i)).map_err(|e| err_text(&e)));
            });
        } else {
            if let Some(d) = self.gp.store.ready().filter(|_| here) {
                let f = store(d);
                return self.set_uninstall_facts(id, Ok(f));
            }
            self.call::<GameDetail, _>(Request::GameDetail { game: id.0, refresh: false }, cx, move |this, r, _| {
                this.set_uninstall_facts(id, r.map(|d| store(&d)).map_err(|e| err_text(&e)));
            });
        }
    }

    /// Why the dialog's game can't be uninstalled now.
    pub(super) fn uninstall_blocked(&self, id: Gid) -> Option<&'static str> {
        let g = self.game(id)?;
        let checking = g.job.as_ref().is_some_and(|j| matches!(j.kind, JobKind::Verify | JobKind::Import) && !j.done);
        let busy = checking || g.install_job().is_some() || self.dl.queue.iter().any(|e| Gid(e.game_ref()) == id && e.state.is_active());
        blocked(g.running(), busy)
    }

    /// The dialog's controls in focus order (the primary one last).
    pub(super) const UNINSTALL_CONTROLS: [DCtl; 3] = [DCtl::Prefix, DCtl::Cancel, DCtl::Confirm];

    /// A control of the dialog was pressed (Cancel is handled by the caller).
    pub(super) fn uninstall_press(&mut self, c: DCtl, cx: &mut Context<Self>) {
        let Some(DlDialog::Uninstall(d)) = &mut self.dl.dialog else { return };
        match c {
            DCtl::Prefix => d.remove_prefix = !d.remove_prefix,
            DCtl::Confirm => {
                let (id, remove_prefix) = (d.id, d.remove_prefix);
                if let Some(why) = self.uninstall_blocked(id) {
                    return self.show_toast(why, cx);
                }
                self.dl.dialog = None;
                self.uninstall(id, remove_prefix, cx);
            }
            _ => {}
        }
    }

    fn uninstall(&mut self, id: Gid, remove_prefix: bool, cx: &mut Context<Self>) {
        let name = self.game_name(id);
        self.show_toast(format!("Uninstalling {name}…"), cx);
        self.call::<serde_json::Value, _>(Request::Uninstall { game: id.0.into(), remove_prefix }, cx, move |this, r, cx| {
            match r {
                Ok(v) => {
                    let r: UninstallResult = serde_json::from_value(v).unwrap_or_default();
                    let mut msg = format!("Uninstalled {name}");
                    if r.prefix_removed {
                        msg += " and its compatibility folder";
                    }
                    if !r.exports_removed.is_empty() {
                        msg += &format!("; removed {}", r.exports_removed.join(" and "));
                    }
                    this.show_toast(msg, cx);
                }
                Err(e) => this.show_toast(format!("Couldn't uninstall {name}: {}", err_text(&e)), cx),
            }
            this.load_library(false, cx);
            this.load_global_exports(cx);
            this.load_exports(cx);
            this.load_queue(cx);
            if this.nav.page == Page::Game(id) {
                // The page's install facts are the old ones.
                this.gp.info = Default::default();
                this.gp.store = Default::default();
                this.load_details(id, cx);
            }
        });
    }

    /// Title, subtitle, body, command and primary button of the dialog.
    pub(super) fn uninstall_view(
        &mut self,
        th: &Theme,
        d: &UninstallDialog,
        f: &dyn Fn(DCtl) -> bool,
        cx: &mut Context<Self>,
    ) -> (String, String, Vec<AnyElement>, String, (Icon, String, bool)) {
        let id = d.id;
        let name = self.game_name(id);
        let store = id.store();
        let (dir, prefix) = self
            .game(id)
            .and_then(|g| g.entry.installed.as_ref())
            .map(|g| (g.install_dir.clone(), g.prefix.clone()))
            .unwrap_or_default();
        let facts = match &d.facts {
            Some(Ok(f)) => Some(f.clone()),
            _ => None,
        };
        let size = match &d.facts {
            None => "measuring…".to_owned(),
            Some(Ok(UninstallFacts { size: Some(s), .. })) => gb(*s),
            Some(_) => "size unknown".to_owned(),
        };
        let blocked = self.uninstall_blocked(id);
        let mut body = vec![];
        if let Some(why) = blocked {
            body.push(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(th.warn_soft)
                    .text_size(px(12.5))
                    .text_color(th.warn)
                    .child(div().pt(px(1.)).child(icon(Icon::TriangleAlert, 14., th.warn)))
                    .child(div().flex_1().min_w_0().child(why))
                    .into_any_element(),
            );
        }
        let muted = |s: &str| div().text_size(px(12.)).text_color(th.tx3).child(s.to_owned());
        body.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(kv(th, "Install folder", div().flex().flex_col().gap(px(2.)).child(mono(12., th.tx1).child(tilde(&dir))).child(muted(folder_note(store)))))
                .child(kv(th, "Size", div().child(size)))
                .into_any_element(),
        );
        let on = d.remove_prefix;
        let has_prefix = !prefix.as_os_str().is_empty();
        if has_prefix {
            let loss = prefix_loss(store, facts.as_ref().and_then(|f| f.cloud_saves));
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(if on { th.bad_soft } else { th.ink1 })
                    .border_1()
                    .border_color(if on { th.bad } else { th.line1 })
                    .text_size(px(12.5))
                    .child(
                        div()
                            .id("dlg-prefix")
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .child(checkbox(th, on))
                            .child(div().font_weight(MEDIUM).child("Also delete its compatibility folder"))
                            .focus_ring(f(DCtl::Prefix), th, 6.)
                            .on_click(cx.listener(|this, _, _, cx| this.dl_dialog_press(DCtl::Prefix, cx))),
                    )
                    .child(mono(11.5, th.tx2).child(tilde(&prefix)))
                    .child(div().text_size(px(12.)).text_color(if on { th.bad } else { th.tx3 }).child(if on {
                        loss
                    } else {
                        "Kept: a reinstall picks up its settings and any saves in it. Tick this to free the space and start fresh.".to_owned()
                    }))
                    .into_any_element(),
            );
        }
        let (desktop, steam) = match &self.ui.exports {
            Some(Ok(l)) => exports_of(l, id),
            _ => self.gp.exports.ready().map(|l| exports_of(l, id)).unwrap_or_default(),
        };
        body.push(muted(exports_note(desktop, steam)).into_any_element());
        let cli = format!("quiver uninstall {id}{}", if on { " --prefix" } else { "" });
        let sub = format!("{} · {}", store.label(), if on { "with its compatibility folder" } else { "keeps its compatibility folder" });
        let label = if on { "Uninstall and delete" } else { "Uninstall" };
        (format!("Uninstall {name}?"), sub, body, cli, (Icon::Trash, label.to_owned(), blocked.is_none()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_while_running_or_downloading() {
        assert!(blocked(true, false).unwrap().contains("running"));
        assert!(blocked(false, true).unwrap().contains("queue"));
        assert_eq!(blocked(false, false), None);
    }

    #[test]
    fn says_what_the_prefix_loses() {
        assert!(prefix_loss(StoreId::Ubisoft, Some(false)).contains("exist only here"));
        assert!(prefix_loss(StoreId::Ubisoft, Some(true)).contains("other save stores"));
        assert!(prefix_loss(StoreId::Epic, Some(true)).contains("not synced yet"));
        assert!(prefix_loss(StoreId::Epic, None).contains("cloud doesn't keep"));
        assert!(folder_note(StoreId::Epic).contains("didn't ship"));
        assert!(folder_note(StoreId::Ubisoft).contains("everything in it"));
    }

    #[test]
    fn exports_that_go_with_it() {
        let rec = |game: quiver_api::GameRef, kind| ExportStatus {
            record: quiver_api::exports::ExportRecord {
                key: format!("quiver:{}", game.key()),
                kind,
                game,
                profile_id: None,
                name: "x".into(),
                path: "/x".into(),
                appid: None,
                steam_user: None,
                written_at: chrono::Utc::now(),
                hash: String::new(),
                staged: false,
            },
            state: quiver_api::exports::ExportState::Ok,
        };
        let epic = quiver_api::GameRef::new(StoreId::Epic, "Hornbill").unwrap();
        let list = [rec(epic, ExportKind::Desktop), rec(quiver_api::GameRef::ubisoft(1), ExportKind::Steam)];
        assert_eq!(exports_of(&list, Gid(epic)), (true, false));
        assert_eq!(exports_of(&list, Gid::from(1)), (false, true));
        assert!(exports_note(true, true).contains("Steam shortcut"));
    }
}
