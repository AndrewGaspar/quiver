//! Hyprland over its request socket (`.socket.sock`, what `hyprctl` uses), for the Sight
//! panel: where the game is (its window and monitor), giving focus back to it on close,
//! closing it gracefully (Quit), and the blur rule behind the panel.
//!
//! Hyprland's command syntax changed twice, and this has to work on all three:
//!
//! | config | detected by | layer rule | dispatch |
//! |---|---|---|---|
//! | Lua (0.55+ with `hyprland.lua`) | `eval hl.version()` → `ok` | `eval hl.layer_rule({…})` | `dispatch hl.dsp.focus({ window = 'address:…' })` |
//! | hyprlang ≥ 0.53 | `eval` refused, `j/version` tag ≥ 0.53 | `keyword layerrule blur on, ignore_alpha A, match:namespace R` | `dispatch focuswindow address:…` |
//! | hyprlang < 0.53 | `eval` unknown | `keyword layerrule blur,R` + `ignorealpha A,R` | same |
//!
//! The rule is a runtime one: nothing is written to the user's config. Under Lua it is a
//! named rule kept in a Lua global, enabled while the panel is up and disabled when it
//! closes; a config reload drops it (and the global), and the next open adds it again.
//! hyprlang keyword rules can't be removed without a reload, so they're added once per
//! `quiver-sight` run and left: they match only the panel's own namespace.
//!
//! Blur follows the user's own `decoration:blur` (size, passes): when it's off, the panel
//! falls back to the plain dim, and says so.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// The Lua global and rule name the panel's blur rule lives under (unique: Lua rule names
/// share one map with the user's own named rules).
const RULE_NAME: &str = "quiver-sight-panel-blur";
const RULE_GLOBAL: &str = "quiver_sight_panel_blur";
/// Pixels at or under this alpha aren't blurred (fully transparent margins, if any).
pub const IGNORE_ALPHA: f32 = 0.02;

/// Hyprland's request socket, if a Hyprland session is reachable.
pub fn socket() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty() && !s.contains('/'))?;
    let mut bases = Vec::new();
    if let Some(r) = std::env::var_os("XDG_RUNTIME_DIR") {
        bases.push(PathBuf::from(r));
    }
    // SAFETY: getuid has no preconditions.
    bases.push(PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    bases.into_iter().map(|b| b.join("hypr").join(&sig).join(".socket.sock")).find(|p| p.exists())
}

/// Which command syntax this Hyprland speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Lua,
    /// hyprlang config, 0.53+ rule syntax (`match:namespace`).
    Keyword,
    /// hyprlang config before 0.53 (`layerrule = blur,<ns>`).
    KeywordOld,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Window {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub pid: i64,
    /// A monitor id.
    #[serde(default)]
    pub monitor: i64,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Monitor {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub transform: i32,
    #[serde(default)]
    pub focused: bool,
}

fn one() -> f32 {
    1.0
}

impl Monitor {
    /// The output's size in logical px (what a full-output layer surface gets).
    pub fn logical(&self) -> (f32, f32) {
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        let (w, h) = if self.transform % 2 == 1 { (self.height, self.width) } else { (self.width, self.height) };
        ((w as f32 / s).round(), (h as f32 / s).round())
    }
}

/// The user's blur settings (`decoration:blur:*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Blur {
    pub enabled: bool,
    pub size: i64,
    pub passes: i64,
}

pub struct Hypr {
    sock: PathBuf,
    pub flavor: Flavor,
    /// hyprlang keyword rules added in this run (they can't be taken back).
    keyword_rule: bool,
}

/// The reply means success (Hyprland answers `ok`; anything else is an error or a warning).
pub fn ok(reply: &str) -> bool {
    reply.trim() == "ok"
}

