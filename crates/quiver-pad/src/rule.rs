//! The udev rule that keeps lab pads away from the host (`contrib/lab/72-quiver-lab-pad.rules`),
//! and the checks that it did.
//!
//! The rule gives every lab pad's event and js nodes `root:quiver-lab-pad 0660`, no `uaccess`
//! tag (so no ACL for the session user) and no seat, and libinput ignores them. The group is a
//! host gid in the user's subordinate range, so nothing on the host runs with it, and rootless
//! podman maps it to a gid only the container has ([`Plan::container_gid`]).
//!
//! **Before** a pad is made: [`plan`] (the rule is installed and is ours, its group resolves,
//! the gid is in the subgid range, it isn't one of the user's groups, it has no members).
//! **After**: [`wait_hidden`] polls each node until udev has finished with it (its database
//! entry carries `QUIVER_LAB_PAD=1`) and then requires root ownership, the group, mode 0660, no
//! ACL, no current `uaccess` tag, and that this process (the user) can't open it. The caller
//! destroys the pad when any of it fails.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const RULE: &str = include_str!("../../../contrib/lab/72-quiver-lab-pad.rules");
pub const SYSUSERS: &str = include_str!("../../../contrib/lab/quiver-lab-pad.sysusers");
pub const RULE_FILE: &str = "72-quiver-lab-pad.rules";
pub const MARKER: &str = "quiver-lab-pad rule v1";
pub const GROUP: &str = "quiver-lab-pad";
/// The gid the shipped files give the group.
pub const GID: u32 = 105000;

/// udev's rule folders, in precedence order (a file in /etc hides the same name in /usr/lib).
pub const RULE_DIRS: &[&str] = &["/etc/udev/rules.d", "/run/udev/rules.d", "/usr/lib/udev/rules.d", "/lib/udev/rules.d"];

/// How to install the rule (root's job; Quiver never does it). `quiver lab pad-rule` prints
/// the rule this build carries.
pub fn install_hint() -> String {
    format!(
        "lab pads need the udev rule that hides them from the host (a system package installs it). As root:\n  \
         sudo groupadd --system --gid {GID} {GROUP}\n  \
         quiver lab pad-rule | sudo install -Dm644 /dev/stdin /etc/udev/rules.d/{RULE_FILE}\n  \
         sudo udevadm control --reload\n\
         then quiver lab doctor"
    )
}

/// What a pad needs, checked before one is made.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub rule: PathBuf,
    pub group: String,
    /// The host gid the nodes get.
    pub gid: u32,
    /// The same gid inside a lab container (rootless podman, --userns=keep-id).
    pub container_gid: u32,
}

/// The rule file udev uses for our name, if any.
pub fn find_rule(dirs: &[&str]) -> Option<PathBuf> {
    dirs.iter().map(|d| Path::new(d).join(RULE_FILE)).find(|p| p.exists())
}

/// The group a rule's text assigns (`GROUP:="…"`), when it's ours.
pub fn rule_group(text: &str) -> Result<String> {
    if !text.contains(MARKER) {
        bail!("it isn't Quiver's lab pad rule (no '{MARKER}' in it)");
    }
    let g = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| l.split("GROUP:=\"").nth(1).and_then(|r| r.split('"').next()))
        .context("it sets no GROUP:=")?;
    Ok(g.to_owned())
}

/// `name:x:gid:members` from /etc/group text.
pub fn group_entry(group_file: &str, name: &str) -> Option<(u32, Vec<String>)> {
    group_file.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() < 4 || f[0] != name {
            return None;
        }
        let gid = f[2].parse().ok()?;
        Some((gid, f[3].split(',').filter(|m| !m.is_empty()).map(str::to_owned).collect()))
    })
}

/// (start, count) ranges of `user` (or `uid`) in /etc/subgid text.
pub fn subgid_ranges(subgid: &str, user: &str, uid: u32) -> Vec<(u32, u32)> {
    subgid
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(':').collect();
            if f.len() != 3 || (f[0] != user && f[0] != uid.to_string()) {
                return None;
            }
            Some((f[1].parse().ok()?, f[2].parse().ok()?))
        })
        .collect()
}

/// The container gid rootless podman with `--userns=keep-id` gives host gid `host` (in the
/// user's subordinate ranges, concatenated in order, after the user's own gid `my_gid`).
///
/// The rootless namespace maps 0 to the user's gid and 1.. to the subordinate ranges; keep-id
/// then maps container `my_gid` to 0, container 0..my_gid-1 to 1..my_gid, and the rest to
/// themselves. Measured: subgid 100000:65536, gid 1000 → host 105000 is container 5001.
pub fn container_gid(host: u32, ranges: &[(u32, u32)], my_gid: u32) -> Option<u32> {
    let mut base = 1u32;
    for (start, count) in ranges {
        if host >= *start && host < start + count {
            let i = base + (host - start);
            return Some(if i <= my_gid { i - 1 } else { i });
        }
        base += count;
    }
    None
}

