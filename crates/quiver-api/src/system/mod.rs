//! `quiver system setup`: the parts of Quiver that need root (design/INSTALL.md).
//!
//! [`check`] looks at this machine without root and without changing anything ([`Facts`]),
//! and says for each piece whether it is **ok, missing, outdated, not needed here**, or that it
//! **can't tell** (a firewall whose rules only root may read), with the feature it unlocks and
//! a plain reason. The same pass makes a [`Plan`]: exactly the missing and outdated pieces,
//! which [`script::install`] turns into one commented, idempotent shell script for the person
//! to read and run with `sudo` once. [`script::undo`] removes what setup installed.
//!
//! Quiver never runs anything as root by itself, and agents never run setup: they report
//! what's missing (`quiver doctor --json`, the lab brief).

pub mod facts;
pub mod firewall;
pub mod script;
pub mod shipped;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use facts::{Facts, Features, Iface, Services};
use firewall::{Coverage, Kind, PortGroup, Scope, Server, Wanted};
use shipped::Shipped;

/// What `quiver system setup` installed, one `file <path>` / `group <name>` / `linger <user>`
/// line each, so `--undo` removes those and nothing else. Root-owned, world-readable.
pub const SETUP_LIST: &str = "/var/lib/quiver/system-setup.list";

/// udev's rule folders, in precedence order (a file in /etc hides the same name in /usr/lib).
pub const UDEV_DIRS: [&str; 4] = ["/etc/udev/rules.d", "/run/udev/rules.d", "/usr/lib/udev/rules.d", "/lib/udev/rules.d"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Ok,
    Missing,
    /// Installed, but an older version than this Quiver ships.
    Outdated,
    NotNeeded,
    /// Can't tell without root.
    Unknown,
    /// Something else is in the way (a file of that name that isn't Quiver's, a group with
    /// members); setup leaves it alone and says what to do.
    Conflict,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Missing => "missing",
            State::Outdated => "outdated",
            State::NotNeeded => "not needed here",
            State::Unknown => "can't tell",
            State::Conflict => "conflict",
        }
    }

    /// Worth telling the person about (doctor, the app, the lab brief).
    pub fn attention(self) -> bool {
        matches!(self, State::Missing | State::Outdated | State::Conflict)
    }

    fn rank(self) -> u8 {
        match self {
            State::NotNeeded => 0,
            State::Ok => 1,
            State::Unknown => 2,
            State::Outdated => 3,
            State::Missing => 4,
            State::Conflict => 5,
        }
    }

    fn worst(a: State, b: State) -> State {
        if b.rank() > a.rank() { b } else { a }
    }
}

/// One part of a piece (a rule file, a group, a scope's firewall rules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub what: String,
    pub state: State,
    pub detail: String,
}

/// One of design/INSTALL.md §1's pieces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    /// `lab-pads`, `stream-pads`, `headless-uinput`, `firewall-profiles`,
    /// `firewall-moonlight`, `firewall-remote`, `linger`.
    pub id: String,
    pub title: String,
    pub state: State,
    /// One line: where it stands.
    pub summary: String,
    /// The feature it unlocks.
    pub unlocks: String,
    /// Why it needs root, in plain words (the app shows this).
    pub reason: String,
    pub items: Vec<Item>,
    /// What to run about it (`quiver system setup`, `… --firewall lan,tailnet`, `quiver
    /// remote boot on`), when something is to be done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub user: String,
    pub firewall: Kind,
    /// The networks firewall rules are for (`192.168.50.0/24 (wlp191s0)`, `the tailnet
    /// (tailscale0)`).
    pub scopes: Vec<String>,
    /// The installed Quiver's copies of the files (`~/.local/lib/quiver/contrib`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contrib: Option<String>,
    /// Why setup can't copy them (missing, or not this build's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contrib_problem: Option<String>,
    pub pieces: Vec<Piece>,
}

