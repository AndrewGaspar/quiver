//! Mod overlays: an overlayfs mount over a game's install folder, visible only to the
//! processes of one launch.
//!
//! [`Namespace::create`] starts a short-lived holder process whose `pre_exec` (after
//! `fork`, before `exec`) calls `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, maps the caller's
//! uid and gid to themselves (not to root: Wine refuses a prefix owned by another uid),
//! makes the mount tree a slave of the host's, and calls `mount(2)` for the overlay while
//! it still holds the new namespace's capabilities. The daemon keeps the namespace alive
//! through `/proc/<holder>/ns/{user,mnt}` file descriptors and lets the holder exit.
//!
//! [`Namespace::enter`] makes a command join that namespace (`setns` in `pre_exec`), so the
//! bridge, Proton, its wineserver and the game all see the same overlay while the daemon
//! itself never enters it. Exec drops the namespace capabilities (the uid isn't 0 inside),
//! so the game runs with the same rights as before. The namespace goes away once its last
//! process exits and the daemon drops the [`Namespace`].
//!
//! **A streamed launch's pad group** ([`Namespace::create_with`]): a stream's hidden pads
//! (design/REMOTE.md) belong to a group no process of the person's has. The launch's
//! namespace then maps that group too, which an unprivileged process can't write into its own
//! `gid_map`: the holder stops after `unshare` and this process maps it from outside with
//! `newuidmap`/`newgidmap` (the person's own ids, and the pad gid from their `/etc/subgid`
//! range), as the stream host's own pad namespace does (`unshare --map-groups`). Every
//! process that enters sets its supplementary groups to its own gid and the pad group, so the
//! bridge, Proton, the wineserver, Wine's controller driver and the game can all open the pads,
//! and nothing outside the launch can. A launch with a stream and no mods gets the namespace
//! with nothing mounted.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// One overlay mount: `lower` (topmost first) stacked over `target`'s own contents,
/// mounted at `target`, with writes going to `upper`.
#[derive(Debug, Clone)]
pub struct OverlaySpec {
    /// Mount point; also the bottom layer (the vanilla install).
    pub target: PathBuf,
    /// Layers above the install, topmost first.
    pub lower: Vec<PathBuf>,
    /// Writable layer and overlayfs's work directory (same filesystem, both created if
    /// missing).
    pub upper: PathBuf,
    pub work: PathBuf,
    /// Folders inside `target` that stay the host's: each is cloned (`open_tree`) before
    /// the overlay is mounted and moved back on top of it after, so writes there reach the
    /// host folder, never `upper`. A profile's own settings folder keeps the saves inside it
    /// shared this way (design/PROFILES.md decision 4). Created if missing.
    pub shared: Vec<PathBuf>,
}

/// Escape a path for overlayfs's option string (`\`, `:` and `,` are separators).
fn escape(p: &Path) -> Result<String> {
    let s = p.to_str().with_context(|| format!("{} is not UTF-8", p.display()))?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | ':' | ',') {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

impl OverlaySpec {
    /// The `mount(2)` data string.
    pub fn options(&self) -> Result<String> {
        let mut lower: Vec<String> = self.lower.iter().map(|p| escape(p)).collect::<Result<_>>()?;
        lower.push(escape(&self.target)?);
        // userxattr: unprivileged overlayfs keeps its metadata in user.overlay.* xattrs.
        Ok(format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            lower.join(":"),
            escape(&self.upper)?,
            escape(&self.work)?
        ))
    }

    fn check(&self) -> Result<()> {
        for p in std::iter::once(&self.target).chain(&self.lower) {
            if !p.is_dir() {
                bail!("{} is not a directory", p.display());
            }
        }
        for p in [&self.upper, &self.work] {
            if p.starts_with(&self.target) || self.lower.iter().any(|l| p.starts_with(l)) {
                bail!("{} must not be inside a lower layer", p.display());
            }
        }
        for s in &self.shared {
            if !s.starts_with(&self.target) || s == &self.target {
                bail!("{} must be a folder inside {}", s.display(), self.target.display());
            }
            std::fs::create_dir_all(s).with_context(|| format!("creating {}", s.display()))?;
        }
        std::fs::create_dir_all(&self.upper)?;
        std::fs::create_dir_all(&self.work)?;
        Ok(())
    }
}

// The new mount API's flags (linux/mount.h), not all in the libc crate.
const OPEN_TREE_CLONE: u32 = 1;
const AT_RECURSIVE: u32 = 0x8000;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;

fn cstr(s: impl AsRef<OsStr>) -> Result<CString> {
    CString::new(s.as_ref().as_bytes()).context("path contains a NUL byte")
}

/// A user + mount namespace with an overlay mounted, held open by file descriptors.
#[derive(Debug)]
pub struct Namespace {
    user: File,
    mnt: File,
    /// The stream pad group mapped in, which every process entering gets.
    pad_gid: Option<u32>,
}

