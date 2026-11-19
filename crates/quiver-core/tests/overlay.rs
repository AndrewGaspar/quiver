//! The overlay mechanism end to end, unprivileged: a fake install and a layer in a temp
//! dir, mounted over the install inside a user + mount namespace, with commands run in it
//! the way the launcher runs Proton.
//!
//! Skipped (with a message) where user namespaces can't mount, e.g. inside some sandboxes.
//! Set `QUIVER_REQUIRE_OVERLAY=1` to make that a failure instead.

use std::path::Path;
use std::process::{Command, Stdio};

use quiver_core::overlay::{self, Namespace, OverlaySpec};

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn run(ns: &Namespace, cwd: Option<&Path>, script: &str) -> String {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    ns.enter(&mut cmd, cwd).unwrap();
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn available() -> bool {
    if overlay::supported() {
        return true;
    }
    if std::env::var_os("QUIVER_REQUIRE_OVERLAY").is_some() {
        panic!("user namespaces are not available here");
    }
    eprintln!("skipping: this environment can't create user + mount namespaces");
    false
}

/// A profile's own settings folder with the saves inside it kept shared
/// (design/PROFILES.md decision 4): settings writes land in the profile's copy, save writes
/// in the real folder, and both are visible inside.
#[test]
fn shared_folder_inside_an_overlay_stays_the_hosts() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let dir = t.join("pfx/Documents/Remedy/AlanWakeRemastered");
    write(&dir.join("resolution.xml"), "vanilla");
    write(&dir.join("savegames/aw1/savegame.aws"), "chapter 3");
    let upper = t.join("profile/prefix/AlanWakeRemastered");
    write(&upper.join("resolution.xml"), "hdr on");
    // A copy of the saves from before (a profile that forked them) is hidden, not used.
    write(&upper.join("savegames/aw1/savegame.aws"), "stale fork");
    let spec = OverlaySpec {
        target: dir.clone(),
        lower: vec![],
        upper: upper.clone(),
        work: t.join("profile/work"),
        shared: vec![dir.join("savegames"), dir.join("not-yet")],
    };
    let ns = Namespace::create(&spec).unwrap();
    let d = dir.display();
    let out = run(
        &ns,
        None,
        &format!(
            "cat '{d}/resolution.xml'; echo; cat '{d}/savegames/aw1/savegame.aws'; echo; \
             echo 'chapter 4' > '{d}/savegames/aw1/savegame.aws'; echo new > '{d}/savegames/aw2'; \
             echo hdr > '{d}/resolution.xml'; echo later > '{d}/not-yet/x'"
        ),
    );
    assert_eq!(out, "hdr on\nchapter 3\n");
    // Saves went to the real folder; settings to the profile's copy.
    assert_eq!(read(&dir.join("savegames/aw1/savegame.aws")), "chapter 4\n");
    assert_eq!(read(&dir.join("savegames/aw2")), "new\n");
    assert_eq!(read(&dir.join("not-yet/x")), "later\n", "a shared folder made for the launch");
    assert_eq!(read(&dir.join("resolution.xml")), "vanilla");
    assert_eq!(read(&upper.join("resolution.xml")), "hdr\n");
    assert_eq!(read(&upper.join("savegames/aw1/savegame.aws")), "stale fork");
    // A shared folder outside the target is refused.
    let bad = OverlaySpec { shared: vec![t.join("elsewhere")], ..spec };
    assert!(Namespace::create(&bad).is_err());
}