impl Report {
    /// Pieces the person should hear about.
    pub fn attention(&self) -> impl Iterator<Item = &Piece> {
        self.pieces.iter().filter(|p| p.state.attention())
    }

    pub fn piece(&self, id: &str) -> Option<&Piece> {
        self.pieces.iter().find(|p| p.id == id)
    }

    /// The lines a client shows: `ok        Lab controllers: …`.
    pub fn lines(&self) -> Vec<String> {
        self.pieces.iter().map(|p| format!("{:<16} {}: {}", p.state.word(), p.title, p.summary)).collect()
    }
}

/// What `quiver system setup` was asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    /// `--firewall lan,tailnet`: add firewall rules for these scopes (`None`: check every
    /// scope, add none).
    pub firewall: Option<Wanted>,
    /// `--at-boot`: quiverd at boot without a sign-in (lingering).
    pub at_boot: bool,
}

/// What the script will do: exactly the missing and outdated pieces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// (file, destination, why).
    pub files: Vec<(Shipped, String, String)>,
    /// Groups to create (sysusers), with why.
    pub groups: Vec<(&'static str, u32)>,
    /// Re-apply the uinput rule to the node that exists.
    pub uinput_trigger: bool,
    pub firewall: Option<Kind>,
    /// Firewall rules to add: (server, scope, what's missing; empty when the rules can't be
    /// read and the script asks the firewall itself).
    pub rules: Vec<(Server, Scope, Vec<PortGroup>)>,
    /// The features (for ports when a profile doesn't fit).
    pub features: Features,
    /// `loginctl enable-linger <user>` (only where that needs root).
    pub linger: bool,
    /// Lines the script prints as comments: what Quiver won't do for you.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.groups.is_empty() && !self.uinput_trigger && self.rules.is_empty() && !self.linger
    }
}

/// The check and the plan from one look at the machine.
#[derive(Debug, Clone)]
pub struct Checked {
    pub report: Report,
    pub plan: Plan,
}

/// The lines of what setup installed before ([`SETUP_LIST`]).
pub fn setup_list(f: &Facts) -> Vec<String> {
    f.read(SETUP_LIST).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// A shipped file as installed: the one udev (or the firewall) would use.
struct Found {
    path: String,
    text: String,
}

fn find(f: &Facts, dirs: &[&str], name: &str) -> Option<Found> {
    dirs.iter().find_map(|d| {
        let path = format!("{d}/{name}");
        let p = f.path(&path);
        if !p.exists() {
            return None;
        }
        // An unreadable file still exists: say so rather than call it missing.
        Some(Found { text: std::fs::read_to_string(&p).unwrap_or_default(), path })
    })
}

/// Who put it there, for the summary.
fn origin(path: &str, list: &[String]) -> &'static str {
    if list.iter().any(|l| l.strip_prefix("file ") == Some(path)) {
        "by quiver system setup"
    } else if path.starts_with("/usr/") || path.starts_with("/lib/") {
        "by the package"
    } else if path.starts_with("/run/") {
        "at run time"
    } else {
        "by hand"
    }
}

