//! The check against fake roots, the scripts against golden files, and the scripts run
//! twice (and undone) against a fake root with fake system tools on `PATH`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::firewall::{Kind, Wanted};
use super::*;

/// A fake root and the facts that go with it.
struct Fake {
    dir: tempfile::TempDir,
    facts: Facts,
}

impl Fake {
    fn root(&self) -> &Path {
        &self.facts.root
    }

    fn write(&self, abs: &str, text: &str) {
        let p = self.root().join(abs.trim_start_matches('/'));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, abs: &str) -> String {
        std::fs::read_to_string(self.root().join(abs.trim_start_matches('/'))).unwrap_or_default()
    }

    fn check(&self, opts: Options) -> Checked {
        check(&self.facts, &opts)
    }
}

fn iface(name: &str, virtual_: bool, addrs: &[(&str, u8)]) -> Iface {
    Iface { name: name.into(), virtual_, up: true, addrs: addrs.iter().map(|(a, p)| (a.parse().unwrap(), *p)).collect() }
}

/// The shipped files, as the installed Quiver has them (`<libdir>/quiver/contrib`).
fn contrib(dir: &Path) -> PathBuf {
    let c = dir.join("contrib");
    for s in shipped::ALL {
        let p = c.join(s.src);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s.text).unwrap();
    }
    c
}

/// An empty machine: no groups, no rules, no firewall; the person seated, nothing on.
fn bare() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let src = contrib(dir.path());
    let facts = Facts {
        root: dir.path().join("root"),
        user: "ajg".into(),
        uid: 1000,
        gid: 1000,
        groups: vec![1000, 998],
        seated: true,
        uinput_gid: Some(0),
        ifaces: vec![iface("lo", true, &[("127.0.0.1", 8)]), iface("wlp191s0", false, &[("192.168.50.189", 24), ("fe80::5cae:5291:4c71:9167", 64)]), iface("tailscale0", true, &[("100.119.207.102", 32)])],
        services: Services::default(),
        linger: false,
        linger_needs_root: Some(false),
        boot_enabled: false,
        features: Features::default(),
        contrib: Some(src),
    };
    let f = Fake { dir, facts };
    f.write("/etc/group", "root:x:0:\nwheel:x:998:ajg\najg:x:1000:\n");
    f.write("/etc/subgid", "ajg:100000:65536\n");
    f
}

impl Fake {
    fn root_path(&self) -> PathBuf {
        self.facts.root.clone()
    }
}

/// This laptop on 2026-11-21: both pad rules and groups installed by hand (the lab rule's
/// comments are from an older build), ufw on with the person's own Moonlight rules from the
/// LAN and tailscale0, remote play and Moonlight on.
fn this_machine() -> Fake {
    let mut f = bare();
    let w = |f: &Fake, p: &str, t: &str| {
        let full = f.root_path().join(p.trim_start_matches('/'));
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, t).unwrap();
    };
    w(&f, "/etc/group", "root:x:0:\nwheel:x:998:ajg\najg:x:1000:\nquiver-lab-pad:x:105000:\nquiver-stream-pad:x:105001:\n");
    w(&f, "/etc/subgid", "ajg:100000:65536\n");
    w(&f, "/etc/udev/rules.d/72-quiver-lab-pad.rules", &shipped::LAB_RULE.text.replace("§16", "§15"));
    w(&f, "/etc/udev/rules.d/72-quiver-stream-pad.rules", shipped::STREAM_RULE.text);
    w(&f, "/usr/lib/udev/rules.d/60-steam-input.rules", "KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\n");
    w(&f, "/etc/ufw/ufw.conf", "ENABLED=yes\nLOGLEVEL=low\n");
    w(&f, "/etc/ufw/user.rules", firewall::tests::USER_RULES);
    w(&f, "/etc/ufw/user6.rules", firewall::tests::USER6_RULES);
    f.facts.services.ufw = true;
    f.facts.features.remote = true;
    f.facts.features.moonlight = true;
    f
}

