//! quiverd as a systemd user service (`contrib/quiverd.service`, design/SERVICE.md).
//!
//! - **The unit** `quiverd.service` runs quiverd in `quiver.slice`, never in a terminal's
//!   scope (where systemd-oomd or closing the terminal would take it, and the games it
//!   started, along). Games and stream hosts get scopes of their own under the same slice.
//! - **Starting it** ([`start_daemon`]): the person's own daemon (default socket and data
//!   folders, [`default_instance`]) is started as the unit when it is installed, else as a
//!   transient `quiverd.service` (`systemd-run --user --unit=quiverd`). Any other daemon (a
//!   scratch one with its own folders) gets a scope of its own under `quiver.slice`, and only
//!   without systemd is it a plain child of the caller.
//! - **Boot**: enabled, the unit is wanted by `default.target`; with lingering on, the user
//!   manager (and so quiverd) starts at boot without anyone signing in ([`BootStatus`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The unit's name (kept stable: install scripts, packages and people's `systemctl` habits
/// name it).
pub const UNIT: &str = "quiverd.service";
/// The slice quiverd, its games and its stream hosts run in.
pub const SLICE: &str = "quiver.slice";
/// quiverd's exit status when another daemon already serves its socket. Not a failure:
/// the unit names it in `SuccessExitStatus=` and `RestartPreventExitStatus=`.
pub const EXIT_ALREADY_RUNNING: i32 = 75;
/// Live tests only: the unit a scratch daemon treats as its own (must start with
/// `quiver-test-`; a scratch daemon is never the person's [`UNIT`]).
pub const TEST_UNIT_ENV: &str = "QUIVER_TEST_UNIT";
/// Variables a desktop session sets that games need (their display), refreshed from the
/// user manager for launches when quiverd started before the session (at boot).
pub const SESSION_VARS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The unit this daemon is, or would be, in: [`UNIT`], or a live test's scratch unit.
pub fn unit_name() -> String {
    test_unit().unwrap_or_else(|| UNIT.to_owned())
}

/// A live test's scratch unit (`QUIVER_TEST_UNIT=quiver-test-….service`).
pub fn test_unit() -> Option<String> {
    std::env::var(TEST_UNIT_ENV).ok().filter(|u| u.starts_with("quiver-test-") && u.ends_with(".service"))
}

/// The leaf unit of a `/proc/<pid>/cgroup` (cgroup v2: the `0::` line), such as
/// `quiverd.service` or `kitty-6428-0.scope`.
pub fn cgroup_unit(text: &str) -> Option<&str> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    path.rsplit('/').next().filter(|u| u.contains('.'))
}

/// A process's cgroup path (cgroup v2).
pub fn cgroup_of(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    text.lines().find_map(|l| l.strip_prefix("0::")).map(str::to_owned)
}

/// The leaf unit this process runs in.
pub fn current_unit() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup_unit(&text).map(str::to_owned)
}

/// `/run/user/<uid>`: where the user manager and its bus live, whatever `XDG_RUNTIME_DIR`
/// a scratch process has.
pub fn real_runtime_dir() -> PathBuf {
    // SAFETY: getuid has no failure modes.
    PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
}

