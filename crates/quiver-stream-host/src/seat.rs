//! Is a person at this machine? Decides whether a stream's pads may be ordinary devices.
//!
//! A uinput pad is global to the kernel: on a machine where someone is seated, an ordinary
//! one would reach their desktop (the compositor, Steam, their own games, hyprpad). So:
//!
//! - **Plain pads** (ordinary uinput devices) only when the host was told it is a headless
//!   server (`--headless-server`, or quiverd's config) **and** logind shows no session on any
//!   seat: not a graphical one, not a text console. Both, so neither a wrong flag on a desktop
//!   nor a server with someone at its console gets them.
//! - Otherwise **hidden pads**: the stream pad rule (`contrib/stream/72-quiver-stream-pad.rules`)
//!   hides them from every host process, and only the stream's own program, run with that
//!   group in a user namespace ([`crate::sandbox`]), can open them.
//! - Without the rule, **no pads**; the reason is reported to the client.

use std::path::Path;

/// A logind session that puts a person at the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSession {
    pub id: String,
    pub seat: String,
    pub kind: String,
    pub user: String,
}

/// Parse one `/run/systemd/sessions/<id>` file (KEY=value lines).
pub fn parse_session(id: &str, text: &str) -> Option<SeatSession> {
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('='))).unwrap_or("").to_owned();
    let seat = get("SEAT");
    let kind = get("TYPE");
    let class = get("CLASS");
    let remote = get("REMOTE") == "1";
    // A session on a seat, or a local graphical one, is a person here. Greeters count: a
    // login screen is a seat a person can sit at.
    let graphical = matches!(kind.as_str(), "x11" | "wayland" | "mir");
    if remote || (seat.is_empty() && !graphical) || class == "background" || class == "manager" {
        return None;
    }
    Some(SeatSession { id: id.to_owned(), seat, kind, user: get("USER") })
}

/// The sessions under `dir` (`/run/systemd/sessions`) that put a person at the machine.
pub fn seated_sessions(dir: &Path) -> Vec<SeatSession> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if id.ends_with(".ref") {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(e.path())
            && let Some(s) = parse_session(&id, &t)
        {
            out.push(s);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// How a stream's pads are made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadMode {
    /// Ordinary uinput pads: an opted-in headless server with nobody seated.
    Plain,
    /// Hidden by the stream pad rule; only the sandboxed program (with group `gid`) opens them.
    Hidden { gid: u32 },
    /// No pads, and why (for the client's notice).
    Off(String),
}

/// Decide, from the opt-in, who is seated, and whether the stream pad rule is usable
/// (`Ok(gid)` or why not).
pub fn decide(headless_server: bool, seated: &[SeatSession], rule: Result<u32, String>) -> PadMode {
    if headless_server && seated.is_empty() {
        return PadMode::Plain;
    }
    match rule {
        Ok(gid) => PadMode::Hidden { gid },
        Err(why) => {
            let who = seated.first().map(|s| format!("{} is signed in on {} ({})", s.user, if s.seat.is_empty() { "this machine" } else { &s.seat }, s.kind));
            let lead = match (headless_server, who) {
                (true, Some(w)) => format!("--headless-server was given, but {w}, so pads must be hidden from the desktop"),
                (_, Some(w)) => format!("{w}, so pads must be hidden from the desktop"),
                (_, None) => "pads must be hidden from the desktop unless this is a headless server (--headless-server)".to_owned(),
            };
            PadMode::Off(format!("{lead}, and {why}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAYLAND: &str = "UID=1000\nUSER=ajg\nACTIVE=1\nSTATE=active\nREMOTE=0\nTYPE=wayland\nCLASS=user\nSEAT=seat0\nVTNR=1\n";
    const SSH: &str = "UID=1000\nUSER=ajg\nACTIVE=0\nSTATE=online\nREMOTE=1\nTYPE=tty\nCLASS=user\nSERVICE=sshd\n";
    const CONSOLE: &str = "UID=0\nUSER=root\nACTIVE=1\nREMOTE=0\nTYPE=tty\nCLASS=user\nSEAT=seat0\nVTNR=1\n";
    const MANAGER: &str = "UID=1000\nUSER=ajg\nTYPE=unspecified\nCLASS=manager\n";

    #[test]
    fn who_is_seated() {
        assert_eq!(parse_session("2", WAYLAND).map(|s| s.kind), Some("wayland".into()));
        assert_eq!(parse_session("5", SSH), None);
        assert!(parse_session("c1", CONSOLE).is_some(), "a text console on a seat is a person");
        assert_eq!(parse_session("3", MANAGER), None);
    }

    #[test]
    fn plain_pads_need_both_the_flag_and_an_empty_seat() {
        let seated = vec![parse_session("2", WAYLAND).unwrap()];
        assert_eq!(decide(true, &[], Err("no rule".into())), PadMode::Plain);
        assert_eq!(decide(true, &seated, Ok(105001)), PadMode::Hidden { gid: 105001 });
        assert_eq!(decide(false, &[], Ok(105001)), PadMode::Hidden { gid: 105001 });
        match decide(true, &seated, Err("the rule isn't installed".into())) {
            PadMode::Off(why) => assert!(why.contains("ajg is signed in on seat0") && why.contains("the rule isn't installed"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(decide(false, &[], Err("x".into())), PadMode::Off(_)));
    }

    #[test]
    fn sessions_from_a_folder() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("2"), WAYLAND).unwrap();
        std::fs::write(t.path().join("5"), SSH).unwrap();
        std::fs::write(t.path().join("2.ref"), "").unwrap();
        let s = seated_sessions(t.path());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "2");
        assert!(seated_sessions(Path::new("/nonexistent")).is_empty());
    }
}
