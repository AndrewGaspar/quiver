//! The stream pad rule's checks, and the user namespace a stream's program runs in so that it,
//! and nothing else on the machine, can open the stream's hidden pads.
//!
//! The namespace maps the person's own uid and gid to themselves and the pad group's gid to
//! itself (newuidmap/newgidmap, which allow exactly the person's own ids and their
//! `/etc/subgid` range). Inside it, this binary (`--sandbox-exec`) mounts a private tmpfs on
//! `/tmp/.X11-unix` (the real one is root's, which the namespace sees as unmapped, and
//! Xwayland refuses a socket folder not owned by root or the user; the host still reaches the
//! display through its abstract socket), sets the supplementary groups to just the person's
//! gid and the pad group, drops every capability, and runs the program:
//!
//! ```text
//! unshare --user --mount --map-users=U:U:1 --map-groups=G:G:1 --map-groups=P:P:1 --setgroups=allow --keep-caps \
//!   quiver-stream-host --sandbox-exec P -- PROGRAM…
//! ```
//!
//! The program is the same user with the same files, GPU and PipeWire; it has the pad group and
//! loses the person's other supplementary groups. No root step, no setuid helper of Quiver's.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use quiver_pad::rule;

pub const RULE: &str = include_str!("../../../contrib/stream/72-quiver-stream-pad.rules");
pub const SYSUSERS: &str = include_str!("../../../contrib/sysusers/quiver.conf");
pub const RULE_FILE: &str = "72-quiver-stream-pad.rules";
pub const MARKER: &str = "quiver-stream-pad rule v1";
pub const GROUP: &str = "quiver-stream-pad";
pub const GID: u32 = 105001;
/// The udev database line the rule leaves on every stream pad node.
pub const UDEV_MARK: &str = "E:QUIVER_STREAM_PAD=1";
pub const NAME_PREFIX: &str = "Quiver Stream Pad ";
pub const PHYS_PREFIX: &str = "quiver-stream/";

/// How to install the rule (the person does it; Quiver never runs anything as root).
pub fn install_hint() -> String {
    format!(
        "stream pads need the udev rule that hides them from your desktop: run `quiver system setup` (it shows the one root step, \
         then runs it with sudo), or by hand:\n  \
         sudo groupadd --system --gid {GID} {GROUP}\n  \
         sudo install -Dm644 contrib/stream/{RULE_FILE} /etc/udev/rules.d/{RULE_FILE}\n  \
         sudo udevadm control --reload"
    )
}

/// The pad's name and phys for slot `k` of stream `session`.
pub fn pad_name(session: &str, k: u8) -> String {
    if k == 0 { format!("{NAME_PREFIX}{session}") } else { format!("{NAME_PREFIX}{session} P{}", k + 1) }
}

pub fn pad_phys(session: &str, k: u8) -> String {
    format!("{PHYS_PREFIX}{session}/{k}")
}

/// The group a rule's text assigns, when the text is the stream pad rule.
pub fn rule_group(text: &str) -> Result<String> {
    if !text.contains(MARKER) {
        bail!("it isn't Quiver's stream pad rule (no '{MARKER}' in it)");
    }
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| l.split("GROUP:=\"").nth(1).and_then(|r| r.split('"').next()))
        .map(str::to_owned)
        .context("it sets no GROUP:=")
}

