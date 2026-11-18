//! Saves & progress for an Epic game (design/STORE-EPIC.md §9.5): Epic's play time and the
//! sessions Quiver reports, the cloud saves here and in Epic's cloud side by side (with
//! the conflict view and its two choices), backups with restore, and the achievements.
//!
//! Writes to the person's Epic account (the first upload, the first play-time report) and
//! anything that replaces files (keeping one side of a conflict, a restore) ask by
//! relabelling their button first: the second press does it.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::epic::EpicRequest;
use quiver_api::epicgame::{EpicBackup, EpicGameRequest, EpicPlaytime, EpicSaves, FileState, Resolve, SaveMode, SessionReport, SyncReport, SyncState};

use super::page::{Load, kv, load_note, note, small};
use crate::app::Quiver;
use crate::err_text;
use crate::model::{GameRow, Gid, clock, day, hours};
use crate::theme::Theme;
use crate::ui::{Icon, MEDIUM, PillKind, card, card_header, icon, mono, pill};

fn game_req(r: EpicGameRequest) -> Request {
    Request::Epic(EpicRequest::Game(r))
}

fn when(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|t| format!("{} {}", day(t), clock(t))).unwrap_or_else(|| "—".into())
}

fn size(b: u64) -> String {
    if b >= 1_048_576 { format!("{:.1} MB", b as f64 / 1_048_576.) } else { format!("{} KB", b.div_ceil(1024)) }
}

/// What a button asks before it acts (its second press does it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpicConfirm {
    FirstUpload,
    Keep(Resolve),
    Restore(String),
    FirstReport,
}

fn file_pill(th: &Theme, s: FileState) -> AnyElement {
    let (k, label) = match s {
        FileState::InSync => (PillKind::Ok, "In sync"),
        FileState::LocalNewer => (PillKind::Warn, "Newer on this PC"),
        FileState::CloudNewer => (PillKind::Accent, "Newer in the cloud"),
        FileState::Conflict => (PillKind::Bad, "Changed on both"),
        FileState::DeletedHere => (PillKind::Muted, "Deleted here"),
        FileState::Unknown => (PillKind::Muted, "Not compared"),
    };
    pill(th, k, None, label).into_any_element()
}

impl Quiver {
    pub fn load_epic_saves(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.gp.epic_saves = Load::Loading;
        self.call::<EpicSaves, _>(game_req(EpicGameRequest::Saves { game: id.0, cloud: true }), cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.epic_saves = match r {
                    Ok(s) => Load::Ready(s),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
        self.gp.epic_pt = Load::Loading;
        self.call::<EpicPlaytime, _>(game_req(EpicGameRequest::Playtime { game: Some(id.0), refresh: false }), cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.epic_pt = match r {
                    Ok(p) => Load::Ready(p),
                    Err(e) => Load::Failed(err_text(&e)),
                };
            }
        });
    }

