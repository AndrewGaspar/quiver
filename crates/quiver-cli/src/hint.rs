//! The "please wrap up" hint on every `quiver` command (design/LAB.md §19,
//! `quiver_core::lab::hint`): one line on stderr, never stdout, when a staged quiverd update
//! is waiting on a lab instance of the caller's. Quick (quiverd's answer is cached for a few
//! seconds), never starts quiverd, and silent on any failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use quiver_api::daemon::{DaemonRequest, DaemonStatus, RestartWaiting};
use quiver_api::{Client, Request};
use quiver_core::lab::hint::{self, Cached, State};
use quiver_core::lab::owner::Owner;

/// What a command gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A `quiver lab …` command: its own instances' hint, and the neutral line about
    /// instances whose owner is gone or can't be told.
    Lab,
    /// Any other command: only its own instances' hint.
    Other,
    /// It frees an instance (`lab kill`, `down`, `rm`): no hint, and the cached answer goes.
    Freeing,
    /// No hint (`quiver daemon …` shows it all itself; internal processes; a status bar).
    None,
}

/// quiverd's answer, at most this long.
const ASK_WITHIN: Duration = Duration::from_millis(600);

fn state_path() -> PathBuf {
    let sock = quiver_api::socket_path();
    sock.parent().map(Path::to_path_buf).unwrap_or_default().join("hints.json")
}

pub(crate) fn disabled() -> bool {
    std::env::var(hint::NO_HINTS).is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"))
}

/// quiverd's restart status, if it's running and answers in time (never starts it).
pub(crate) async fn daemon_status(source: &str) -> Option<DaemonStatus> {
    let ask = async {
        let c = Client::connect_running(source).await.ok()?;
        c.call::<DaemonStatus>(Request::Daemon(DaemonRequest::Status)).await.ok()
    };
    tokio::time::timeout(ASK_WITHIN, ask).await.ok().flatten()
}

/// What a staged update waits on, fresh from quiverd (`quiver lab status`).
pub(crate) async fn restart_waiting(source: &str) -> Option<RestartWaiting> {
    daemon_status(source).await?.restart_waiting
}

/// Print the hint for this command, if one is due.
pub(crate) async fn maybe(source: &str, kind: Kind) {
    if kind == Kind::None || disabled() {
        return;
    }
    let path = state_path();
    let mut st = State::read(&path);
    if kind == Kind::Freeing {
        if st.cached.take().is_some() {
            let _ = st.write(&path);
        }
        return;
    }
    let now = Utc::now();
    let cached = match st.fresh(now) {
        Some(c) => c.clone(),
        None => {
            let s = daemon_status(source).await;
            let c = Cached { at: now, holds: s.as_ref().map(|s| s.holds.clone()).unwrap_or_default(), waiting: s.and_then(|s| s.restart_waiting) };
            st.cached = Some(c.clone());
            let _ = st.write(&path);
            c
        }
    };
    let Some(w) = State::waiting_now(&cached, now).filter(|w| !w.lab.is_empty()) else { return };
    let caller = Owner::current(source, None);
    let key = caller.key();
    if !st.due(&key, now) {
        return;
    }
    let Some(h) = hint::hint(&w, &cached.holds, &caller, kind == Kind::Lab, Path::new("/proc"), now) else { return };
    eprintln!("{}", h.text);
    st.shown_now(&key, now);
    let _ = st.write(&path);
    if !h.mine.is_empty() {
        let tell = async {
            let c = Client::connect_running(source).await.ok()?;
            c.call::<serde_json::Value>(Request::Daemon(DaemonRequest::Hinted { instances: h.mine.clone(), owner: caller.name() })).await.ok()
        };
        let _ = tokio::time::timeout(ASK_WITHIN, tell).await;
    }
}