/// Check the rule is installed and usable: ours, its group resolves, has no members, isn't
/// one of the person's groups, and is inside their subgid range (so the namespace can map it).
/// Returns the gid.
pub fn check_rule() -> Result<u32> {
    let path: PathBuf = rule::RULE_DIRS.iter().map(|d| std::path::Path::new(d).join(RULE_FILE)).find(|p| p.exists()).with_context(|| format!("the udev rule {RULE_FILE} isn't installed\n{}", install_hint()))?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let group = rule_group(&text).with_context(|| path.display().to_string())?;
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let (gid, members) = rule::group_entry(&groups, &group).with_context(|| format!("{} gives the pads group '{group}', which doesn't exist\n{}", path.display(), install_hint()))?;
    if !members.is_empty() {
        bail!("group '{group}' has members ({}): they could open every stream pad", members.join(", "));
    }
    let my_gid = unsafe { libc::getgid() };
    let mut mine = vec![0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(mine.len() as i32, mine.as_mut_ptr()) }.max(0) as usize;
    if gid == 0 || gid == my_gid || mine[..n].contains(&gid) {
        bail!("group '{group}' (gid {gid}) is one of yours: you could open the pads");
    }
    let user = std::env::var("USER").unwrap_or_default();
    let uid = unsafe { libc::getuid() };
    let ranges = rule::subgid_ranges(&std::fs::read_to_string("/etc/subgid").unwrap_or_default(), &user, uid);
    if !ranges.iter().any(|(s, c)| gid >= *s && gid < s + c) {
        bail!("group '{group}' is gid {gid}, outside your subordinate gids ({ranges:?}): the stream's namespace can't be given it");
    }
    Ok(gid)
}

/// The command that runs `program` with the pad group, as the module docs describe. `exe` is
/// this crate's binary (`quiver-stream-host`), which does the inside part.
pub fn wrap(exe: &std::path::Path, pad_gid: u32, program: &[String]) -> Vec<String> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut c: Vec<String> = vec![
        "unshare".into(),
        "--user".into(),
        "--mount".into(),
        format!("--map-users={uid}:{uid}:1"),
        format!("--map-groups={gid}:{gid}:1"),
        format!("--map-groups={pad_gid}:{pad_gid}:1"),
        "--setgroups=allow".into(),
        "--keep-caps".into(),
        exe.display().to_string(),
        "--sandbox-exec".into(),
        pad_gid.to_string(),
        "--".into(),
    ];
    c.extend(program.iter().cloned());
    c
}