/// Everything a pad needs, before one is made (the module docs).
pub fn plan() -> Result<Plan> {
    let rule = find_rule(RULE_DIRS).with_context(|| format!("the udev rule {RULE_FILE} isn't installed\n{}", install_hint()))?;
    let text = std::fs::read_to_string(&rule).with_context(|| format!("read {}", rule.display()))?;
    let group = rule_group(&text).with_context(|| format!("{}", rule.display()))?;
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let (gid, members) = group_entry(&groups, &group).or_else(|| lookup_group(&group).map(|g| (g, Vec::new()))).with_context(|| {
        format!("{} gives the pads' nodes group '{group}', which doesn't exist: udev then leaves them in the default group. As root: sudo groupadd --system --gid {GID} {group}; sudo udevadm control --reload", rule.display())
    })?;
    if !members.is_empty() {
        bail!("group '{group}' (gid {gid}) has members ({}): they could open every lab pad. It must have none", members.join(", "));
    }
    let my_gid = unsafe { libc::getgid() };
    let uid = unsafe { libc::getuid() };
    if gid == my_gid || my_groups().contains(&gid) || gid == 0 {
        bail!("group '{group}' (gid {gid}) is one of yours: you could open the pads");
    }
    let user = std::env::var("USER").unwrap_or_default();
    let ranges = subgid_ranges(&std::fs::read_to_string("/etc/subgid").unwrap_or_default(), &user, uid);
    let container_gid = container_gid(gid, &ranges, my_gid).with_context(|| {
        format!("group '{group}' is gid {gid}, outside your subordinate gids ({ranges:?} in /etc/subgid): a lab container can't be given it. Make it a gid in that range (the shipped one is {GID})")
    })?;
    Ok(Plan { rule, group, gid, container_gid })
}

fn lookup_group(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

fn my_groups() -> Vec<u32> {
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v = vec![0 as libc::gid_t; n as usize];
    let n = unsafe { libc::getgroups(n, v.as_mut_ptr()) };
    v.truncate(n.max(0) as usize);
    v
}

/// What a node looks like after udev, as the checks see it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeCheck {
    pub path: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub acl: bool,
    /// udev's database entry for it says our rule ran (`E:QUIVER_LAB_PAD=1`).
    pub rule_ran: bool,
    /// It still has the `uaccess` tag (a current tag, `Q:`).
    pub uaccess: bool,
    /// This process (the user) could open it for reading.
    pub openable: bool,
}

impl NodeCheck {
    /// Why it isn't hidden (None: it is).
    pub fn problem(&self, gid: u32) -> Option<String> {
        let p = self.path.display();
        if !self.rule_ran {
            return Some(format!("{p}: udev's entry for it doesn't show the lab pad rule ran"));
        }
        if self.openable {
            return Some(format!("{p}: you can open it, so Steam and your games could"));
        }
        if self.acl {
            return Some(format!("{p}: it has an ACL (a session's uaccess)"));
        }
        if self.uaccess {
            return Some(format!("{p}: it still has the uaccess tag"));
        }
        if self.uid != 0 || self.gid != gid || self.mode & 0o777 != 0o660 {
            return Some(format!("{p}: it is {}:{} {:o}, not root:{gid} 660", self.uid, self.gid, self.mode & 0o777));
        }
        None
    }
}

/// Look at one node now.
pub fn check_node(path: &Path) -> Result<NodeCheck> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !md.file_type().is_char_device() {
        bail!("{} isn't a character device", path.display());
    }
    let rdev = md.rdev();
    let (maj, min) = (libc::major(rdev), libc::minor(rdev));
    let db = std::fs::read_to_string(format!("/run/udev/data/c{maj}:{min}")).unwrap_or_default();
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let acl = unsafe { libc::getxattr(c.as_ptr(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) } >= 0;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    Ok(NodeCheck {
        path: path.to_owned(),
        uid: md.uid(),
        gid: md.gid(),
        mode: md.mode(),
        acl,
        rule_ran: db.lines().any(|l| l == "E:QUIVER_LAB_PAD=1"),
        uaccess: db.lines().any(|l| l == "Q:uaccess"),
        openable: fd >= 0,
    })
}