/// `0.56.2` from `j/version`'s `tag` (`v0.56.2-469-g7ac272db9a`), as (major, minor).
pub fn version_of(tag: &str) -> Option<(u32, u32)> {
    let t = tag.trim().trim_start_matches('v');
    let mut it = t.split(['.', '-']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// The flavor from the replies to `eval hl.version()` and `j/version`.
pub fn flavor_of(eval_reply: &str, version_tag: Option<&str>) -> Flavor {
    if ok(eval_reply) || eval_reply.trim_start().starts_with("warning") {
        return Flavor::Lua;
    }
    match version_tag.and_then(version_of) {
        Some((0, minor)) if minor < 53 => Flavor::KeywordOld,
        // `eval` refused on a hyprlang config, or unknown on an unknown version: 0.53 syntax
        // unless the version says it's older.
        _ if eval_reply.contains("unknown request") && version_tag.and_then(version_of).is_none() => Flavor::KeywordOld,
        _ => Flavor::Keyword,
    }
}

/// The commands that add (or re-enable) the panel's blur rule for namespace `ns`.
pub fn blur_rule_commands(flavor: Flavor, ns: &str) -> Vec<String> {
    let re = format!("^({})$", regex_escape(ns));
    match flavor {
        Flavor::Lua => vec![format!(
            "eval if {g} == nil then {g} = hl.layer_rule({{ name = '{n}', blur = true, ignore_alpha = {a}, match = {{ namespace = '{re}' }} }}) end {g}:set_enabled(true)",
            g = RULE_GLOBAL,
            n = RULE_NAME,
            a = IGNORE_ALPHA,
        )],
        Flavor::Keyword => vec![format!("keyword layerrule blur on, ignore_alpha {IGNORE_ALPHA}, match:namespace {re}")],
        Flavor::KeywordOld => vec![format!("keyword layerrule blur,{re}"), format!("keyword layerrule ignorealpha {IGNORE_ALPHA},{re}")],
    }
}

/// The command that turns the rule off again (Lua only; `None`: it stays).
pub fn blur_off_command(flavor: Flavor) -> Option<String> {
    (flavor == Flavor::Lua).then(|| format!("eval if {g} ~= nil then {g}:set_enabled(false) end", g = RULE_GLOBAL))
}

/// Focus and close commands for a window address (`0x55d0…`, as `j/clients` prints it).
pub fn focus_command(flavor: Flavor, address: &str) -> String {
    match flavor {
        Flavor::Lua => format!("dispatch hl.dsp.focus({{ window = 'address:{}' }})", lua_safe(address)),
        _ => format!("dispatch focuswindow address:{address}"),
    }
}

pub fn close_command(flavor: Flavor, address: &str) -> String {
    match flavor {
        Flavor::Lua => format!("dispatch hl.dsp.window.close({{ window = 'address:{}' }})", lua_safe(address)),
        _ => format!("dispatch closewindow address:{address}"),
    }
}

/// An address is hex; keep anything else out of the Lua string.
fn lua_safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { vec![c] } else { vec!['\\', c] }).collect()
}

/// `j/getoption` replies: `{"option": …, "bool": false}`, older `{"int": 0}`.
fn option_int(reply: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(reply).ok()?;
    v.get("int").and_then(|x| x.as_i64()).or_else(|| v.get("bool").and_then(|x| x.as_bool()).map(i64::from))
}