/// The inside part (`quiver-stream-host --sandbox-exec GID -- PROGRAM…`), run by `unshare`
/// with the namespace's capabilities: the private X socket folder, the groups, no
/// capabilities, then exec. Returns only on failure.
pub fn sandbox_exec(pad_gid: u32, program: &[String]) -> anyhow::Error {
    let run = || -> Result<std::convert::Infallible> {
        if program.is_empty() {
            bail!("--sandbox-exec needs a program");
        }
        let dir = std::path::Path::new("/tmp/.X11-unix");
        if !dir.exists() {
            std::fs::create_dir_all(dir).context("create /tmp/.X11-unix")?;
        }
        let r = unsafe { libc::mount(c"quiver-stream-x11".as_ptr(), c"/tmp/.X11-unix".as_ptr(), c"tmpfs".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, c"mode=1777,size=64k".as_ptr() as *const libc::c_void) };
        if r != 0 {
            bail!("mount a private /tmp/.X11-unix: {}", std::io::Error::last_os_error());
        }
        let groups = [unsafe { libc::getgid() }, pad_gid];
        if unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } != 0 {
            bail!("setgroups: {}", std::io::Error::last_os_error());
        }
        drop_capabilities()?;
        let err = std::os::unix::process::CommandExt::exec(std::process::Command::new(&program[0]).args(&program[1..]));
        Err(anyhow::Error::from(err).context(format!("run {}", program[0])))
    };
    match run() {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

/// Clear the ambient set, empty the bounding set, and drop every permitted, effective and
/// inheritable capability.
fn drop_capabilities() -> Result<()> {
    unsafe {
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
        for cap in 0..64 {
            libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
        }
    }
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut h = Header { version: 0x2008_0522, pid: 0 };
    let d = [Data { effective: 0, permitted: 0, inheritable: 0 }; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &mut h as *mut Header, d.as_ptr()) } != 0 {
        bail!("capset: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// The group `contrib/udev/72-quiver-uinput.rules` gives `/dev/uinput` (design/INSTALL.md §7).
pub const UINPUT_GROUP: &str = "quiver-uinput";
pub const UINPUT: &str = "/dev/uinput";

/// What to say when `/dev/uinput` can't be opened.
pub fn uinput_hint() -> String {
    "nobody is signed in at this machine, so no seat gives Quiver /dev/uinput: run `quiver system setup` \
     (it installs the rule that lets the stream host open it)"
        .into()
}

/// Open `/dev/uinput` for pads. Directly when the person may (their seat's uaccess ACL);
/// otherwise, when the uinput rule is installed, through a helper (`exe --uinput-open GID`) in a
/// user namespace that has the rule's group and nothing else, which hands the descriptor back
/// and exits. Nothing of the person's holds the group for longer than that open.
pub fn open_uinput(exe: Option<&std::path::Path>) -> Result<std::fs::File> {
    let e = match quiver_pad::uinput::Pad::open(std::path::Path::new(UINPUT)) {
        Ok(f) => return Ok(f),
        Err(e) => e,
    };
    let denied = e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied));
    if !denied {
        return Err(e);
    }
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let Some((gid, _)) = rule::group_entry(&groups, UINPUT_GROUP) else {
        bail!("{e:#}: {}", uinput_hint());
    };
    let node_gid = std::fs::metadata(UINPUT).map(|m| std::os::unix::fs::MetadataExt::gid(&m)).ok();
    if node_gid != Some(gid) {
        bail!(
            "{e:#}: {UINPUT} isn't group {UINPUT_GROUP} yet ({}); run `quiver system setup`",
            node_gid.map(|g| format!("it is gid {g}")).unwrap_or_else(|| "can't stat it".into())
        );
    }
    let exe = match exe {
        Some(e) => e.to_owned(),
        None => std::env::current_exe().context("find quiver-stream-host for the uinput helper")?,
    };
    open_via_helper(&exe, gid).with_context(|| format!("open {UINPUT} through the {UINPUT_GROUP} helper"))
}

/// `unshare … exe --uinput-open GID`: the namespace maps the person's own ids and the uinput
/// group's gid, as [`wrap`] does for the pad group.
pub fn uinput_argv(exe: &std::path::Path, uid: u32, my_gid: u32, gid: u32) -> Vec<String> {
    vec![
        "unshare".into(),
        "--user".into(),
        format!("--map-users={uid}:{uid}:1"),
        format!("--map-groups={my_gid}:{my_gid}:1"),
        format!("--map-groups={gid}:{gid}:1"),
        "--setgroups=allow".into(),
        "--keep-caps".into(),
        exe.display().to_string(),
        "--uinput-open".into(),
        gid.to_string(),
    ]
}

fn open_via_helper(exe: &std::path::Path, gid: u32) -> Result<std::fs::File> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let mut sv = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } != 0 {
        bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    // SAFETY: socketpair just made both.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
    let tv = libc::timeval { tv_sec: 10, tv_usec: 0 };
    // SAFETY: a valid socket and a timeval of the size given.
    unsafe { libc::setsockopt(sv[0], libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as u32) };
    let argv = uinput_argv(exe, unsafe { libc::getuid() }, unsafe { libc::getgid() }, gid);
    let child_fd = sv[1];
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if child_fd == 3 {
                let fl = libc::fcntl(3, libc::F_GETFD);
                libc::fcntl(3, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
            } else if libc::dup2(child_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("run {}", argv.join(" ")))?;
    drop(theirs);
    let got = recv_fd(&ours);
    let out = child.wait_with_output()?;
    match got {
        Ok(Some(fd)) => Ok(std::fs::File::from(fd)),
        _ => bail!("{}", String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("the helper sent nothing")),
    }
}

/// The helper's side (`quiver-stream-host --uinput-open GID`, run by `unshare` with the
/// namespace's capabilities): take the group, open `/dev/uinput`, drop every capability, send
/// the descriptor on fd 3, exit.
pub fn uinput_open_child(gid: u32) -> Result<()> {
    use std::os::fd::{AsFd, BorrowedFd};
    let groups = [unsafe { libc::getgid() }, gid];
    if unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } != 0 {
        bail!("setgroups: {}", std::io::Error::last_os_error());
    }
    let file = quiver_pad::uinput::Pad::open(std::path::Path::new(UINPUT));
    drop_capabilities()?;
    let file = file?;
    // SAFETY: the parent put the socket at fd 3 for us, and it stays open until we exit.
    let sock = unsafe { BorrowedFd::borrow_raw(3) };
    send_fd(sock, file.as_fd())
}

