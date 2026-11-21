//! The "please wrap up" hint (design/LAB.md §19): when quiverd has a new build staged and a
//! lab instance's run is what keeps it from restarting onto it, the `quiver` commands of
//! that instance's owner print one line on stderr asking it to stop the run at a good point.
//! Agents (Claude Code, Codex, muse) can't be sent a message, but they run `quiver` all the
//! time, so the hint rides on their own commands' output. Nothing is ever forced.
//!
//! - **Whose:** the caller's owner ([`Owner::current`], what `quiver lab whoami` prints)
//!   against each blocking run's owner ([`owner::same_session`]). Lab commands also see a
//!   short neutral line for an instance whose owner can't be told, and one whose owner has
//!   exited (it may be abandoned: tell the person, don't stop it).
//! - **How often:** once per command, at most once per [`EVERY`] per session ([`State`], a
//!   small file in `$XDG_RUNTIME_DIR/quiver/`). Wording sharpens after 30 and 60 minutes.
//! - **What it asks:** stop the run (`quiver lab kill <i>`): the bridge's connection to the
//!   broker is what holds quiverd, so the container may stay up; pausing the clock frees
//!   nothing (the bridge stays connected). A session holding quiverd itself (`quiver daemon
//!   hold --for …`, which records its owner) isn't reminded while the hold lasts.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use quiver_api::daemon::{Hold, LabBlocker, RestartWaiting, human_secs};
use serde::{Deserialize, Serialize};

use super::owner::{self, Owner, Same};

/// At most one hint per session this often.
pub const EVERY: Duration = Duration::minutes(5);
/// The daemon's answer is reused for this long (a burst of commands asks once).
pub const CACHE_FOR: Duration = Duration::seconds(20);
/// `QUIVER_NO_HINTS=1` turns hints off.
pub const NO_HINTS: &str = "QUIVER_NO_HINTS";

/// The hint for one command.
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    /// The line, without its newline.
    pub text: String,
    /// The caller's own instances it asks about (to tell quiverd they were asked).
    pub mine: Vec<String>,
}

/// One blocker as the caller sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Whose {
    Mine,
    /// Another live session's (or one that can't be checked): its own commands get the hint.
    Other,
    /// Its owner's process has exited.
    Gone,
    /// No owner recorded, or not enough to tell.
    Unsure,
}

fn parse_owner(v: Option<&serde_json::Value>) -> Option<Owner> {
    serde_json::from_value(v?.clone()).ok()
}

fn whose(b: &LabBlocker, caller: &Owner, proc_root: &Path) -> Whose {
    let Some(o) = parse_owner(b.owner.as_ref()) else { return Whose::Unsure };
    let gone = o.alive(proc_root) == Some(false);
    match owner::same_session(caller, &o) {
        Same::Yes => Whose::Mine,
        _ if gone => Whose::Gone,
        Same::No => Whose::Other,
        Same::Unsure => Whose::Unsure,
    }
}