/// A fresh headless server: nothing installed, nobody seated, ufw on with rules only root
/// may read, polkit asking for a password to linger.
fn headless() -> Fake {
    let mut f = bare();
    f.facts.seated = false;
    f.facts.services.ufw = true;
    f.facts.linger_needs_root = Some(true);
    f.facts.features.remote = true;
    f.facts.features.moonlight = true;
    f.facts.ifaces = vec![iface("lo", true, &[("127.0.0.1", 8)]), iface("enp3s0", false, &[("10.0.0.20", 24)]), iface("tailscale0", true, &[("100.70.1.2", 32), ("fd7a:115c:a1e0::12", 128)])];
    let root = f.root_path();
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/group"), "root:x:0:\najg:x:1000:\n").unwrap();
    std::fs::write(root.join("etc/subgid"), "ajg:100000:65536\n").unwrap();
    std::fs::create_dir_all(root.join("etc/ufw")).unwrap();
    std::fs::write(root.join("etc/ufw/ufw.conf"), "ENABLED=yes\n").unwrap();
    f
}

fn states(r: &Report) -> Vec<(String, State)> {
    r.pieces.iter().map(|p| (p.id.clone(), p.state)).collect()
}

fn st(r: &Report, id: &str) -> State {
    r.piece(id).unwrap().state
}

#[test]
fn this_machine_recognises_the_hand_installed_pieces() {
    let f = this_machine();
    let c = f.check(Options::default());
    let r = &c.report;
    assert_eq!(st(r, "lab-pads"), State::Ok, "{:#?}", r.piece("lab-pads"));
    assert!(r.piece("lab-pads").unwrap().summary.contains("by hand") && r.piece("lab-pads").unwrap().summary.contains("comments differ"), "{}", r.piece("lab-pads").unwrap().summary);
    assert_eq!(st(r, "stream-pads"), State::Ok);
    assert_eq!(st(r, "headless-uinput"), State::NotNeeded, "seated, boot off");
    assert_eq!(st(r, "firewall-profiles"), State::Missing);
    // Moonlight: the person's own rules allow it from the LAN and tailscale0: no duplicates.
    let m = r.piece("firewall-moonlight").unwrap();
    assert_eq!(m.state, State::Ok, "{m:#?}");
    assert!(m.items.iter().all(|i| i.detail.contains("by your own rules")), "{m:#?}");
    // Remote play has no rule of its own yet.
    let rp = r.piece("firewall-remote").unwrap();
    assert_eq!(rp.state, State::Missing);
    assert!(rp.summary.contains("TCP 47990; UDP 48001–48061"), "{}", rp.summary);
    assert_eq!(st(r, "linger"), State::NotNeeded);
    assert_eq!(r.firewall, Kind::Ufw);
    assert_eq!(r.scopes, ["192.168.50.0/24 (wlp191s0)", "the tailnet (tailscale0)"]);
    // Without --firewall the plan opens nothing; it installs the profile file only.
    assert!(c.plan.rules.is_empty());
    assert_eq!(c.plan.files.iter().map(|(s, d, _)| (s.src, d.as_str())).collect::<Vec<_>>(), [("ufw/quiver", "/etc/ufw/applications.d/quiver")]);
    assert!(c.plan.groups.is_empty());
    // Moonlight bound to the tailnet only: LAN rules wouldn't matter.
    let mut g = this_machine();
    g.facts.features.moonlight_tailnet_only = true;
    let m = g.check(Options::default()).report;
    assert_eq!(m.piece("firewall-moonlight").unwrap().items.iter().map(|i| i.what.as_str()).collect::<Vec<_>>(), ["the tailnet (tailscale0)"]);
    // With it: remote play only, from the LAN and the tailnet.
    let c = f.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: false });
    let added: Vec<String> = c.plan.rules.iter().map(|(s, sc, _)| format!("{} {}", s.profile(), sc.describe())).collect();
    assert_eq!(added, ["Quiver Remote 192.168.50.0/24 (wlp191s0)", "Quiver Remote the tailnet (tailscale0)"]);
}