/// Wait (up to `timeout`) until udev is done with every node and it is hidden; the error
/// names what isn't. udev sets the group and mode before it runs the late uaccess step, so a
/// node is only judged once its database entry is there and says our rule ran.
pub fn wait_hidden(nodes: &[PathBuf], gid: u32, timeout: Duration) -> Result<Vec<NodeCheck>> {
    let end = Instant::now() + timeout;
    loop {
        let checks: Vec<NodeCheck> = nodes.iter().map(|n| check_node(n)).collect::<Result<_>>()?;
        // Fail at once if this process can open a node: hidden must mean hidden now.
        if let Some(c) = checks.iter().find(|c| c.openable) {
            bail!("{}", c.problem(gid).unwrap());
        }
        let problems: Vec<String> = checks.iter().filter_map(|c| c.problem(gid)).collect();
        if problems.is_empty() {
            return Ok(checks);
        }
        if Instant::now() > end {
            bail!("the lab pad rule didn't hide the pad: {}", problems.join("; "));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_rule() {
        assert_eq!(rule_group(RULE).unwrap(), GROUP);
        assert!(rule_group("GROUP:=\"x\"").is_err(), "not ours without the marker");
        // The rule must undo uaccess before 73-seat-late.rules turns it into an ACL.
        assert!(RULE_FILE.starts_with("72-"));
        let rules: Vec<&str> = RULE.lines().filter(|l| !l.trim_start().starts_with('#')).collect();
        let body = rules.join("\n");
        for must in ["TAG-=\"uaccess\"", "TAG-=\"seat\"", "LIBINPUT_IGNORE_DEVICE}=\"1\"", "MODE:=\"0660\"", "OWNER:=\"root\"", "KERNEL!=\"event*|js*\"", "ATTRS{phys}!=\"quiver-lab/*\"", "ATTRS{name}!=\"Quiver Lab Pad *\"", "QUIVER_LAB_PAD}=\"1\""] {
            assert!(body.contains(must), "rule lacks {must}");
        }
        assert!(SYSUSERS.contains(&format!("g {GROUP} {GID}")));
        // The names the rule matches are the ones the pads get.
        assert!(crate::model::pad_name("awr", 1).starts_with(crate::model::NAME_PREFIX));
        assert!(crate::model::pad_phys("awr", 0).starts_with(crate::model::PHYS_PREFIX));
    }

    #[test]
    fn gids_map_like_rootless_keep_id() {
        let r = subgid_ranges("root:1:2\najg:100000:65536\n", "ajg", 1000);
        assert_eq!(r, vec![(100000, 65536)]);
        assert_eq!(subgid_ranges("1000:200000:10\n", "nobody", 1000), vec![(200000, 10)]);
        // Measured with podman 5 (crates/quiver-pad docs): host 105000 is container 5001.
        assert_eq!(container_gid(105000, &r, 1000), Some(5001));
        // Host 100000..100999 are container 0..999; 101000 is 1001 (1000 is the user's own).
        assert_eq!(container_gid(100000, &r, 1000), Some(0));
        assert_eq!(container_gid(100999, &r, 1000), Some(999));
        assert_eq!(container_gid(101000, &r, 1000), Some(1001));
        assert_eq!(container_gid(99999, &r, 1000), None);
        assert_eq!(container_gid(165536, &r, 1000), None);
        // A second range follows the first.
        assert_eq!(container_gid(300004, &[(100000, 65536), (300000, 10)], 1000), Some(65541));
    }

    #[test]
    fn group_entries() {
        let g = "wheel:x:998:ajg\nquiver-lab-pad:x:105000:\nbad:x:12:a,b\n";
        assert_eq!(group_entry(g, "quiver-lab-pad"), Some((105000, vec![])));
        assert_eq!(group_entry(g, "bad"), Some((12, vec!["a".into(), "b".into()])));
        assert_eq!(group_entry(g, "none"), None);
    }

    #[test]
    fn node_problems() {
        let ok = NodeCheck { path: "/dev/input/event9".into(), uid: 0, gid: GID, mode: 0o20660, acl: false, rule_ran: true, uaccess: false, openable: false };
        assert_eq!(ok.problem(GID), None);
        assert!(NodeCheck { openable: true, ..ok.clone() }.problem(GID).unwrap().contains("you can open it"));
        assert!(NodeCheck { acl: true, ..ok.clone() }.problem(GID).unwrap().contains("ACL"));
        assert!(NodeCheck { rule_ran: false, ..ok.clone() }.problem(GID).unwrap().contains("rule ran"));
        assert!(NodeCheck { gid: 1000, ..ok.clone() }.problem(GID).unwrap().contains("not root:105000 660"));
        assert!(NodeCheck { mode: 0o20666, ..ok.clone() }.problem(GID).is_some());
    }

    #[test]
    fn installed_rule_lookup() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("etc"), t.path().join("usr"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let dirs = [a.to_str().unwrap(), b.to_str().unwrap()];
        assert_eq!(find_rule(&dirs), None);
        std::fs::write(b.join(RULE_FILE), RULE).unwrap();
        assert_eq!(find_rule(&dirs), Some(b.join(RULE_FILE)));
        std::fs::write(a.join(RULE_FILE), "# overridden\n").unwrap();
        assert_eq!(find_rule(&dirs), Some(a.join(RULE_FILE)), "/etc wins");
    }

    #[test]
    fn a_plain_file_isnt_a_node() {
        let t = tempfile::NamedTempFile::new().unwrap();
        assert!(check_node(t.path()).is_err());
    }
}