/// A shipped file's state at `dirs`; adds it to the plan when missing or outdated.
fn file_item(f: &Facts, s: Shipped, dirs: &[&str], list: &[String], plan: &mut Plan, why: &str) -> Item {
    let name = s.file_name();
    let want = s.version();
    match find(f, dirs, name) {
        None => {
            plan.files.push((s, s.dest.to_owned(), why.to_owned()));
            Item { what: name.into(), state: State::Missing, detail: format!("not installed (setup puts it in {})", parent(s.dest)) }
        }
        Some(found) => match shipped::version(&found.text, s.marker) {
            None if found.text.is_empty() => Item { what: name.into(), state: State::Unknown, detail: format!("{} exists but isn't readable", found.path) },
            None => Item { what: name.into(), state: State::Conflict, detail: format!("{} isn't Quiver's (no '{}{want}' line); setup leaves it alone: move it away and run setup again", found.path, s.marker) },
            Some(v) if v < want => {
                // Replace it where it is when it's in /etc (ours, by its marker); a package's
                // older copy in /usr/lib gets an /etc copy that takes precedence.
                let dest = if found.path.starts_with("/etc/") { found.path.clone() } else { s.dest.to_owned() };
                plan.files.push((s, dest, why.to_owned()));
                Item { what: name.into(), state: State::Outdated, detail: format!("v{v} < v{want} in {} ({})", found.path, origin(&found.path, list)) }
            }
            Some(v) => {
                let same = found.text == s.text;
                Item {
                    what: name.into(),
                    state: State::Ok,
                    detail: format!("v{v} in {}, installed {}{}", parent(&found.path), origin(&found.path, list), if same { "" } else { " (its comments differ from this build's; the rule is the same version)" }),
                }
            }
        },
    }
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or(p)
}

/// `name:x:gid:members` from /etc/group text.
pub fn group_entry(text: &str, name: &str) -> Option<(u32, Vec<String>)> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() < 4 || f[0] != name {
            return None;
        }
        Some((f[2].parse().ok()?, f[3].split(',').filter(|m| !m.is_empty()).map(str::to_owned).collect()))
    })
}

fn subgid_ranges(text: &str, user: &str, uid: u32) -> Vec<(u32, u32)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(':').collect();
            if f.len() != 3 || (f[0] != user && f[0] != uid.to_string()) {
                return None;
            }
            Some((f[1].parse().ok()?, f[2].parse().ok()?))
        })
        .collect()
}

