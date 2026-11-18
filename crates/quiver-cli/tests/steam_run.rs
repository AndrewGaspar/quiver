//! `quiver steam-run` for a Steam game that isn't a Ubisoft copy, end to end against a scratch
//! quiverd (its own XDG dirs, HOME and perf shm root in a temp folder) and a fixture Steam
//! tree under that HOME. No real Steam file is read or written, Steam isn't started, no game
//! runs: a shell script stands in for Steam's `%command%` and records what it was started
//! with.
//!
//! Needs `quiverd` built next to `quiver` (`cargo test --workspace` builds both); skipped
//! otherwise.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

const APP: u32 = 870780;

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

    fn localconfig(&self) -> PathBuf {
        self.steam().join("userdata/1001/config/localconfig.vdf")
    }

    /// The scratch environment, for quiverd and for every `quiver` run.
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
            // Nothing on the desktop.
            .env("QUIVER_SIGHT_SPAWN", "0")
            .env("RUST_LOG", "warn")
            .env_remove("QUIVER_SOURCE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("QUIVER_TEST_SESSION")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("SteamAppId")
            .env_remove("STEAM_COMPAT_DATA_PATH")
            // A `systemctl` that does nothing: `steam-run` tries `systemctl --user start
            // quiverd` when the daemon is down, which must never reach the user's own unit.
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
        String::from_utf8(self.quiver(args).stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.quiver(args).stdout).unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// A Steam tree with one Proton game (CONTROL) and one user, as a client leaves it.
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
        &format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"{APP}\"\n\t\"name\"\t\t\"Control\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Control\"\n\t\"LastOwner\"\t\t\"76561197960266729\"\n\t\"SizeOnDisk\"\t\t\"1000\"\n}}\n"),
    );
    write(&steam.join("steamapps/common/Control/Control_DX12.exe"), "MZ");
    std::fs::create_dir_all(steam.join(format!("steamapps/compatdata/{APP}/pfx"))).unwrap();
    std::fs::create_dir_all(e.dir("shm")).unwrap();
    let fake = e.dir("bin/systemctl");
    write(&fake, "#!/bin/sh\nexit 1\n");
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Steam's own launch options for another game, and play time for this one: the hook
    // must leave both alone.
    write(
        &e.localconfig(),
        &format!("\"UserLocalConfigStore\"\n{{\n\t\"Software\"\n\t{{\n\t\t\"Valve\"\n\t\t{{\n\t\t\t\"Steam\"\n\t\t\t{{\n\t\t\t\t\"apps\"\n\t\t\t\t{{\n\t\t\t\t\t\"{APP}\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"Playtime\"\t\t\"600\"\n\t\t\t\t\t}}\n\t\t\t\t\t\"620\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"gamemoderun %command%\"\n\t\t\t\t\t}}\n\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"),
    );
    e
}