#[test]
fn versions_conflicts_and_package_copies() {
    let f = this_machine();
    // An older lab rule in /etc is replaced where it is.
    f.write("/etc/udev/rules.d/72-quiver-lab-pad.rules", "# quiver-lab-pad rule v0\n");
    // Someone else's file under the stream rule's name is left alone.
    f.write("/etc/udev/rules.d/72-quiver-stream-pad.rules", "# not ours\n");
    // The package's copy of the uinput rule in /usr/lib counts.
    f.write("/usr/lib/udev/rules.d/72-quiver-uinput.rules", shipped::UINPUT_RULE.text);
    let c = f.check(Options::default());
    let lab = c.report.piece("lab-pads").unwrap();
    assert_eq!(lab.state, State::Outdated);
    assert!(lab.summary.contains("v0 < v1"), "{}", lab.summary);
    assert!(c.plan.files.iter().any(|(s, d, _)| s.src == shipped::LAB_RULE.src && d == "/etc/udev/rules.d/72-quiver-lab-pad.rules"));
    let stream = c.report.piece("stream-pads").unwrap();
    assert_eq!(stream.state, State::Conflict);
    assert!(!c.plan.files.iter().any(|(s, ..)| s.src == shipped::STREAM_RULE.src), "never overwrite a file that isn't ours");
    let u = c.report.piece("headless-uinput").unwrap();
    assert!(u.summary.contains("by the package") || u.state == State::NotNeeded, "{u:#?}");
    // A group with members is a conflict, not ok.
    f.write("/etc/group", "ajg:x:1000:\nquiver-lab-pad:x:105000:bob\nquiver-stream-pad:x:105001:\n");
    assert_eq!(f.check(Options::default()).report.piece("lab-pads").unwrap().items[1].state, State::Conflict);
    // A gid outside the subordinate range, too.
    f.write("/etc/group", "ajg:x:1000:\nquiver-lab-pad:x:900:\nquiver-stream-pad:x:105001:\n");
    assert_eq!(f.check(Options::default()).report.piece("lab-pads").unwrap().items[1].state, State::Conflict);
}

#[test]
fn what_is_needed_follows_what_is_on() {
    let mut f = bare();
    let r = f.check(Options::default()).report;
    // Nothing streams: no stream pads, no uinput, no firewall.
    assert_eq!(st(&r, "lab-pads"), State::Missing);
    assert_eq!(st(&r, "stream-pads"), State::NotNeeded);
    assert_eq!(st(&r, "headless-uinput"), State::NotNeeded);
    assert_eq!(st(&r, "firewall-moonlight"), State::NotNeeded);
    assert_eq!(st(&r, "firewall-profiles"), State::NotNeeded);
    // Streams with nobody seated need uinput; at boot needs lingering (no root here).
    f.facts.features.moonlight = true;
    f.facts.seated = false;
    let c = f.check(Options { firewall: None, at_boot: true });
    assert_eq!(st(&c.report, "headless-uinput"), State::Missing);
    let l = c.report.piece("linger").unwrap();
    assert_eq!((l.state, l.fix.as_deref()), (State::Missing, Some("quiver remote boot on")));
    assert!(!c.plan.linger, "linger goes through quiver remote boot on, not the script");
    // No firewall: nothing to allow.
    assert!(c.report.piece("firewall-moonlight").unwrap().summary.contains("no firewall"));
    // nftables alone: Quiver doesn't edit it, and says what to allow.
    f.facts.services.nftables = true;
    let c = f.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: false });
    assert_eq!(st(&c.report, "firewall-moonlight"), State::Unknown);
    assert!(c.plan.rules.is_empty() && c.plan.notes.iter().any(|n| n.contains("TCP 47984, 47989, 48010")));
    // ufw over nftables: ufw.
    f.facts.services.ufw = true;
    assert_eq!(Kind::of(&f.facts), Kind::Ufw);
    f.facts.services = Services { firewalld: true, ..Default::default() };
    assert_eq!(Kind::of(&f.facts), Kind::Firewalld);
}

#[test]
fn unreadable_ufw_rules_cant_be_told() {
    let f = headless();
    let c = f.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: true });
    let m = c.report.piece("firewall-moonlight").unwrap();
    assert_eq!(m.state, State::Unknown, "{m:#?}");
    assert!(m.items[0].detail.contains("can't tell without root"));
    // The script asks ufw itself, for every scope and server.
    assert_eq!(c.plan.rules.len(), 4);
    assert!(c.plan.linger, "lingering needs root on this server");
    assert_eq!(st(&c.report, "headless-uinput"), State::Missing);
}

/// The golden scripts: `UPDATE_GOLDEN=1 cargo test -p quiver-api system::` rewrites them.
fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/system/golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} missing (UPDATE_GOLDEN=1)", path.display()));
    assert_eq!(got, want, "{} differs (UPDATE_GOLDEN=1 to accept)", path.display());
}