impl Namespace {
    /// Create the namespace and mount `spec` in it. The calling process stays where it is.
    pub fn create(spec: &OverlaySpec) -> Result<Namespace> {
        Self::create_many(std::slice::from_ref(spec))
    }

    /// Create the namespace with every overlay in `specs` mounted, in order (e.g. the
    /// install folder and a profile's own prefix folders). One mount failing fails all.
    pub fn create_many(specs: &[OverlaySpec]) -> Result<Namespace> {
        if specs.is_empty() {
            bail!("no overlay to mount");
        }
        Self::create_with(specs, None)
    }

    /// As [`Self::create_many`] (`specs` may be empty), and with `pad_gid` (a stream's pad
    /// group, inside the person's subgid range) mapped through newuidmap/newgidmap and given
    /// to every process that enters (the module docs).
    pub fn create_with(specs: &[OverlaySpec], pad_gid: Option<u32>) -> Result<Namespace> {
        for spec in specs {
            spec.check()?;
        }
        // SAFETY: getuid/getgid can't fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        // Everything the child touches is prepared here: after fork it may only make
        // async-signal-safe calls (no allocation).
        let uid_map = format!("{uid} {uid} 1\n");
        let gid_map = format!("{gid} {gid} 1\n");
        let options: Vec<String> = specs.iter().map(|s| s.options()).collect::<Result<_>>()?;
        let mounts: Vec<(CString, CString)> =
            specs.iter().zip(&options).map(|(s, o)| Ok((cstr(&s.target)?, CString::new(o.clone())?))).collect::<Result<_>>()?;
        // Folders that stay the host's, with a slot each for the cloned tree's descriptor
        // (filled in the child: no allocation after fork).
        let shared: Vec<CString> = specs.iter().flat_map(|s| &s.shared).map(cstr).collect::<Result<_>>()?;
        let mut shared_fds: Vec<libc::c_int> = vec![-1; shared.len()];
        let options = options.join(" | ");
        let (mut err_r, err_w) = io::pipe()?;
        let err_fd = err_w.as_raw_fd();
        // Without a pad group the holder writes its own maps; with one, the mapper does.
        let maps: Vec<(&'static std::ffi::CStr, Vec<u8>, &'static [u8])> = if pad_gid.is_some() {
            Vec::new()
        } else {
            vec![
                (c"/proc/self/setgroups", b"deny".to_vec(), b"writing setgroups"),
                (c"/proc/self/uid_map", uid_map.into_bytes(), b"writing uid_map"),
                (c"/proc/self/gid_map", gid_map.into_bytes(), b"writing gid_map"),
            ]
        };
        let mapper = match pad_gid {
            Some(pad) => Some(Mapper::start(uid, gid, pad)?),
            None => None,
        };
        let mapper_fds = mapper.as_ref().map(|m| (m.ready_w.as_raw_fd(), m.go_r.as_raw_fd(), m.go_w));

        // `cat` blocks on its stdin until we close it: just long enough to open its
        // namespace files.
        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: the closure only makes raw syscalls on data prepared above.
        unsafe {
            cmd.pre_exec(move || {
                let fail = |step: &[u8]| {
                    let e = io::Error::last_os_error();
                    libc::write(err_fd, step.as_ptr().cast(), step.len());
                    Err(e)
                };
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return fail(b"unshare(CLONE_NEWUSER|CLONE_NEWNS)");
                }
                if let Some((ready_w, go_r, go_w)) = mapper_fds {
                    // Our pid to the mapper outside, then wait for its verdict.
                    libc::close(go_w);
                    let pid = libc::getpid().to_ne_bytes();
                    if libc::write(ready_w, pid.as_ptr().cast(), pid.len()) != pid.len() as isize {
                        return fail(b"handing the pid to newuidmap/newgidmap");
                    }
                    let mut ok = [1u8];
                    if libc::read(go_r, ok.as_mut_ptr().cast(), 1) != 1 || ok[0] != 0 {
                        return fail(b"mapping the ids with newuidmap/newgidmap");
                    }
                }
                for (path, content, step) in &maps {
                    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if fd < 0 {
                        return fail(step);
                    }
                    let n = libc::write(fd, content.as_ptr().cast(), content.len());
                    libc::close(fd);
                    if n != content.len() as isize {
                        return fail(step);
                    }
                }
                // Our mount must never propagate back to the host (and host mounts may
                // still appear in here).
                if libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_SLAVE, std::ptr::null())
                    != 0
                {
                    return fail(b"making / a slave mount");
                }
                // Clone the shared folders while they're still the host's.
                for (path, fd) in shared.iter().zip(shared_fds.iter_mut()) {
                    let r = libc::syscall(
                        libc::SYS_open_tree,
                        libc::AT_FDCWD,
                        path.as_ptr(),
                        (OPEN_TREE_CLONE | libc::O_CLOEXEC as u32 | AT_RECURSIVE) as libc::c_uint,
                    );
                    if r < 0 {
                        return fail(b"open_tree(shared folder)");
                    }
                    *fd = r as libc::c_int;
                }
                for (target, data) in &mounts {
                    if libc::mount(c"overlay".as_ptr(), target.as_ptr(), c"overlay".as_ptr(), 0, data.as_ptr().cast()) != 0 {
                        let e = io::Error::last_os_error();
                        let step = b"mount(overlay) on ";
                        libc::write(err_fd, step.as_ptr().cast(), step.len());
                        libc::write(err_fd, target.as_ptr().cast(), target.as_bytes().len());
                        return Err(e);
                    }
                }
                // Put them back on top of the overlays.
                for (path, fd) in shared.iter().zip(shared_fds.iter()) {
                    if libc::syscall(libc::SYS_move_mount, *fd, c"".as_ptr(), libc::AT_FDCWD, path.as_ptr(), MOVE_MOUNT_F_EMPTY_PATH) != 0 {
                        return fail(b"move_mount(shared folder)");
                    }
                    libc::close(*fd);
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn();
        drop(err_w); // so the read below ends once the child has exec'd or exited
        let mapped = mapper.map(Mapper::finish);
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let mut step = String::new();
                let _ = err_r.read_to_string(&mut step);
                if step.is_empty() {
                    return Err(e).context("starting the overlay namespace holder (cat)");
                }
                if let Some(Err(why)) = mapped {
                    return Err(e).context(format!("the launch's namespace: {step} failed: {why}"));
                }
                return Err(e).context(format!("overlay namespace: {step} failed (options: {options})"));
            }
        };
        let pid = child.id();
        let open = |kind: &str| File::open(format!("/proc/{pid}/ns/{kind}")).with_context(|| format!("opening {kind} namespace"));
        let ns = (open("user"), open("mnt"));
        drop(child.stdin.take());
        let _ = child.wait();
        Ok(Namespace { user: ns.0?, mnt: ns.1?, pad_gid })
    }

