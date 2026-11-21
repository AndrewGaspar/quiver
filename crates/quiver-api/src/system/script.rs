//! The one root step, as a shell script the person reads first (design/INSTALL.md §4).
//!
//! - It installs exactly the [`Plan`]: files copied byte for byte from the installed Quiver
//!   (each checked against the sha256 this build ships before it's copied), the groups, the
//!   firewall rules (from the LAN and the tailnet only, tagged `quiver`), lingering.
//! - Every step checks first, so running it twice changes nothing the second time.
//! - `QUIVER_SETUP_ROOT` points every path at a fake root (tests); unset, it's `/` and the
//!   script refuses to run as anyone but root.
//! - What it installed goes in [`super::SETUP_LIST`], which `--undo` reads.

use std::fmt::Write as _;
use std::path::Path;

use super::firewall::{Kind, PortGroup, Scope, Server, TAG, TAILNET_V4, TAILNET_V6, TAILSCALE_IFACE};
use super::shipped::{self, Shipped};
use super::{Facts, Plan, Report, SETUP_LIST, State};

/// Single-quote for sh.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:,=@+".contains(c)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

const PRELUDE: &str = r#"set -eu

# Every path below is under $ROOT: empty for the real system; tests point it at a fake one.
ROOT=${QUIVER_SETUP_ROOT:-}
if [ -z "$ROOT" ] && [ "$(id -u)" != 0 ]; then
    echo "This is Quiver's root step: run it with sudo (sudo sh $0)." >&2
    exit 1
fi
LIST="$ROOT@LIST@"
mkdir -p "$(dirname "$LIST")"
touch "$LIST"
note() { grep -qxF "$1" "$LIST" || echo "$1" >>"$LIST"; }
udev_changed=0
"#;