#[test]
fn any_steam_game_gets_its_launch_plan_and_a_local_session() {
    let quiverd = Path::new(env!("CARGO_BIN_EXE_quiver")).with_file_name("quiverd");
    if !quiverd.is_file() {
        eprintln!("skipped: {} isn't built (cargo test --workspace builds it)", quiverd.display());
        return;
    }
    let e = setup();
    let _d = e.daemon(&quiverd);
    let game = format!("steam:{APP}");
    let before = std::fs::read_to_string(e.localconfig()).unwrap();

    // Settings, the HUD and a tweak for a Steam game: each says it isn't hooked.
    let out = e.stdout(&["set", &game, "env.PROTON_LOG=1", "wrappers=env QUIVER_TEST_WRAPPED=1", "args=-dx12"]);
    assert!(out.contains("settings revision 1") && out.contains("isn't hooked") && out.contains(&format!("quiver steam hook {game} on")), "{out}");
    // The Sight layer needs no step (it's in every launch); showing the HUD at launch does.
    let out = e.stdout(&["hud", "show", &game]);
    assert!(out.contains("isn't hooked") && out.contains("HUD at launch shown"), "{out}");
    let out = e.stdout(&["tweaks", "set", &game, "tex.anisotropy=16"]);
    assert!(out.contains("isn't hooked"), "{out}");
    let plan = e.stdout(&["tweaks", "plan", &game]);
    assert!(plan.contains("isn't hooked"), "{plan}");

    // The hook: status, on, off. Exactly reversible; staged instead when a Steam client
    // happens to run on this machine (then nothing is written at all).
    let s = e.json(&["steam", "hook", &game, "--json"]);
    assert_eq!((s["state"].as_str(), s["hookable"].as_bool(), s["steam_user"].as_str()), (Some("off"), Some(true), Some("1001")), "{s}");
    assert!(s["command"].as_str().unwrap().ends_with(&format!("steam-run --app {APP}")), "{s}");
    let on = e.json(&["steam", "hook", &game, "on", "--json"]);
    let after_on = std::fs::read_to_string(e.localconfig()).unwrap();
    if on["staged"].as_bool() == Some(true) {
        assert_eq!(on["state"], "waiting");
        assert_eq!(after_on, before, "staged: nothing written while Steam runs");
    } else {
        assert_eq!(on["state"], "on", "{on}");
        assert!(after_on.contains(&format!("--app {APP} %command%")) && after_on.contains("\"Playtime\"\t\t\"600\"") && after_on.contains("gamemoderun %command%"));
        let plan = e.stdout(&["tweaks", "plan", &game]);
        assert!(plan.contains("through quiver steam-run"), "{plan}");
    }
    let off = e.json(&["steam", "hook", &game, "off", "--json"]);
    assert_eq!(off["state"], "off", "{off}");
    assert_eq!(std::fs::read_to_string(e.localconfig()).unwrap(), before, "back exactly as it was");

    // Steam's launch, with the hook in front: a script stands in for Steam's command.
    let script = e.dir("fake-steam-command.sh");
    write(&script, "#!/bin/sh\n# Stands in for Steam's reaper and the game.\nout=\"$1\"; shift\nenv > \"$out.env\"\nprintf '%s\\n' \"$@\" > \"$out.args\"\nsleep 3\n");
    let record = e.dir("launch");
    let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
    e.isolate(&mut c);
    c.env("SteamAppId", APP.to_string())
        .env("STEAM_COMPAT_DATA_PATH", e.steam().join(format!("steamapps/compatdata/{APP}")))
        .env("STEAM_COMPAT_INSTALL_PATH", e.steam().join("steamapps/common/Control"))
        // What a person typed before the hook.
        .env("ENABLE_VKBASALT", "1")
        .args(["--source", "steam", "steam-run", "--app", &APP.to_string(), "/bin/sh"])
        .arg(&script)
        .arg(&record)
        .arg("-steam");
    let mut launch = c.spawn().unwrap();
    let pid = launch.id();

    // While it runs: a local session for the game, holding the launch's perf session.
    let t = Instant::now();
    let session = loop {
        let list = e.json(&["sessions", "--json"]);
        if let Some(s) = list.as_array().unwrap().iter().find(|s| s["game"] == game.as_str()) {
            break s.clone();
        }
        assert!(t.elapsed() < Duration::from_secs(15), "no session for {game}: {list}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!((session["product_id"].as_u64(), session["pids"][0].as_u64()), (Some(0), Some(pid as u64)), "{session}");
    let perf_session = session["session"].as_str().expect("the launch's perf session").to_owned();
    let line = e.stdout(&["sessions"]);
    assert!(line.contains(&game) && line.contains("Control") && line.contains("local history"), "{line}");
    let hud = e.json(&["hud", "status", &game, "--json"]);
    assert_eq!((hud["session"].as_str(), hud["enabled"].as_bool(), hud["game"].as_str()), (Some(perf_session.as_str()), Some(true), Some(game.as_str())), "{hud}");
    assert_eq!(hud["on"].as_bool(), Some(true), "shown at launch, as set: {hud}");
    let sight = e.stdout(&["sight", "status"]);
    assert!(sight.contains(&format!("{perf_session} {game} — Sight layer in; HUD shown")), "{sight}");

    assert!(launch.wait().unwrap().success());
    let env = std::fs::read_to_string(record.with_extension("env")).unwrap();
    let var = |k: &str| env.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_owned);
    // Nothing swapped: Steam's command ran as it was, with the plan around it.
    assert_eq!(std::fs::read_to_string(record.with_extension("args")).unwrap(), "-steam\n-dx12\n", "the game's arguments, then the plan's");
    assert_eq!(var("QUIVER_TEST_WRAPPED").as_deref(), Some("1"), "the wrapper ran in front of Steam's command");
    assert_eq!(var("PROTON_LOG").as_deref(), Some("1"), "the game's settings");
    assert_eq!(var("QUIVER_SESSION").as_deref(), Some(perf_session.as_str()));
    assert_eq!(var("QUIVER_GAME").as_deref(), Some(game.as_str()), "the game's ref, for hyprpad and scripts");
    assert_eq!(var("QUIVER_SIGHT").as_deref(), Some("1"), "the HUD is on for the game");
    assert!(var("QUIVER_PERF_DIR").is_some_and(|d| d.starts_with(e.dir("shm").to_str().unwrap())), "the scratch shm root");
    assert_eq!(var("VK_INSTANCE_LAYERS").as_deref(), Some("VK_LAYER_VKBASALT_post_processing:VK_LAYER_QUIVER_sight"), "Sight below the person's vkBasalt");
    assert_eq!(var("ENABLE_VKBASALT").as_deref(), Some("1"), "Steam's environment is kept");
    assert!(var("DXVK_CONFIG").is_some_and(|c| c.contains("samplerAnisotropy = 16")), "the game's tweak: {:?}", var("DXVK_CONFIG"));
    assert_eq!(var("SteamAppId").as_deref(), Some(APP.to_string().as_str()));

    // When Steam's command exits, the session ends: in the play history, perf summary and
    // all, with the perf session and its shm dir gone.
    let t = Instant::now();
    while e.json(&["sessions", "--json"]).as_array().unwrap().iter().any(|s| s["game"] == game.as_str()) {
        assert!(t.elapsed() < Duration::from_secs(10), "the session didn't end");
        std::thread::sleep(Duration::from_millis(100));
    }
    let history = std::fs::read_to_string(e.dir(&format!("data/quiver/games/steam-{APP}/playtime.jsonl"))).unwrap();
    let rec: Value = serde_json::from_str(history.lines().next().unwrap()).unwrap();
    assert!(rec["seconds"].as_u64().is_some_and(|s| s >= 2) && rec["settings_revision"] == 1, "{rec}");
    assert!(e.json(&["perf", "sessions", "--json"]).as_array().unwrap().is_empty());
    assert!(!std::fs::read_dir(e.dir("shm")).map(|d| d.flatten().any(|q| q.path().join(&perf_session).exists())).unwrap_or(false));
    let log = std::fs::read_to_string(e.dir(&format!("data/quiver/steam-run/{APP}/steam-run.log"))).unwrap();
    assert!(log.contains("nothing swapped") && log.contains("play session held"), "{log}");

    // quiverd down: the game still starts, unchanged, and no daemon is started from inside
    // Steam's launch.
    drop(_d);
    let mut c = Command::new(env!("CARGO_BIN_EXE_quiver"));
    e.isolate(&mut c);
    let plain = e.dir("plain");
    let st = c.env("SteamAppId", APP.to_string()).args(["--source", "steam", "steam-run", "/bin/sh"]).arg(&script).arg(&plain).arg("-steam").status().unwrap();
    assert!(st.success());
    assert_eq!(std::fs::read_to_string(plain.with_extension("args")).unwrap(), "-steam\n");
    let env = std::fs::read_to_string(plain.with_extension("env")).unwrap();
    assert!(!env.contains("QUIVER_SESSION=") && !env.contains("PROTON_LOG="), "nothing of the plan without quiverd");
    assert!(std::os::unix::net::UnixStream::connect(e.dir("run").join("quiver/quiverd.sock")).is_err(), "no quiverd started inside the launch");
}
