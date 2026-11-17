//! Under Hyprland, open the relaunched window where the old one was: same workspace, same
//! floating/tiled state, same position and size when floating, fullscreen when it was, and
//! without taking focus or switching workspaces unless the old window had focus.
//!
//! A new process can't take over the old window's surface, so the new window is placed by a
//! Hyprland exec rule. Hyprland ties an exec rule to a token it puts in the spawned process's
//! environment (`HL_EXEC_RULE_TOKEN`), and matches a mapping window by reading that variable from
//! the window's `/proc/<pid>/environ` (it spawns its own dialog helpers the same way). So:
//!
//! 1. [`capture`]: find our own client in `hyprctl -j clients` (by pid) and record where it is.
//! 2. [`register`]: `hyprctl dispatch exec "[workspace 17 silent; float; …] <tiny command>"`,
//!    where the tiny command only writes the token Hyprland gave it to a file.
//! 3. The caller starts the new build itself, as before (our env, args, log, process group and
//!    child handle all stay as they were) with `HL_EXEC_RULE_TOKEN=<token>` added. Its window
//!    gets the rule the moment it maps: no flash on the current workspace, no move afterwards.
//!
//! Exec rules expire a minute after they are made, and the new process drops the token from its
//! environment at startup ([`forget_tokens`]) so nothing it starts later inherits the placement.
//!
//! The Lua config front end (Hyprland 0.55+) takes `hyprctl dispatch <lua expression>`; the
//! classic one takes `hyprctl dispatch exec <args>`. [`register`] tries the Lua form first and
//! falls back to the classic one; each fails without side effects on the other front end.
//!
//! Everything fails soft: no Hyprland, no `hyprctl`, an unexpected reply or no token, and the
//! relaunch goes ahead unplaced.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The variable Hyprland matches exec rules on.
pub const TOKEN_ENV: &str = "HL_EXEC_RULE_TOKEN";
/// Hyprland launch tokens that only make sense for the process they were given to (the
/// initial-workspace token would send a relaunch back to the workspace the app was first
/// started on).
pub const ENV_DROP: &[&str] = &[TOKEN_ENV, "HL_INITIAL_WORKSPACE_TOKEN"];

/// How long one `hyprctl` call may take.
const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(2);
/// How long Hyprland gets to run the token command.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(2);

pub fn session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|s| !s.is_empty())
}