const PUT_FN: &str = r#"
# put REL DEST SHA256: install $SRC/REL as DEST unless DEST already is that file. The
# source must be the exact file this Quiver ships.
put() {
    if ! printf '%s  %s\n' "$3" "$SRC/$1" | sha256sum -c --status; then
        echo "setup: $SRC/$1 isn't the file this Quiver ships (sha256 $3); reinstall Quiver and run quiver system setup again" >&2
        exit 1
    fi
    if [ -f "$ROOT$2" ] && cmp -s "$SRC/$1" "$ROOT$2"; then
        return 0
    fi
    install -Dm644 "$SRC/$1" "$ROOT$2"
    note "file $2"
    echo "installed $2"
    case $2 in */udev/rules.d/*) udev_changed=1 ;; esac
}
"#;

const UFW_FNS: &str = r#"
# ufw_has SPEC WHERE: `ufw status` already allows SPEC (47984,47989,48010/tcp) from WHERE (a
# subnet, or "on IFACE"), as a rule of your own may.
ufw_has() {
    ufw status | awk -v spec="$1" -v where="$2" '
        where ~ /^on / { split(where, w, " "); if ($1 == spec && $2 == "on" && $3 == w[2] && $4 == "ALLOW") found = 1; next }
        $1 == spec && $2 == "ALLOW" && ($3 == where || ($3 == "IN" && $4 == where)) { found = 1 }
        END { exit !found }'
}
"#;

fn put_line(s: &Shipped, dest: &str) -> String {
    format!("put {} {} {}\n", s.src, sh_quote(dest), s.sha256())
}

fn ufw_where(scope: &Scope) -> (String, String) {
    match scope {
        Scope::Lan { net, .. } => (net.to_string(), format!("from {net}")),
        Scope::Tailnet => (format!("on {TAILSCALE_IFACE}"), format!("in on {TAILSCALE_IFACE}")),
    }
}

fn ufw_lines(out: &mut String, server: Server, scope: &Scope, missing: &[PortGroup], plan: &Plan) {
    let lan = matches!(scope, Scope::Lan { .. });
    let groups = server.groups(&plan.features, lan);
    let (where_, rule_from) = ufw_where(scope);
    let what = if missing.is_empty() { "can't read the rules without root, so this asks ufw first".to_owned() } else { format!("{} not allowed yet", missing.iter().map(PortGroup::human).collect::<Vec<_>>().join("; ")) };
    let _ = writeln!(out, "# {} from {}: {what}.", server.label(), scope.describe());
    let has: Vec<String> = groups.iter().map(|g| format!("ufw_has {} {}", g.spec(), sh_quote(&where_))).collect();
    if server.profile_fits(&plan.features) {
        let _ = writeln!(out, "{} || ufw allow {rule_from} to any app {} comment {TAG}", has.join(" && "), sh_quote(server.profile()));
    } else {
        // Ports other than the profile's: a rule per group, still tagged.
        let _ = writeln!(out, "# (other ports than the \"{}\" profile's are configured, so the rules name them)", server.profile());
        for g in &groups {
            let _ = writeln!(out, "ufw_has {} {} || ufw allow proto {} {rule_from} to any port {} comment {TAG}", g.spec(), sh_quote(&where_), g.proto.name(), g.ports());
        }
    }
}

fn firewalld_lines(out: &mut String, server: Server, scope: &Scope, plan: &Plan) {
    let (zone_of, sources): (String, Vec<(&str, String)>) = match scope {
        Scope::Lan { iface, net } => (iface.clone(), vec![(if net.v6() { "ipv6" } else { "ipv4" }, net.to_string())]),
        Scope::Tailnet => (TAILSCALE_IFACE.into(), vec![("ipv4", format!("{}/{}", TAILNET_V4.0, TAILNET_V4.1)), ("ipv6", format!("{}/{}", TAILNET_V6.0, TAILNET_V6.1))]),
    };
    let _ = writeln!(out, "# {} from {} (the zone {zone_of} is in, else the default zone).", server.label(), scope.describe());
    if !server.profile_fits(&plan.features) {
        let _ = writeln!(
            out,
            "# Other ports than the quiver-{} service's are configured; firewalld rich rules can't carry a tag, so allow\n# {} yourself.",
            server.service().trim_start_matches("quiver-"),
            server.groups(&plan.features, matches!(scope, Scope::Lan { .. })).iter().map(PortGroup::human).collect::<Vec<_>>().join("; ")
        );
        return;
    }
    let _ = writeln!(out, "z=$(firewall-cmd --get-zone-of-interface={zone_of} 2>/dev/null || firewall-cmd --get-default-zone)");
    for (family, src) in sources {
        let rule = format!("rule family=\"{family}\" source address=\"{src}\" service name=\"{}\" accept", server.service());
        let q = sh_quote(&rule);
        let _ = writeln!(out, "firewall-cmd --permanent --zone=\"$z\" --query-rich-rule={q} >/dev/null 2>&1 || firewall-cmd --permanent --zone=\"$z\" --add-rich-rule={q}");
    }
}

/// The install script for `plan` (`None`: nothing needs root). `src` is the installed
/// Quiver's `contrib` folder; `now` goes in the header.
pub fn install(report: &Report, plan: &Plan, f: &Facts, src: &Path, now: &str) -> Option<String> {
    if plan.is_empty() {
        return None;
    }
    let mut s = String::new();
    let _ = writeln!(s, "#!/bin/sh");
    let _ = writeln!(s, "# Quiver: the one root step (design/INSTALL.md), written by `quiver system setup` on {now}");
    let _ = writeln!(s, "# for {}. It installs only what `quiver system setup --check` found missing or outdated:", f.user);
    for p in report.pieces.iter().filter(|p| p.state.attention()) {
        let _ = writeln!(s, "#   {}: {} ({})", p.title, p.state.word(), p.unlocks);
    }
    let _ = writeln!(s, "# Running it again changes nothing. `quiver system setup --undo` removes what it installed.");
    let _ = writeln!(s, "# Files are copied byte for byte from the installed Quiver, never written here.");
    let _ = writeln!(s, "SRC={}", sh_quote(&src.display().to_string()));
    s.push_str(&PRELUDE.replace("@LIST@", SETUP_LIST));
    s.push_str(PUT_FN);

    let files = |pred: &dyn Fn(&Shipped) -> bool| -> Vec<(Shipped, String)> { plan.files.iter().filter(|(x, ..)| pred(x)).map(|(x, d, _)| (*x, d.clone())).collect() };
    let mut step = 0;
    let mut head = |s: &mut String, title: &str| {
        step += 1;
        let _ = writeln!(s, "\n# {step}. {title}");
    };

    if !plan.groups.is_empty() || plan.files.iter().any(|(x, ..)| *x == shipped::SYSUSERS) {
        head(&mut s, "Groups (contrib/sysusers/quiver.conf). Nobody is ever a member: each gid is in your subordinate\n#    range, so only a lab container or a stream's own namespace is given one.");
        let _ = write!(s, "{}", put_line(&shipped::SYSUSERS, shipped::SYSUSERS.dest));
        let names: Vec<&str> = plan.groups.iter().map(|(g, _)| *g).collect();
        let _ = writeln!(s, "new=\"\"\nfor g in {}; do\n    getent group \"$g\" >/dev/null || new=\"$new $g\"\ndone", names.join(" "));
        let _ = writeln!(s, "if [ -n \"$new\" ]; then\n    systemd-sysusers \"$ROOT{}\"\n    for g in $new; do\n        getent group \"$g\" >/dev/null && note \"group $g\" && echo \"created group $g\"\n    done\nfi", shipped::SYSUSERS.dest);
    }
    let rules = files(&|x| x.dest.contains("/udev/"));
    if !rules.is_empty() {
        head(&mut s, "udev rules (each carries its marker line, so Quiver recognises its version).");
        for (x, dest) in &rules {
            let why = plan.files.iter().find(|(y, ..)| y == x).map(|(_, _, w)| w.as_str()).unwrap_or("");
            let _ = writeln!(s, "# {why}: {}", x.file_name());
            s.push_str(&put_line(x, dest));
        }
    }
    if !rules.is_empty() || plan.uinput_trigger {
        let _ = writeln!(s, "if [ \"$udev_changed\" = 1 ]; then\n    udevadm control --reload\nfi");
    }
    if plan.uinput_trigger {
        let _ = writeln!(s, "# Apply the uinput rule to the /dev/uinput that exists now (new pads need nothing).");
        let _ = writeln!(s, "if [ -e \"$ROOT/sys/devices/virtual/misc/uinput\" ]; then\n    udevadm trigger --action=change /sys/devices/virtual/misc/uinput\nfi");
    }

    let kind = plan.firewall.unwrap_or(Kind::None);
    let fw_files = files(&|x| x.dest.contains("/ufw/") || x.dest.contains("/firewalld/"));
    if !fw_files.is_empty() || !plan.rules.is_empty() {
        head(
            &mut s,
            &format!("Firewall ({}): Quiver's profiles, then rules from your LAN and tailnet only, never from\n#    anywhere, for the servers you have on. Each rule is tagged \"{TAG}\" so --undo finds it.", kind.name()),
        );
        for (x, dest) in &fw_files {
            s.push_str(&put_line(x, dest));
        }
        match kind {
            Kind::Ufw if !plan.rules.is_empty() => {
                s.push_str(UFW_FNS);
                for (server, scope, missing) in &plan.rules {
                    ufw_lines(&mut s, *server, scope, missing, plan);
                }
            }
            Kind::Firewalld if !plan.rules.is_empty() => {
                let _ = writeln!(s, "firewall-cmd --reload   # so it knows Quiver's services");
                for (server, scope, _) in &plan.rules {
                    firewalld_lines(&mut s, *server, scope, plan);
                }
                let _ = writeln!(s, "firewall-cmd --reload");
            }
            _ => {}
        }
    }
    for n in &plan.notes {
        let _ = writeln!(s, "# Not done here: {n}");
    }
    if plan.linger {
        head(&mut s, "Lingering: your user manager (and quiverd.service, once enabled) starts at boot, before anyone\n#    signs in. polkit doesn't let you turn it on yourself here.");
        let u = sh_quote(&f.user);
        let _ = writeln!(s, "if [ ! -e \"$ROOT/var/lib/systemd/linger/{}\" ]; then\n    loginctl enable-linger {u}\n    note \"linger {}\"\n    echo \"lingering on for {}\"\nfi", f.user, f.user, f.user);
    }
    let _ = writeln!(s, "\necho \"done: check with quiver system setup --check\"");
    Some(s)
}

/// The undo script: removes what setup installed (its list), only while each file still
/// carries Quiver's marker; groups it made, once nothing uses them; firewall rules tagged
/// `quiver`. Rules and files it didn't install are left alone.
pub fn undo(f: &Facts, now: &str) -> String {
    let list = super::setup_list(f);
    let kind = Kind::of(f);
    let mut s = String::new();
    let _ = writeln!(s, "#!/bin/sh");
    let _ = writeln!(s, "# Quiver: undo the root step (design/INSTALL.md), written by `quiver system setup --undo` on {now}");
    let _ = writeln!(s, "# for {}. It removes only what `quiver system setup` installed ({SETUP_LIST}) and the", f.user);
    let _ = writeln!(s, "# firewall rules tagged \"{TAG}\". Everything else stays, the rules you installed by hand included:");
    let mut alone = Vec::new();
    for dir in super::UDEV_DIRS.iter().take(1).chain(["/etc/sysusers.d", "/etc/ufw/applications.d", "/etc/firewalld/services"].iter()) {
        for x in shipped::ALL {
            let p = format!("{dir}/{}", x.file_name());
            if parent(x.dest) == *dir && f.path(&p).exists() && !list.iter().any(|l| l == &format!("file {p}")) {
                alone.push(p);
            }
        }
    }
    if alone.is_empty() {
        let _ = writeln!(s, "#   (no other Quiver files)");
    }
    for p in &alone {
        let _ = writeln!(s, "#   {p} (not installed by setup)");
    }
    if kind == Kind::Ufw {
        let _ = writeln!(s, "#   ufw rules without the \"{TAG}\" comment");
    }
    s.push_str(&PRELUDE.replace("@LIST@", SETUP_LIST));
    let _ = writeln!(
        s,
        r#"
# rm_ours PATH MARKER: remove PATH if it still is Quiver's file.
rm_ours() {{
    [ -f "$ROOT$1" ] || return 0
    if ! grep -qF "$2" "$ROOT$1"; then
        echo "left alone: $1 (it isn't Quiver's any more)"
        return 0
    fi
    rm -f "$ROOT$1"
    echo "removed $1"
    case $1 in */udev/rules.d/*) udev_changed=1 ;; esac
}}"#
    );
    let files: Vec<&str> = list.iter().filter_map(|l| l.strip_prefix("file ")).collect();
    if !files.is_empty() {
        let _ = writeln!(s, "\n# Files setup installed.");
        for p in &files {
            if let Some(x) = shipped::by_dest_name(p) {
                let _ = writeln!(s, "rm_ours {} {}", sh_quote(p), sh_quote(x.marker));
            }
        }
        let _ = writeln!(s, "if [ \"$udev_changed\" = 1 ]; then\n    udevadm control --reload\nfi");
    }
    match kind {
        Kind::Ufw => {
            let _ = writeln!(s, "\n# ufw rules tagged \"{TAG}\" (yours, without the tag, stay), last first so the numbers hold.");
            let _ = writeln!(
                s,
                "i=0\nwhile n=$(ufw status numbered | sed -n 's/^\\[ *\\([0-9][0-9]*\\)\\].*# {TAG}$/\\1/p' | tail -n 1) && [ -n \"$n\" ]; do\n    i=$((i + 1))\n    [ \"$i\" -le 200 ] || {{ echo \"setup: ufw rule $n won't go away\" >&2; exit 1; }}\n    ufw --force delete \"$n\"\ndone"
            );
        }
        Kind::Firewalld => {
            let _ = writeln!(s, "\n# firewalld rich rules naming Quiver's services, in every zone.");
            let _ = writeln!(
                s,
                "for z in $(firewall-cmd --get-zones); do\n    firewall-cmd --permanent --zone=\"$z\" --list-rich-rules | grep 'service name=\"quiver-' | while IFS= read -r r; do\n        firewall-cmd --permanent --zone=\"$z\" --remove-rich-rule=\"$r\"\n    done\ndone\nfirewall-cmd --reload"
            );
        }
        _ => {}
    }
    let groups: Vec<&str> = list.iter().filter_map(|l| l.strip_prefix("group ")).collect();
    if !groups.is_empty() {
        let _ = writeln!(s, "\n# Groups setup made, once no rule names them and they have no members.");
        let _ = writeln!(
            s,
            "for g in {}; do\n    getent group \"$g\" >/dev/null || continue\n    if grep -qsE \"GROUP:?=\\\"$g\\\"\" \"$ROOT\"/etc/udev/rules.d/* \"$ROOT\"/usr/lib/udev/rules.d/*; then\n        echo \"kept group $g: a rule still uses it\"\n    elif [ -n \"$(getent group \"$g\" | cut -d: -f4)\" ]; then\n        echo \"kept group $g: it has members\"\n    else\n        groupdel \"$g\" && echo \"removed group $g\"\n    fi\ndone",
            groups.join(" ")
        );
        if files.contains(&shipped::SYSUSERS.dest) {
            let _ = writeln!(s, "# (sysusers would make them again at boot from {}, removed above)", shipped::SYSUSERS.dest);
        }
    }
    if let Some(user) = list.iter().find_map(|l| l.strip_prefix("linger ")) {
        let _ = writeln!(s, "\n# Lingering, which setup turned on.\nloginctl disable-linger {}", sh_quote(user));
    }
    let _ = writeln!(s, "\nrm -f \"$LIST\"\necho \"done: check with quiver system setup --check\"");
    s
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or(p)
}

/// The one line for a client to show when a piece needs the person (`None`: nothing to say).
pub fn hint(report: &Report, id: &str) -> Option<String> {
    let p = report.piece(id)?;
    match p.state {
        State::Missing | State::Outdated => Some(format!("{}: {} ({})", p.title, p.summary, p.fix.as_deref().unwrap_or("quiver system setup"))),
        State::Unknown if p.fix.is_some() => Some(format!("{}: {} ({})", p.title, p.summary, p.fix.as_deref().unwrap_or_default())),
        _ => None,
    }
}