/// The group's state; adds it to the plan when missing.
fn group_item(f: &Facts, (name, want_gid): (&'static str, u32), list: &[String], plan: &mut Plan) -> (Item, Option<u32>) {
    let groups = f.read("/etc/group").unwrap_or_default();
    let entry = group_entry(&groups, name).or_else(|| (f.root == Path::new("/")).then(|| lookup_group(name).map(|g| (g, Vec::new()))).flatten());
    let what = format!("group {name}");
    let Some((gid, members)) = entry else {
        plan.groups.push((name, want_gid));
        return (Item { what, state: State::Missing, detail: format!("doesn't exist (setup creates it, gid {want_gid})") }, None);
    };
    let by = if list.iter().any(|l| l == &format!("group {name}")) { "made by quiver system setup" } else { "already there" };
    if !members.is_empty() {
        return (Item { what, state: State::Conflict, detail: format!("gid {gid} has members ({}): they could open every device it guards; remove them (sudo gpasswd -d USER {name})", members.join(", ")) }, Some(gid));
    }
    if gid == f.gid || f.groups.contains(&gid) {
        return (Item { what, state: State::Conflict, detail: format!("gid {gid} is one of your own groups, so you could open the devices it guards") }, Some(gid));
    }
    let ranges = subgid_ranges(&f.read("/etc/subgid").unwrap_or_default(), &f.user, f.uid);
    if !ranges.iter().any(|(s, c)| gid >= *s && gid < s + c) {
        return (
            Item { what, state: State::Conflict, detail: format!("gid {gid} is outside your subordinate gids ({ranges:?} in /etc/subgid), so no namespace can be given it (the shipped gid is {want_gid})") },
            Some(gid),
        );
    }
    let note = if gid == want_gid { String::new() } else { format!(" (not the shipped {want_gid}, which is fine: it's in your subordinate range)") };
    (Item { what, state: State::Ok, detail: format!("gid {gid}, no members, {by}{note}") }, Some(gid))
}

fn lookup_group(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getgrnam returns a static entry or null; read at once.
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

fn rollup(items: &[Item]) -> State {
    items.iter().fold(State::NotNeeded, |a, i| State::worst(a, i.state))
}

fn summary(items: &[Item]) -> String {
    let bad: Vec<String> = items.iter().filter(|i| i.state != State::Ok).map(|i| format!("{}: {}", i.what, i.detail)).collect();
    if bad.is_empty() { items.iter().map(|i| format!("{} {}", i.what, i.detail)).collect::<Vec<_>>().join("; ") } else { bad.join("; ") }
}

/// A rule file and its group, as one piece.
#[allow(clippy::too_many_arguments)]
fn pad_piece(f: &Facts, id: &str, title: &str, unlocks: &str, reason: &str, rule: Shipped, group: (&'static str, u32), needed: Result<(), String>, list: &[String], plan: &mut Plan) -> (Piece, Option<u32>) {
    let mut sub = Plan::default();
    let mut items = vec![file_item(f, rule, &UDEV_DIRS, list, &mut sub, title)];
    let (g, gid) = group_item(f, group, list, &mut sub);
    items.push(g);
    let state = rollup(&items);
    let piece = |state: State, summary: String, items: Vec<Item>, fix: Option<String>| Piece { id: id.into(), title: title.into(), state, summary, unlocks: unlocks.into(), reason: reason.into(), items, fix };
    if let Err(why) = &needed
        && state != State::Ok
    {
        return (piece(State::NotNeeded, why.clone(), items, None), gid);
    }
    plan.files.extend(sub.files);
    for g in sub.groups {
        if !plan.groups.contains(&g) {
            plan.groups.push(g);
        }
    }
    let fix = matches!(state, State::Missing | State::Outdated).then(|| "quiver system setup".to_owned());
    let s = if state == State::Ok { format!("installed ({})", summary(&items)) } else { summary(&items) };
    (piece(state, s, items, fix), gid)
}

#[allow(clippy::too_many_arguments)]
fn firewall_piece(f: &Facts, server: Server, kind: Kind, scopes: &[Scope], opts: &Options, rules: &Option<(Vec<firewall::UfwRule>, Vec<firewall::UfwRule>)>, zones: &Option<firewall::FirewalldZones>, plan: &mut Plan) -> Piece {
    let (id, title, unlocks, reason) = match server {
        Server::Moonlight => (
            "firewall-moonlight",
            "Firewall: Moonlight",
            "Moonlight clients (TVs, phones, other PCs) reaching this machine",
            "The firewall blocks Moonlight's ports until a rule allows them from your LAN and tailnet. Changing firewall rules needs root.",
        ),
        Server::Remote => (
            "firewall-remote",
            "Firewall: remote play",
            "your other Quiver machines reaching this one (remote play, streams)",
            "The firewall blocks remote play's control port and stream ports until a rule allows them from your LAN and tailnet. Changing firewall rules needs root.",
        ),
    };
    let mk = |state: State, summary: String, items: Vec<Item>, fix: Option<String>| Piece { id: id.into(), title: title.into(), state, summary, unlocks: unlocks.into(), reason: reason.into(), items, fix };
    let fx = &f.features;
    if !server.on(fx) {
        return mk(State::NotNeeded, format!("{} is off", server.label()), vec![], None);
    }
    let fix_cmd = "quiver system setup --firewall lan,tailnet".to_owned();
    match kind {
        Kind::None => return mk(State::NotNeeded, "no firewall is running here, so nothing blocks its ports".into(), vec![], None),
        Kind::Nftables => {
            let groups = server.groups(fx, true);
            plan.notes.push(format!("{}: allow {} from your LAN and tailnet in your nftables rules yourself (Quiver doesn't edit them)", server.label(), groups.iter().map(PortGroup::human).collect::<Vec<_>>().join("; ")));
            return mk(
                State::Unknown,
                format!("nftables rules are loaded directly (no ufw or firewalld); Quiver doesn't edit them. Allow {} from your LAN and tailnet yourself", groups.iter().map(PortGroup::human).collect::<Vec<_>>().join("; ")),
                vec![],
                None,
            );
        }
        _ => {}
    }
    if scopes.is_empty() || (server.tailnet_only(fx) && !scopes.contains(&Scope::Tailnet)) {
        return mk(State::Unknown, "no network to allow it from (no LAN interface is up, or it listens on the tailnet and there's no tailscale0)".into(), vec![], None);
    }
    let add = |scope: &Scope| opts.firewall.is_some_and(|w| match scope {
        Scope::Lan { .. } => w.lan,
        Scope::Tailnet => w.tailnet,
    });
    let mut items = Vec::new();
    for scope in scopes {
        if server.tailnet_only(fx) && matches!(scope, Scope::Lan { .. }) {
            continue;
        }
        let groups = server.groups(fx, matches!(scope, Scope::Lan { .. }));
        let cov: Option<Coverage> = match kind {
            Kind::Ufw => rules.as_ref().map(|(v4, v6)| firewall::ufw_coverage(v4, v6, &groups, scope)),
            Kind::Firewalld => zones.as_ref().map(|z| firewall::firewalld_coverage(z, server, &groups, scope)),
            _ => None,
        };
        match cov {
            None => {
                items.push(Item { what: scope.describe(), state: State::Unknown, detail: format!("can't tell without root ({} rules aren't readable)", kind.name()) });
                if add(scope) {
                    plan.rules.push((server, scope.clone(), Vec::new()));
                }
            }
            Some(c) if c.missing.is_empty() => {
                let by = match (c.by_hand, c.by_quiver) {
                    (true, true) => "by your own rules and Quiver's",
                    (false, true) => "by Quiver's rules (tagged quiver)",
                    _ => "by your own rules",
                };
                let wide = if c.from_anywhere { "; a rule allows some of it from anywhere, which is wider than Quiver needs" } else { "" };
                items.push(Item { what: scope.describe(), state: State::Ok, detail: format!("allowed {by}{wide}") });
            }
            Some(c) => {
                let what = c.missing.iter().map(PortGroup::human).collect::<Vec<_>>().join("; ");
                items.push(Item { what: scope.describe(), state: State::Missing, detail: format!("{what} not allowed yet") });
                if add(scope) {
                    plan.rules.push((server, scope.clone(), c.missing));
                }
            }
        }
    }
    let state = rollup(&items);
    let s = items.iter().map(|i| format!("{}: {}", i.what, i.detail)).collect::<Vec<_>>().join("; ");
    let fix = matches!(state, State::Missing | State::Unknown).then_some(fix_cmd);
    mk(state, format!("{}: {s}", kind.name()), items, fix)
}

/// Look at the machine (see the module docs).
pub fn check(f: &Facts, opts: &Options) -> Checked {
    let list = setup_list(f);
    let mut plan = Plan { features: f.features.clone(), ..Default::default() };
    let mut pieces = Vec::new();
    let streams = f.features.remote || f.features.moonlight;

    let (lab, _) = pad_piece(
        f,
        "lab-pads",
        "Lab controllers",
        "game controllers in lab instances (quiver lab … --pads)",
        "Lab controllers need a udev rule and a group so a lab instance's virtual pads are seen only inside its container, never by your desktop, Steam or your games. udev rules and groups are root's.",
        shipped::LAB_RULE,
        shipped::LAB_GROUP,
        Ok(()),
        &list,
        &mut plan,
    );
    pieces.push(lab);
    let (stream, _) = pad_piece(
        f,
        "stream-pads",
        "Stream controllers",
        "game controllers in streams from this machine",
        "Stream controllers need a udev rule and a group so games can open them while your desktop can't. udev rules and groups are root's.",
        shipped::STREAM_RULE,
        shipped::STREAM_GROUP,
        if streams { Ok(()) } else { Err("nothing streams from this machine (remote play and Moonlight are off)".into()) },
        &list,
        &mut plan,
    );
    pieces.push(stream);
    let headless = !f.seated || f.boot_enabled || opts.at_boot;
    let need_uinput = match (streams, headless) {
        (false, _) => Err("nothing streams from this machine (remote play and Moonlight are off)".to_owned()),
        (true, false) => Err("you're signed in at this machine, and your seat gives Quiver /dev/uinput; a machine that streams with nobody signed in at it needs this".to_owned()),
        (true, true) => Ok(()),
    };
    let (mut uinput, uinput_gid) = pad_piece(
        f,
        "headless-uinput",
        "Controllers with nobody seated",
        "stream controllers while nobody is signed in at this machine (a headless server)",
        "With nobody signed in at the machine there is no seat to give Quiver /dev/uinput, which makes the pads. A udev rule gives it a group only the stream host's helper gets.",
        shipped::UINPUT_RULE,
        shipped::UINPUT_GROUP,
        need_uinput,
        &list,
        &mut plan,
    );
    // Installed but not applied to the node yet (udev applies it at the next event).
    if uinput.state == State::Ok
        && let (Some(want), Some(now)) = (uinput_gid, f.uinput_gid)
        && want != now
    {
        uinput.items.push(Item { what: "/dev/uinput".into(), state: State::Missing, detail: format!("still group {now}: the rule applies at the next udev event (setup re-triggers it)") });
        uinput.state = State::Missing;
        uinput.summary = summary(&uinput.items);
        uinput.fix = Some("quiver system setup".into());
        plan.uinput_trigger = true;
    }
    if plan.files.iter().any(|(s, ..)| *s == shipped::UINPUT_RULE) {
        plan.uinput_trigger = true;
    }
    pieces.push(uinput);

    // Firewall.
    let kind = Kind::of(f);
    plan.firewall = Some(kind);
    let scopes = firewall::scopes(f, opts.firewall);
    let profiles = match kind {
        Kind::Ufw => {
            let mut sub = Plan::default();
            let i = file_item(f, shipped::UFW_PROFILES, &["/etc/ufw/applications.d"], &list, &mut sub, "ufw application profiles");
            Some((vec![i], sub.files))
        }
        Kind::Firewalld => {
            let mut sub = Plan::default();
            let dirs = ["/etc/firewalld/services", "/usr/lib/firewalld/services"];
            let i = vec![file_item(f, shipped::FIREWALLD_MOONLIGHT, &dirs, &list, &mut sub, "firewalld service"), file_item(f, shipped::FIREWALLD_REMOTE, &dirs, &list, &mut sub, "firewalld service")];
            Some((i, sub.files))
        }
        _ => None,
    };
    let any_server = f.features.moonlight || f.features.remote;
    let profile_piece = match profiles {
        None => Piece {
            id: "firewall-profiles".into(),
            title: "Firewall profiles".into(),
            state: State::NotNeeded,
            summary: match kind {
                Kind::None => "no firewall is running here".into(),
                _ => "nftables has no profiles; Quiver doesn't edit its rules".into(),
            },
            unlocks: "firewall rules by name (\"Quiver Moonlight\", \"Quiver Remote\")".into(),
            reason: "ufw and firewalld read application profiles from root's folders.".into(),
            items: vec![],
            fix: None,
        },
        Some((items, files)) => {
            let state = rollup(&items);
            let (state, summary_) = if !any_server && state != State::Ok { (State::NotNeeded, "remote play and Moonlight are off".to_owned()) } else { (state, summary(&items)) };
            if state.attention() {
                plan.files.extend(files);
            }
            Piece {
                id: "firewall-profiles".into(),
                title: "Firewall profiles".into(),
                state,
                summary: format!("{}: {summary_}", kind.name()),
                unlocks: "firewall rules by name (\"Quiver Moonlight\", \"Quiver Remote\"), and a checkbox in a firewall GUI".into(),
                reason: format!("{} reads application profiles from root's folders.", kind.name()),
                fix: state.attention().then(|| "quiver system setup".into()),
                items,
            }
        }
    };
    pieces.push(profile_piece);
    let rules = (kind == Kind::Ufw).then(|| match (f.read("/etc/ufw/user.rules"), f.read("/etc/ufw/user6.rules")) {
        (Ok(a), Ok(b)) => Some((firewall::parse_ufw(&a), firewall::parse_ufw(&b))),
        (Ok(a), Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Some((firewall::parse_ufw(&a), Vec::new())),
        _ => None,
    }).flatten();
    let zones = (kind == Kind::Firewalld).then(|| {
        let dir = f.path("/etc/firewalld/zones");
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                let texts: Option<Vec<String>> = rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "xml")).map(|e| std::fs::read_to_string(e.path()).ok()).collect();
                texts.map(|t| firewall::parse_firewalld(&t))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Default::default()),
            Err(_) => None,
        }
    }).flatten();
    for server in [Server::Moonlight, Server::Remote] {
        pieces.push(firewall_piece(f, server, kind, &scopes, opts, &rules, &zones, &mut plan));
    }

    // Lingering.
    let wanted = opts.at_boot || f.boot_enabled;
    let linger = if f.linger {
        Piece { id: "linger".into(), title: "Start at boot".into(), state: State::Ok, summary: "lingering is on: your user manager, and quiverd if it's enabled, start at boot".into(), unlocks: String::new(), reason: String::new(), items: vec![], fix: None }
    } else if !wanted {
        Piece { id: "linger".into(), title: "Start at boot".into(), state: State::NotNeeded, summary: "quiverd starts when you sign in (quiver system setup --at-boot starts it at boot)".into(), unlocks: String::new(), reason: String::new(), items: vec![], fix: None }
    } else {
        let root = f.linger_needs_root != Some(false);
        if root && opts.at_boot {
            plan.linger = true;
        }
        Piece {
            id: "linger".into(),
            title: "Start at boot".into(),
            state: State::Missing,
            summary: if root { format!("lingering is off, and turning it on needs root here (sudo loginctl enable-linger {})", f.user) } else { "lingering is off; you may turn it on yourself (quiver remote boot on)".into() },
            unlocks: String::new(),
            reason: String::new(),
            items: vec![],
            fix: Some(if root { "quiver system setup --at-boot".into() } else { "quiver remote boot on".into() }),
        }
    };
    let linger = Piece {
        unlocks: "quiverd at boot, before anyone signs in (a headless server)".into(),
        reason: "Lingering starts your user manager at boot. logind lets you turn it on yourself on most systems; where it doesn't, it needs root.".into(),
        ..linger
    };
    pieces.push(linger);

    // The source files the plan would copy.
    let contrib_problem = match &f.contrib {
        None if !plan.files.is_empty() => Some("Quiver's installed copies of these files aren't there (<libdir>/quiver/contrib): reinstall Quiver (tools/install-user.sh), or set QUIVER_CONTRIB_DIR to a checkout's contrib folder".to_owned()),
        None => None,
        Some(dir) => plan.files.iter().find_map(|(s, ..)| {
            let p = dir.join(s.src);
            match std::fs::read(&p) {
                Err(_) => Some(format!("{} is missing: reinstall Quiver", p.display())),
                Ok(b) if shipped::sha256(&b) != s.sha256() => Some(format!("{} isn't the file this quiver ships (an older install?): reinstall Quiver", p.display())),
                Ok(_) => None,
            }
        }),
    };
    let report = Report {
        user: f.user.clone(),
        firewall: kind,
        scopes: scopes.iter().map(Scope::describe).collect(),
        contrib: f.contrib.as_ref().map(|p| p.display().to_string()),
        contrib_problem,
        pieces,
    };
    Checked { report, plan }
}

#[cfg(test)]
mod tests;
