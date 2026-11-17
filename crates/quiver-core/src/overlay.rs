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
        std::fs::create_dir_all(&self.upper)?;
        std::fs::create_dir_all(&self.work)?;
        Ok(())
    }
}

fn cstr(s: impl AsRef<OsStr>) -> Result<CString> {
    CString::new(s.as_ref().as_bytes()).context("path contains a NUL byte")
}

/// A user + mount namespace with an overlay mounted, held open by file descriptors.
#[derive(Debug)]
pub struct Namespace {
    user: File,
    mnt: File,
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
        let options = options.join(" | ");
        let (mut err_r, err_w) = io::pipe()?;
        let err_fd = err_w.as_raw_fd();

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
                for (path, content, step) in [
                    (c"/proc/self/setgroups", &b"deny"[..], &b"writing setgroups"[..]),
                    (c"/proc/self/uid_map", uid_map.as_bytes(), b"writing uid_map"),
                    (c"/proc/self/gid_map", gid_map.as_bytes(), b"writing gid_map"),
                ] {
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
                for (target, data) in &mounts {
                    if libc::mount(c"overlay".as_ptr(), target.as_ptr(), c"overlay".as_ptr(), 0, data.as_ptr().cast()) != 0 {
                        let e = io::Error::last_os_error();
                        let step = b"mount(overlay) on ";
                        libc::write(err_fd, step.as_ptr().cast(), step.len());
                        libc::write(err_fd, target.as_ptr().cast(), target.as_bytes().len());
                        return Err(e);
                    }
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn();
        drop(err_w); // so the read below ends once the child has exec'd or exited
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let mut step = String::new();
                let _ = err_r.read_to_string(&mut step);
                if step.is_empty() {
                    return Err(e).context("starting the overlay namespace holder (cat)");
                }
                return Err(e).context(format!("overlay namespace: {step} failed (options: {options})"));
            }
        };
        let pid = child.id();
        let open = |kind: &str| File::open(format!("/proc/{pid}/ns/{kind}")).with_context(|| format!("opening {kind} namespace"));
        let ns = (open("user"), open("mnt"));
        drop(child.stdin.take());
        let _ = child.wait();
        Ok(Namespace { user: ns.0?, mnt: ns.1? })
    }

    /// Make `cmd` run inside the namespace, starting in `cwd` as seen there (joining a
    /// mount namespace resets the working directory, so `Command::current_dir` would be
    /// lost; pass it here instead).
    pub fn enter(&self, cmd: &mut Command, cwd: Option<&Path>) -> Result<()> {
        let user: OwnedFd = self.user.try_clone()?.into();
        let mnt: OwnedFd = self.mnt.try_clone()?.into();
        let cwd = cwd.map(cstr).transpose()?;
        // SAFETY: raw syscalls on descriptors and a string owned by the closure.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) != 0
                    || libc::setns(mnt.as_raw_fd(), libc::CLONE_NEWNS) != 0
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
        Ok(Namespace { user: self.user.try_clone()?, mnt: self.mnt.try_clone()? })
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

    #[test]
    fn options_escape_separators() {
        let spec = OverlaySpec {
            target: "/g/My Game".into(),
            lower: vec!["/m/top".into(), "/m/a:b,c".into()],
            upper: "/m/up".into(),
            work: "/m/work".into(),
        };
        assert_eq!(
            spec.options().unwrap(),
            r"lowerdir=/m/top:/m/a\:b\,c:/g/My Game,upperdir=/m/up,workdir=/m/work,userxattr"
        );
    }
}
