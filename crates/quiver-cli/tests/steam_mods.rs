//! Mods for a Steam game launched through `quiver steam-run`, end to end (design/STORE-STEAM.md
//! §10): a scratch quiverd (its own XDG dirs, HOME with a fixture Steam tree, perf shm root in
//! a temp folder), a mod installed with `quiver mods steam:<appid>`, and a shell script standing
//! in for Steam's `%command%`. The script reads files the overlay adds and replaces in the
//! install folder, writes one, and runs a nested `bwrap --unshare-user --bind` (when bwrap is
//! installed) the way pressure-vessel does, recording what each saw. No real Steam file is read
//! or written, Steam isn't started, no game runs.
//!
//! Needs `quiverd` built next to `quiver` (`cargo test --workspace` builds both), and user
//! namespaces that can mount overlayfs; skipped otherwise (`QUIVER_REQUIRE_OVERLAY=1` makes that
//! a failure).

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

const APP: u32 = 292030;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn home(&self) -> PathBuf {
        self.dir("home")
    }

    fn steam(&self) -> PathBuf {
        self.home().join(".local/share/Steam")
    }

    fn install(&self) -> PathBuf {
        self.steam().join("steamapps/common/The Witcher 3")
    }

    fn prefix(&self) -> PathBuf {
        self.steam().join(format!("steamapps/compatdata/{APP}/pfx"))
    }

    fn data(&self) -> PathBuf {
        self.dir("data/quiver")
    }

    fn isolate(&self, c: &mut Command) {
        c.env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("HOME", self.home())
            .env("QUIVER_BROKER", "127.0.0.1:0")
            // Never the user's /dev/shm/quiver-<uid>: it holds live perf sessions.
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_SIGHT_SPAWN", "0")
            .env("RUST_LOG", "warn")
            .env_remove("QUIVER_SOURCE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("QUIVER_TEST_SESSION")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("SteamAppId")
            .env_remove("STEAM_COMPAT_DATA_PATH")
            // A `systemctl` that does nothing: never the user's own quiverd unit.
            .env("PATH", format!("{}:{}", self.dir("bin").display(), std::env::var("PATH").unwrap_or_default()));
    }

    fn daemon(&self, quiverd: &Path) -> Daemon {
        let mut c = Command::new(quiverd);
        self.isolate(&mut c);
        c.stdout(std::process::Stdio::null()).stderr(std::fs::File::create(self.dir("quiverd.log")).unwrap());
        let child = Daemon(c.spawn().unwrap());
        let sock = self.dir("run").join("quiver/quiverd.sock");
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "quiverd didn't start: {}", std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }

    fn quiver(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
        self.isolate(&mut c);
        let out = c.args(args).output().unwrap();
        assert!(out.status.success(), "quiver {args:?} failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        out
    }

    fn stdout(&self, args: &[&str]) -> String {
        let o = self.quiver(args);
        format!("{}{}", String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.quiver(args).stdout).unwrap()
    }

    /// `quiver steam-run --app APP` as Steam starts it: its environment, the install folder
    /// as working directory, and the stand-in for Steam's command. In a process group of its
    /// own (as Steam's launch is), so the test can signal it like Steam's Stop does.
    fn steam_run(&self, extra: &[&str], record: &Path, vars: &[(&str, &str)]) -> Child {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
        self.isolate(&mut c);
        c.env("SteamAppId", APP.to_string())
            .env("STEAM_COMPAT_DATA_PATH", self.steam().join(format!("steamapps/compatdata/{APP}")))
            .env("STEAM_COMPAT_INSTALL_PATH", self.install())
            .env("QUIVER_TEST_INSTALL", self.install())
            .current_dir(self.install())
            .args(["--source", "steam", "steam-run", "--app", &APP.to_string()])
            .args(extra)
            .arg("/bin/sh")
            .arg(self.dir("fake-steam-command.sh"))
            .arg(record)
            .arg("-steam")
            .stderr(std::fs::File::create(record.with_extension("stderr")).unwrap())
            .process_group(0);
        for (k, v) in vars {
            c.env(k, v);
        }
        c.spawn().unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// A Steam tree with one Proton game (The Witcher 3) and one user.
fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let e = Env { root: tmp.path().to_owned(), _tmp: tmp };
    write(&e.dir("data/quiver/accounts.json"), r#"{"accounts":[]}"#);
    let cfg = serde_json::json!({
        "library": e.dir("lib"), "prefixes": e.dir("pfx"), "proton": e.dir("GE-Proton11-12"), "sdk_source": e.dir("sdk"),
        "save_backups": e.dir("backups"), "report_playtime": false, "steam_offers": false, "env": []
    });
    write(&e.dir("data/quiver/config.json"), &cfg.to_string());
    let steam = e.steam();
    write(&steam.join("config/loginusers.vdf"), "\"users\"\n{\n\t\"76561197960266729\"\n\t{\n\t\t\"AccountName\"\t\t\"alice\"\n\t\t\"PersonaName\"\t\t\"Alice\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t}\n}\n");
    write(&steam.join("steamapps/libraryfolders.vdf"), &format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n", steam.display()));
    write(
        &steam.join(format!("steamapps/appmanifest_{APP}.acf")),
        &format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"{APP}\"\n\t\"name\"\t\t\"The Witcher 3\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"The Witcher 3\"\n\t\"LastOwner\"\t\t\"76561197960266729\"\n\t\"SizeOnDisk\"\t\t\"1000\"\n}}\n"),
    );
    write(&e.install().join("bin/x64/witcher3.exe"), "MZ");
    write(&e.install().join("content/intro.usm"), "vanilla intro");
    write(&e.prefix().join("drive_c/users/steamuser/Documents/The Witcher 3/user.settings"), "[Visuals]\nMotionBlur=true\n");
    write(&e.steam().join("userdata/1001/config/localconfig.vdf"), "\"UserLocalConfigStore\"\n{\n}\n");
    std::fs::create_dir_all(e.dir("shm")).unwrap();
    let fake = e.dir("bin/systemctl");
    write(&fake, "#!/bin/sh\nexit 1\n");
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // The mod: replaces the intro, adds a file, sets a launch argument and patches a config
    // file in Steam's prefix.
    write(
        &e.dir("w3-mod/quiver-mod.toml"),
        "id = \"w3-test\"\nname = \"Test mod\"\nsave_compatible = true\ngames = [\"steam:292030\"]\n[launch]\nargs = [\"-net\"]\n[[patch]]\nfile = \"drive_c/users/steamuser/Documents/The Witcher 3/user.settings\"\nsection = \"Visuals\"\nset = { MotionBlur = \"false\" }\n",
    );
    write(&e.dir("w3-mod/files/content/intro.usm"), "modded intro");
    write(&e.dir("w3-mod/files/content/hud.bundle"), "hud from the mod");
    // Stands in for Steam's reaper, pressure-vessel and the game: records what it sees.
    write(
        &e.dir("fake-steam-command.sh"),
        r#"#!/bin/sh
out="$1"; shift
i="$QUIVER_TEST_INSTALL"
{
  echo "pid=$$"
  echo "mnt=$(readlink /proc/self/ns/mnt)"
  echo "cwd=$(pwd)"
  echo "intro=$(cat content/intro.usm)"
  echo "hud=$(cat "$i/content/hud.bundle" 2>/dev/null || echo none)"
  if command -v bwrap >/dev/null 2>&1; then
    echo "bwrap=$(bwrap --unshare-user --ro-bind / / --bind "$i" "$i" cat "$i/content/intro.usm" 2>&1)"
  fi
} > "$out.tmp"
echo "written by the game" > "$i/content/written.txt"
printf '%s\n' "$@" > "$out.args"
mv "$out.tmp" "$out"
if [ -n "$QUIVER_TEST_EXIT" ]; then sleep 1; exit "$QUIVER_TEST_EXIT"; fi
exec sleep "${QUIVER_TEST_SLEEP:-2}"
"#,
    );
    e
}

fn fields(path: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(path).unwrap().lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

fn wait_for(path: &Path, child: &mut Child) {
    let t = Instant::now();
    while !path.exists() {
        if let Ok(Some(st)) = child.try_wait() {
            panic!("steam-run exited early ({st}): {}", std::fs::read_to_string(path.with_extension("stderr")).unwrap_or_default());
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the command never ran: {}", std::fs::read_to_string(path.with_extension("stderr")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every process's pid whose mount namespace is `mnt` (`mnt:[…]`).
fn in_namespace(mnt: &str) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| std::fs::read_link(format!("/proc/{pid}/ns/mnt")).is_ok_and(|l| l.to_string_lossy() == mnt))
        .collect()
}

/// The children of `pid`, over all its threads.
fn children(pid: u32) -> Vec<u32> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|t| std::fs::read_to_string(t.path().join("children")).unwrap_or_default().split_whitespace().filter_map(|p| p.parse().ok()).collect::<Vec<u32>>())
        .collect()
}

fn wait_gone(mnt: &str) {
    let t = Instant::now();
    while !in_namespace(mnt).is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "processes left in the mods' namespace: {:?}", in_namespace(mnt));
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_no_session(e: &Env) {
    let game = format!("steam:{APP}");
    let t = Instant::now();
    while e.json(&["sessions", "--json"]).as_array().unwrap().iter().any(|s| s["game"] == game.as_str()) {
        assert!(t.elapsed() < Duration::from_secs(10), "the session didn't end");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_steam_games_mods_reach_steams_command_through_the_namespace() {
    let quiverd = Path::new(env!("CARGO_BIN_EXE_quiver")).with_file_name("quiverd");
    if !quiverd.is_file() {
        eprintln!("skipped: {} isn't built (cargo test --workspace builds it)", quiverd.display());
        return;
    }
    if !quiver_overlay_works() {
        assert!(std::env::var_os("QUIVER_REQUIRE_OVERLAY").is_none(), "user namespaces can't mount overlays here");
        eprintln!("skipped: user namespaces can't mount overlays here");
        return;
    }
    let e = setup();
    let _d = e.daemon(&quiverd);
    let game = format!("steam:{APP}");
    let own_mnt = std::fs::read_link("/proc/self/ns/mnt").unwrap().to_string_lossy().into_owned();

    // The mod and a profile, through `quiver mods steam:<appid>`.
    let out = e.stdout(&["mods", &game, "install", e.dir("w3-mod").to_str().unwrap()]);
    assert!(out.contains("Test mod"), "{out}");
    let out = e.stdout(&["mods", &game, "profile", "create", "Modded", "--layers", "w3-test"]);
    assert!(out.contains("Modded"), "{out}");
    let out = e.stdout(&["mods", &game, "default", "Modded"]);
    assert!(out.contains("Play in Steam runs Modded") && out.contains("real saves") && out.contains("steam hook"), "{out}");
    let hist = std::fs::read_to_string(e.data().join(format!("games/steam-{APP}/history.jsonl"))).unwrap();
    let s: Value = serde_json::from_str(hist.lines().last().unwrap()).unwrap();
    let profile_id = s["settings"]["steam_profile"].as_str().expect("stored as steam_profile").to_owned();
    assert!(s["settings"].get("profile").is_none(), "{s}");
    assert!(e.stdout(&["get", &game]).contains(&profile_id));
    let list = e.stdout(&["mods", &game, "list"]);
    assert!(list.contains(&format!("The Witcher 3 ({game})")) && list.contains("Steam Cloud"), "{list}");
    // Ubisoft-shaped folders stay untouched; the game's own are `steam-<appid>`.
    assert!(e.data().join(format!("games/steam-{APP}/profiles/{profile_id}.jsonl")).is_file());
    assert!(e.data().join(format!("mods/steam-{APP}/layers")).is_dir());

    // 1. The profile from `steam_profile`: overlays, so Steam's command is spawned inside the
    //    namespace and sees the mod's files; nested bwrap (pressure-vessel) does too.
    let rec = e.dir("modded");
    let mut launch = e.steam_run(&[], &rec, &[("QUIVER_TEST_SLEEP", "3")]);
    let pid = launch.id();
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert_eq!(f["intro"], "modded intro", "the overlay replaced the file: {f:?}");
    assert_eq!(f["hud"], "hud from the mod", "and added one");
    assert_eq!(f["cwd"], e.install().to_string_lossy(), "Steam's working directory, as seen inside");
    assert_ne!(f["mnt"], own_mnt, "its own mount namespace");
    assert_ne!(f["pid"], pid.to_string(), "spawned: steam-run stays the parent");
    if let Some(b) = f.get("bwrap") {
        assert_eq!(b, "modded intro", "a nested bwrap --unshare-user --bind sees the overlay");
    } else {
        eprintln!("note: bwrap isn't installed; the pressure-vessel stand-in was skipped");
    }
    assert_eq!(std::fs::read_to_string(rec.with_extension("args")).unwrap(), "-steam\n-net\n", "the mod's launch argument");
    // The holder is gone: steam-run's one child is Steam's command.
    assert_eq!(children(pid), [f["pid"].parse::<u32>().unwrap()], "only Steam's command under steam-run");
    // A session while it runs, with the profile.
    let sessions = e.json(&["sessions", "--json"]);
    let s = sessions.as_array().unwrap().iter().find(|s| s["game"] == game.as_str()).unwrap_or_else(|| panic!("no session: {sessions}"));
    assert_eq!((s["pids"][0].as_u64(), s["profile"].as_str()), (Some(pid as u64), Some(profile_id.as_str())), "{s}");
    let st = launch.wait().unwrap();
    assert!(st.success(), "{st}: {}", std::fs::read_to_string(rec.with_extension("stderr")).unwrap_or_default());
    wait_gone(&f["mnt"]);
    // The real install never changed; the game's write went to the profile's upper layer.
    assert_eq!(std::fs::read_to_string(e.install().join("content/intro.usm")).unwrap(), "vanilla intro");
    assert!(!e.install().join("content/written.txt").exists());
    assert!(e.data().join(format!("mods/steam-{APP}/profiles/{profile_id}/upper/content/written.txt")).is_file());
    // The config patch went to Steam's prefix, recorded the Quiver way.
    let settings = e.prefix().join("drive_c/users/steamuser/Documents/The Witcher 3/user.settings");
    assert!(std::fs::read_to_string(&settings).unwrap().contains("MotionBlur=false"));
    assert!(e.data().join(format!("mods/steam-{APP}/patches.json")).is_file());
    // The play history and the activity log say what ran.
    wait_no_session(&e);
    let history = std::fs::read_to_string(e.data().join(format!("games/steam-{APP}/playtime.jsonl"))).unwrap();
    let r: Value = serde_json::from_str(history.lines().last().unwrap()).unwrap();
    assert_eq!(r["profile"].as_str(), Some(profile_id.as_str()), "{r}");
    assert_eq!(r["mods"][0]["name"].as_str(), Some("Test mod"), "{r}");
    let activity = std::fs::read_to_string(e.data().join("activity.jsonl")).unwrap();
    assert!(activity.contains("Steam launch of The Witcher 3 with profile 'Modded'") && activity.contains("from Steam with profile"), "{activity}");
    let log = std::fs::read_to_string(e.data().join(format!("steam-run/{APP}/steam-run.log"))).unwrap();
    assert!(log.contains("mods: profile 'Modded'") && log.contains("spawn (in the mods' namespace)") && log.contains("mounted 1 overlay"), "{log}");

    // 2. `--profile vanilla` from the launch options: nothing to mount, so `exec` as before
    //    (same pid, the host's view), and the config patch is put back.
    let rec = e.dir("vanilla");
    let mut launch = e.steam_run(&["--profile", "vanilla"], &rec, &[]);
    let pid = launch.id();
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert_eq!((f["intro"].as_str(), f["hud"].as_str()), ("vanilla intro", "none"), "{f:?}");
    assert_eq!((f["pid"].clone(), f["mnt"].clone()), (pid.to_string(), own_mnt.clone()), "exec: the pid and the host's mounts");
    assert!(launch.wait().unwrap().success());
    assert!(std::fs::read_to_string(&settings).unwrap().contains("MotionBlur=true"), "vanilla puts the file's own value back");
    std::fs::remove_file(e.install().join("content/written.txt")).unwrap();
    wait_no_session(&e);

    // 3. Steam's Stop: SIGTERM to the launch's process group ends the game, and steam-run
    //    exits the way its child did.
    let rec = e.dir("stopped");
    let mut launch = e.steam_run(&[], &rec, &[("QUIVER_TEST_SLEEP", "60")]);
    let pid = launch.id();
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert!(Command::new("kill").args(["-TERM", &format!("-{pid}")]).status().unwrap().success());
    let st = launch.wait().unwrap();
    assert_eq!(st.signal(), Some(libc_sigterm()), "steam-run ends by the same signal: {st}");
    wait_gone(&f["mnt"]);
    wait_no_session(&e);

    // 4. A signal to steam-run alone is passed on to Steam's command.
    let rec = e.dir("forwarded");
    let mut launch = e.steam_run(&[], &rec, &[("QUIVER_TEST_SLEEP", "60")]);
    let pid = launch.id();
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert!(Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap().success());
    let st = launch.wait().unwrap();
    assert_eq!(st.signal(), Some(libc_sigterm()), "{st}");
    wait_gone(&f["mnt"]);
    wait_no_session(&e);

    // 5. The exit status passes through.
    let rec = e.dir("exit7");
    let mut launch = e.steam_run(&[], &rec, &[("QUIVER_TEST_EXIT", "7")]);
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert_eq!(launch.wait().unwrap().code(), Some(7));
    wait_gone(&f["mnt"]);
    wait_no_session(&e);

    // 6. steam-run killed outright: its child is told to end (parent-death signal), so
    //    nothing outlives it in the namespace.
    let rec = e.dir("killed");
    let mut launch = e.steam_run(&[], &rec, &[("QUIVER_TEST_SLEEP", "60")]);
    let pid = launch.id();
    wait_for(&rec, &mut launch);
    let f = fields(&rec);
    assert!(Command::new("kill").args(["-KILL", &pid.to_string()]).status().unwrap().success());
    let _ = launch.wait();
    wait_gone(&f["mnt"]);
    wait_no_session(&e);
}

fn libc_sigterm() -> i32 {
    15
}

/// Whether this machine lets an unprivileged process mount overlayfs in a user namespace.
fn quiver_overlay_works() -> bool {
    let t = tempfile::tempdir().unwrap();
    for d in ["lower", "upper", "work", "target"] {
        std::fs::create_dir_all(t.path().join(d)).unwrap();
    }
    // The mechanism steam-run uses.
    let spec = quiver_core::overlay::OverlaySpec { target: t.path().join("target"), lower: vec![t.path().join("lower")], upper: t.path().join("upper"), work: t.path().join("work") };
    quiver_core::overlay::Namespace::create(&spec).is_ok()
}