/// The script with the test's temp paths replaced, so golden files are stable.
fn stable(f: &Fake, s: &str) -> String {
    s.replace(&f.facts.contrib.as_ref().unwrap().display().to_string(), "/home/ajg/.local/lib/quiver/contrib")
}

const NOW: &str = "2026-11-21 12:00";

#[test]
fn golden_scripts() {
    let f = this_machine();
    let c = f.check(Options::default());
    golden("this-machine.sh", &stable(&f, &script::install(&c.report, &c.plan, &f.facts, f.facts.contrib.as_ref().unwrap(), NOW).unwrap()));
    let c = f.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: false });
    golden("this-machine-firewall.sh", &stable(&f, &script::install(&c.report, &c.plan, &f.facts, f.facts.contrib.as_ref().unwrap(), NOW).unwrap()));
    let h = headless();
    let c = h.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: true });
    golden("headless.sh", &stable(&h, &script::install(&c.report, &c.plan, &h.facts, h.facts.contrib.as_ref().unwrap(), NOW).unwrap()));
    golden("this-machine-undo.sh", &script::undo(&f.facts, NOW));
}

#[test]
fn nothing_to_do_is_no_script() {
    let mut f = this_machine();
    f.write("/etc/ufw/applications.d/quiver", shipped::UFW_PROFILES.text);
    f.facts.features.remote = false;
    let c = f.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: false });
    assert!(c.plan.is_empty(), "{:?}", c.plan);
    assert!(script::install(&c.report, &c.plan, &f.facts, Path::new("/x"), NOW).is_none());
}

// ------------------------------------------------------------------------------------------
// Running the scripts against a fake root.

const FAKE_TOOLS: &[(&str, &str)] = &[
    ("getent", r#"[ "$1" = group ] || exit 2; grep "^$2:" "$QUIVER_SETUP_ROOT/etc/group""#),
    (
        "systemd-sysusers",
        r#"echo "systemd-sysusers $*" >>"$QUIVER_SETUP_ROOT/calls.log"
grep '^g ' "$1" | while read -r _ name gid; do
    grep -q "^$name:" "$QUIVER_SETUP_ROOT/etc/group" || echo "$name:x:$gid:" >>"$QUIVER_SETUP_ROOT/etc/group"
done"#,
    ),
    ("groupdel", r#"echo "groupdel $*" >>"$QUIVER_SETUP_ROOT/calls.log"; sed -i "/^$1:/d" "$QUIVER_SETUP_ROOT/etc/group""#),
    ("udevadm", r#"echo "udevadm $*" >>"$QUIVER_SETUP_ROOT/calls.log""#),
    (
        "loginctl",
        r#"echo "loginctl $*" >>"$QUIVER_SETUP_ROOT/calls.log"
mkdir -p "$QUIVER_SETUP_ROOT/var/lib/systemd/linger"
case $1 in enable-linger) touch "$QUIVER_SETUP_ROOT/var/lib/systemd/linger/$2" ;; disable-linger) rm -f "$QUIVER_SETUP_ROOT/var/lib/systemd/linger/$2" ;; esac"#,
    ),
    (
        "ufw",
        r#"st="$QUIVER_SETUP_ROOT/ufw.state"; touch "$st"
case $1 in
status)
    if [ "${2:-}" = numbered ]; then
        n=0
        while IFS='|' read -r to from c; do n=$((n + 1)); printf '[%2d] %-26s ALLOW IN    %s%s\n' "$n" "$to" "$from" "${c:+ # $c}"; done <"$st"
    else
        printf 'Status: active\n\nTo                         Action      From\n--                         ------      ----\n'
        while IFS='|' read -r to from c; do printf '%-26s ALLOW       %s%s\n' "$to" "$from" "${c:+ # $c}"; done <"$st"
    fi ;;
allow)
    shift; to=""; from="Anywhere"; c=""; proto=""; port=""; iface=""
    while [ $# -gt 0 ]; do
        case $1 in
            from) from=$2; shift 2 ;; in) shift ;; on) iface=$2; shift 2 ;; to) shift 2 ;;
            app) to=$2; shift 2 ;; proto) proto=$2; shift 2 ;; port) port=$2; shift 2 ;; comment) c=$2; shift 2 ;;
            *) echo "fake ufw: $1?" >&2; exit 2 ;;
        esac
    done
    [ -n "$port" ] && to="$port/$proto"
    [ -n "$iface" ] && to="$to on $iface"
    line="$to|$from|$c"
    if grep -qxF "$line" "$st"; then echo "Skipping adding existing rule"; else echo "$line" >>"$st"; echo "ufw allow $line" >>"$QUIVER_SETUP_ROOT/calls.log"; fi ;;
