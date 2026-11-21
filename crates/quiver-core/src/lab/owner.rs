//! Who brought an instance up and who started each run (design/LAB.md "Owners"), so the
//! sessions sharing the lab can tell whose instance is whose before taking one down.
//!
//! An owner is found from the `quiver` process's ancestry in `/proc`: the nearest agent host
//! (Claude Code's `claude`, Codex's `codex`, `muse`), else the terminal it runs in (a tmux
//! pane, a kitty window, a tty). Only argv and a fixed list of environment keys
//! ([`ENV_KEYS`]: ids and labels, never secrets) are read, and only this process's own
//! environment: other processes' `environ` is never opened.
//!
//! What each host carries (seen on the machine this was written on):
//! - Claude Code: `claude --resume <name>` (the name the person resumed it by; `--resume`
//!   alone or a uuid: no name), and in its children `CLAUDECODE=1`, `CLAUDE_PID` (the
//!   `claude` process) and `CLAUDE_CODE_SESSION_ID`.
//! - Codex: commands run under one shared `codex app-server … --managed-daemon` that serves
//!   every Codex TUI, so the TUI above it in the ancestry is just the one that started the
//!   daemon, often another session. The thread comes from `CODEX_THREAD_ID` (Codex sets it
//!   for its commands), else from a TUI's `codex resume <id>` when the command runs right
//!   under it; its name from `$CODEX_HOME/session_index.jsonl` (`thread_name`).
//! - muse: `muse resume <id>`.
//! - A terminal: `TMUX_PANE` under tmux, `KITTY_WINDOW_ID` under kitty.
//!
//! Overrides: `QUIVER_LAB_OWNER=<name>` or `--owner <name>` label the owner outright; the
//! `--source` label (`agent:pharos-lab`) is kept alongside as who, inside that session.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The instance's owner, in the instance's folder: who ran the last `quiver lab up`.
pub const FILE: &str = "owner.json";

/// The environment keys read for identification. Nothing else is ever read.
pub const ENV_KEYS: &[&str] = &[
    "QUIVER_LAB_OWNER",
    OWNER_HANDOFF,
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_HOME",
    "TMUX_PANE",
    "KITTY_WINDOW_ID",
    "HOME",
];

/// A `quiver lab … --bg` job's owner, found by the foreground process (the job runs
/// detached, without the ancestry), as JSON.
pub const OWNER_HANDOFF: &str = "QUIVER_LAB_OWNER_DETECTED";

type Lookup<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// The environment, through [`ENV_KEYS`] only.
pub struct Env<'a>(Lookup<'a>);

impl<'a> Env<'a> {
    /// This process's environment.
    pub fn process() -> Env<'static> {
        Env(Box::new(|k| std::env::var(k).ok()))
    }
    pub fn from_fn(f: impl Fn(&str) -> Option<String> + 'a) -> Self {
        Env(Box::new(f))
    }
    /// A key's value (empty: none). Keys outside [`ENV_KEYS`] are refused.
    pub fn get(&self, key: &str) -> Option<String> {
        if !ENV_KEYS.contains(&key) {
            debug_assert!(false, "{key} isn't an owner key");
            return None;
        }
        (self.0)(key).filter(|v| !v.trim().is_empty())
    }
}

/// Who started something in the lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// The host: `claude`, `codex`, `muse`, `tmux`, `kitty`, `terminal` (a tty and nothing
    /// else known) or `unknown`.
    pub agent: String,
    /// The session's name (Claude's `--resume` name, Codex's thread name, a tmux pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Its id (Claude's session id, Codex's thread id, muse's session id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The host's process (its window's shell for a terminal): gone means the session is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// `pid`'s start time (clock ticks after boot, `/proc/<pid>/stat`): a reused pid isn't it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<u64>,
    /// The host's terminal (`pts/13`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    /// The `--source` label (`agent:pharos-lab`), unless it was the default `cli`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// An explicit name (`--owner`, `QUIVER_LAB_OWNER`): shown instead of the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub at: DateTime<Utc>,
}

