//! The daemon's own lifecycle: its build, a staged update, what it waits on before
//! restarting onto it, and holds that keep it from restarting (`Request::Daemon`).
//!
//! quiverd restarts itself onto a newly installed build once nothing needs it (no game
//! session, no running job, no perf subscription or HUD, no request in flight, no hold).
//! While it restarts it refuses requests with [`RESTARTING`]; [`crate::Client`] retries
//! those, and requests the old daemon never read, on the new daemon.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `Reply::Err::code` of a request refused because the daemon is restarting onto a new
/// build. It did not run; sending it again shortly reaches the new daemon.
pub const RESTARTING: &str = "restarting";

/// The longest hold with a time limit (`for_secs`); renew it for longer.
pub const MAX_HOLD_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// [`DaemonStatus`].
    Status,
    /// Keep quiverd from restarting onto a new build. With `for_secs` (at most
    /// [`MAX_HOLD_SECS`]) the hold lasts that long whoever made it; without, it lasts as
    /// long as the connection that asked (it goes when that client exits or crashes).
    /// Returns the [`Hold`].
    Hold {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_secs: Option<u64>,
    },
    /// Release a hold by id; with `all`, every hold. Neither: the only hold there is
    /// (an error if there are several). Returns the released `Vec<Hold>`.
    Release {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
}

/// What `quiver daemon status` shows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// The crate version (`Status::version`).
    pub version: String,
    /// The installed build's full version (`0.1.0.r512.gabc1234`), when built by
    /// `tools/install-user.sh`; else the crate version.
    pub build: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub pid: u32,
    /// The binary this daemon restarts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// When this build started (after the last restart).
    pub started: DateTime<Utc>,
    /// Restarts onto new builds since the daemon was first started (same pid).
    #[serde(default)]
    pub restarts: u32,
    /// The build it restarted from, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted_from: Option<String>,
    /// Restarts onto new builds on their own (off with `QUIVER_AUTO_RESTART=0`).
    pub auto_restart: bool,
    /// How long nothing must need the daemon before it restarts.
    pub idle_secs: u64,
    /// A new build is installed where this one was started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<StagedUpdate>,
    /// Refusing new work: the restart is under way.
    #[serde(default)]
    pub restarting: bool,
    /// What keeps the daemon busy right now, in words ("AC Origins (bridge)", "1 perf
    /// subscription", "hold #2: measuring"). Empty: idle.
    #[serde(default)]
    pub waiting_on: Vec<String>,
    /// Seconds the daemon has been idle (none while busy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_for_secs: Option<u64>,
    #[serde(default)]
    pub holds: Vec<Hold>,
    /// The unit its cgroup names: `quiverd.service`, or a terminal's scope for one started
    /// outside the unit (`service`, design/SERVICE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// It runs as `quiverd.service`.
    #[serde(default)]
    pub in_unit: bool,
    /// Moving into the unit: "moving into quiverd.service once nothing needs it", or why
    /// that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relocation: Option<String>,
}

/// A new build found on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StagedUpdate {
    /// When the daemon noticed it.
    pub since: DateTime<Utc>,
    /// Its version and commit from the install manifest
    /// (`~/.local/state/quiver/installed.json`), when it lists this binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Why it isn't restarting onto it, if not just "busy" (e.g. the last attempt failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub id: u64,
    pub reason: String,
    /// The holder's `source` label.
    pub source: String,
    pub since: DateTime<Utc>,
    /// When a timed hold runs out; none: it lasts as long as its connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
}

impl Hold {
    /// "hold #2: measuring (agent, 1h 12m left)".
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let left = match self.expires {
            Some(t) => format!("{} left", human_secs((t - now).num_seconds().max(0) as u64)),
            None => "while its client runs".into(),
        };
        format!("hold #{}: {} ({}, {left})", self.id, self.reason, self.source)
    }
}

/// `3725` -> "1h 2m", `75` -> "1m 15s", `600` -> "10m", `9` -> "9s".
pub fn human_secs(s: u64) -> String {
    let (h, m, sec) = (s / 3600, s / 60 % 60, s % 60);
    match (h, m, sec) {
        (0, 0, _) => format!("{sec}s"),
        (0, _, 0) => format!("{m}m"),
        (0, _, _) => format!("{m}m {sec}s"),
        (_, 0, _) => format!("{h}h"),
        _ => format!("{h}h {m}m"),
    }
}

/// A duration as people write it: `90`, `90s`, `30m`, `2h`, `1h30m`, `1d`.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (mut total, mut num) = (0u64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            ' ' if num.is_empty() => continue,
            _ => return Err(format!("bad duration '{s}' (try 90s, 30m, 2h or 1h30m)")),
        };
        let n: u64 = num.parse().map_err(|_| format!("bad duration '{s}' (a number goes before each unit)"))?;
        total = total.saturating_add(n.saturating_mul(unit));
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("bad duration '{s}' (a unit goes after each number: s, m, h, d)"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Ok(90));
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("1h30m"), Ok(5400));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("1d"), Ok(86400));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("1h30").is_err());
        assert_eq!(human_secs(9), "9s");
        assert_eq!(human_secs(75), "1m 15s");
        assert_eq!(human_secs(3725), "1h 2m");
        assert_eq!(human_secs(600), "10m");
        assert_eq!(human_secs(7200), "2h");
    }

    #[test]
    fn wire() {
        let r: crate::Request = serde_json::from_str(r#"{"method":"daemon","params":{"op":"hold","reason":"x","for_secs":60}}"#).unwrap();
        assert!(matches!(r, crate::Request::Daemon(DaemonRequest::Hold { for_secs: Some(60), .. })));
        let r: crate::Request = serde_json::from_str(r#"{"method":"daemon","params":{"op":"release"}}"#).unwrap();
        assert!(matches!(r, crate::Request::Daemon(DaemonRequest::Release { id: None, all: false })));
        // An error reply from an older daemon has no code; a closing notice parses.
        let e: crate::Reply = serde_json::from_str(r#"{"err":{"id":1,"error":"x"}}"#).unwrap();
        assert!(matches!(e, crate::Reply::Err { code: None, .. }));
        let c: crate::Reply = serde_json::from_str(r#"{"closing":{"reason":"restarting"}}"#).unwrap();
        assert!(matches!(c, crate::Reply::Closing { .. }));
    }
}
