//! The Sight panel's control plane (design/SIGHT.md §14): `quiver sight toggle|open|close`,
//! Shift+F2 read in the game by the Sight layer, and `quiver-sight`'s reports.
//!
//! quiverd draws nothing and keeps no idea of its own whether the panel is open: a toggle is
//! passed on as a toggle and `quiver-sight`, which knows, decides. quiverd's part is to pick
//! the session (the one named, else the focused game, else the only one running), make sure
//! `quiver-sight` runs, broadcast the command ([`PerfEvent::Panel`]) and keep it as the last
//! command, so a `quiver-sight` started for it can still act on it
//! ([`PanelCommand::fresh`]). The report `quiver-sight` sends back is what
//! `quiver sight status` shows.

use anyhow::Result;
use chrono::Utc;
use quiver_api::perf::PerfEvent;
use quiver_api::sight::{PanelAction, PanelCommand, PanelKeys, PanelReport, PanelSection, SightStatus};

use super::PerfHub;

#[derive(Default)]
pub struct PanelState {
    seq: u64,
    last: Option<PanelCommand>,
    report: Option<(PanelReport, chrono::DateTime<Utc>)>,
    /// Shift+F2 presses seen per session (for status).
    presses: std::collections::BTreeMap<String, u32>,
}

impl PerfHub {
    fn panel_lock(&self) -> std::sync::MutexGuard<'_, PanelState> {
        self.panel.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Passes a panel command on to `quiver-sight` (starting it when it opens something).
    pub fn panel_command(&self, action: PanelAction, session: Option<&str>, section: Option<PanelSection>, by: &str) -> Result<PanelCommand> {
        let (session, monitor) = if action == PanelAction::Close {
            (None, None)
        } else {
            let st = self.lock();
            let id = Self::pick(&st, session, None)?.to_owned();
            let monitor = st.sessions.get(&id).and_then(|s| s.monitor.clone());
            (Some(id), monitor)
        };
        let cmd = {
            let mut p = self.panel_lock();
            p.seq += 1;
            let c = PanelCommand { seq: p.seq, action, session, section, monitor, by: by.to_owned(), at: Utc::now() };
            p.last = Some(c.clone());
            c
        };
        if action != PanelAction::Close {
            self.sight.ensure(self.data_dir(), "Sight panel");
        }
        tracing::info!(seq = cmd.seq, ?action, session = ?cmd.session, section = ?section, by, "Sight panel command");
        self.emit(PerfEvent::Panel(cmd.clone()));
        Ok(cmd)
    }

    /// Shift+F2 presses a session's layer read in the game (`drain_once`): one toggle per
    /// drain whatever the count (two presses within 50 ms are a bounce, not open-then-close).
    pub fn panel_keys(&self, session: &str, presses: u32) {
        if presses == 0 {
            return;
        }
        *self.panel_lock().presses.entry(session.to_owned()).or_default() += presses;
        match self.panel_command(PanelAction::Toggle, Some(session), None, "keys") {
            Ok(_) => tracing::info!(session, "panel key: toggle"),
            Err(e) => tracing::info!(session, "panel key ignored: {e:#}"),
        }
    }

    pub fn panel_report(&self, report: PanelReport) {
        let mut p = self.panel_lock();
        if p.report.as_ref().is_none_or(|(r, _)| r.open != report.open || r.backdrop != report.backdrop) {
            tracing::info!(open = report.open, section = ?report.section, backdrop = report.backdrop.as_str(), detail = %report.backdrop_detail, "Sight panel reported");
        }
        p.report = Some((report, Utc::now()));
    }

    pub fn panel_status(&self) -> SightStatus {
        let keys: Vec<PanelKeys> = {
            let st = self.lock();
            let p = self.panel_lock();
            st.sessions
                .values()
                .filter_map(|s| {
                    let l = s.key_layer()?;
                    let (keys, _) = l.status.as_ref().map_or(("", ""), super::keybinds::layer_keys);
                    Some(PanelKeys { session: s.id.clone(), keys: keys.into(), presses: p.presses.get(&s.id).copied().unwrap_or(0) })
                })
                .collect()
        };
        let p = self.panel_lock();
        SightStatus {
            running: self.sight.running(),
            panel: p.report.as_ref().map(|(r, _)| r.clone()),
            reported_at: p.report.as_ref().map(|(_, t)| *t),
            last_command: p.last.clone(),
            keys,
        }
    }

    /// A session ended: an open panel for it closes (`quiver-sight` also sees the session
    /// end; this keeps the status honest if it doesn't).
    pub fn panel_session_ended(&self, session: &str) {
        let mut p = self.panel_lock();
        p.presses.remove(session);
        if let Some((r, _)) = &mut p.report
            && r.session.as_deref() == Some(session)
        {
            r.open = false;
        }
    }
}