#[test]
fn overlay_in_user_namespace() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let install = t.join("Games/My Game");
    let (layer_a, layer_b) = (t.join("mods/layers/la"), t.join("mods/layers/lb"));
    let (upper, work) = (t.join("mods/profiles/p1/upper"), t.join("mods/profiles/p1/work"));
    write(&install.join("data/a.txt"), "vanilla a");
    write(&install.join("data/b.txt"), "vanilla b");
    write(&install.join("game.exe"), "exe");
    write(&layer_a.join("data/a.txt"), "layer a");
    write(&layer_a.join("data/b.txt"), "layer a's b");
    write(&layer_b.join("data/b.txt"), "layer b");
    write(&layer_b.join("extra/new.txt"), "new");

    let spec = OverlaySpec { target: install.clone(), lower: vec![layer_b.clone(), layer_a.clone()], upper: upper.clone(), work, shared: vec![] };
    let ns = Namespace::create(&spec).unwrap();

    // Layers stack over the install at its own path; the top layer wins.
    let seen = run(&ns, None, &format!("cat '{0}/data/a.txt'; echo; cat '{0}/data/b.txt'; echo; cat '{0}/extra/new.txt'; echo; cat '{0}/game.exe'", install.display()));
    assert_eq!(seen, "layer a\nlayer b\nnew\nexe");
    // The uid inside is ours (not root), so Wine accepts the prefix.
    let uid = run(&ns, None, "id -u; id -g");
    let me = format!("{}\n{}\n", unsafe { libc::getuid() }, unsafe { libc::getgid() });
    assert_eq!(uid, me);
    // The working directory is inside the overlay.
    assert_eq!(run(&ns, Some(&install.join("data")), "cat a.txt"), "layer a");

    // Writes land in the upper dir; the host's install and layers are untouched.
    run(&ns, Some(&install), "echo saved > data/a.txt && echo log > new.log && rm data/b.txt && mkdir -p cfg && echo x > cfg/c.ini");
    assert_eq!(read(&install.join("data/a.txt")), "vanilla a");
    assert_eq!(read(&install.join("data/b.txt")), "vanilla b");
    assert!(!install.join("new.log").exists());
    assert!(!install.join("extra").exists());
    assert_eq!(read(&layer_a.join("data/a.txt")), "layer a");
    assert_eq!(read(&layer_b.join("data/b.txt")), "layer b");
    assert_eq!(read(&upper.join("data/a.txt")), "saved\n");
    assert_eq!(read(&upper.join("new.log")), "log\n");
    assert_eq!(read(&upper.join("cfg/c.ini")), "x\n");
    // Inside, the deletion and the writes persist across processes (one shared mount).
    assert_eq!(run(&ns, Some(&install), "cat data/a.txt; test ! -e data/b.txt && echo gone"), "saved\ngone\n");

    // The host can still find namespaced processes by their environment (the launcher
    // polls /proc/<pid>/environ for WINEPREFIX).
    let mut cmd = Command::new("sleep");
    cmd.arg("30").env("WINEPREFIX", "/quiver-test-prefix").stdout(Stdio::null());
    ns.enter(&mut cmd, None).unwrap();
    let mut child = cmd.spawn().unwrap();
    let pid = child.id();
    let mut environ = Vec::new();
    for _ in 0..50 {
        // Until exec finishes, /proc shows the parent's (forked) environment.
        environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        if environ.split(|&b| b == 0).any(|kv| kv == b"WINEPREFIX=/quiver-test-prefix") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(environ.split(|&b| b == 0).any(|kv| kv == b"WINEPREFIX=/quiver-test-prefix"));
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(cmdline.starts_with(b"sleep\0"));
    child.kill().unwrap();
    child.wait().unwrap();

    // A second namespace (another launch) gets its own mount; the first is unaffected.
    drop(ns);
    let host_view = std::fs::read_to_string(install.join("data/a.txt")).unwrap();
    assert_eq!(host_view, "vanilla a");
    // The mount never showed up on the host.
    let mounts = read(Path::new("/proc/self/mountinfo"));
    assert!(!mounts.contains(install.to_str().unwrap().replace(' ', "\\040").as_str()));
}