/// Send one descriptor with SCM_RIGHTS.
pub fn send_fd(sock: std::os::fd::BorrowedFd<'_>, fd: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut byte = [b'u'];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut cbuf = [0u64; 4];
    // SAFETY: a zeroed msghdr is valid; the fields set below point at live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    // SAFETY: the control buffer holds one fd's message (CMSG_SPACE(4) = 24 ≤ 32 bytes).
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut i32, fd.as_raw_fd());
    }
    if unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) } != 1 {
        bail!("sendmsg: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Receive one descriptor ([`send_fd`]); `None` when the other end closed without one.
pub fn recv_fd(sock: &impl std::os::fd::AsRawFd) -> Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let mut byte = [0u8];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut cbuf = [0u64; 4];
    // SAFETY: as in send_fd.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf);
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        bail!("recvmsg: {}", std::io::Error::last_os_error());
    }
    // SAFETY: recvmsg filled the control buffer it was given.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if n == 1 && !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const i32);
            return Ok(Some(std::os::fd::OwnedFd::from_raw_fd(fd)));
        }
    }
    Ok(None)
}

/// One node's state after udev, for the hidden check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub acl: bool,
    pub rule_ran: bool,
    pub uaccess: bool,
    /// This process (the person's uid, with their groups) could open it.
    pub openable: bool,
}

/// Why a node isn't hidden, or None.
pub fn problem(s: &NodeState, gid: u32) -> Option<String> {
    if s.openable {
        return Some("you can open it, so your desktop, Steam and your games could".into());
    }
    if !s.rule_ran {
        return Some("udev's entry for it doesn't show the stream pad rule ran".into());
    }
    if s.acl {
        return Some("it has an ACL (a session's uaccess)".into());
    }
    if s.uaccess {
        return Some("it still has the uaccess tag".into());
    }
    if s.uid != 0 || s.gid != gid || s.mode & 0o777 != 0o660 {
        return Some(format!("it is {}:{} {:o}, not root:{gid} 660", s.uid, s.gid, s.mode & 0o777));
    }
    None
}

pub fn node_state(path: &std::path::Path) -> Result<NodeState> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !md.file_type().is_char_device() {
        bail!("{} isn't a character device", path.display());
    }
    let rdev = md.rdev();
    let db = std::fs::read_to_string(format!("/run/udev/data/c{}:{}", libc::major(rdev), libc::minor(rdev))).unwrap_or_default();
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let acl = unsafe { libc::getxattr(c.as_ptr(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) } >= 0;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    Ok(NodeState { uid: md.uid(), gid: md.gid(), mode: md.mode(), acl, rule_ran: db.lines().any(|l| l == UDEV_MARK), uaccess: db.lines().any(|l| l == "Q:uaccess"), openable: fd >= 0 })
}