    fn epic_sync(&mut self, id: Gid, resolve: Option<Resolve>, confirmed: bool, cx: &mut Context<Self>) {
        self.gp.epic_confirm = None;
        self.gp.busy = true;
        self.call::<SyncReport, _>(game_req(EpicGameRequest::SavesSync { game: id.0, resolve, confirmed }), cx, move |this, r, cx| {
            this.gp.busy = false;
            match r {
                Ok(r) => this.show_toast(format!("Saves: {}", r.summary()), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            }
            this.load_epic_saves(id, cx);
            this.load_store_detail(id, true, cx);
        });
    }

    fn epic_restore(&mut self, id: Gid, backup: String, cx: &mut Context<Self>) {
        self.gp.epic_confirm = None;
        self.call::<serde_json::Value, _>(game_req(EpicGameRequest::SavesRestore { game: id.0, backup, confirmed: true }), cx, move |this, r, cx| {
            match r {
                Ok(_) => this.show_toast("Backup restored: it uploads with the next sync", cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            }
            this.load_epic_saves(id, cx);
        });
    }

    fn epic_send(&mut self, id: Gid, cx: &mut Context<Self>) {
        self.gp.epic_confirm = None;
        self.call::<EpicPlaytime, _>(game_req(EpicGameRequest::PlaytimeSend { confirmed: true }), cx, move |this, r, cx| {
            match r {
                Ok(p) => this.show_toast(format!("{} play session(s) sent to Epic", p.queue.len()), cx),
                Err(e) => this.show_toast(err_text(&e), cx),
            }
            this.load_epic_saves(id, cx);
            this.load_store_detail(id, true, cx);
        });
    }

    /// A button that asks first: pressed once it says `ask`; pressed again it runs `run`.
    #[allow(clippy::too_many_arguments)]
    fn epic_ask_button(
        &self,
        idx: (&'static str, usize),
        th: &Theme,
        ic: Icon,
        label: &str,
        ask: &str,
        what: EpicConfirm,
        run: impl Fn(&mut Quiver, &mut Context<Quiver>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed = self.gp.epic_confirm.as_ref() == Some(&what);
        small(idx, th, Some(ic), if armed { ask.to_owned() } else { label.to_owned() })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.gp.epic_confirm.as_ref() == Some(&what) {
                    run(this, cx);
                } else {
                    this.gp.epic_confirm = Some(what.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    pub(super) fn render_epic_saves(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if self.gp.id == id && self.gp.epic_saves.is_idle() {
            self.load_epic_saves(id, cx);
        }
        if self.gp.store.is_idle() {
            self.load_store_detail(id, false, cx);
        }
        let saves = self.gp.epic_saves.ready().cloned();
        let top = div().flex().flex_wrap().gap(px(16.)).child(self.epic_playtime_card(th, id, cx).flex_1().min_w(px(280.))).child(self.epic_cloud_card(th, id, saves.as_ref(), cx).flex_1().min_w(px(320.)));
        let mut col = div().flex().flex_col().gap(px(18.)).child(top);
        if let Some(s) = &saves
            && s.mode == SaveMode::Launcher
        {
            col = col.child(self.epic_files_card(th, s));
            col = col.child(self.epic_backups_card(th, id, &s.backups, cx));
        }
        if let Some(d) = self.gp.store.ready().filter(|_| self.gp.id == id).cloned() {
            col = col.child(self.render_store_achievements(th, &d));
        }
        col.into_any_element()
    }

    fn epic_playtime_card(&self, th: &Theme, id: Gid, cx: &mut Context<Self>) -> gpui::Div {
        let total = self.gp.store.ready().filter(|_| self.gp.id == id).and_then(|d| d.playtime).map(|p| p.total_seconds);
        let mut c = card(th).child(card_header(th, "Play time"));
        let mut body = div().flex().flex_col().gap(px(8.)).p(px(16.));
        body = body.child(div().text_size(px(26.)).font_weight(MEDIUM).child(total.map(hours).unwrap_or_else(|| "—".into()))).child(div().text_size(px(11.5)).text_color(th.tx3).child("Epic's own total, as its launcher shows it"));
        match &self.gp.epic_pt {
            Load::Ready(p) => {
                let state = match (p.reporting, p.reports.open()) {
                    (false, _) => "Not reported (play-time reporting is off)",
                    (true, false) => "Reported once you say yes to the first report",
                    (true, true) => "Each session is reported to Epic when it ends",
                };
                body = body.child(kv(th, "Sessions", div().child(state)));
                let waiting = p.queue.iter().filter(|q| matches!(q.state, SessionReport::Held | SessionReport::Queued)).count();
                for q in p.queue.iter().take(4) {
                    let s = match q.state {
                        SessionReport::Reported => "reported",
                        SessionReport::Held => "waiting for your yes",
                        SessionReport::Queued => "queued",
                        SessionReport::Off => "kept here",
                        SessionReport::Failed => "refused by Epic",
                        SessionReport::Unconfirmed => "sent, no answer (not resent)",
                        SessionReport::Unknown => "?",
                    };
                    body = body.child(div().text_size(px(12.)).text_color(th.tx2).child(format!("{} · {} min · {s}", when(Some(q.started)), q.seconds / 60)));
                }
                if p.reporting && waiting > 0 {
                    let first = !p.reports.open();
                    let b = if first {
                        self.epic_ask_button(("epic-pt-send", 0), th, Icon::CloudUpload, "Report to Epic", "Report to your Epic account?", EpicConfirm::FirstReport, move |this, cx| this.epic_send(id, cx), cx)
                    } else {
                        small(("epic-pt-send", 1usize), th, Some(Icon::CloudUpload), "Send now").on_click(cx.listener(move |this, _, _, cx| this.epic_send(id, cx))).into_any_element()
                    };
                    body = body.child(div().flex().child(b));
                }
            }
            l => body = body.children(load_note(th, l, "play sessions")),
        }
        c = c.child(body);
        c
    }

    fn epic_cloud_card(&self, th: &Theme, id: Gid, s: Option<&EpicSaves>, cx: &mut Context<Self>) -> gpui::Div {
        let Some(s) = s else { return card(th).child(card_header(th, "Cloud saves")).children(load_note(th, &self.gp.epic_saves, "saves")) };
        if s.mode == SaveMode::InGame {
            return card(th)
                .child(card_header(th, "Cloud saves").child(div().flex_1()).child(pill(th, PillKind::Muted, None, "Saves sync in game")))
                .child(note(th, "Epic's catalog names no save folder for the launcher, so the game keeps its own saves: if it has cloud saves, it syncs them itself through Epic Online Services while it runs. Quiver has nothing to sync."));
        }
        let (k, label) = match s.state {
            SyncState::InSync => (PillKind::Ok, "In sync"),
            SyncState::LocalNewer => (PillKind::Warn, "Newer on this PC"),
            SyncState::CloudNewer => (PillKind::Accent, "Newer in the cloud"),
            SyncState::Both => (PillKind::Accent, "Changes on both sides"),
            SyncState::Conflict => (PillKind::Bad, "Conflict"),
            SyncState::Empty => (PillKind::Muted, "No saves yet"),
            SyncState::Unknown => (PillKind::Muted, "Not compared"),
        };
        let mut body = div().flex().flex_col().gap(px(8.)).p(px(16.));
        body = body.child(kv(th, "Account", div().child(s.account.clone().unwrap_or_default())));
        body = body.child(kv(th, "Folder", mono(12., th.tx2).truncate().child(s.folder.clone().unwrap_or_default())));
        body = body.child(kv(
            th,
            "On this PC",
            match (&s.dir, &s.dir_note) {
                (Some(d), _) => mono(12., th.tx2).truncate().child(crate::model::tilde(d)),
                (None, n) => div().text_color(th.tx3).child(n.clone().unwrap_or_default()),
            },
        ));
        body = body.child(kv(
            th,
            "In the cloud",
            div().child(match (&s.cloud, &s.cloud_error) {
                (Some(c), _) => format!("{} · {} file(s) · {} versions kept", when(c.at), c.files, s.cloud_versions),
                (None, Some(e)) => format!("unreadable: {e}"),
                (None, None) => "nothing uploaded yet".into(),
            }),
        ));
        if let Some(l) = &s.last_sync {
            body = body.child(kv(th, "Last sync", div().child(format!("{} ({})", when(Some(l.at)), l.by))));
        }
        if let Some(n) = &s.next {
            body = body.child(div().text_size(px(12.)).text_color(if s.state == SyncState::Conflict { th.bad } else { th.tx3 }).child(n.clone()));
        }
        let mut buttons = div().flex().flex_wrap().gap(px(8.)).pt(px(4.));
        let busy = self.gp.busy;
        match s.state {
            SyncState::Conflict if !busy => {
                buttons = buttons
                    .child(self.epic_ask_button(("epic-keep", 0), th, Icon::CloudUpload, "Keep this PC's", "Upload this PC's over the cloud's?", EpicConfirm::Keep(Resolve::KeepLocal), move |this, cx| this.epic_sync(id, Some(Resolve::KeepLocal), true, cx), cx))
                    .child(self.epic_ask_button(("epic-keep", 1), th, Icon::CloudDownload, "Keep the cloud's", "Replace this PC's (backed up first)?", EpicConfirm::Keep(Resolve::KeepCloud), move |this, cx| this.epic_sync(id, Some(Resolve::KeepCloud), true, cx), cx));
            }
            SyncState::LocalNewer | SyncState::Both if !busy && !s.uploads.open() => {
                buttons = buttons.child(self.epic_ask_button(("epic-sync", 0), th, Icon::CloudUpload, "Upload to Epic", "Upload to your Epic account?", EpicConfirm::FirstUpload, move |this, cx| this.epic_sync(id, None, true, cx), cx));
            }
            _ if !busy && s.dir.is_some() => {
                buttons = buttons.child(small(("epic-sync", 1usize), th, Some(Icon::RefreshCw), "Sync now").on_click(cx.listener(move |this, _, _, cx| this.epic_sync(id, None, false, cx))));
            }
            _ => {}
        }
        body = body.child(buttons);
        card(th).child(card_header(th, "Epic cloud saves").child(div().flex_1()).child(pill(th, k, None, label))).child(body)
    }

    fn epic_files_card(&self, th: &Theme, s: &EpicSaves) -> gpui::Div {
        let side = |x: &Option<quiver_api::epicgame::SaveSide>| x.as_ref().map(|x| format!("{} · {}", when(x.modified), size(x.size))).unwrap_or_else(|| "—".into());
        let conflicts = s.files.iter().filter(|f| f.state == FileState::Conflict).count();
        let rows = s.files.iter().enumerate().map(|(i, f)| {
            div()
                .id(("epic-file", i))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(8.))
                .py(px(6.))
                .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                .when(f.state == FileState::Conflict, |d| d.bg(th.bad_soft))
                .child(div().flex_1().min_w_0().child(mono(12., th.tx1).truncate().child(f.path.clone())))
                .child(div().w(px(170.)).flex_none().text_size(px(11.5)).text_color(th.tx2).child(side(&f.local)))
                .child(div().w(px(170.)).flex_none().text_size(px(11.5)).text_color(th.tx2).child(side(&f.cloud)))
                .child(div().w(px(150.)).flex_none().flex().justify_end().child(file_pill(th, f.state)))
        });
        let header = div()
            .flex()
            .gap(px(12.))
            .px(px(16.))
            .pt(px(10.))
            .text_size(px(11.)).text_color(th.tx3)
            .child(div().flex_1().child("File"))
            .child(div().w(px(170.)).child("This PC"))
            .child(div().w(px(170.)).child("Cloud (newest version)"))
            .child(div().w(px(150.)));
        card(th)
            .child(card_header(th, "Save files").child(div().flex_1()).when(conflicts > 0, |d| d.child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(th.bad).child(icon(Icon::TriangleAlert, 13., th.bad)).child(format!("{conflicts} changed on both sides")))))
            .when(s.files.is_empty(), |d| d.child(note(th, "No save files here or in the cloud yet.")))
            .when(!s.files.is_empty(), |d| d.child(header).child(div().flex().flex_col().p(px(8.)).children(rows)))
    }

    fn epic_backups_card(&self, th: &Theme, id: Gid, list: &[EpicBackup], cx: &mut Context<Self>) -> gpui::Div {
        let rows: Vec<AnyElement> = list
            .iter()
            .take(10)
            .enumerate()
            .map(|(i, b)| {
                let bid = b.id.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(8.))
                    .py(px(7.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(div().flex_1().min_w_0().flex().flex_col().child(div().font_weight(MEDIUM).text_size(px(12.5)).child(when(b.created))).child(div().text_size(px(11.5)).text_color(th.tx3).child(format!("{} · {} file(s) · {}", b.reason.clone().unwrap_or_else(|| "backup".into()), b.files, size(b.bytes)))))
                    .child(self.epic_ask_button(("epic-restore", i), th, Icon::RotateCcw, "Restore", "Restore (current backed up)?", EpicConfirm::Restore(bid.clone()), move |this, cx| this.epic_restore(id, bid.clone(), cx), cx))
                    .into_any_element()
            })
            .collect();
        card(th)
            .child(card_header(th, "Backups"))
            .when(rows.is_empty(), |d| d.child(note(th, "None yet. Quiver copies the files a cloud download or a restore would replace before it replaces them.")))
            .child(div().flex().flex_col().p(px(8.)).children(rows))
    }
}
