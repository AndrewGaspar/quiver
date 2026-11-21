//! Readiness: what an instance's game has got to, as the supervisor observes it and writes
//! it to `<instance>/state.json`. Agents wait on these (`quiver lab wait <inst> window`),
//! never on fixed sleeps.
//!
//! The ladder Quiver observes itself is `process` (the game's exe runs in the instance),
//! `window` (gamescope has a window to show), `frame` (the screen isn't blank). Above that
//! a mod reports what only the game knows (`menu`, `loading`, `in-world`, …) by writing
//! the first line of `%QUIVER_LAB%\state` (design/LAB.md "The lab channel").

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The rungs Quiver itself observes, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Launched; no game process yet.
    Starting,
    Process,
    Window,
    Frame,
    /// The game exited (or was killed).
    Exited,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Process => "process",
            Phase::Window => "window",
            Phase::Frame => "frame",
            Phase::Exited => "exited",
        }
    }
    pub fn parse(s: &str) -> Option<Phase> {
        Some(match s {
            "starting" => Phase::Starting,
            "process" => Phase::Process,
            "window" => Phase::Window,
            "frame" => Phase::Frame,
            "exited" => Phase::Exited,
            _ => return None,
        })
    }
}

/// What a mod reported through the lab channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModState {
    /// The first word of the state file's first line (`in-world`).
    pub state: String,
    /// `key=value` lines after it (`location=alexandria`, `fps=144`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, String>,
    pub at: DateTime<Utc>,
}

/// Parse the lab channel's state file. `None` for an empty file.
pub fn parse_mod_state(text: &str, at: DateTime<Utc>) -> Option<ModState> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next()?;
    let state: String = first.split_whitespace().next()?.chars().take(48).collect();
    if !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') {
        return None;
    }
    let detail = lines
        .filter_map(|l| l.split_once('='))
        .take(64)
        .map(|(k, v)| (k.trim().chars().take(64).collect(), v.trim().chars().take(512).collect()))
        .collect();
    Some(ModState { state, detail, at })
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exit {
    pub at: DateTime<Utc>,
    /// `killed` (by the supervisor), `exited` (on its own), `violation` (Connect appeared).
    pub how: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One launch of the game in an instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// `r<id>` names its folder under `lab/runs/` and is in every log line about it.
    pub id: u32,
    /// Its session id (`lab-<instance>-r<id>`).
    pub session: String,
    pub started: DateTime<Utc>,
    pub phase: Phase,
    /// When each phase was first reached.
    #[serde(default)]
    pub reached: BTreeMap<Phase, DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_state: Option<ModState>,
    /// Mod states seen, each with when it was first reached.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mod_reached: BTreeMap<String, DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    /// What the launch did (mod files, profile, licence), for the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Who launched it (design/LAB.md "Owners"); also in the run's folder (`owner.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<super::owner::Owner>,
}

impl Run {
    pub fn new(id: u32, session: String, now: DateTime<Utc>) -> Self {
        Run {
            id,
            session,
            started: now,
            phase: Phase::Starting,
            reached: BTreeMap::from([(Phase::Starting, now)]),
            mod_state: None,
            mod_reached: BTreeMap::new(),
            pids: Vec::new(),
            exit: None,
            notes: Vec::new(),
            owner: None,
        }
    }

    /// Move up to `phase` (never down, except to `Exited`). True if it changed.
    pub fn reach(&mut self, phase: Phase, now: DateTime<Utc>) -> bool {
        if self.phase == Phase::Exited || (phase <= self.phase && phase != Phase::Exited) || phase == self.phase {
            return false;
        }
        // A window implies a process (and a frame a window), even if a poll missed it.
        for p in [Phase::Process, Phase::Window, Phase::Frame] {
            if p <= phase && phase != Phase::Exited {
                self.reached.entry(p).or_insert(now);
            }
        }
        self.reached.entry(phase).or_insert(now);
        self.phase = phase;
        true
    }

    pub fn set_mod_state(&mut self, s: ModState) -> bool {
        if self.mod_state.as_ref().is_some_and(|m| m.state == s.state && m.detail == s.detail) {
            return false;
        }
        self.mod_reached.entry(s.state.clone()).or_insert(s.at);
        self.mod_state = Some(s);
        true
    }
}

/// The supervisor's view of an instance (`state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub instance: String,
    /// Bumped on every change.
    pub seq: u64,
    pub updated: Option<DateTime<Utc>>,
    pub container_up: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    /// Every safety check that fired (a Connect process, a credential file), newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
}

impl State {
    /// The container is gone: nothing runs in it and nothing supervises it. A run still going
    /// ends `container stopped`.
    pub fn container_gone(&mut self, now: DateTime<Utc>) {
        self.container_up = false;
        self.display = None;
        self.supervisor_pid = None;
        if let Some(r) = self.run.as_mut().filter(|r| r.phase != Phase::Exited) {
            r.reach(Phase::Exited, now);
            r.exit = Some(Exit { at: now, how: "container stopped".into(), detail: String::new() });
        }
    }