/// Wait (up to `timeout`) until udev has hidden every node; an openable node fails at once.
pub fn wait_hidden(nodes: &[PathBuf], gid: u32, timeout: std::time::Duration) -> Result<()> {
    let end = std::time::Instant::now() + timeout;
    loop {
        let mut problems = Vec::new();
        for n in nodes {
            let s = node_state(n)?;
            if s.openable {
                bail!("{}: {}", n.display(), problem(&s, gid).unwrap());
            }
            if let Some(p) = problem(&s, gid) {
                problems.push(format!("{}: {p}", n.display()));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() > end {
            bail!("the stream pad rule didn't hide the pad: {}", problems.join("; "));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_rule_matches_our_names_and_hides() {
        assert_eq!(rule_group(RULE).unwrap(), GROUP);
        assert!(rule_group(quiver_pad::rule::RULE).is_err(), "the lab's rule isn't ours");
        let body: String = RULE.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
        for must in [
            "TAG-=\"uaccess\"",
            "TAG-=\"seat\"",
            "LIBINPUT_IGNORE_DEVICE}=\"1\"",
            "MODE:=\"0660\"",
            "OWNER:=\"root\"",
            "KERNEL!=\"event*|js*\"",
            "ATTRS{phys}!=\"quiver-stream/*\"",
            "ATTRS{name}!=\"Quiver Stream Pad *\"",
            "QUIVER_STREAM_PAD}=\"1\"",
        ] {
            assert!(body.contains(must), "rule lacks {must}");
        }
        assert!(SYSUSERS.contains(&format!("g {GROUP} {GID}")));
        assert!(pad_name("s1", 0).starts_with(NAME_PREFIX) && pad_name("s1", 1).ends_with(" P2"));
        assert!(pad_phys("s1", 3).starts_with(PHYS_PREFIX));
        // Distinct from the lab's, so neither rule matches the other's pads.
        assert!(!pad_name("x", 0).starts_with("Quiver Lab Pad"));
    }

    #[test]
    fn the_namespace_has_only_the_pad_group_and_no_capabilities() {
        let w = wrap(std::path::Path::new("/x/quiver-stream-host"), 105001, &["gamescope".into(), "--".into(), "game".into()]).join(" ");
        assert!(w.starts_with("unshare --user --mount --map-users="));
        assert!(w.ends_with("--map-groups=105001:105001:1 --setgroups=allow --keep-caps /x/quiver-stream-host --sandbox-exec 105001 -- gamescope -- game"), "{w}");
    }

    #[test]
    fn the_uinput_helper_gets_only_the_uinput_group() {
        let a = uinput_argv(std::path::Path::new("/x/quiver-stream-host"), 1000, 1000, 105002).join(" ");
        assert_eq!(a, "unshare --user --map-users=1000:1000:1 --map-groups=1000:1000:1 --map-groups=105002:105002:1 --setgroups=allow --keep-caps /x/quiver-stream-host --uinput-open 105002");
        assert!(SYSUSERS.contains(&format!("g {UINPUT_GROUP} 105002")));
        assert!(uinput_hint().contains("quiver system setup") && install_hint().contains("quiver system setup"));
    }

    #[test]
    fn a_descriptor_crosses_the_socket() {
        use std::io::{Read, Seek, Write};
        use std::os::fd::{AsFd, FromRawFd, OwnedFd};
        let mut sv = [0i32; 2];
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) }, 0);
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(b"pad").unwrap();
        send_fd(a.as_fd(), f.as_fd()).unwrap();
        let mut got = std::fs::File::from(recv_fd(&b).unwrap().unwrap());
        got.rewind().unwrap();
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "pad");
        drop(a);
        assert!(recv_fd(&b).unwrap().is_none(), "closed without one");
    }

    #[test]
    fn node_problems() {
        let ok = NodeState { uid: 0, gid: 105001, mode: 0o20660, acl: false, rule_ran: true, uaccess: false, openable: false };
        assert_eq!(problem(&ok, 105001), None);
        assert!(problem(&NodeState { openable: true, ..ok.clone() }, 105001).unwrap().contains("you can open it"));
        assert!(problem(&NodeState { acl: true, ..ok.clone() }, 105001).is_some());
        assert!(problem(&NodeState { gid: 105000, ..ok.clone() }, 105001).is_some());
        assert!(problem(&NodeState { mode: 0o20666, ..ok.clone() }, 105001).is_some());
        assert!(problem(&NodeState { rule_ran: false, ..ok }, 105001).is_some());
    }
}