/// This user's name and home folder from the password database.
pub fn passwd() -> Option<(String, PathBuf)> {
    use std::ffi::CStr;
    let mut buf = vec![0u8; 16 * 1024];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pw` and `buf`, both ours and large enough (else ERANGE).
    let r = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    // SAFETY: on success both point into `buf`, NUL-terminated.
    let name = unsafe { CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned();
    let home = unsafe { CStr::from_ptr(pw.pw_dir) }.to_string_lossy().into_owned();
    Some((name, PathBuf::from(home)))
}

/// Whether an environment (`vars`) is the person's default daemon's: the socket and data
/// folders the unit's quiverd uses (`HOME` from the password database, `XDG_RUNTIME_DIR`
/// `/run/user/<uid>`, the XDG base folders unset or at their defaults) and no `QUIVER_*`
/// setting. `Err`: why not. Only such a daemon is started as, or moved into, the unit; any
/// other is someone's scratch daemon.
pub fn default_instance_in(vars: &[(String, String)], runtime: &Path, home: &Path) -> Result<(), String> {
    let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty());
    let same = |a: &str, b: &Path| Path::new(a.trim_end_matches('/')) == b;
    match get("XDG_RUNTIME_DIR") {
        Some(r) if same(r, runtime) => {}
        Some(r) => return Err(format!("XDG_RUNTIME_DIR is {r}, not {}", runtime.display())),
        None => return Err("XDG_RUNTIME_DIR is not set".into()),
    }
    match get("HOME") {
        Some(h) if same(h, home) => {}
        Some(h) => return Err(format!("HOME is {h}, not {}", home.display())),
        None => return Err("HOME is not set".into()),
    }
    for (k, default) in [("XDG_DATA_HOME", ".local/share"), ("XDG_CONFIG_HOME", ".config"), ("XDG_STATE_HOME", ".local/state"), ("XDG_CACHE_HOME", ".cache")] {
        if let Some(v) = get(k)
            && !same(v, &home.join(default))
        {
            return Err(format!("{k} is {v}"));
        }
    }
    if let Some((k, _)) = vars.iter().find(|(k, _)| k.starts_with("QUIVER_") || k == "UBID_BROKER") {
        return Err(format!("{k} is set"));
    }
    Ok(())
}

/// [`default_instance_in`] for this process.
pub fn default_instance() -> Result<(), String> {
    let vars: Vec<(String, String)> = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
    let home = passwd().map(|(_, h)| h).ok_or("no password database entry")?;
    default_instance_in(&vars, &real_runtime_dir(), &home)
}

/// The unit as the user manager has it (`systemctl --user show`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitState {
    /// `loaded`, `not-found`, `masked`, …
    pub load: String,
    /// `active`, `inactive`, `failed`, `activating`, …
    pub active: String,
    /// `enabled`, `disabled`, `static`, `linked`, … (empty for a transient unit)
    pub file: String,
}

impl UnitState {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("LoadState", v)) => s.load = v.trim().to_owned(),
                Some(("ActiveState", v)) => s.active = v.trim().to_owned(),
                Some(("UnitFileState", v)) => s.file = v.trim().to_owned(),
                _ => {}
            }
        }
        s
    }

    /// The unit file is installed (or a transient one runs).
    pub fn loaded(&self) -> bool {
        self.load == "loaded"
    }

    pub fn enabled(&self) -> bool {
        matches!(self.file.as_str(), "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias")
    }
}

/// How a client starts the daemon nobody is listening for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    /// `systemctl --user start quiverd.service`.
    Unit,
    /// `systemd-run --user --unit=quiverd --slice=quiver.slice …`: a transient service with
    /// the unit's settings (no unit file installed).
    Transient,
    /// `systemd-run --user --scope --slice=quiver.slice`: a scratch daemon, with the caller's
    /// environment, in a scope of its own.
    Scope,
    /// A plain child of the caller (no systemd).
    Spawn,
}

/// The choice, from what is known: the daemon wanted is the person's default one, the
/// unit's state (`None`: the user manager can't be reached), and whether `systemd-run` can
/// make scopes.
pub fn choose(default: bool, unit: Option<&UnitState>, systemd_run: bool) -> Autostart {
    if default {
        match unit {
            Some(u) if u.loaded() => return Autostart::Unit,
            // Masked: the person doesn't want the unit; never start it under its name.
            Some(u) if systemd_run && u.load != "masked" => return Autostart::Transient,
            _ => {}
        }
    }
    if systemd_run && unit.is_some() { Autostart::Scope } else { Autostart::Spawn }
}

/// The systemd tools, found on a `PATH` (tests give one with fakes).
#[derive(Debug, Clone)]
pub struct Systemd {
    path: OsString,
}

impl Systemd {
    pub fn from_env() -> Self {
        Self { path: std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()) }
    }

    pub fn with_path(path: impl Into<OsString>) -> Self {
        Self { path: path.into() }
    }

    pub fn tool(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path).map(|d| d.join(name)).find(|p| is_executable(p))
    }

    /// A command for `name` that reaches the user manager (through the real runtime folder,
    /// even from a scratch process).
    fn cmd(&self, name: &str) -> Option<Command> {
        let mut c = Command::new(self.tool(name)?);
        c.env("XDG_RUNTIME_DIR", real_runtime_dir()).env("PATH", &self.path).stdin(Stdio::null());
        Some(c)
    }

    /// `systemctl --user show` for `unit`; `None`: no systemctl or no user manager.
    pub fn unit_state(&self, unit: &str) -> Option<UnitState> {
        let out = self.cmd("systemctl")?.args(["--user", "show", "-p", "LoadState", "-p", "ActiveState", "-p", "UnitFileState", "--", unit]).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| UnitState::parse(&String::from_utf8_lossy(&out.stdout))).filter(|s| !s.load.is_empty())
    }

    /// `systemctl --user <verb> [flags] <unit>`, failing with its message.
    pub fn systemctl(&self, verb: &str, flags: &[&str], unit: &str) -> Result<()> {
        let mut c = self.cmd("systemctl").context("systemctl not found")?;
        c.args(["--user", verb]).args(flags).args(["--", unit]);
        run(c, &format!("systemctl --user {verb} {unit}"))
    }

    /// Whether `systemd-run` can reach the user manager's bus.
    pub fn can_scope(&self) -> bool {
        self.tool("systemd-run").is_some() && (real_runtime_dir().join("bus").exists() || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some())
    }

    /// The argv prefix that runs a command in a new scope `unit` (a `.scope` name, or
    /// systemd's own) under [`SLICE`]: `systemd-run --user --scope … --`. `systemd-run`
    /// execs the command in place, so its pid, environment and descriptors are the
    /// caller's. Give the command the real runtime folder (`XDG_RUNTIME_DIR`, for the
    /// bus); `restore_runtime` is the one the command itself gets back.
    pub fn scope_argv(&self, unit: Option<&str>, restore_runtime: Option<&str>) -> Option<Vec<String>> {
        let run = self.tool("systemd-run")?;
        let mut a = vec![run.to_string_lossy().into_owned(), "--user".into(), "--scope".into(), "--collect".into(), "--quiet".into(), format!("--slice={SLICE}")];
        if let Some(u) = unit {
            a.push(format!("--unit={u}"));
        }
        if let Some(r) = restore_runtime {
            a.push(format!("--setenv=XDG_RUNTIME_DIR={r}"));
        }
        a.push("--".into());
        Some(a)
    }

    /// `systemd-run` arguments for a transient `quiverd.service` running `exe` with the
    /// unit's settings, the caller's session variables passed on.
    pub fn transient_argv(&self, exe: &Path, env: &[(String, String)]) -> Option<Vec<String>> {
        let run = self.tool("systemd-run")?;
        let mut a: Vec<String> = vec![run.to_string_lossy().into_owned(), "--user".into(), "--unit=quiverd".into(), format!("--slice={SLICE}")];
        a.extend(
            [
                "--description=Quiver launcher daemon (quiverd)",
                "--collect",
                "--quiet",
                "--property=Type=notify",
                "--property=KillMode=process",
                "--property=Restart=on-failure",
                "--property=RestartSec=2",
                "--property=TimeoutStartSec=60",
            ]
            .map(String::from),
        );
        a.push(format!("--property=SuccessExitStatus={EXIT_ALREADY_RUNNING}"));
        a.push(format!("--property=RestartPreventExitStatus={EXIT_ALREADY_RUNNING}"));
        for (k, v) in env.iter().filter(|(k, _)| SESSION_VARS.contains(&k.as_str())) {
            a.push(format!("--setenv={k}={v}"));
        }
        a.push("--".into());
        a.push(exe.to_string_lossy().into_owned());
        Some(a)
    }

    /// The user manager's environment (`systemctl --user show-environment`), the plain
    /// `KEY=value` lines (values systemctl had to quote are left out).
    pub fn manager_environment(&self) -> Option<Vec<(String, String)>> {
        let out = self.cmd("systemctl")?.args(["--user", "show-environment"]).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| parse_environment(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// `systemctl show-environment` output as pairs, without `$'…'`-quoted values.
pub fn parse_environment(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(|l| l.split_once('=')).filter(|(_, v)| !v.starts_with("$'")).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run `c` to the end, failing with its stderr.
fn run(mut c: Command, what: &str) -> Result<()> {
    let out = c.stdout(Stdio::null()).stderr(Stdio::piped()).output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        bail!("{what} failed{}", if err.is_empty() { String::new() } else { format!(": {err}") });
    }
    Ok(())
}

/// Start the daemon for `socket` (nobody listens there): see the module docs. `exe` is the
/// quiverd to run, `log` where a scope or a child's output goes. Returns how.
pub fn start_daemon(sd: &Systemd, socket: &Path, exe: &Path, log: &Path) -> Result<Autostart> {
    let default = socket == crate::socket_path() && default_instance().is_ok();
    start_with(sd, default, exe, log)
}

/// [`start_daemon`] once it is known whether the daemon is the person's default one.
fn start_with(sd: &Systemd, default: bool, exe: &Path, log: &Path) -> Result<Autostart> {
    let unit = sd.unit_state(UNIT);
    let how = choose(default, unit.as_ref(), sd.can_scope());
    tracing::debug!(?how, default, ?unit, "starting quiverd");
    match how {
        Autostart::Unit => match sd.systemctl("start", &[], UNIT) {
            Ok(()) => return Ok(how),
            // A unit that won't start (start limit hit, a broken file): still start one, in
            // a scope; it moves into the unit by itself once that works and it is idle.
            Err(e) => tracing::warn!("{e:#}; starting quiverd in a scope instead"),
        },
        Autostart::Transient => {
            let env: Vec<(String, String)> = std::env::vars().collect();
            let argv = sd.transient_argv(exe, &env).context("systemd-run not found")?;
            let mut c = sd.cmd("systemd-run").context("systemd-run not found")?;
            c.args(&argv[1..]);
            match run(c, "systemd-run --user --unit=quiverd") {
                Ok(()) => return Ok(how),
                Err(e) => tracing::warn!("{e:#}; starting quiverd in a scope instead"),
            }
        }
        Autostart::Scope | Autostart::Spawn => {}
    }
    spawn(sd, exe, log)
}

/// Start `exe` in the background in a scope of its own under [`SLICE`] when systemd can,
/// else as a plain child in its own process group. Output goes to `log`.
fn spawn(sd: &Systemd, exe: &Path, log: &Path) -> Result<Autostart> {
    use std::os::unix::process::CommandExt;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let ours = std::env::var("XDG_RUNTIME_DIR").ok();
    let real = real_runtime_dir();
    let restore = ours.as_deref().filter(|r| Path::new(r) != real);
    let scope = if sd.can_scope() && sd.unit_state(UNIT).is_some() { sd.scope_argv(None, restore) } else { None };
    let how = if scope.is_some() { Autostart::Scope } else { Autostart::Spawn };
    let mut cmd = match &scope {
        Some(a) => {
            let mut c = Command::new(&a[0]);
            c.args(&a[1..]).arg(exe).env("XDG_RUNTIME_DIR", &real);
            c
        }
        None => Command::new(exe),
    };
    cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    cmd.process_group(0); // survive the client's terminal closing (and its Ctrl-C)
    match cmd.spawn() {
        Ok(mut child) => {
            // Reap it: systemd-run execs quiverd in place, so this is the daemon itself.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(how)
        }
        Err(e) => bail!("could not start {}: {e}", exe.display()),
    }
}

/// Whether the daemon starts at boot, and why not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootStatus {
    /// `quiverd.service` is installed.
    pub installed: bool,
    /// It is enabled (wanted by `default.target`): it starts when the user manager does.
    pub enabled: bool,
    /// Lingering is on: the user manager starts at boot, without anyone signing in.
    pub linger: bool,
    /// The daemon answering runs as the unit (else it moves in once idle).
    #[serde(default)]
    pub in_unit: bool,
    /// The command for the person to run when Quiver couldn't turn lingering on itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger_command: Option<String>,
    /// Anything else worth saying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl BootStatus {
    /// Both halves: the unit enabled and lingering on.
    pub fn at_boot(&self) -> bool {
        self.enabled && self.linger
    }

    /// "yes (linger on)", "no (linger off)", "at login only (linger off)".
    pub fn describe(&self) -> String {
        let linger = if self.linger { "linger on" } else { "linger off" };
        match (self.installed, self.enabled, self.linger) {
            (false, _, _) => format!("no: {UNIT} isn't installed ({linger})"),
            (true, true, true) => format!("yes ({linger})"),
            (true, true, false) => format!("at sign-in only ({linger})"),
            (true, false, _) => format!("no ({linger})"),
        }
    }
}

/// Lingering for `user`: `/var/lib/systemd/linger/<user>` exists.
pub fn linger(user: &str) -> bool {
    Path::new("/var/lib/systemd/linger").join(user).exists()
}

/// The command a person runs to turn lingering on when they may not themselves.
pub fn linger_command(user: &str) -> String {
    format!("sudo loginctl enable-linger {user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn cgroup_leaf() {
        assert_eq!(cgroup_unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/kitty-6428-0.scope\n"), Some("kitty-6428-0.scope"));
        assert_eq!(cgroup_unit("0::/user.slice/user-1000.slice/user@1000.service/quiver.slice/quiverd.service\n"), Some("quiverd.service"));
        // cgroup v1 lines first, then the unified one.
        assert_eq!(cgroup_unit("12:pids:/x\n0::/a/b/quiverd.service\n"), Some("quiverd.service"));
        assert_eq!(cgroup_unit("0::/\n"), None);
        assert_eq!(cgroup_unit(""), None);
    }

    #[test]
    fn default_instance_rules() {
        let rt = Path::new("/run/user/1000");
        let home = Path::new("/home/p");
        let base = [("HOME", "/home/p"), ("XDG_RUNTIME_DIR", "/run/user/1000"), ("PATH", "/usr/bin"), ("RUST_LOG", "debug")];
        assert_eq!(default_instance_in(&vars(&base), rt, home), Ok(()));
        // The XDG folders at their defaults (Omarchy sets them) are still the default.
        let mut v = vars(&base);
        v.extend(vars(&[("XDG_DATA_HOME", "/home/p/.local/share"), ("XDG_CONFIG_HOME", "/home/p/.config/"), ("XDG_STATE_HOME", "/home/p/.local/state"), ("XDG_CACHE_HOME", "/home/p/.cache")]));
        assert_eq!(default_instance_in(&v, rt, home), Ok(()));
        // A scratch daemon's: its own runtime folder, HOME, data folder or settings.
        for (k, val) in [("XDG_RUNTIME_DIR", "/tmp/x/run"), ("HOME", "/tmp/x/home"), ("XDG_DATA_HOME", "/tmp/x/data"), ("QUIVER_SHM_ROOT", "/tmp/x/shm"), ("QUIVER_BROKER", "127.0.0.1:1"), ("UBID_BROKER", "x")] {
            let mut v: Vec<(String, String)> = vars(&base).into_iter().filter(|(n, _)| n != k).collect();
            v.push((k.into(), val.into()));
            assert!(default_instance_in(&v, rt, home).unwrap_err().contains(k), "{k}");
        }
        assert!(default_instance_in(&vars(&[("HOME", "/home/p")]), rt, home).is_err(), "no runtime folder");
    }

    #[test]
    fn unit_state_parse() {
        let s = UnitState::parse("LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n");
        assert!(s.loaded() && !s.enabled());
        assert_eq!(s.active, "inactive");
        let s = UnitState::parse("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n");
        assert!(!s.loaded());
        assert!(UnitState::parse("UnitFileState=enabled\n").enabled());
    }

    #[test]
    fn autostart_choice() {
        let loaded = UnitState { load: "loaded".into(), ..Default::default() };
        let missing = UnitState { load: "not-found".into(), ..Default::default() };
        let masked = UnitState { load: "masked".into(), ..Default::default() };
        assert_eq!(choose(true, Some(&loaded), true), Autostart::Unit);
        assert_eq!(choose(true, Some(&loaded), false), Autostart::Unit);
        assert_eq!(choose(true, Some(&missing), true), Autostart::Transient);
        assert_eq!(choose(true, Some(&masked), true), Autostart::Scope);
        assert_eq!(choose(true, Some(&missing), false), Autostart::Spawn);
        // No user manager: a plain child.
        assert_eq!(choose(true, None, true), Autostart::Spawn);
        // A scratch daemon is never the unit, and never the transient quiverd.service.
        assert_eq!(choose(false, Some(&loaded), true), Autostart::Scope);
        assert_eq!(choose(false, Some(&missing), true), Autostart::Scope);
        assert_eq!(choose(false, Some(&loaded), false), Autostart::Spawn);
    }

    #[test]
    fn argvs() {
        let dir = tempfile::tempdir().unwrap();
        fake(dir.path(), "systemd-run", "exit 0");
        let sd = Systemd::with_path(dir.path());
        let run = dir.path().join("systemd-run").display().to_string();
        let a = sd.scope_argv(Some("quiver-game-x.scope"), Some("/tmp/s/run")).unwrap();
        assert_eq!(a, [&run, "--user", "--scope", "--collect", "--quiet", "--slice=quiver.slice", "--unit=quiver-game-x.scope", "--setenv=XDG_RUNTIME_DIR=/tmp/s/run", "--"]);
        let env = vars(&[("WAYLAND_DISPLAY", "wayland-1"), ("SECRET_TOKEN", "x"), ("DISPLAY", ":1")]);
        let a = sd.transient_argv(Path::new("/h/.local/bin/quiverd"), &env).unwrap().join(" ");
        for want in ["--user --unit=quiverd --slice=quiver.slice", "--property=Type=notify", "--property=KillMode=process", "--property=Restart=on-failure", "--property=RestartPreventExitStatus=75", "--setenv=WAYLAND_DISPLAY=wayland-1", "--setenv=DISPLAY=:1", "-- /h/.local/bin/quiverd"] {
            assert!(a.contains(want), "{want} in {a}");
        }
        assert!(!a.contains("SECRET_TOKEN"), "only session variables pass: {a}");
        assert!(Systemd::with_path("/nonexistent").scope_argv(None, None).is_none());
    }

    fn fake(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The tools on a fake PATH: `systemctl` answers `show` as told and logs every call.
    fn fake_systemd(load: &str, start_ok: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let l = log.display();
        let start = if start_ok { "exit 0" } else { "echo 'Job failed' >&2; exit 1" };
        fake(
            dir.path(),
            "systemctl",
            &format!(
                "echo \"systemctl $*\" >> {l}\ncase \"$2\" in\n  show) printf 'LoadState={load}\\nActiveState=inactive\\nUnitFileState=disabled\\n' ;;\n  start) {start} ;;\nesac"
            ),
        );
        fake(dir.path(), "systemd-run", &format!("echo \"systemd-run $*\" >> {l}"));
        (dir, log)
    }

    fn calls(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    #[test]
    fn fake_systemctl_unit_and_transient() {
        let (dir, log) = fake_systemd("loaded", true);
        let sd = Systemd::with_path(dir.path());
        assert!(sd.unit_state(UNIT).unwrap().loaded());
        sd.systemctl("start", &[], UNIT).unwrap();
        assert!(calls(&log).contains("systemctl --user start -- quiverd.service"), "{}", calls(&log));

        let (dir, log) = fake_systemd("loaded", false);
        let err = Systemd::with_path(dir.path()).systemctl("start", &[], UNIT).unwrap_err();
        assert!(format!("{err:#}").contains("Job failed"), "{err:#}");
        drop(log);

        // No systemctl at all: no manager.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(Systemd::with_path(empty.path()).unit_state(UNIT), None);
    }

    /// `start_daemon` for a scratch socket (never the default daemon): a scope when
    /// systemd-run is there, else a plain child; never `systemctl start`.
    #[test]
    fn scratch_daemon_never_starts_the_unit() {
        let (dir, log) = fake_systemd("loaded", true);
        let sd = Systemd::with_path(dir.path());
        let scratch = dir.path().join("run/quiver/quiverd.sock");
        let exe = dir.path().join("quiverd");
        fake(dir.path(), "quiverd", &format!("echo \"quiverd ran\" >> {}", log.display()));
        let how = start_daemon(&sd, &scratch, &exe, &dir.path().join("quiverd.log")).unwrap();
        let want = if sd.can_scope() { Autostart::Scope } else { Autostart::Spawn };
        assert_eq!(how, want);
        // The background child logs once it ran.
        let t = std::time::Instant::now();
        let needle = if how == Autostart::Scope { "systemd-run --user --scope" } else { "quiverd ran" };
        while !calls(&log).contains(needle) {
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "{}", calls(&log));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let c = calls(&log);
        assert!(!c.contains("systemctl --user start"), "{c}");
        if how == Autostart::Scope {
            assert!(c.contains("--slice=quiver.slice") && c.contains(&exe.display().to_string()), "{c}");
        }
    }

    /// The person's default daemon: the unit when installed, a transient quiverd.service
    /// when not, a scope when the unit won't start.
    #[test]
    fn default_daemon_through_systemd() {
        let wait_for = |log: &Path, needle: &str| {
            let t = std::time::Instant::now();
            while !calls(log).contains(needle) {
                assert!(t.elapsed() < std::time::Duration::from_secs(10), "{needle} in {}", calls(log));
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let exe = Path::new("/h/.local/bin/quiverd");

        let (dir, log) = fake_systemd("loaded", true);
        let sd = Systemd::with_path(dir.path());
        assert_eq!(start_with(&sd, true, exe, &dir.path().join("q.log")).unwrap(), Autostart::Unit);
        let c = calls(&log);
        assert!(c.contains("systemctl --user start -- quiverd.service") && !c.contains("systemd-run"), "{c}");

        let (dir, log) = fake_systemd("not-found", true);
        let sd = Systemd::with_path(dir.path());
        assert_eq!(start_with(&sd, true, exe, &dir.path().join("q.log")).unwrap(), Autostart::Transient);
        let c = calls(&log);
        assert!(c.contains("systemd-run --user --unit=quiverd --slice=quiver.slice") && c.contains("-- /h/.local/bin/quiverd"), "{c}");
        assert!(!c.contains("systemctl --user start"), "{c}");

        if sd.can_scope() {
            let (dir, log) = fake_systemd("loaded", false);
            let sd = Systemd::with_path(dir.path());
            assert_eq!(start_with(&sd, true, exe, &dir.path().join("q.log")).unwrap(), Autostart::Scope);
            wait_for(&log, "systemd-run --user --scope");
            assert!(calls(&log).contains("systemctl --user start -- quiverd.service"));
        }
    }

    #[test]
    fn boot_words() {
        let b = |installed, enabled, linger| BootStatus { installed, enabled, linger, ..Default::default() };
        assert_eq!(b(true, true, true).describe(), "yes (linger on)");
        assert!(b(true, true, true).at_boot());
        assert_eq!(b(true, true, false).describe(), "at sign-in only (linger off)");
        assert_eq!(b(true, false, true).describe(), "no (linger on)");
        assert!(b(false, false, false).describe().contains("isn't installed"));
        assert_eq!(linger_command("p"), "sudo loginctl enable-linger p");
    }

    #[test]
    fn manager_environment_lines() {
        let v = parse_environment("PATH=/usr/bin\nWAYLAND_DISPLAY=wayland-1\nWEIRD=$'a\\nb'\n");
        assert_eq!(v, vars(&[("PATH", "/usr/bin"), ("WAYLAND_DISPLAY", "wayland-1")]));
    }
}
