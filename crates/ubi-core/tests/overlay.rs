//! The overlay mechanism end to end, unprivileged: a fake install and a layer in a temp
//! dir, mounted over the install inside a user + mount namespace, with commands run in it
//! the way the launcher runs Proton.
//!
//! Skipped (with a message) where user namespaces can't mount, e.g. inside some sandboxes.
//! Set `QUIVER_REQUIRE_OVERLAY=1` to make that a failure instead.

use std::path::Path;
use std::process::{Command, Stdio};

use ubi_core::overlay::{self, Namespace, OverlaySpec};

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

    let spec = OverlaySpec { target: install.clone(), lower: vec![layer_b.clone(), layer_a.clone()], upper: upper.clone(), work };
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
    };
    let err = Namespace::create(&spec).unwrap_err().to_string();
    assert!(err.contains("missing"), "{err}");
    // Upper inside the install would loop.
    let spec = OverlaySpec { target: t.join("install"), lower: vec![], upper: t.join("install/up"), work: t.join("work") };
    assert!(Namespace::create(&spec).is_err());
}

/// Wine through the overlay, the way a modded launch runs it: wineserver starts inside the
/// namespace (it opens files for its clients, so it must see the overlay), reads a layered
/// file and writes one that lands in the upper dir. Needs a Proton build, so it's opt-in:
/// `QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton10-34 cargo test
/// -p ubi-core --test overlay -- --ignored`. Runs headless in a throwaway prefix and HOME.
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
    let spec = OverlaySpec { target: install.clone(), lower: vec![layer], upper: t.join("upper"), work: t.join("work") };
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