impl Owner {
    fn bare(agent: &str) -> Owner {
        Owner { agent: agent.into(), session: None, session_id: None, pid: None, pid_start: None, tty: None, source: None, label: None, at: Utc::now() }
    }

    /// The short name: `codex:pharos-xreal`, `claude:pharos/agent:pharos-lab`, `terminal`.
    pub fn name(&self) -> String {
        let base = match &self.label {
            Some(l) => l.clone(),
            None => match self.session.as_deref().or(self.session_id.as_deref().map(short_id)) {
                Some(s) => format!("{}:{s}", self.agent),
                None => self.agent.clone(),
            },
        };
        match &self.source {
            Some(s) if *s != base => format!("{base}/{s}"),
            _ => base,
        }
    }

    /// Whether the owner's process still runs (`None`: no process recorded). A dead owner is
    /// a strong hint that the instance is abandoned.
    pub fn alive(&self, proc_root: &Path) -> Option<bool> {
        let pid = self.pid?;
        let Some(p) = read_proc(proc_root, pid) else { return Some(false) };
        Some(self.pid_start.is_none_or(|s| s == p.start))
    }

    /// The details line: `pid 527764 on pts/6, alive`.
    pub fn details(&self, proc_root: &Path) -> String {
        let mut parts = Vec::new();
        if let Some(id) = &self.session_id
            && self.session.as_deref() != Some(id)
        {
            parts.push(format!("session {id}"));
        }
        match (self.pid, self.alive(proc_root)) {
            (Some(pid), Some(true)) => parts.push(format!("pid {pid} alive")),
            (Some(pid), _) => parts.push(format!("pid {pid} GONE (probably abandoned)")),
            (None, _) => parts.push("no process to check".into()),
        }
        if let Some(t) = &self.tty {
            parts.push(format!("on {t}"));
        }
        parts.push(format!("since {}", self.at.with_timezone(&chrono::Local).format("%F %T")));
        parts.join(", ")
    }

    /// The JSON the CLI prints: the owner, its name, and whether it's alive.
    pub fn report(&self, proc_root: &Path) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["name"] = self.name().into();
        v["alive"] = serde_json::json!(self.alive(proc_root));
        v
    }

    /// The owner of this process: the handed-over one of a background job, else detected
    /// from `/proc`; with `label` (`--owner`, else `QUIVER_LAB_OWNER`) and `source`.
    pub fn current(source: &str, label: Option<&str>) -> Owner {
        let env = Env::process();
        let mut o = env.get(OWNER_HANDOFF).and_then(|j| serde_json::from_str::<Owner>(&j).ok()).unwrap_or_else(|| detect(Path::new("/proc"), std::process::id(), &env));
        o.at = Utc::now();
        o.label = label.map(str::to_owned).or_else(|| env.get("QUIVER_LAB_OWNER")).map(|l| clean_label(&l)).filter(|l| !l.is_empty());
        o.source = (source != "cli" && !source.is_empty()).then(|| clean_label(source));
        o
    }

    /// What a background job hands to its detached process ([`OWNER_HANDOFF`]).
    pub fn handoff(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn read(path: &Path) -> Option<Owner> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        super::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// A label as given, printable and at most 64 characters.
fn clean_label(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(64).collect()
}

/// A long id cut for the table (`01a1fe7c`); the whole id is in `session_id`.
fn short_id(id: &str) -> &str {
    if id.len() > 13 && id.is_char_boundary(8) { &id[..8] } else { id }
}

/// What `/proc/<pid>` says about a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub tty_nr: u32,
    pub start: u64,
    pub argv: Vec<String>,
}

impl ProcInfo {
    /// The program's name: argv0's file name, else comm.
    pub fn name(&self) -> &str {
        let a = self.argv.first().map(|a| a.rsplit('/').next().unwrap_or(a)).unwrap_or("");
        if a.is_empty() { &self.comm } else { a }
    }
    /// The terminal (`pts/6`, `tty2`), if it has one.
    pub fn tty(&self) -> Option<String> {
        tty_name(self.tty_nr)
    }
}