    /// The stream pad group the namespace has, if any.
    pub fn pad_gid(&self) -> Option<u32> {
        self.pad_gid
    }

    /// Make `cmd` run inside the namespace, starting in `cwd` as seen there (joining a
    /// mount namespace resets the working directory, so `Command::current_dir` would be
    /// lost; pass it here instead).
    pub fn enter(&self, cmd: &mut Command, cwd: Option<&Path>) -> Result<()> {
        let user: OwnedFd = self.user.try_clone()?.into();
        let mnt: OwnedFd = self.mnt.try_clone()?.into();
        let cwd = cwd.map(cstr).transpose()?;
        // SAFETY: getgid can't fail.
        let groups: Option<[libc::gid_t; 2]> = self.pad_gid.map(|p| [unsafe { libc::getgid() }, p]);
        // SAFETY: raw syscalls on descriptors and a string owned by the closure.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) != 0
                    || libc::setns(mnt.as_raw_fd(), libc::CLONE_NEWNS) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                // Joining gives the namespace's capabilities until exec: enough to take the
                // pad group (the other supplementary groups aren't mapped in here).
                if let Some(g) = &groups
                    && libc::setgroups(g.len(), g.as_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if let Some(dir) = &cwd
                    && libc::chdir(dir.as_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// A second handle to the same namespace.
    pub fn try_clone(&self) -> Result<Namespace> {
        Ok(Namespace { user: self.user.try_clone()?, mnt: self.mnt.try_clone()?, pad_gid: self.pad_gid })
    }
}

/// Maps a holder's ids from outside: it waits for the holder's pid on `ready`, runs
/// `newuidmap pid U U 1` and `newgidmap pid G G 1 P P 1`, and answers on `go` (0: mapped).
struct Mapper {
    ready_w: io::PipeWriter,
    go_r: io::PipeReader,
    /// The write end the thread answers on; the holder closes its copy.
    go_w: libc::c_int,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

impl Mapper {
    fn start(uid: u32, gid: u32, pad: u32) -> Result<Mapper> {
        let (mut ready_r, ready_w) = io::pipe()?;
        let (go_r, go_w) = io::pipe()?;
        let go_fd = go_w.as_raw_fd();
        let thread = std::thread::Builder::new().name("ns-mapper".into()).spawn(move || {
            let mut go_w = go_w;
            let mut pid = [0u8; 4];
            // EOF: the holder ended before it got here (its own error says why).
            if ready_r.read_exact(&mut pid).is_err() {
                return Err("the holder ended first".to_owned());
            }
            let pid = i32::from_ne_bytes(pid).to_string();
            let run = |tool: &str, args: Vec<String>| -> Result<(), String> {
                let out = Command::new(tool).arg(&pid).args(&args).stdin(Stdio::null()).output().map_err(|e| format!("{tool}: {e}"))?;
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!("{tool} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
                }
            };
            let r = run("newuidmap", [uid, uid, 1].map(|v| v.to_string()).to_vec()).and_then(|()| run("newgidmap", [gid, gid, 1, pad, pad, 1].map(|v| v.to_string()).to_vec()));
            let _ = io::Write::write_all(&mut go_w, &[u8::from(r.is_err())]);
            r
        })?;
        Ok(Mapper { ready_w, go_r, go_w: go_fd, thread })
    }

    /// After the holder was spawned (or failed to be): what the mapping came to.
    fn finish(self) -> Result<(), String> {
        drop(self.ready_w);
        drop(self.go_r);
        self.thread.join().unwrap_or_else(|_| Err("the mapper thread panicked".into()))
    }
}

/// Whether this process may create a user + mount namespace and mount in it (false under
/// sandboxes that forbid it, or with unprivileged user namespaces disabled).
pub fn supported() -> bool {
    let mut cmd = Command::new("true");
    // SAFETY: raw syscalls only.
    unsafe {
        cmd.pre_exec(|| {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gid from this user's subordinate range (any works as a stand-in for the stream pad
    /// group), when newgidmap can map one.
    fn a_subgid() -> Option<u32> {
        let user = std::env::var("USER").ok()?;
        let line = std::fs::read_to_string("/etc/subgid").ok()?.lines().find(|l| l.split(':').next() == Some(user.as_str()))?.to_owned();
        let start: u32 = line.split(':').nth(1)?.parse().ok()?;
        (Path::new("/usr/bin/newgidmap").exists() && Path::new("/usr/bin/newuidmap").exists() && supported()).then_some(start + 7)
    }

    fn groups_inside(ns: &Namespace) -> (String, String) {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "id -G; cat /proc/self/gid_map | tr -s ' ' | tr '\\n' ';'"]);
        ns.enter(&mut cmd, None).unwrap();
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        (lines.next().unwrap_or_default().to_owned(), lines.next().unwrap_or_default().to_owned())
    }

    /// A streamed launch's namespace: the extra group mapped (newgidmap) and given to every
    /// process that enters, with mods mounted or with nothing mounted.
    #[test]
    fn the_launch_namespace_gives_the_pad_group() {
        let Some(pad) = a_subgid() else {
            eprintln!("skip: no subgid range, newgidmap or user namespaces");
            return;
        };
        // SAFETY: getgid can't fail.
        let gid = unsafe { libc::getgid() };
        let ns = Namespace::create_with(&[], Some(pad)).unwrap();
        assert_eq!(ns.pad_gid(), Some(pad));
        let (groups, map) = groups_inside(&ns);
        assert_eq!(groups, format!("{gid} {pad}"), "gid_map: {map}");
        assert!(map.contains(&format!("{pad} {pad} 1")), "{map}");

        let t = tempfile::tempdir().unwrap();
        let (target, top) = (t.path().join("game"), t.path().join("mod"));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(top.join("from-the-mod"), "1").unwrap();
        let spec = OverlaySpec { target: target.clone(), lower: vec![top], upper: t.path().join("up"), work: t.path().join("work"), shared: vec![] };
        let ns = Namespace::create_with(std::slice::from_ref(&spec), Some(pad)).unwrap();
        assert_eq!(groups_inside(&ns).0, format!("{gid} {pad}"));
        let mut cmd = Command::new("cat");
        cmd.arg(target.join("from-the-mod"));
        ns.enter(&mut cmd, None).unwrap();
        assert_eq!(cmd.output().unwrap().stdout, b"1", "the mod is mounted too");
        assert!(!target.join("from-the-mod").exists(), "only inside");
    }

    /// A gid outside the person's range can't be mapped: creating fails (and says why), so
    /// the launch can go on without it rather than hand out a group nobody has.
    #[test]
    fn an_unmappable_group_fails_clearly() {
        if a_subgid().is_none() {
            return;
        }
        let e = Namespace::create_with(&[], Some(1)).unwrap_err();
        assert!(format!("{e:#}").contains("newgidmap"), "{e:#}");
    }

    #[test]
    fn options_escape_separators() {
        let spec = OverlaySpec {
            target: "/g/My Game".into(),
            lower: vec!["/m/top".into(), "/m/a:b,c".into()],
            upper: "/m/up".into(),
            work: "/m/work".into(),
            shared: vec![],
        };
        assert_eq!(
            spec.options().unwrap(),
            r"lowerdir=/m/top:/m/a\:b\,c:/g/My Game,upperdir=/m/up,workdir=/m/work,userxattr"
        );
    }
}