/// Drop Hyprland's launch tokens from this process's environment, so the windows of anything it
/// starts aren't placed by them. Hyprland reads a window's tokens from `/proc/<pid>/environ`,
/// which this doesn't change, so our own window is still placed.
///
/// # Safety
/// Must be called before any other thread exists (it changes the environment).
pub unsafe fn forget_tokens() {
    for k in ENV_DROP {
        if std::env::var_os(k).is_some() {
            // SAFETY: the caller guarantees we're single-threaded.
            unsafe { std::env::remove_var(k) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Where the window is

/// Where the old window was, as Hyprland saw it. Coordinates are Hyprland's logical layout
/// coordinates.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Placement {
    pub address: String,
    pub workspace_id: i64,
    pub workspace_name: String,
    pub floating: bool,
    /// Global position.
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub monitor: i64,
    /// The monitor's global position (exec-rule moves are monitor-local).
    pub monitor_at: [i32; 2],
    /// Hyprland's fullscreen mode: 0 none, 1 maximized, 2 fullscreen (3 both).
    pub fullscreen: u8,
    pub pinned: bool,
    /// It had keyboard focus (and so its workspace is the one being looked at).
    pub focused: bool,
}

#[derive(Debug, Deserialize)]
struct Client {
    #[serde(default)]
    address: String,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    at: [f64; 2],
    #[serde(default)]
    size: [f64; 2],
    #[serde(default)]
    workspace: Workspace,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    monitor: i64,
    #[serde(default)]
    pid: i64,
    #[serde(default)]
    pinned: bool,
    /// An int (mode bits) on current Hyprland, a bool on old ones.
    #[serde(default)]
    fullscreen: serde_json::Value,
    #[serde(default, rename = "focusHistoryID")]
    focus_history: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct Workspace {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct Monitor {
    id: i64,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

fn yes() -> bool {
    true
}

fn fullscreen_mode(v: &serde_json::Value) -> u8 {
    match v {
        serde_json::Value::Bool(b) => u8::from(*b) * 2,
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0).min(3) as u8,
        _ => 0,
    }
}

/// `pid`'s window in `hyprctl -j clients` output: a mapped one, the visible one of a group,
/// the most recently focused if there are several.
fn find_client(clients: &str, pid: u32) -> Option<Client> {
    let all: Vec<Client> = serde_json::from_str(clients).ok()?;
    all.into_iter()
        .filter(|c| c.pid == pid as i64 && c.mapped && !c.address.is_empty())
        .min_by_key(|c| (c.hidden, c.focus_history.unwrap_or(i64::MAX)))
}

/// A monitor's global position from `hyprctl -j monitors`.
fn monitor_at(monitors: &str, id: i64) -> Option<[i32; 2]> {
    let all: Vec<Monitor> = serde_json::from_str(monitors).ok()?;
    all.iter().find(|m| m.id == id).map(|m| [m.x.round() as i32, m.y.round() as i32])
}

/// The focused window's address from `hyprctl -j activewindow` (`{}` when nothing has focus).
fn active_address(active: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(active)
        .ok()?
        .get("address")?
        .as_str()
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
}

/// Put together a [`Placement`] from `hyprctl -j` output.
fn placement_from(clients: &str, monitors: Option<&str>, active: Option<&str>, pid: u32) -> Option<Placement> {
    let c = find_client(clients, pid)?;
    let round = |v: [f64; 2]| [v[0].round() as i32, v[1].round() as i32];
    let focused = match active.map(active_address) {
        Some(a) => a.as_deref() == Some(c.address.as_str()),
        // No answer: the most recently focused window is the best guess.
        None => c.focus_history == Some(0),
    };
    Some(Placement {
        monitor_at: monitors.and_then(|m| monitor_at(m, c.monitor)).unwrap_or_default(),
        workspace_id: c.workspace.id,
        workspace_name: c.workspace.name,
        floating: c.floating,
        at: round(c.at),
        size: round(c.size),
        monitor: c.monitor,
        fullscreen: fullscreen_mode(&c.fullscreen),
        pinned: c.pinned,
        focused,
        address: c.address,
    })
}

/// Where `pid`'s window is now, if Hyprland is running and has one.
pub fn capture(pid: u32) -> Option<Placement> {
    if !session() {
        return None;
    }
    let clients = match hyprctl(&["-j", "clients"]) {
        Ok(c) => c,
        Err(e) => {
            tracing::info!("relaunch: can't ask Hyprland for windows: {e}");
            return None;
        }
    };
    let p = placement_from(
        &clients,
        hyprctl(&["-j", "monitors"]).ok().as_deref(),
        hyprctl(&["-j", "activewindow"]).ok().as_deref(),
        pid,
    );
    if p.is_none() {
        tracing::info!("relaunch: Hyprland lists no window for pid {pid}");
    }
    p
}

// ---------------------------------------------------------------------------------------------
// Placing the new one

/// The value of a `workspace` rule for this workspace, or `None` if its name can't be put in
/// an exec rule.
fn workspace_spec(id: i64, name: &str) -> Option<String> {
    if name.contains([';', ']', '\n']) {
        return None;
    }
    Some(if name.starts_with("special") {
        // "special:foo", or "special" for the unnamed one.
        name.to_owned()
    } else if id > 0 && (name.is_empty() || name == id.to_string()) {
        id.to_string()
    } else if !name.is_empty() {
        format!("name:{name}")
    } else {
        return None;
    })
}

impl Placement {
    /// The exec rules that open a window here, e.g. `workspace 17 silent; tile;
    /// no_initial_focus; focus_on_activate 0`. Effect names are Hyprland 0.53+'s (unknown ones
    /// are skipped by older ones).
    pub fn exec_rules(&self) -> Option<String> {
        let ws = workspace_spec(self.workspace_id, &self.workspace_name)?;
        let mut r = Vec::new();
        // A focused window is on the workspace being looked at, so going there switches
        // nothing; `silent` would also keep the new window from taking focus.
        r.push(if self.focused { format!("workspace {ws}") } else { format!("workspace {ws} silent") });
        if self.floating {
            r.push("float".into());
            let [w, h] = self.size;
            if w > 0 && h > 0 {
                r.push(format!("size {w} {h}"));
            }
            let [x, y] = [self.at[0] - self.monitor_at[0], self.at[1] - self.monitor_at[1]];
            r.push(format!("move {x} {y}"));
            if self.pinned {
                r.push("pin".into());
            }
        } else {
            r.push("tile".into());
        }
        if self.fullscreen & 2 != 0 {
            r.push("fullscreen".into());
        } else if self.fullscreen & 1 != 0 {
            r.push("maximize".into());
        }
        if !self.focused {
            // `silent` already keeps focus where it is. `no_initial_focus` says so again, but
            // Hyprland also skips fullscreening a window that has it, so not for those.
            if self.fullscreen == 0 {
                r.push("no_initial_focus".into());
            }
            r.push("focus_on_activate 0".into());
        }
        Some(r.join("; "))
    }

    /// One line for the log.
    pub fn describe(&self) -> String {
        let mut s = format!("workspace {}", self.workspace_name);
        if self.floating {
            s += &format!(", floating at {},{} size {}x{}", self.at[0], self.at[1], self.size[0], self.size[1]);
        } else {
            s += &format!(", tiled at {},{} size {}x{}", self.at[0], self.at[1], self.size[0], self.size[1]);
        }
        if self.fullscreen != 0 {
            s += &format!(", fullscreen {}", self.fullscreen);
        }
        if self.pinned {
            s += ", pinned";
        }
        if self.focused {
            s += ", focused";
        }
        s
    }
}

/// A Lua string literal.
fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `hyprctl` arguments that exec `cmd` with `rules`: the Lua front end's form, then the
/// classic one's.
fn exec_dispatches(rules: &str, cmd: &str) -> [Vec<String>; 2] {
    let exec = format!("[{rules}] {cmd}");
    [
        vec!["dispatch".into(), format!("hl.dsp.exec_cmd({})", lua_str(&exec))],
        vec!["dispatch".into(), "exec".into(), exec],
    ]
}

/// Whether a `hyprctl dispatch` reply means it worked.
fn dispatch_ok(reply: &str) -> bool {
    reply.trim() == "ok"
}

/// The shell command Hyprland runs to hand us the exec-rule token it made.
fn token_command(tmp: &Path, file: &Path) -> String {
    let (tmp, file) = (sh_quote(&tmp.to_string_lossy()), sh_quote(&file.to_string_lossy()));
    format!("printf %s \"${TOKEN_ENV}\" > {tmp} && mv -f {tmp} {file}")
}

fn valid_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 128 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Have Hyprland make an exec rule that opens a window at `p`, and return the token a process
/// needs in its environment for its window to get it. `dir` is where the token is handed over.
pub fn register(p: &Placement, dir: &Path) -> Result<String, String> {
    let rules = p.exec_rules().ok_or("the workspace's name can't go in a window rule")?;
    let file = dir.join(format!("hypr-token-{}", std::process::id()));
    let tmp = file.with_extension("tmp");
    let _ = std::fs::remove_file(&file);
    let cmd = token_command(&tmp, &file);

    let mut replies = Vec::new();
    let mut sent = false;
    for args in exec_dispatches(&rules, &cmd) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match hyprctl(&args) {
            Ok(r) if dispatch_ok(&r) => {
                sent = true;
                break;
            }
            Ok(r) => replies.push(r.trim().lines().next().unwrap_or_default().to_owned()),
            Err(e) => return Err(e),
        }
    }
    if !sent {
        return Err(format!("Hyprland refused the exec rule: {}", replies.join(" / ")));
    }

    let start = Instant::now();
    let token = loop {
        if let Ok(t) = std::fs::read_to_string(&file) {
            break t;
        }
        if start.elapsed() > TOKEN_TIMEOUT {
            let _ = std::fs::remove_file(&tmp);
            return Err("Hyprland didn't run the token command".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let _ = std::fs::remove_file(&file);
    let token = token.trim().to_owned();
    if !valid_token(&token) {
        return Err(if token.is_empty() {
            "this Hyprland doesn't hand out exec-rule tokens".into()
        } else {
            format!("unexpected exec-rule token {token:?}")
        });
    }
    tracing::info!("relaunch: Hyprland exec rule [{rules}]");
    Ok(token)
}

/// Run `hyprctl`, with a time limit; its stdout.
fn hyprctl(args: &[&str]) -> Result<String, String> {
    let mut child = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("hyprctl: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    // Read on a thread so a wedged hyprctl can't hold us past the timeout.
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).map(|_| s)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > HYPRCTL_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("hyprctl timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(format!("hyprctl: {e}")),
        }
    };
    let out = reader.join().map_err(|_| "hyprctl: reader panicked".to_string())?.map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("hyprctl {}: {status}", args.join(" ")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from Hyprland 0.56.2's `hyprctl -j clients`.
    const CLIENTS: &str = r#"[{
        "address": "0x5645d4f55b40", "mapped": true, "hidden": false, "at": [0, 58], "size": [2048, 1222],
        "workspace": { "id": 11, "name": "11" }, "floating": false, "monitor": 1, "class": "kitty",
        "pid": 658085, "pinned": false, "fullscreen": 0, "focusHistoryID": 19
    },{
        "address": "0x5645d4aaaaaa", "mapped": true, "hidden": true, "at": [1616, 42], "size": [420, 1226],
        "workspace": { "id": 17, "name": "17" }, "floating": false, "monitor": 1, "class": "quiver",
        "pid": 3742922, "pinned": false, "fullscreen": 0, "focusHistoryID": 1
    },{
        "address": "0x5645d581a170", "mapped": true, "hidden": false, "at": [1616, 42], "size": [420, 1226],
        "workspace": { "id": 17, "name": "17" }, "floating": false, "monitor": 1, "class": "quiver",
        "pid": 3742922, "pinned": false, "fullscreen": 0, "focusHistoryID": 3, "grouped": []
    },{
        "address": "0x5645d5a36360", "mapped": true, "hidden": false, "at": [2660, 150], "size": [1000, 700],
        "workspace": { "id": -98, "name": "special:scratch" }, "floating": true, "monitor": 0,
        "class": "quiver", "pid": 4242, "pinned": true, "fullscreen": 2, "focusHistoryID": 0
    }]"#;
    const MONITORS: &str = r#"[{ "id": 0, "name": "DP-1", "x": 2560, "y": 0, "width": 3840 },
                               { "id": 1, "name": "eDP-2", "x": 0, "y": 0, "width": 2560 }]"#;

    #[test]
    fn finds_our_window_in_hyprctl_clients() {
        let p = placement_from(CLIENTS, Some(MONITORS), Some("{}"), 3742922).unwrap();
        assert_eq!(p.address, "0x5645d581a170", "the visible one, not the hidden group member");
        assert_eq!((p.workspace_id, p.workspace_name.as_str()), (17, "17"));
        assert!(!p.floating && !p.focused && !p.pinned);
        assert_eq!((p.at, p.size, p.monitor, p.monitor_at, p.fullscreen), ([1616, 42], [420, 1226], 1, [0, 0], 0));

        let active = r#"{ "address": "0x5645d5a36360", "pid": 4242 }"#;
        let p = placement_from(CLIENTS, Some(MONITORS), Some(active), 4242).unwrap();
        assert!(p.floating && p.pinned && p.focused);
        assert_eq!((p.monitor_at, p.fullscreen, p.workspace_id), ([2560, 0], 2, -98));

        assert_eq!(placement_from(CLIENTS, None, None, 1), None, "no window for that pid");
        assert_eq!(placement_from("not json", None, None, 4242), None);
        // Without activewindow, the most recently focused window counts as focused.
        assert!(placement_from(CLIENTS, None, None, 4242).unwrap().focused);
        // Old Hyprland: fullscreen is a bool.
        let old = r#"[{ "address": "0x1", "at": [1, 2], "size": [3, 4], "pid": 9,
                        "workspace": { "id": 2, "name": "2" }, "fullscreen": true }]"#;
        assert_eq!(placement_from(old, None, None, 9).unwrap().fullscreen, 2);
    }

    fn tiled() -> Placement {
        Placement {
            address: "0x1".into(),
            workspace_id: 17,
            workspace_name: "17".into(),
            at: [1616, 42],
            size: [420, 1226],
            ..Default::default()
        }
    }

    #[test]
    fn exec_rules_for_each_kind_of_window() {
        assert_eq!(
            tiled().exec_rules().unwrap(),
            "workspace 17 silent; tile; no_initial_focus; focus_on_activate 0",
            "unfocused: don't take focus or switch"
        );
        assert_eq!(Placement { focused: true, ..tiled() }.exec_rules().unwrap(), "workspace 17; tile");

        let floating = Placement {
            floating: true,
            at: [2660, 150],
            size: [1000, 700],
            monitor_at: [2560, 0],
            pinned: true,
            ..tiled()
        };
        assert_eq!(
            floating.exec_rules().unwrap(),
            "workspace 17 silent; float; size 1000 700; move 100 150; pin; no_initial_focus; focus_on_activate 0",
            "moves are monitor-local"
        );
        assert_eq!(
            Placement { fullscreen: 2, focused: true, ..tiled() }.exec_rules().unwrap(),
            "workspace 17; tile; fullscreen"
        );
        assert_eq!(
            Placement { fullscreen: 1, focused: true, ..tiled() }.exec_rules().unwrap(),
            "workspace 17; tile; maximize"
        );
        assert_eq!(
            Placement { fullscreen: 2, ..tiled() }.exec_rules().unwrap(),
            "workspace 17 silent; tile; fullscreen; focus_on_activate 0",
            "no_initial_focus would keep it from going fullscreen"
        );
    }

    #[test]
    fn workspace_specs() {
        assert_eq!(workspace_spec(3, "3").as_deref(), Some("3"));
        assert_eq!(workspace_spec(3, "").as_deref(), Some("3"));
        assert_eq!(workspace_spec(-1337, "games").as_deref(), Some("name:games"));
        assert_eq!(workspace_spec(5, "web").as_deref(), Some("name:web"));
        assert_eq!(workspace_spec(-98, "special:scratch").as_deref(), Some("special:scratch"));
        assert_eq!(workspace_spec(-99, "special").as_deref(), Some("special"));
        assert_eq!(workspace_spec(-1, "a;b"), None);
        assert_eq!(workspace_spec(-1, "x]"), None);
        assert_eq!(workspace_spec(-1, ""), None);
    }

    #[test]
    fn dispatch_strings_for_both_config_front_ends() {
        let tok = token_command(Path::new("/run/user/1000/quiver/hypr-token-7.tmp"), Path::new("/run/user/1000/quiver/it's"));
        assert_eq!(
            tok,
            r#"printf %s "$HL_EXEC_RULE_TOKEN" > '/run/user/1000/quiver/hypr-token-7.tmp' && mv -f '/run/user/1000/quiver/hypr-token-7.tmp' '/run/user/1000/quiver/it'\''s'"#
        );
        let [lua, classic] = exec_dispatches("workspace 9 silent; float", r#"printf %s "$X" > '/a\b'"#);
        assert_eq!(lua, ["dispatch", r#"hl.dsp.exec_cmd("[workspace 9 silent; float] printf %s \"$X\" > '/a\\b'")"#]);
        assert_eq!(classic, ["dispatch", "exec", r#"[workspace 9 silent; float] printf %s "$X" > '/a\b'"#]);
        assert_eq!(lua_str("a\nb\u{1}"), r#""a\nb\1""#);

        assert!(dispatch_ok("ok\n"));
        assert!(!dispatch_ok("error: [string \"return hl.dispatch(exec ...\"]:1: ']' expected"));
        assert!(!dispatch_ok("Invalid dispatcher"));
        assert!(valid_token("004047ee-c727-4930-a032-9285dc67e95e"));
        assert!(!valid_token(""));
        assert!(!valid_token("a b"));
    }
}