/// `tty_nr` (`/proc/<pid>/stat` field 7) as a device name.
pub fn tty_name(tty_nr: u32) -> Option<String> {
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00);
    match major {
        0 => None,
        136..=143 => Some(format!("pts/{}", minor + (major - 136) * 256)),
        4 if minor < 64 => Some(format!("tty{minor}")),
        _ => Some(format!("tty({major},{minor})")),
    }
}

/// `<root>/<pid>/stat` and `cmdline`.
pub fn read_proc(root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = root.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_owned();
    let f: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let ppid = f.get(1)?.parse().ok()?;
    let tty_nr = f.get(4)?.parse::<i64>().ok()? as u32;
    let start = f.get(19)?.parse().ok()?;
    let argv = std::fs::read(dir.join("cmdline"))
        .map(|b| b.split(|c| *c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
        .unwrap_or_default();
    Some(ProcInfo { pid, ppid, comm, tty_nr, start, argv })
}

/// `pid`'s parents, nearest first (not `pid` itself; at most 64, never pid 1 or 0).
pub fn ancestry(root: &Path, pid: u32) -> Vec<ProcInfo> {
    let mut out: Vec<ProcInfo> = Vec::new();
    let mut next = read_proc(root, pid).map(|p| p.ppid);
    while let Some(p) = next.filter(|p| *p > 1) {
        if out.len() >= 64 || out.iter().any(|q| q.pid == p) {
            break;
        }
        let Some(info) = read_proc(root, p) else { break };
        next = Some(info.ppid);
        out.push(info);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Claude,
    /// A Codex TUI or `codex exec`.
    Codex,
    /// Codex's shared app-server (or one of its helpers): it serves every Codex session.
    CodexServer,
    Muse,
    Tmux,
    Kitty,
}

fn host_of(p: &ProcInfo) -> Option<Host> {
    let name = p.name();
    Some(match name {
        "claude" => Host::Claude,
        "codex" if p.argv.iter().skip(1).any(|a| a == "app-server") => Host::CodexServer,
        "codex" => Host::Codex,
        n if n.starts_with("codex-") => Host::CodexServer,
        "muse" => Host::Muse,
        "kitty" => Host::Kitty,
        n if n == "tmux" || n.starts_with("tmux:") || p.comm.starts_with("tmux") => Host::Tmux,
        _ => return None,
    })
}

/// The value after `flag` (`--resume x`, `--resume=x`), if it isn't another flag.
fn flag_value(argv: &[String], flags: &[&str]) -> Option<String> {
    for (i, a) in argv.iter().enumerate() {
        for f in flags {
            if a == f {
                return argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned();
            }
            if let Some(v) = a.strip_prefix(f).and_then(|r| r.strip_prefix('=')) {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// `<tool> resume <id>` (Codex, muse): the word after `resume`.
fn resume_word(argv: &[String]) -> Option<String> {
    let i = argv.iter().skip(1).position(|a| a == "resume")? + 1;
    argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
}

/// A uuid-looking id (`01a1fe7c-a581-7cc0-8726-e1ca42a83389`).
fn is_id(s: &str) -> bool {
    s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// A name or an id, as `(session, session_id)`.
fn name_or_id(s: Option<String>) -> (Option<String>, Option<String>) {
    match s {
        Some(s) if is_id(&s) => (None, Some(s)),
        Some(s) => (Some(clean_label(&s)), None),
        None => (None, None),
    }
}

fn with_proc(mut o: Owner, p: &ProcInfo) -> Owner {
    o.pid = Some(p.pid);
    o.pid_start = Some(p.start);
    o.tty = p.tty();
    o
}

/// A Codex thread's name, from `<codex_home>/session_index.jsonl` (the last entry for it).
pub fn codex_thread_name(codex_home: &Path, thread: &str) -> Option<String> {
    let text = std::fs::read_to_string(codex_home.join("session_index.jsonl")).ok()?;
    text.lines()
        .rev()
        .filter(|l| l.contains(thread))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"].as_str() == Some(thread))
        .and_then(|v| v["thread_name"].as_str().map(clean_label))
        .filter(|n| !n.is_empty())
}

/// A Codex TUI resumed on `thread` (`codex resume <thread>`), anywhere under `root`.
fn codex_tui_of(root: &Path, thread: &str) -> Option<ProcInfo> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| read_proc(root, pid))
        .find(|p| host_of(p) == Some(Host::Codex) && resume_word(&p.argv).as_deref() == Some(thread))
}

/// The owner of process `pid`, from its ancestry under `root` (`/proc`) and `env`.
pub fn detect(root: &Path, pid: u32, env: &Env) -> Owner {
    let chain = ancestry(root, pid);
    let agent = chain.iter().position(|p| matches!(host_of(p), Some(Host::Claude | Host::Codex | Host::CodexServer | Host::Muse)));
    if let Some(i) = agent {
        let p = &chain[i];
        return match host_of(p) {
            Some(Host::Claude) => claude(p, env),
            Some(Host::Muse) => {
                let (session, session_id) = name_or_id(resume_word(&p.argv));
                with_proc(Owner { session, session_id, ..Owner::bare("muse") }, p)
            }
            _ => codex(root, &chain[i..], env),
        };
    }
    // Not in the ancestry (a detached process): what the environment says.
    if let Some(p) = env.get("CLAUDE_PID").and_then(|p| p.parse().ok()).and_then(|pid| read_proc(root, pid)).filter(|p| host_of(p) == Some(Host::Claude)) {
        return claude(&p, env);
    }
    if env.get("CLAUDECODE").as_deref() == Some("1") {
        return Owner { session_id: env.get("CLAUDE_CODE_SESSION_ID"), ..Owner::bare("claude") };
    }
    if env.get("CODEX_THREAD_ID").is_some() {
        return codex(root, &[], env);
    }
    terminal(&chain, env)
}

fn claude(p: &ProcInfo, env: &Env) -> Owner {
    let (session, id) = name_or_id(flag_value(&p.argv, &["--resume", "-r"]));
    with_proc(Owner { session, session_id: id.or_else(|| env.get("CLAUDE_CODE_SESSION_ID")), ..Owner::bare("claude") }, p)
}

/// `chain`: from the nearest Codex process up.
fn codex(root: &Path, chain: &[ProcInfo], env: &Env) -> Owner {
    let mut o = Owner::bare("codex");
    let direct = chain.first().filter(|p| host_of(p) == Some(Host::Codex));
    let thread = env.get("CODEX_THREAD_ID").or_else(|| direct.and_then(|p| resume_word(&p.argv)));
    let (name, id) = name_or_id(thread);
    o.session = name;
    o.session_id = id;
    // The TUI: the command's own parent, else one resumed on the thread. Never the TUI above a
    // shared app-server: that's whichever session started the daemon.
    let tui = match (direct, &o.session_id) {
        (Some(p), id) if id.is_none() || resume_word(&p.argv).is_none_or(|w| Some(&w) == id.as_ref()) => Some(p.clone()),
        (_, Some(id)) => codex_tui_of(root, id),
        _ => None,
    };
    if let Some(p) = &tui {
        o = with_proc(o, p);
    }
    if o.session.is_none()
        && let Some(id) = &o.session_id
    {
        let home = env.get("CODEX_HOME").map(PathBuf::from).or_else(|| env.get("HOME").map(|h| Path::new(&h).join(".codex")));
        o.session = home.and_then(|h| codex_thread_name(&h, id));
    }
    o
}

fn terminal(chain: &[ProcInfo], env: &Env) -> Owner {
    // The process right under the terminal (the pane's or window's shell) lives as long as it.
    let under = |host: Host| chain.iter().position(|p| host_of(p) == Some(host)).and_then(|i| i.checked_sub(1)).map(|i| &chain[i]);
    if let Some(pane) = env.get("TMUX_PANE") {
        let o = Owner { session: Some(clean_label(&pane)), ..Owner::bare("tmux") };
        return match under(Host::Tmux) {
            Some(p) => with_proc(o, p),
            None => o,
        };
    }
    if let Some(p) = under(Host::Kitty) {
        return with_proc(Owner { session: env.get("KITTY_WINDOW_ID").map(|w| clean_label(&w)), ..Owner::bare("kitty") }, p);
    }
    // The outermost process on a terminal: the login shell.
    match chain.iter().rev().find(|p| p.tty_nr != 0) {
        Some(p) => with_proc(Owner::bare("terminal"), p),
        None => Owner::bare("unknown"),
    }
}

/// The instances running `game` now (container up, a run that hasn't exited), each with
/// who's using it: the run's owner, else the instance's. For quiverd's "waiting on" line.
pub fn running(lab: &super::Lab, game: &quiver_api::GameRef) -> Vec<(String, Option<Owner>)> {
    lab.list()
        .into_iter()
        .filter_map(|n| {
            let inst = lab.instance(&n);
            let m = inst.load().ok()?;
            if m.game != *game {
                return None;
            }
            let st: super::ready::State = serde_json::from_slice(&std::fs::read(inst.state_path()).ok()?).ok()?;
            let run = st.run.filter(|r| st.container_up && r.phase != super::ready::Phase::Exited)?;
            Some((n, run.owner.or_else(|| Owner::read(&inst.owner_path()))))
        })
        .collect()
}

/// `lab origins, codex:pharos-xreal` (`; ` between instances): what a lab holder of `game`'s
/// play session is. `None`: no instance seems to run it.
pub fn describe_running(lab: &super::Lab, game: &quiver_api::GameRef) -> Option<String> {
    let all = running(lab, game);
    (!all.is_empty()).then(|| {
        all.iter()
            .map(|(n, o)| match o {
                Some(o) => format!("lab {n}, {}", o.name()),
                None => format!("lab {n}"),
            })
            .collect::<Vec<_>>()
            .join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake `/proc`: (pid, ppid, comm, tty_nr, argv).
    fn fake(procs: &[(u32, u32, &str, u32, &[&str])]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (pid, ppid, comm, tty, argv) in procs {
            let d = t.path().join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            let stat = format!("{pid} ({comm}) S {ppid} {pid} {pid} {tty} -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 {} 1000 100", 5000 + pid);
            std::fs::write(d.join("stat"), stat).unwrap();
            std::fs::write(d.join("cmdline"), argv.iter().map(|a| format!("{a}\0")).collect::<String>()).unwrap();
        }
        t
    }

    fn env(pairs: &[(&str, &str)]) -> Env<'static> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Env::from_fn(move |k| m.get(k).cloned())
    }

    const PTS6: u32 = (136 << 8) | 6;
    const PTS13: u32 = (136 << 8) | 13;

    #[test]
    fn claude_by_its_resume_name() {
        // kitty > bash > claude --resume pharos > bash -c > quiver
        let t = fake(&[
            (523230, 806717, "kitty", 0, &["kitty", "--working-directory", "/w"]),
            (523315, 523230, "bash", PTS6, &["/usr/bin/bash", "--posix"]),
            (527764, 523315, "claude", PTS6, &["/home/u/.local/share/mise/installs/claude/latest/claude", "--resume", "pharos"]),
            (900, 527764, "bash", 0, &["/usr/bin/bash", "-c", "quiver lab up origins"]),
            (901, 900, "quiver", 0, &["quiver", "lab", "up", "origins"]),
        ]);
        let o = detect(t.path(), 901, &env(&[("CLAUDECODE", "1"), ("CLAUDE_CODE_SESSION_ID", "f9e75e07-6acd-4dff-84cc-2476b92eb2e7")]));
        assert_eq!((o.agent.as_str(), o.session.as_deref(), o.pid, o.tty.as_deref()), ("claude", Some("pharos"), Some(527764), Some("pts/6")));
        assert_eq!(o.session_id.as_deref(), Some("f9e75e07-6acd-4dff-84cc-2476b92eb2e7"));
        assert_eq!(o.pid_start, Some(5000 + 527764));
        assert_eq!(o.name(), "claude:pharos");
        assert_eq!(o.alive(t.path()), Some(true));
        // With a --source: who, inside that session.
        let o = Owner { source: Some("agent:pharos-lab".into()), ..o };
        assert_eq!(o.name(), "claude:pharos/agent:pharos-lab");
        // `--resume` alone: no name; the session id stands in, cut short.
        let t = fake(&[(10, 1, "claude", PTS6, &["claude", "--resume"]), (11, 10, "quiver", 0, &["quiver"])]);
        let o = detect(t.path(), 11, &env(&[("CLAUDE_CODE_SESSION_ID", "f9e75e07-6acd-4dff-84cc-2476b92eb2e7")]));
        assert_eq!((o.session.as_deref(), o.name().as_str()), (None, "claude:f9e75e07"));
        let t = fake(&[(10, 1, "claude", PTS6, &["claude", "--resume=quiver"]), (11, 10, "quiver", 0, &["quiver"])]);
        assert_eq!(detect(t.path(), 11, &env(&[])).name(), "claude:quiver");
    }

    #[test]
    fn codex_through_its_shared_app_server() {
        // The case that started this: the app-server's parent TUI is the `pharos` thread, but
        // the command is the `pharos-xreal` thread's (CODEX_THREAD_ID), which no TUI resumed.
        let t = fake(&[
            (762948, 3101086, "codex", PTS13, &["/home/u/.local/share/mise/installs/codex/latest/bin/codex", "resume", "01a1fb56-c016-73e1-847c-666eb25d22e2"]),
            (763124, 762948, "codex", 0, &["/home/u/.codex/packages/standalone/releases/0.171.0/bin/codex", "app-server", "--remote-control", "--listen", "unix://", "--managed-daemon"]),
            (29976, 763124, "bash", 0, &["/usr/bin/bash", "-c", "quiver --source agent:codex lab up xreal-origins"]),
            (29977, 29976, "quiver", 0, &["quiver", "--source", "agent:codex", "lab", "up", "xreal-origins"]),
        ]);
        let home = t.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"01a1fb56-c016-73e1-847c-666eb25d22e2\",\"thread_name\":\"pharos\",\"updated_at\":\"2026-11-20T23:42:35Z\"}\n\
             {\"id\":\"01a1fe7c-a581-7cc0-8726-e1ca42a83389\",\"thread_name\":\"untitled\",\"updated_at\":\"2026-11-21T14:00:00Z\"}\n\
             {\"id\":\"01a1fe7c-a581-7cc0-8726-e1ca42a83389\",\"thread_name\":\"pharos-xreal\",\"updated_at\":\"2026-11-21T14:22:44Z\"}\n",
        )
        .unwrap();
        let home_s = home.display().to_string();
        let o = detect(t.path(), 29977, &env(&[("CODEX_THREAD_ID", "01a1fe7c-a581-7cc0-8726-e1ca42a83389"), ("CODEX_HOME", &home_s)]));
        assert_eq!(o.name(), "codex:pharos-xreal");
        assert_eq!(o.session_id.as_deref(), Some("01a1fe7c-a581-7cc0-8726-e1ca42a83389"));
        assert_eq!(o.pid, None, "never the TUI above the shared app-server");
        assert_eq!(o.alive(t.path()), None);
        // Without the thread id: codex, and no guess at a session.
        let o = detect(t.path(), 29977, &env(&[]));
        assert_eq!((o.name().as_str(), o.pid), ("codex", None));
        // A TUI resumed on the thread is found wherever it is.
        let o = detect(t.path(), 29977, &env(&[("CODEX_THREAD_ID", "01a1fb56-c016-73e1-847c-666eb25d22e2"), ("CODEX_HOME", &home_s)]));
        assert_eq!((o.name().as_str(), o.pid, o.tty.as_deref()), ("codex:pharos", Some(762948), Some("pts/13")));
        // A command right under a TUI (`codex exec`, an older Codex): that TUI.
        let t2 = fake(&[(50, 1, "codex", PTS13, &["codex", "resume", "01a1fb56-c016-73e1-847c-666eb25d22e2"]), (51, 50, "bash", 0, &["bash"]), (52, 51, "quiver", 0, &["quiver"])]);
        let o = detect(t2.path(), 52, &env(&[("CODEX_HOME", &home_s)]));
        assert_eq!((o.name().as_str(), o.pid), ("codex:pharos", Some(50)));
    }

    #[test]
    fn muse_and_terminals() {
        let t = fake(&[
            (1614427, 1614242, "bash", PTS6, &["bash"]),
            (1663182, 1614427, "muse", PTS6, &["muse", "resume", "01a1fa93-182e-77b0-b9eb-fc98436bd3dc", "--yolo"]),
            (7, 1663182, "quiver", 0, &["quiver"]),
        ]);
        let o = detect(t.path(), 7, &env(&[]));
        assert_eq!((o.agent.as_str(), o.session_id.as_deref(), o.pid), ("muse", Some("01a1fa93-182e-77b0-b9eb-fc98436bd3dc"), Some(1663182)));
        assert_eq!(o.name(), "muse:01a1fa93");
        // A tmux pane: the pane's shell is what lives as long as it.
        let t = fake(&[(830490, 1, "tmux: server", 0, &["tmux"]), (40, 830490, "bash", PTS13, &["-bash"]), (41, 40, "quiver", PTS13, &["quiver"])]);
        let o = detect(t.path(), 41, &env(&[("TMUX_PANE", "%3")]));
        assert_eq!((o.name().as_str(), o.pid, o.tty.as_deref()), ("tmux:%3", Some(40), Some("pts/13")));
        // A kitty window, and a plain tty.
        let t = fake(&[(60, 1, "kitty", 0, &["kitty"]), (61, 60, "bash", PTS6, &["bash"]), (62, 61, "quiver", PTS6, &["quiver"])]);
        assert_eq!(detect(t.path(), 62, &env(&[("KITTY_WINDOW_ID", "12")])).name(), "kitty:12");
        let t = fake(&[(70, 1, "login", PTS6, &["login"]), (71, 70, "bash", PTS6, &["bash"]), (72, 71, "quiver", PTS6, &["quiver"])]);
        let o = detect(t.path(), 72, &env(&[]));
        assert_eq!((o.name().as_str(), o.pid), ("terminal", Some(70)));
        let t = fake(&[(80, 1, "systemd-run", 0, &["x"]), (81, 80, "quiver", 0, &["quiver"])]);
        assert_eq!(detect(t.path(), 81, &env(&[])).name(), "unknown");
    }

    #[test]
    fn detached_processes_go_by_the_environment() {
        // A setsid'd process: no agent in the ancestry, CLAUDE_PID still names it.
        let t = fake(&[(10, 1, "claude", PTS6, &["claude", "--resume", "quiver"]), (20, 1, "quiver", 0, &["quiver"])]);
        let o = detect(t.path(), 20, &env(&[("CLAUDE_PID", "10")]));
        assert_eq!((o.name().as_str(), o.pid), ("claude:quiver", Some(10)));
        // A CLAUDE_PID that isn't claude (a reused pid) is ignored.
        let t = fake(&[(10, 1, "bash", 0, &["bash"]), (20, 1, "quiver", 0, &["quiver"])]);
        assert_eq!(detect(t.path(), 20, &env(&[("CLAUDE_PID", "10")])).name(), "unknown");
        assert_eq!(detect(t.path(), 20, &env(&[("CLAUDE_PID", "10"), ("CLAUDECODE", "1")])).agent, "claude");
    }

    #[test]
    fn liveness_and_pid_reuse() {
        let t = fake(&[(10, 1, "claude", PTS6, &["claude"])]);
        let mut o = Owner { pid: Some(10), pid_start: Some(5010), ..Owner::bare("claude") };
        assert_eq!(o.alive(t.path()), Some(true));
        o.pid_start = Some(1);
        assert_eq!(o.alive(t.path()), Some(false), "the same pid, started later: someone else");
        o.pid = Some(11);
        assert_eq!(o.alive(t.path()), Some(false));
        assert!(o.details(t.path()).contains("GONE"));
    }

    #[test]
    fn only_allowlisted_keys_are_read() {
        let e = Env::from_fn(|_| Some("value".into()));
        assert_eq!(e.get("TMUX_PANE").as_deref(), Some("value"));
        // Anything else is refused (and trips a debug assertion: detection never asks).
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.get("ANTHROPIC_API_KEY")));
        assert!(matches!(r, Err(_) | Ok(None)));
    }

    #[test]
    fn labels_and_round_trip() {
        let o = Owner { label: Some("xreal rig".into()), source: Some("agent:pharos-lab".into()), session: Some("pharos".into()), ..Owner::bare("claude") };
        assert_eq!(o.name(), "xreal rig/agent:pharos-lab");
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(FILE);
        o.write(&p).unwrap();
        assert_eq!(Owner::read(&p).unwrap(), o);
        // The hand-over to a background job is the same JSON.
        assert_eq!(serde_json::from_str::<Owner>(&o.handoff()).unwrap(), o);
        let v = o.report(t.path());
        assert_eq!(v["name"], "xreal rig/agent:pharos-lab");
        assert!(v["alive"].is_null());
        assert_eq!(clean_label(" a\u{7}b "), "ab");
    }

    #[test]
    fn tty_names() {
        assert_eq!(tty_name(PTS6).as_deref(), Some("pts/6"));
        assert_eq!(tty_name((136 << 8) | (1 << 20)).as_deref(), Some("pts/256"));
        assert_eq!(tty_name((4 << 8) | 2).as_deref(), Some("tty2"));
        assert_eq!(tty_name(0), None);
    }

    #[test]
    fn instances_running_a_game() {
        let t = tempfile::tempdir().unwrap();
        let lab = super::super::Lab::new(t.path().into());
        let m = super::super::manifest::tests::sample();
        for (name, up, exited) in [("origins", true, false), ("t2", true, true), ("t3", false, false)] {
            let inst = lab.instance(name);
            std::fs::create_dir_all(&inst.dir).unwrap();
            m.save(&inst.manifest_path()).unwrap();
            let mut run = super::super::ready::Run::new(3, format!("lab-{name}-r3"), Utc::now());
            if exited {
                run.reach(super::super::ready::Phase::Exited, Utc::now());
            }
            if name == "origins" {
                run.owner = Some(Owner { session: Some("pharos-xreal".into()), ..Owner::bare("codex") });
            }
            let st = super::super::ready::State { instance: name.into(), container_up: up, run: Some(run), ..Default::default() };
            std::fs::write(inst.state_path(), serde_json::to_vec(&st).unwrap()).unwrap();
        }
        assert_eq!(describe_running(&lab, &m.game).as_deref(), Some("lab origins, codex:pharos-xreal"));
        assert_eq!(describe_running(&lab, &quiver_api::GameRef::ubisoft(1)), None);
        // No run owner: the instance's.
        let o = lab.instance("origins");
        let mut st: super::super::ready::State = serde_json::from_slice(&std::fs::read(o.state_path()).unwrap()).unwrap();
        st.run.as_mut().unwrap().owner = None;
        std::fs::write(o.state_path(), serde_json::to_vec(&st).unwrap()).unwrap();
        assert_eq!(describe_running(&lab, &m.game).as_deref(), Some("lab origins"));
        Owner { session: Some("quiver".into()), ..Owner::bare("claude") }.write(&o.owner_path()).unwrap();
        assert_eq!(describe_running(&lab, &m.game).as_deref(), Some("lab origins, claude:quiver"));
    }
}