/// Layer deletions end to end: layers made by the mod store (whiteouts and an opaque
/// folder), launched through its overlay spec. Inside the namespace the deleted files are
/// gone and the replaced folder holds only the layer's files; the host's install still has
/// everything.
#[test]
fn deletions_in_user_namespace() {
    use quiver_core::mods::ModStore;
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let install = t.join("Games/My Game");
    for (p, s) in [
        ("Videos/Intro.bik", "intro"),
        ("Videos/Logo.bik", "logo"),
        ("Videos/Credits/roll.bik", "credits"),
        ("DataPC/keep.txt", "keep"),
        ("DataPC/old.txt", "old"),
        ("Shaders/a.fx", "a"),
        ("game.exe", "exe"),
    ] {
        write(&install.join(p), s);
    }
    let store = ModStore::new(&t.join("data"), 1);
    // Bottom: a mod folder that adds a shader, replaces DataPC wholesale and ships its own
    // whiteout (made like any mod tool would, with mknod).
    let src = t.join("src");
    write(&src.join("datapc/new.txt"), "new");
    write(&src.join("shaders/b.fx"), "b");
    let status = Command::new("mknod").arg(src.join("shaders/a.fx")).args(["c", "0", "0"]).status().unwrap();
    assert!(status.success());
    let rework = store.add_layer_with("Rework", Some(&src), &["DATAPC".into()], Some(&install), "cli").unwrap();
    assert_eq!(rework.layer.deletes, ["DataPC", "Shaders/a.fx"]);
    // Top: deletions only ("delete the intro videos"), in the wrong case on purpose.
    let no_intro = store
        .add_layer_with("No intro", None, &["videos/intro.BIK".into(), "videos/credits".into()], Some(&install), "agent")
        .unwrap();
    assert_eq!(no_intro.layer.deletes, ["Videos/Credits", "Videos/Intro.bik"]);
    store.create_profile("P", &["Rework".into(), "No intro".into()], "cli", None).unwrap();
    let launch = store.launch(Some("P"), &install).unwrap().unwrap();
    let ns = Namespace::create(&launch.overlay).unwrap();

    let i = install.display();
    let seen = run(&ns, None, &format!("cd '{i}' && find . -mindepth 1 | LC_ALL=C sort"));
    assert_eq!(
        seen,
        "./DataPC\n./DataPC/new.txt\n./Shaders\n./Shaders/b.fx\n./Videos\n./Videos/Logo.bik\n./game.exe\n",
        "merged view"
    );
    assert_eq!(run(&ns, None, &format!("cat '{i}/DataPC/new.txt'; test ! -e '{i}/Videos/Intro.bik' && echo gone")), "newgone\n");
    // Writing a deleted name inside the namespace lands in the upper dir, as usual.
    run(&ns, Some(&install), "echo mine > Videos/Intro.bik");
    assert_eq!(run(&ns, Some(&install), "cat Videos/Intro.bik"), "mine\n");

    // The host still has every file, unchanged.
    for (p, s) in [("Videos/Intro.bik", "intro"), ("Videos/Credits/roll.bik", "credits"), ("DataPC/old.txt", "old"), ("Shaders/a.fx", "a")] {
        assert_eq!(read(&install.join(p)), s, "{p}");
    }
    assert!(!install.join("DataPC/new.txt").exists());
    drop(ns);

    // Taking a deletion back shows the file again in the next launch (same upper dir).
    store.change_deletions("No intro", &[], &["Videos/Credits".into()], Some(&install), "cli", None).unwrap();
    let ns = Namespace::create(&store.launch(Some("P"), &install).unwrap().unwrap().overlay).unwrap();
    assert_eq!(run(&ns, None, &format!("cat '{i}/Videos/Credits/roll.bik'; ls '{i}/Videos'")), "creditsCredits\nIntro.bik\nLogo.bik\n");
    // What the game wrote over a deleted file stays in the profile's upper dir.
    assert_eq!(run(&ns, None, &format!("cat '{i}/Videos/Intro.bik'")), "mine\n");
}