/// `"a"`, `"a" and "b"`, `"a", "b" and "c"`.
fn quoted(names: &[&str]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    match q.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The hint for `caller` (`lab_cmd`: the command is a `quiver lab …` one), or none.
pub fn hint(w: &RestartWaiting, holds: &[Hold], caller: &Owner, lab_cmd: bool, proc_root: &Path, now: DateTime<Utc>) -> Option<Hint> {
    if w.lab.is_empty() {
        return None;
    }
    // Holding quiverd yourself says you know (until the hold runs out).
    let holding = holds.iter().any(|h| h.active(now) && parse_owner(h.owner.as_ref()).is_some_and(|o| owner::same_session(caller, &o) == Same::Yes));
    if holding {
        return None;
    }
    // Whole minutes once it's a minute or more.
    let waited = human_secs(if w.waiting_secs < 60 { w.waiting_secs } else { w.waiting_secs - w.waiting_secs % 60 });
    let mins = w.waiting_secs / 60;
    let sorted: Vec<(&LabBlocker, Whose)> = w.lab.iter().map(|b| (b, whose(b, caller, proc_root))).collect();
    let mine: Vec<&str> = sorted.iter().filter(|(_, k)| *k == Whose::Mine).map(|(b, _)| b.instance.as_str()).collect();
    if !mine.is_empty() {
        let (one, which) = (mine.len() == 1, quoted(&mine));
        let what = if one { format!("your lab instance {which}") } else { format!("your lab instances {which}") };
        let kill = mine.iter().map(|i| format!("`{}`", LabBlocker::free_command(i))).collect::<Vec<_>>().join(" and ");
        let run = if one { "the run" } else { "their runs" };
        let launch = if one { "launch it again" } else { "launch them again" };
        let head = if mins >= 60 {
            format!("a quiverd update has been waiting {waited} on {what}, and the person is waiting for it. Unless you're mid-step, please wrap up now:")
        } else if mins >= 30 {
            format!("a quiverd update has been waiting {waited} on {what}. Please wrap up soon:")
        } else {
            format!("a quiverd update is waiting on {what} ({waited}). At a good stopping point,")
        };
        let text = format!(
            "quiver: {head} save what you need and stop {run} with {kill} (the container can stay up; pausing the clock doesn't free it), \
             then {launch} once quiverd has restarted (`quiver daemon status`). Need longer? `quiver daemon hold --for 30m --reason \"…\"` \
             (it holds the restart and stops these reminders while it lasts)."
        );
        return Some(Hint { text, mine: mine.iter().map(|s| s.to_string()).collect() });
    }
    if !lab_cmd {
        return None;
    }
    let gone: Vec<&(&LabBlocker, Whose)> = sorted.iter().filter(|(_, k)| *k == Whose::Gone).collect();
    let unsure: Vec<&(&LabBlocker, Whose)> = sorted.iter().filter(|(_, k)| *k == Whose::Unsure).collect();
    if unsure.is_empty() && gone.len() == 1 {
        let b = gone[0].0;
        let o = b.owner_name().unwrap_or("its owner");
        return Some(Hint {
            text: format!(
                "quiver: a quiverd update is waiting ({waited}) on lab instance \"{}\", whose owner ({o}) has exited; it may be abandoned. \
                 Don't stop an instance you didn't start without asking: mention it to the person.",
                b.instance
            ),
            mine: vec![],
        });
    }
    if unsure.is_empty() && gone.is_empty() {
        return None;
    }
    let mut items: Vec<String> = unsure
        .iter()
        .map(|(b, _)| match b.owner_name() {
            Some(o) => format!("{} ({o})", b.instance),
            None => format!("{} (owner not recorded)", b.instance),
        })
        .collect();
    items.extend(gone.iter().map(|(b, _)| format!("{} ({}, exited)", b.instance, b.owner_name().unwrap_or("owner"))));
    Some(Hint {
        text: format!(
            "quiver: a quiverd update is waiting ({waited}) on lab instances: {}. If one is yours, stop its run at a good point \
             (`quiver lab kill NAME`) and launch it again after the restart; if not, leave it to its owner or the person.",
            items.join(", ")
        ),
        mine: vec![],
    })
}

/// What the hint remembers between commands (`$XDG_RUNTIME_DIR/quiver/hints.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// When each session ([`Owner::key`]) was last shown a hint.
    #[serde(default)]
    pub shown: BTreeMap<String, DateTime<Utc>>,
    /// quiverd's last answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<Cached>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cached {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<RestartWaiting>,
    #[serde(default)]
    pub holds: Vec<Hold>,
}

impl State {
    pub fn read(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        super::write_atomic(path, &serde_json::to_vec(self)?)
    }

    /// Whether `key` may be shown a hint at `now`.
    pub fn due(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.shown.get(key).is_none_or(|t| now - *t >= EVERY || *t > now)
    }