--force)
    [ "$2" = delete ] || exit 2
    sed -i "${3}d" "$st"; echo "ufw delete $3" >>"$QUIVER_SETUP_ROOT/calls.log" ;;
*) exit 2 ;;
esac"#,
    ),
];

fn tools(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, body) in FAKE_TOOLS {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    bin
}

fn run(f: &Fake, script: &str) -> String {
    let path = f.dir.path().join("script.sh");
    std::fs::write(&path, script).unwrap();
    let bin = tools(f.dir.path());
    let out = Command::new("sh")
        .arg(&path)
        .env("QUIVER_SETUP_ROOT", f.root_path())
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "script failed:\n{text}\n---\n{script}");
    text
}

fn calls(f: &Fake) -> String {
    let p = f.root_path().join("calls.log");
    let s = std::fs::read_to_string(&p).unwrap_or_default();
    let _ = std::fs::remove_file(&p);
    s
}

#[test]
fn the_script_is_idempotent_and_undo_takes_only_its_own() {
    let mut h = headless();
    // The person allowed Moonlight's TCP ports from the LAN by hand, with no comment, and
    // installed the lab rule and its group themselves.
    let root = h.root_path();
    std::fs::write(root.join("ufw.state"), "47984,47989,48010/tcp|10.0.0.0/24|\n").unwrap();
    std::fs::create_dir_all(root.join("etc/udev/rules.d")).unwrap();
    std::fs::write(root.join("etc/udev/rules.d/72-quiver-lab-pad.rules"), shipped::LAB_RULE.text).unwrap();
    std::fs::write(root.join("etc/group"), "root:x:0:\najg:x:1000:\nquiver-lab-pad:x:105000:\n").unwrap();
    std::fs::create_dir_all(root.join("sys/devices/virtual/misc/uinput")).unwrap();

    let c = h.check(Options { firewall: Some(Wanted { lan: true, tailnet: true }), at_boot: true });
    assert_eq!(st(&c.report, "lab-pads"), State::Ok, "the hand-installed rule and group");
    let s = script::install(&c.report, &c.plan, &h.facts, h.facts.contrib.as_ref().unwrap(), NOW).unwrap();
    let out = run(&h, &s);
    assert!(out.contains("installed /etc/udev/rules.d/72-quiver-stream-pad.rules") && out.contains("created group quiver-stream-pad"), "{out}");
    let first = calls(&h);
    for want in ["systemd-sysusers", "udevadm control --reload", "udevadm trigger --action=change /sys/devices/virtual/misc/uinput", "ufw allow Quiver Moonlight|10.0.0.0/24|quiver", "ufw allow Quiver Remote on tailscale0|Anywhere|quiver", "loginctl enable-linger ajg"] {
        assert!(first.contains(want), "{want} in\n{first}");
    }
    let group = h.read("/etc/group");
    assert!(group.contains("quiver-stream-pad:x:105001:") && group.contains("quiver-uinput:x:105002:"), "{group}");
    for x in [shipped::STREAM_RULE, shipped::UINPUT_RULE, shipped::SYSUSERS, shipped::UFW_PROFILES] {
        assert_eq!(h.read(x.dest), x.text, "{} byte for byte", x.dest);
    }
    let list = h.read(SETUP_LIST);
    assert!(!list.contains("72-quiver-lab-pad.rules") && !list.contains("group quiver-lab-pad"), "the person's pieces aren't setup's:\n{list}");
    assert!(list.contains("group quiver-uinput") && list.contains("linger ajg"), "{list}");
    let state = h.read("/ufw.state");

    // Twice: nothing changes.
    let out2 = run(&h, &s);
    assert_eq!(calls(&h).lines().filter(|l| !l.starts_with("udevadm trigger")).collect::<Vec<_>>(), Vec::<&str>::new(), "second run:\n{out2}");
    assert_eq!(h.read("/ufw.state"), state);
    assert_eq!(h.read(SETUP_LIST), list);

    // The check now sees setup's pieces (udev would have given /dev/uinput the group).
    h.facts.linger = true;
    h.facts.uinput_gid = Some(105002);
    let after = h.check(Options { firewall: None, at_boot: true }).report;
    for id in ["lab-pads", "stream-pads", "headless-uinput", "firewall-profiles", "linger"] {
        assert_eq!(st(&after, id), State::Ok, "{id}: {:#?}", after.piece(id));
    }
    assert!(after.piece("stream-pads").unwrap().summary.contains("by quiver system setup"));

    // Undo: setup's files, groups, tagged rules and lingering go; the person's stay.
    let u = script::undo(&h.facts, NOW);
    assert!(u.contains("/etc/udev/rules.d/72-quiver-lab-pad.rules (not installed by setup)"), "{u}");
    let out = run(&h, &u);
    let log = calls(&h);
    assert!(h.root_path().join("etc/udev/rules.d/72-quiver-lab-pad.rules").exists(), "hand-installed rule stays");
    for x in [shipped::STREAM_RULE, shipped::UINPUT_RULE, shipped::SYSUSERS, shipped::UFW_PROFILES] {
        assert!(!h.root_path().join(x.dest.trim_start_matches('/')).exists(), "{} removed\n{out}", x.dest);
    }
    assert_eq!(h.read("/ufw.state"), "47984,47989,48010/tcp|10.0.0.0/24|\n", "only the untagged rule is left");
    let group = h.read("/etc/group");
    assert!(group.contains("quiver-lab-pad") && !group.contains("quiver-stream-pad") && !group.contains("quiver-uinput"), "{group}");
    assert!(log.contains("loginctl disable-linger ajg") && log.contains("udevadm control --reload"), "{log}");
    assert!(!h.root_path().join(SETUP_LIST.trim_start_matches('/')).exists());
    // Undo again: nothing left to do.
    run(&h, &script::undo(&h.facts, NOW));
    assert!(!calls(&h).contains("delete"));
}