/// Mods v2: a profile with its own copy of a prefix folder (the game's settings under
/// `Documents/My Games`). Inside the launch's namespace the folder is overlaid: the game
/// sees the prefix's files with the profile's changes on top, a config patch applied before
/// the mount shows in there, and what the game writes lands in the profile's folder. The
/// host's prefix (what vanilla sees) is untouched. The install overlay is mounted alongside.
#[test]
fn prefix_config_in_user_namespace() {
    use quiver_core::mods::{ModStore, Package};
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let prefix = t.join("pfx");
    let install = t.join("Games/My Game");
    write(&install.join("game.exe"), "exe");
    let dir = "drive_c/users/steamuser/Documents/My Games/My Game";
    let ini = prefix.join(dir).join("Settings.ini");
    write(&ini, "; the game's own\r\n[Graphics]\r\nQuality=low\r\nVSync=1\r\n");
    write(&prefix.join("drive_c/users/steamuser/Documents/other.txt"), "shared");
    let game = quiver_api::Game {
        product_id: 77,
        name: "My Game".into(),
        install_dir: install.clone(),
        prefix: prefix.clone(),
        exe: "game.exe".into(),
        args: None,
        language: None,
        game: None,
    };
    let store = ModStore::new(&t.join("data"), 77).with_game(&game, None);
    let pkg = t.join("pkg");
    write(
        &pkg.join("quiver-mod.toml"),
        &format!(
            "id = \"ultra\"\nname = \"Ultra\"\nsave_compatible = true\n[[patch]]\nfile = \"{dir}/Settings.ini\"\nsection = \"Graphics\"\nset = {{ Quality = \"ultra\" }}\n"
        ),
    );
    write(&pkg.join("files/extra.txt"), "from the mod");
    store.install(&Package::Dir { path: pkg, name: None }, "cli").unwrap();
    store.create_profile("Pretty", &["ultra".into()], "cli", None).unwrap();
    // A folder the game hasn't made yet works too (created empty before mounting).
    let later = "drive_c/users/steamuser/AppData/Local/My Game";
    store
        .set_profile("Pretty", &[quiver_api::SettingOp::Set { key: "profile_config".into(), value: format!("{dir},{later}") }], "cli", None)
        .unwrap();

    let plan = store.plan(Some("Pretty")).unwrap();
    assert_eq!(plan.overlays.len(), 3);
    let st = plan.apply_patches().unwrap();
    assert_eq!(st[0].state, quiver_api::mods::PatchState::Applied, "{st:?}");
    for o in &plan.overlays {
        std::fs::create_dir_all(&o.target).unwrap();
    }
    let ns = Namespace::create_many(&plan.overlays).unwrap();
    let own = store.profile_prefix(&plan.api.profile.as_ref().unwrap().id);

    let d = prefix.join(dir);
    let seen = run(&ns, Some(&d), "cat Settings.ini");
    assert_eq!(seen, "; the game's own\r\n[Graphics]\r\nQuality=ultra\r\nVSync=1\r\n", "the patch, as the game sees the file");
    // The game writes its settings and new files: they land in the profile's folders.
    run(&ns, Some(&d), "printf 'x' >> Settings.ini && echo new > Profile.dat");
    run(&ns, None, &format!("echo cache > '{}/{later}/cache.bin'", prefix.display()));
    assert!(std::fs::read_to_string(own.join(dir).join("Settings.ini")).unwrap().ends_with("VSync=1\r\nx"));
    assert_eq!(read(&own.join(dir).join("Profile.dat")), "new\n");
    assert_eq!(read(&own.join(later).join("cache.bin")), "cache\n");
    // The prefix itself (vanilla's view) is as it was, apart from the empty folder made
    // for the mount.
    assert_eq!(read(&ini), "; the game's own\r\n[Graphics]\r\nQuality=low\r\nVSync=1\r\n");
    assert!(!prefix.join(dir).join("Profile.dat").exists());
    assert!(std::fs::read_dir(prefix.join(later)).unwrap().next().is_none());
    // Outside the profile's folders the prefix is shared, and the install overlay works.
    assert_eq!(run(&ns, None, &format!("cat '{}/drive_c/users/steamuser/Documents/other.txt'", prefix.display())), "shared");
    assert_eq!(run(&ns, None, &format!("cat '{}/extra.txt'", install.display())), "from the mod");
    drop(ns);
    // The next launch's view (and the plan) shows the profile's own files.
    let info = store.state().unwrap().profiles.remove(0);
    assert_eq!(info.config_dirs[0].files, ["Profile.dat", "Settings.ini"]);
    assert_eq!(info.config_dirs[1].files, ["cache.bin"]);
}

#[test]
fn bad_layer_is_reported() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::create_dir_all(t.join("install")).unwrap();
    let spec = OverlaySpec {
        target: t.join("install"),
        lower: vec![t.join("missing")],
        upper: t.join("up"),
        work: t.join("work"),
        shared: vec![],
    };
    let err = Namespace::create(&spec).unwrap_err().to_string();
    assert!(err.contains("missing"), "{err}");
    // Upper inside the install would loop.
    let spec = OverlaySpec { target: t.join("install"), lower: vec![], upper: t.join("install/up"), work: t.join("work"), shared: vec![] };
    assert!(Namespace::create(&spec).is_err());
}