    /// `key` was shown one at `now` (entries older than a day go).
    pub fn shown_now(&mut self, key: &str, now: DateTime<Utc>) {
        self.shown.retain(|_, t| now - *t < Duration::days(1));
        self.shown.insert(key.to_owned(), now);
    }

    /// quiverd's answer, if it's recent enough to reuse.
    pub fn fresh(&self, now: DateTime<Utc>) -> Option<&Cached> {
        self.cached.as_ref().filter(|c| c.at <= now && now - c.at < CACHE_FOR)
    }

    /// The cached answer as it stands `now`: its wait grown by the time since.
    pub fn waiting_now(c: &Cached, now: DateTime<Utc>) -> Option<RestartWaiting> {
        let mut w = c.waiting.clone()?;
        w.waiting_secs = w.waiting_secs.saturating_add((now - c.at).num_seconds().max(0) as u64);
        Some(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn claude(id: &str) -> Owner {
        serde_json::from_value(serde_json::json!({"agent": "claude", "session_id": id, "at": "2026-11-21T19:00:00Z"})).unwrap()
    }

    fn codex(thread: &str, source: Option<&str>) -> Owner {
        serde_json::from_value(serde_json::json!({"agent": "codex", "session_id": thread, "source": source, "at": "2026-11-21T19:00:00Z"})).unwrap()
    }

    fn blocker(instance: &str, owner: Option<&Owner>) -> LabBlocker {
        LabBlocker {
            instance: instance.into(),
            game: Some("Assassin's Creed® Origins".into()),
            run: Some(7),
            session: Some(format!("lab-{instance}-r7")),
            owner: owner.map(|o| o.report(Path::new("/nonexistent"))),
            free_with: LabBlocker::free_command(instance),
            asked: None,
        }
    }

    fn waiting(mins: u64, lab: Vec<LabBlocker>) -> RestartWaiting {
        RestartWaiting { since: Utc::now(), waiting_secs: mins * 60 + 13, version: Some("0.1.0.r622".into()), waiting_on: vec![], lab }
    }

    fn no_proc() -> PathBuf {
        PathBuf::from("/nonexistent-proc")
    }

    const T1: &str = "01a1ff7d-5b9f-7dc2-a632-14a8fb0160c9";
    const T2: &str = "01a1ff7f-dfa1-7102-8f8b-898e203f1a37";
    const C1: &str = "f9e75e07-6acd-4dff-84cc-2476b92eb2e7";

    #[test]
    fn matching_your_instance() {
        let now = Utc::now();
        let p = no_proc();
        // The same Codex thread (a different --source inside it is still the same session).
        let w = waiting(12, vec![blocker("pharos-content-loader", Some(&codex(T1, Some("agent:codex-ports"))))]);
        let h = hint(&w, &[], &codex(T1, None), false, &p, now).expect("same thread");
        assert_eq!(h.mine, ["pharos-content-loader"]);
        assert!(h.text.starts_with("quiver: a quiverd update is waiting on your lab instance \"pharos-content-loader\" (12m)."), "{}", h.text);
        assert!(h.text.contains("`quiver lab kill pharos-content-loader`") && h.text.contains("quiver daemon hold --for 30m"), "{}", h.text);
        // Another Codex thread: not yours, and nothing for a non-lab command either way.
        assert_eq!(hint(&w, &[], &codex(T2, None), true, &p, now), None);
        // The same Claude session, by its session id; another one isn't.
        let w = waiting(5, vec![blocker("origins", Some(&claude(C1)))]);
        assert!(hint(&w, &[], &claude(C1), false, &p, now).is_some());
        assert_eq!(hint(&w, &[], &claude("36696674-1d53-4eda-8ef5-50dd6e03a863"), true, &p, now), None);
        // A Claude caller and a Codex owner: different hosts.
        assert_eq!(hint(&w, &[], &codex(T1, None), true, &p, now), None);
        // An explicit label (QUIVER_LAB_OWNER=codex:pharos-xreal) matches the thread it names.
        let labelled: Owner = serde_json::from_value(serde_json::json!({"agent": "unknown", "label": "codex:pharos-xreal", "source": "agent:xreal-recorder", "at": "2026-11-21T19:00:00Z"})).unwrap();
        let mut xreal = codex("01a1fe7c-a581-7cc0-8726-e1ca42a83389", None);
        xreal.session = Some("pharos-xreal".into());
        let w = waiting(5, vec![blocker("xreal-origins", Some(&labelled))]);
        assert_eq!(hint(&w, &[], &xreal, false, &p, now).unwrap().mine, ["xreal-origins"]);
    }

    #[test]
    fn owners_gone_or_unknown_get_lab_callers_a_neutral_line() {
        let now = Utc::now();
        // A fake /proc where pid 10 is alive.
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("10")).unwrap();
        std::fs::write(t.path().join("10/stat"), "10 (claude) S 1 10 10 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5010 1000 100").unwrap();
        let mut gone = claude("36696674-1d53-4eda-8ef5-50dd6e03a863");
        gone.source = Some("agent:torchbearer-rt".into());
        gone.pid = Some(845783);
        let w = waiting(20, vec![blocker("awr-rt", Some(&gone))]);
        let me = claude(C1);
        // Not a lab command: nothing.
        assert_eq!(hint(&w, &[], &me, false, t.path(), now), None);
        let h = hint(&w, &[], &me, true, t.path(), now).unwrap();
        assert!(h.text.contains("\"awr-rt\", whose owner (claude:36696674/agent:torchbearer-rt) has exited"), "{}", h.text);
        assert!(h.text.contains("mention it to the person") && h.mine.is_empty());
        // Alive (the same pid, started then): someone else's, no hint.
        let mut alive = gone.clone();
        alive.pid = Some(10);
        alive.pid_start = Some(5010);
        assert_eq!(hint(&waiting(20, vec![blocker("awr-rt", Some(&alive))]), &[], &me, true, t.path(), now), None);
        // No owner recorded: the neutral line, lab commands only.
        let w = waiting(3, vec![blocker("origins", None), blocker("awr-rt", Some(&gone))]);
        let h = hint(&w, &[], &me, true, t.path(), now).unwrap();
        assert_eq!(
            h.text,
            "quiver: a quiverd update is waiting (3m) on lab instances: origins (owner not recorded), awr-rt (claude:36696674/agent:torchbearer-rt, exited). \
             If one is yours, stop its run at a good point (`quiver lab kill NAME`) and launch it again after the restart; if not, leave it to its owner or the person."
        );
        assert_eq!(hint(&w, &[], &me, false, t.path(), now), None);
        // A caller that can't be told (unknown host): unsure about everyone's.
        let unknown: Owner = serde_json::from_value(serde_json::json!({"agent": "unknown", "at": "2026-11-21T19:00:00Z"})).unwrap();
        let w = waiting(3, vec![blocker("pharos-aim-verify", Some(&codex(T2, None)))]);
        assert!(hint(&w, &[], &unknown, true, t.path(), now).unwrap().text.contains("pharos-aim-verify (codex:01a1ff7f)"));
    }

    #[test]
    fn wording_sharpens_and_holds_quiet_it() {
        let now = Utc::now();
        let p = no_proc();
        let me = codex(T1, None);
        let b = || vec![blocker("origins", Some(&me)), blocker("origins-2", Some(&me))];
        assert!(hint(&waiting(0, b()), &[], &me, true, &p, now).unwrap().text.contains("(13s)"), "seconds before a minute");
        let h = hint(&waiting(12, b()), &[], &me, true, &p, now).unwrap();
        assert!(h.text.contains("your lab instances \"origins\" and \"origins-2\" (12m)") && h.text.contains("`quiver lab kill origins` and `quiver lab kill origins-2`"), "{}", h.text);
        assert!(h.text.contains("stop their runs") && h.text.contains("launch them again"));
        let h = hint(&waiting(47, b()), &[], &me, true, &p, now).unwrap();
        assert!(h.text.starts_with("quiver: a quiverd update has been waiting 47m on your lab instances"), "{}", h.text);
        assert!(h.text.contains("Please wrap up soon"));
        let h = hint(&waiting(72, b()), &[], &me, true, &p, now).unwrap();
        assert!(h.text.contains("has been waiting 1h 12m") && h.text.contains("the person is waiting for it"), "{}", h.text);
        // A hold of this session's: quiet until it runs out; another session's changes nothing.
        let hold = |o: &Owner, mins: i64| Hold {
            id: 1,
            reason: "mid-measurement".into(),
            source: "agent".into(),
            since: now,
            expires: Some(now + Duration::minutes(mins)),
            owner: Some(o.report(&p)),
        };
        assert_eq!(hint(&waiting(12, b()), &[hold(&me, 30)], &me, true, &p, now), None);
        assert!(hint(&waiting(12, b()), &[hold(&me, -1)], &me, true, &p, now).is_some(), "ran out");
        assert!(hint(&waiting(12, b()), &[hold(&codex(T2, None), 30)], &me, true, &p, now).is_some());
        // Nothing of the lab's in the way: nothing.
        assert_eq!(hint(&waiting(90, vec![]), &[], &me, true, &p, now), None);
    }

    #[test]
    fn rate_limit_per_session() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("quiver/hints.json");
        let now = Utc::now();
        let mut st = State::read(&path);
        assert!(st.due("codex:a", now));
        st.shown_now("codex:a", now);
        st.write(&path).unwrap();
        let st = State::read(&path);
        assert!(!st.due("codex:a", now + Duration::minutes(4)), "within 5 minutes");
        assert!(st.due("codex:a", now + Duration::minutes(5)));
        assert!(st.due("codex:b", now), "per session");
        // A clock that went back doesn't silence it forever.
        assert!(st.due("codex:a", now - Duration::minutes(1)));
        // Old entries go.
        let mut st = st;
        st.shown_now("codex:b", now + Duration::days(2));
        assert!(!st.shown.contains_key("codex:a"));
        // The cache: reused for a while, its wait grown by the time since.
        let st = State { cached: Some(Cached { at: now, waiting: Some(waiting(10, vec![])), holds: vec![] }), ..Default::default() };
        let c = st.fresh(now + Duration::seconds(5)).unwrap();
        assert_eq!(State::waiting_now(c, now + Duration::seconds(5)).unwrap().waiting_secs, 10 * 60 + 13 + 5);
        assert!(st.fresh(now + CACHE_FOR).is_none());
        // A broken file is an empty state.
        std::fs::write(&path, b"{nope").unwrap();
        assert!(State::read(&path).shown.is_empty());
    }

    #[test]
    fn owner_keys_and_same_session() {
        assert_eq!(codex(T1, Some("agent:x")).key(), format!("codex:{T1}"));
        let mut tmux: Owner = serde_json::from_value(serde_json::json!({"agent": "tmux", "session": "%3", "pid": 40, "pid_start": 5040, "at": "2026-11-21T19:00:00Z"})).unwrap();
        assert_eq!(tmux.key(), "tmux:%3");
        let pane = tmux.clone();
        assert_eq!(owner::same_session(&tmux, &pane), Same::Yes);
        tmux.pid_start = Some(1);
        assert_eq!(owner::same_session(&tmux, &pane), Same::No, "a reused pid is someone else");
        let bare_codex: Owner = serde_json::from_value(serde_json::json!({"agent": "codex", "at": "2026-11-21T19:00:00Z"})).unwrap();
        assert_eq!(owner::same_session(&bare_codex, &codex(T1, None)), Same::Unsure);
        assert_eq!(owner::same_session(&claude(C1), &codex(T1, None)), Same::No);
    }
}