impl Hypr {
    pub fn connect() -> Option<Hypr> {
        let sock = socket()?;
        let mut h = Hypr { sock, flavor: Flavor::Keyword, keyword_rule: false };
        let eval = h.request("eval hl.version()").unwrap_or_default();
        let tag = h.request("j/version").ok().and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok()).and_then(|v| v.get("tag")?.as_str().map(str::to_owned));
        h.flavor = flavor_of(&eval, tag.as_deref());
        tracing::info!(flavor = ?h.flavor, tag = ?tag, "Hyprland");
        Some(h)
    }

    /// Without probing again (the flavor is known).
    pub fn with_flavor(flavor: Flavor) -> Option<Hypr> {
        Some(Hypr { sock: socket()?, flavor, keyword_rule: true })
    }

    pub fn request(&self, cmd: &str) -> std::io::Result<String> {
        request_at(&self.sock, cmd)
    }

    fn command(&self, cmd: &str) -> Result<(), String> {
        match self.request(cmd) {
            Ok(r) if ok(&r) => Ok(()),
            Ok(r) => Err(r.trim().lines().next().unwrap_or("no reply").to_owned()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn active_window(&self) -> Option<Window> {
        let w: Window = serde_json::from_str(&self.request("j/activewindow").ok()?).ok()?;
        (!w.address.is_empty()).then_some(w)
    }

    pub fn clients(&self) -> Vec<Window> {
        self.request("j/clients").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        self.request("j/monitors").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn blur(&self) -> Blur {
        let get = |k: &str| self.request(&format!("j/getoption decoration:blur:{k}")).ok().and_then(|r| option_int(&r));
        Blur { enabled: get("enabled").unwrap_or(0) != 0, size: get("size").unwrap_or(0), passes: get("passes").unwrap_or(0) }
    }

    pub fn focus(&self, address: &str) -> Result<(), String> {
        self.command(&focus_command(self.flavor, address))
    }

    pub fn close(&self, address: &str) -> Result<(), String> {
        self.command(&close_command(self.flavor, address))
    }

    /// Turns the blur rule on for `ns`; the error says why not.
    pub fn blur_on(&mut self, ns: &str) -> Result<(), String> {
        if self.flavor != Flavor::Lua && self.keyword_rule {
            return Ok(());
        }
        for c in blur_rule_commands(self.flavor, ns) {
            self.command(&c)?;
        }
        if self.flavor != Flavor::Lua {
            self.keyword_rule = true;
        }
        Ok(())
    }

    pub fn blur_off(&self) {
        if let Some(c) = blur_off_command(self.flavor)
            && let Err(e) = self.command(&c)
        {
            tracing::debug!("blur rule off: {e}");
        }
    }
}

fn request_at(sock: &Path, cmd: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(cmd.as_bytes())?;
    let mut out = String::new();
    s.take(4 << 20).read_to_string(&mut out)?;
    Ok(out)
}

/// The window of a session's game among `clients`: one whose process (or an ancestor up to
/// a few levels, Wine's) carries `QUIVER_SESSION=<session>`, or is one of `pids`. The
/// active window first.
pub fn game_window<'a>(clients: &'a [Window], active: Option<&'a Window>, session: &str, pids: &[u32], proc_root: &Path) -> Option<&'a Window> {
    let is_game = |w: &Window| w.pid > 0 && (pids.contains(&(w.pid as u32)) || pid_in_session(proc_root, w.pid as u32, session));
    active.filter(|w| is_game(w)).or_else(|| clients.iter().find(|w| is_game(w)))
}

/// `/proc/<pid>/environ` (or its parents', a few levels up) has `QUIVER_SESSION=<session>`.
pub fn pid_in_session(proc_root: &Path, pid: u32, session: &str) -> bool {
    let want = format!("QUIVER_SESSION={session}");
    let mut p = pid;
    for _ in 0..4 {
        if p <= 1 {
            return false;
        }
        if let Ok(env) = std::fs::read(proc_root.join(p.to_string()).join("environ"))
            && env.split(|&b| b == 0).any(|kv| kv == want.as_bytes())
        {
            return true;
        }
        let Some(pp) = std::fs::read_to_string(proc_root.join(p.to_string()).join("stat")).ok().and_then(|s| parent_of(&s)) else { return false };
        p = pp;
    }
    false
}

/// The parent pid from `/proc/<pid>/stat` (after the `(comm)`, which may hold spaces).
fn parent_of(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors() {
        assert_eq!(flavor_of("ok", Some("v0.56.2-469-g7ac272db9a")), Flavor::Lua);
        assert_eq!(flavor_of("eval is only supported with the lua config manager", Some("v0.56.2")), Flavor::Keyword);
        assert_eq!(flavor_of("unknown request", Some("v0.52.1")), Flavor::KeywordOld);
        assert_eq!(flavor_of("unknown request", Some("v0.54.0")), Flavor::Keyword);
        assert_eq!(flavor_of("unknown request", None), Flavor::KeywordOld);
        assert_eq!(version_of("v0.56.2-469-g7ac272db9a"), Some((0, 56)));
        assert_eq!(version_of("0.45.0"), Some((0, 45)));
        assert_eq!(version_of("junk"), None);
    }

    #[test]
    fn rule_commands_match_only_the_panel() {
        let lua = blur_rule_commands(Flavor::Lua, "quiver-sight");
        assert_eq!(
            lua,
            ["eval if quiver_sight_panel_blur == nil then quiver_sight_panel_blur = hl.layer_rule({ name = 'quiver-sight-panel-blur', blur = true, ignore_alpha = 0.02, match = { namespace = '^(quiver-sight)$' } }) end quiver_sight_panel_blur:set_enabled(true)"]
        );
        assert_eq!(blur_rule_commands(Flavor::Keyword, "quiver-sight"), ["keyword layerrule blur on, ignore_alpha 0.02, match:namespace ^(quiver-sight)$"]);
        assert_eq!(blur_rule_commands(Flavor::KeywordOld, "quiver-sight"), ["keyword layerrule blur,^(quiver-sight)$", "keyword layerrule ignorealpha 0.02,^(quiver-sight)$"]);
        assert_eq!(blur_off_command(Flavor::Lua).unwrap(), "eval if quiver_sight_panel_blur ~= nil then quiver_sight_panel_blur:set_enabled(false) end");
        assert_eq!(blur_off_command(Flavor::Keyword), None);
    }

    #[test]
    fn dispatches() {
        assert_eq!(focus_command(Flavor::Lua, "0x55d0ab"), "dispatch hl.dsp.focus({ window = 'address:0x55d0ab' })");
        assert_eq!(focus_command(Flavor::Lua, "0x1'); os.exit()--"), "dispatch hl.dsp.focus({ window = 'address:0x1osexit' })");
        assert_eq!(close_command(Flavor::Lua, "0x1"), "dispatch hl.dsp.window.close({ window = 'address:0x1' })");
        assert_eq!(focus_command(Flavor::Keyword, "0x1"), "dispatch focuswindow address:0x1");
        assert_eq!(close_command(Flavor::KeywordOld, "0x1"), "dispatch closewindow address:0x1");
        assert!(ok("ok\n") && !ok("warning: window not found") && !ok("error: x"));
    }

    #[test]
    fn options_and_monitors() {
        assert_eq!(option_int(r#"{"option": "decoration:blur:enabled", "bool": false, "set": true }"#), Some(0));
        assert_eq!(option_int(r#"{"option": "decoration:blur:size", "int": 8, "set": false }"#), Some(8));
        assert_eq!(option_int("no such option"), None);
        let m = Monitor { width: 2560, height: 1600, scale: 1.6, ..Default::default() };
        assert_eq!(m.logical(), (1600.0, 1000.0));
        let m = Monitor { width: 1920, height: 1080, scale: 1.0, transform: 1, ..Default::default() };
        assert_eq!(m.logical(), (1080.0, 1920.0));
    }

    #[test]
    fn finding_the_game_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |pid: u32, ppid: u32, env: &str| {
            let d = tmp.path().join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("environ"), env.replace(' ', "\0")).unwrap();
            std::fs::write(d.join("stat"), format!("{pid} (wine (x)) S {ppid} 0 0")).unwrap();
        };
        mk(100, 1, "HOME=/h QUIVER_SESSION=s-abc");
        mk(200, 100, "HOME=/h");
        mk(300, 1, "HOME=/h QUIVER_SESSION=s-other");
        let w = |addr: &str, pid: i64| Window { address: addr.into(), pid, ..Default::default() };
        let clients = vec![w("0xterm", 300), w("0xgame", 200)];
        assert_eq!(game_window(&clients, Some(&clients[0]), "s-abc", &[], tmp.path()).map(|w| w.address.as_str()), Some("0xgame"), "a child of the session's process");
        assert_eq!(game_window(&clients, None, "s-none", &[300], tmp.path()).map(|w| w.address.as_str()), Some("0xterm"), "by pid");
        assert!(game_window(&clients, None, "s-none", &[], tmp.path()).is_none());
        assert_eq!(parent_of("12 (a) b) S 7 1"), Some(7));
    }
}