/// Wine through the overlay, the way a modded launch runs it: wineserver starts inside the
/// namespace (it opens files for its clients, so it must see the overlay), reads a layered
/// file and writes one that lands in the upper dir. Needs a Proton build, so it's opt-in:
/// `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 cargo test
/// -p quiver-core --test overlay -- --ignored`. Runs headless in a throwaway prefix and HOME.
#[test]
#[ignore]
fn wine_through_overlay() {
    let Some(proton) = std::env::var_os("QUIVER_TEST_PROTON").map(std::path::PathBuf::from) else {
        panic!("set QUIVER_TEST_PROTON to a Proton build directory");
    };
    assert!(available());
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let install = t.join("install");
    let layer = t.join("layer");
    write(&install.join("a.txt"), "vanilla");
    write(&layer.join("a.txt"), "from the layer");
    let spec = OverlaySpec { target: install.clone(), lower: vec![layer], upper: t.join("upper"), work: t.join("work"), shared: vec![] };
    let ns = Namespace::create(&spec).unwrap();
    let wine = |args: &[&str]| {
        let mut cmd = Command::new(proton.join("files/bin/wine"));
        cmd.args(args)
            .env("WINEPREFIX", t.join("pfx"))
            .env("HOME", t.join("home"))
            .env("XDG_DATA_HOME", t.join("home/.local/share"))
            .env("XDG_CONFIG_HOME", t.join("home/.config"))
            .env("XDG_CACHE_HOME", t.join("home/.cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        ns.enter(&mut cmd, Some(&install)).unwrap();
        let out = cmd.output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    std::fs::create_dir_all(t.join("home")).unwrap();
    let win = |p: &Path| format!("Z:{}", p.display()).replace('/', "\\");
    let (ok, out, err) = wine(&["cmd", "/c", "type", &win(&install.join("a.txt"))]);
    assert!(ok, "{out}\n{err}");
    assert!(out.contains("from the layer"), "{out}\n{err}");
    let (ok, out, err) = wine(&["cmd", "/c", &format!("echo written> {}", win(&install.join("w.txt")))]);
    assert!(ok, "{out}\n{err}");
    let mut kill = Command::new(proton.join("files/bin/wineserver"));
    kill.arg("-w").env("WINEPREFIX", t.join("pfx"));
    let _ = kill.status();
    assert!(!install.join("w.txt").exists());
    assert!(read(&t.join("upper/w.txt")).starts_with("written"));
    assert_eq!(read(&install.join("a.txt")), "vanilla");
}

/// Mods v2: Wine reaching a profile's own prefix folder the way games do, through `C:`
/// (the prefix's `dosdevices/c:` symlink to `drive_c`), sees the overlay, and its writes
/// land in the profile's folder. Opt-in like [`wine_through_overlay`].
#[test]
#[ignore]
fn wine_sees_prefix_config_overlay() {
    let Some(proton) = std::env::var_os("QUIVER_TEST_PROTON").map(std::path::PathBuf::from) else {
        panic!("set QUIVER_TEST_PROTON to a Proton build directory");
    };
    assert!(available());
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let prefix = t.join("pfx");
    std::fs::create_dir_all(t.join("home")).unwrap();
    let wine = |ns: Option<&Namespace>, args: &[&str]| {
        let mut cmd = Command::new(proton.join("files/bin/wine"));
        cmd.args(args)
            .env("WINEPREFIX", &prefix)
            .env("HOME", t.join("home"))
            .env("XDG_DATA_HOME", t.join("home/.local/share"))
            .env("XDG_CONFIG_HOME", t.join("home/.config"))
            .env("XDG_CACHE_HOME", t.join("home/.cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        if let Some(ns) = ns {
            ns.enter(&mut cmd, None).unwrap();
        }
        let out = cmd.output().unwrap();
        let mut kill = Command::new(proton.join("files/bin/wineserver"));
        kill.arg("-w").env("WINEPREFIX", &prefix);
        let _ = kill.status();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // Make the prefix outside any namespace (as Quiver does before a launch).
    let (ok, out, err) = wine(None, &["cmd", "/c", "echo ready"]);
    assert!(ok, "{out}\n{err}");
    let dir = prefix.join("drive_c/quiver-test/MyGame");
    write(&dir.join("settings.ini"), "Quality=low\r\n");
    let upper = t.join("profile/prefix/drive_c/quiver-test/MyGame");
    write(&upper.join("settings.ini"), "Quality=ultra\r\n");
    let spec = OverlaySpec { target: dir.clone(), lower: vec![], upper: upper.clone(), work: t.join("profile/work"), shared: vec![] };
    let ns = Namespace::create(&spec).unwrap();
    let (ok, out, err) = wine(Some(&ns), &["cmd", "/c", r"type C:\quiver-test\MyGame\settings.ini & echo mine> C:\quiver-test\MyGame\new.txt"]);
    assert!(ok, "{out}\n{err}");
    assert!(out.contains("Quality=ultra"), "{out}\n{err}");
    assert!(read(&upper.join("new.txt")).starts_with("mine"));
    assert!(!dir.join("new.txt").exists());
    assert_eq!(read(&dir.join("settings.ini")), "Quality=low\r\n");
}

/// Like [`wine_through_overlay`], through the `proton` script with the launcher's
/// environment. Uses `runinprefix`: `run` adds Proton's steam.exe wrapper, which needs
/// the Steam client library from the real HOME, which this test keeps away from.
/// Opt-in like the Wine test.
#[test]
#[ignore]
fn proton_run_through_overlay() {
    let Some(proton) = std::env::var_os("QUIVER_TEST_PROTON").map(std::path::PathBuf::from) else {
        panic!("set QUIVER_TEST_PROTON to a Proton build directory");
    };
    assert!(available());
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    // No spaces: the command line goes through cmd as one argument.
    let install = t.join("game");
    let layer = t.join("layer");
    write(&install.join("data/a.txt"), "vanilla");
    write(&layer.join("data/a.txt"), "from the layer");
    let spec = OverlaySpec { target: install.clone(), lower: vec![layer], upper: t.join("upper"), work: t.join("work"), shared: vec![] };
    let ns = Namespace::create(&spec).unwrap();
    let prefix = t.join("pfx");
    std::fs::create_dir_all(&prefix).unwrap();
    std::os::unix::fs::symlink(".", prefix.join("pfx")).unwrap();
    std::fs::create_dir_all(t.join("home")).unwrap();
    std::fs::create_dir_all(t.join("steam")).unwrap();
    let win = |p: &Path| format!("Z:{}", p.display()).replace('/', "\\");
    let mut cmd = Command::new("python3");
    cmd.arg(proton.join("proton"))
        .args([
            "runinprefix",
            "cmd",
            "/c",
            &format!(
                "copy {} {} & echo written> {}",
                win(&install.join("data/a.txt")),
                win(&install.join("seen.txt")),
                win(&install.join("w.txt"))
            ),
        ])
        .env("STEAM_COMPAT_DATA_PATH", &prefix)
        .env("WINEPREFIX", &prefix)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", t.join("steam"))
        .env("STEAM_COMPAT_APP_ID", "0")
        .env("SteamAppId", "0")
        .env("SteamGameId", "0")
        .env("HOME", t.join("home"))
        .env("XDG_DATA_HOME", t.join("home/.local/share"))
        .env("XDG_CONFIG_HOME", t.join("home/.config"))
        .env("XDG_CACHE_HOME", t.join("home/.cache"))
        .env("WINEDEBUG", "-all")
        .env("WINEDLLOVERRIDES", "winemenubuilder.exe=d;mscoree,mshtml=")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    ns.enter(&mut cmd, Some(&install)).unwrap();
    let out = cmd.output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let mut kill = Command::new(proton.join("files/bin/wineserver"));
    kill.arg("-w").env("WINEPREFIX", &prefix);
    let _ = kill.status();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // The copy shows what cmd read.
    assert_eq!(read(&t.join("upper/seen.txt")), "from the layer", "{stdout}\n{stderr}");
    assert!(!install.join("w.txt").exists() && !install.join("seen.txt").exists());
    assert!(read(&t.join("upper/w.txt")).starts_with("written"));
    assert_eq!(read(&install.join("data/a.txt")), "vanilla");
}