    /// A `state.json` read back, brought in line with what podman says (`running`). The file
    /// is the supervisor's, and a supervisor that has stopped (`quiver lab down` stops it
    /// before the container) left it as it last was.
    pub fn reconcile(&mut self, running: bool, now: DateTime<Utc>) {
        if running {
            self.container_up = true;
        } else if self.container_up || self.supervisor_pid.is_some() || self.run.as_ref().is_some_and(|r| r.phase != Phase::Exited) {
            self.container_gone(now);
        }
    }
}

/// What `quiver lab wait` waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    Phase(Phase),
    /// A state a mod reports (`in-world`).
    Mod(String),
}

impl Want {
    pub fn parse(s: &str) -> Want {
        match Phase::parse(s) {
            Some(p) => Want::Phase(p),
            None => Want::Mod(s.to_owned()),
        }
    }
}

impl std::fmt::Display for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Want::Phase(p) => f.write_str(p.as_str()),
            Want::Mod(s) => f.write_str(s),
        }
    }
}

/// `Some(true)`: reached in this run (`run_id`, or the current run); `Some(false)`: can't
/// be reached any more (the game exited, the container is down); `None`: keep waiting.
pub fn reached(state: &State, want: &Want, run_id: Option<u32>) -> Option<bool> {
    let Some(run) = state.run.as_ref().filter(|r| run_id.is_none_or(|id| r.id == id)) else {
        return if state.container_up { None } else { Some(false) };
    };
    let done = match want {
        Want::Phase(Phase::Exited) => run.phase == Phase::Exited,
        Want::Phase(p) => run.reached.contains_key(p),
        // The mod's *current* state: waiting for in-world after a reload means now.
        Want::Mod(m) => run.mod_state.as_ref().is_some_and(|s| &s.state == m),
    };
    if done {
        return Some(true);
    }
    if run.phase == Phase::Exited || !state.container_up {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    #[test]
    fn the_ladder_only_climbs() {
        let mut r = Run::new(1, "lab-t1-r1".into(), t(0));
        assert!(r.reach(Phase::Window, t(5)));
        assert!(r.reached.contains_key(&Phase::Process), "a window implies a process");
        assert!(!r.reach(Phase::Process, t(6)));
        assert_eq!(r.phase, Phase::Window);
        assert!(r.reach(Phase::Exited, t(9)));
        assert!(!r.reach(Phase::Frame, t(10)), "nothing after exit");
        assert!(!r.reached.contains_key(&Phase::Frame));
    }

    #[test]
    fn mod_states() {
        let s = parse_mod_state("in-world extra words\nlocation = alexandria\nbad line\n", t(1)).unwrap();
        assert_eq!(s.state, "in-world");
        assert_eq!(s.detail["location"], "alexandria");
        assert!(parse_mod_state("\n\n", t(1)).is_none());
        assert!(parse_mod_state("in/world", t(1)).is_none());
        let mut r = Run::new(1, "s".into(), t(0));
        assert!(r.set_mod_state(s.clone()));
        assert!(!r.set_mod_state(s));
        assert!(r.mod_reached.contains_key("in-world"));
    }

    #[test]
    fn waiting() {
        let mut st = State { instance: "t1".into(), container_up: true, ..Default::default() };
        assert_eq!(reached(&st, &Want::parse("window"), None), None);
        let mut r = Run::new(2, "s".into(), t(0));
        r.reach(Phase::Frame, t(3));
        st.run = Some(r);
        assert_eq!(reached(&st, &Want::parse("window"), Some(2)), Some(true));
        assert_eq!(reached(&st, &Want::parse("window"), Some(3)), None, "another run");
        assert_eq!(reached(&st, &Want::parse("in-world"), None), None);
        st.run.as_mut().unwrap().reach(Phase::Exited, t(4));
        assert_eq!(reached(&st, &Want::parse("in-world"), None), Some(false));
        assert_eq!(reached(&st, &Want::parse("exited"), None), Some(true));
        st.container_up = false;
        st.run = None;
        assert_eq!(reached(&st, &Want::parse("process"), None), Some(false));
    }

    #[test]
    fn a_stale_state_follows_podman() {
        // What a supervisor left when `down` stopped it: the container up, a run at frame.
        let mut r = Run::new(4, "lab-origins-r4".into(), t(0));
        r.reach(Phase::Frame, t(9));
        let stale = State { instance: "origins".into(), container_up: true, display: Some(":10".into()), supervisor_pid: Some(77), run: Some(r), ..Default::default() };
        let mut st = stale.clone();
        st.reconcile(true, t(20));
        assert_eq!(st, stale, "podman agrees: nothing changes");
        st.reconcile(false, t(20));
        assert!(!st.container_up && st.display.is_none() && st.supervisor_pid.is_none());
        let run = st.run.as_ref().unwrap();
        assert_eq!((run.phase, run.exit.as_ref().map(|e| e.how.as_str())), (Phase::Exited, Some("container stopped")));
        assert_eq!(reached(&st, &Want::parse("in-world"), None), Some(false), "nothing to wait for");
        // A run that had ended keeps how it ended.
        let mut st = stale.clone();
        st.run.as_mut().unwrap().reach(Phase::Exited, t(10));
        st.run.as_mut().unwrap().exit = Some(Exit { at: t(10), how: "killed".into(), detail: "by cli".into() });
        st.reconcile(false, t(20));
        assert_eq!(st.run.unwrap().exit.unwrap().how, "killed");
    }
}