#[test]
fn undo_keeps_a_group_a_rule_still_uses() {
    let h = headless();
    let c = h.check(Options::default());
    run(&h, &script::install(&c.report, &c.plan, &h.facts, h.facts.contrib.as_ref().unwrap(), NOW).unwrap());
    calls(&h);
    // The person copies the stream rule into /usr/lib themselves after setup.
    h.write("/usr/lib/udev/rules.d/72-quiver-stream-pad.rules", shipped::STREAM_RULE.text);
    let out = run(&h, &script::undo(&h.facts, NOW));
    assert!(out.contains("kept group quiver-stream-pad: a rule still uses it"), "{out}");
    assert!(h.read("/etc/group").contains("quiver-stream-pad"));
}

#[test]
fn the_script_refuses_a_changed_source() {
    let h = headless();
    let c = h.check(Options::default());
    let src = h.facts.contrib.clone().unwrap();
    let s = script::install(&c.report, &c.plan, &h.facts, &src, NOW).unwrap();
    std::fs::write(src.join(shipped::STREAM_RULE.src), "# tampered\n").unwrap();
    let path = h.dir.path().join("s.sh");
    std::fs::write(&path, &s).unwrap();
    let out = Command::new("sh").arg(&path).env("QUIVER_SETUP_ROOT", h.root_path()).env("PATH", format!("{}:{}", tools(h.dir.path()).display(), std::env::var("PATH").unwrap())).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't the file this Quiver ships"));
    // And the check says so before any script is written.
    assert!(h.check(Options::default()).report.contrib_problem.unwrap().contains("isn't the file this quiver ships"));
}

#[test]
fn without_a_fake_root_it_wants_root() {
    if unsafe { libc::getuid() } == 0 {
        return;
    }
    let h = headless();
    let c = h.check(Options::default());
    let s = script::install(&c.report, &c.plan, &h.facts, h.facts.contrib.as_ref().unwrap(), NOW).unwrap();
    let path = h.dir.path().join("s.sh");
    std::fs::write(&path, &s).unwrap();
    let out = Command::new("sh").arg(&path).env_remove("QUIVER_SETUP_ROOT").output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("run it with sudo"));
}

#[test]
fn report_states_serialize_for_agents() {
    let r = this_machine().check(Options::default()).report;
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j["firewall"], "ufw");
    assert!(j["pieces"].as_array().unwrap().iter().any(|p| p["state"] == "not-needed"));
    let back: Report = serde_json::from_value(j).unwrap();
    assert_eq!(states(&back), states(&r));
}
