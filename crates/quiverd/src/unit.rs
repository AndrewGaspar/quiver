//! quiverd under systemd (design/SERVICE.md): telling the unit it is ready, moving into the
//! unit from outside it, and starting at boot.
//!
//! - **Ready**: under the unit (`Type=notify`) quiverd sends `READY=1` once it listens. The
//!   notify socket is taken out of the environment (games must not inherit it) and handed
//!   to the next build across a restart's exec, which keeps the pid the unit knows.
//! - **Moving in**: a daemon started outside the unit (from a terminal, before the unit was
//!   installed) can't change its cgroup, and exec keeps it. Once nothing needs it, as for a
//!   restart onto a new build (lifecycle.rs), it binds `handover.sock` beside its socket,
//!   asks systemd to start the unit (`--no-block`) and waits. The unit's quiverd finds the
//!   offer first thing, takes the listening sockets (the Unix socket, the broker's, remote
//!   play's) with `SCM_RIGHTS` and says so; the old one exits, and the new one waits for it
//!   to be gone before serving. Connections made meanwhile wait in the backlog, as across an
//!   exec. If the unit doesn't come within [`GIVE_WITHIN`], the old daemon carries on.
//! - **Boot**: `Request::RemoteBoot` enables or disables the unit, and turns lingering on
//!   when the person may (they asked: `quiver remote enable --at-boot` or the Settings
//!   toggle); when they may not, the reply has the command to run.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_api::service::{self, BootStatus, Systemd, UnitState};
use serde::{Deserialize, Serialize};

/// How long a daemon moving into the unit waits for it to come and take its sockets.
pub(crate) const GIVE_WITHIN: Duration = Duration::from_secs(20);
/// How long the unit's daemon waits for the old one to exit after taking its sockets.
pub(crate) const OLD_EXITS_WITHIN: Duration = Duration::from_secs(15);
const ACK_WITHIN: Duration = Duration::from_secs(5);
const MAX_FDS: usize = 8;

// ---------------------------------------------------------------------------------------
// Ready

static NOTIFY: OnceLock<Option<OsString>> = OnceLock::new();

/// This process runs as its unit (`quiverd.service`, or a live test's): its cgroup says.
pub(crate) fn in_unit() -> bool {
    service::current_unit().is_some_and(|u| u == service::unit_name())
}

/// Take `NOTIFY_SOCKET` out of the environment, keeping it only when this is the unit's
/// process (a terminal's can carry one from elsewhere). Call first thing in `main`.
pub(crate) fn take_notify_socket(in_unit: bool) {
    let v = std::env::var_os("NOTIFY_SOCKET");
    // SAFETY: first thing in `main`, before anything reads the environment; games must not
    // inherit it.
    unsafe { std::env::remove_var("NOTIFY_SOCKET") };
    let _ = NOTIFY.set(v.filter(|_| in_unit));
}

/// The notify socket to hand the next build across an exec.
pub(crate) fn notify_socket() -> Option<&'static OsStr> {
    NOTIFY.get().and_then(|v| v.as_deref())
}

/// Send `state` (`READY=1`, `STATUS=…`) to the unit's notify socket, if there is one.
pub(crate) fn notify(state: &str) {
    if let Some(addr) = notify_socket()
        && let Err(e) = send_notify(addr, state)
    {
        tracing::warn!("sd_notify {state:?}: {e}");
    }
}

fn send_notify(addr: &OsStr, state: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let b = addr.as_bytes();
    let addr = match b.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(Path::new(addr))?,
    };
    UnixDatagram::unbound()?.send_to_addr(state.as_bytes(), &addr).map(drop)
}

// ---------------------------------------------------------------------------------------
// Moving into the unit

/// Where a daemon moving into the unit offers its sockets: beside its own socket.
pub(crate) fn handover_path(socket: &Path) -> PathBuf {
    socket.with_file_name("handover.sock")
}

/// Whether, and into which unit, a daemon outside it should move once idle. `default`:
/// whether its environment is the person's default daemon's
/// ([`service::default_instance`]); `test_unit`: a live test's scratch unit, which stands
/// in for that; `state`: the unit's state as systemd has it. `Err`: why not.
pub(crate) fn relocation_target(
    in_unit: bool,
    auto: bool,
    default: Result<(), String>,
    test_unit: Option<String>,
    state: impl FnOnce(&str) -> Option<UnitState>,
) -> Result<String, String> {
    if in_unit {
        return Err("it runs as its unit".into());
    }
    if !auto {
        return Err("restarting onto new builds is off (QUIVER_AUTO_RESTART=0)".into());
    }
    let unit = match test_unit {
        Some(u) => u,
        None => default.map(|()| service::UNIT.to_owned()).map_err(|why| format!("not the person's default daemon ({why})"))?,
    };
    match state(&unit) {
        None => Err("the user manager can't be reached".into()),
        Some(s) if s.loaded() => Ok(unit),
        Some(s) => Err(format!("{unit} isn't installed ({})", s.load)),
    }
}

/// The offer's text, before its descriptors.
#[derive(Debug, Serialize, Deserialize)]
struct Offer {
    v: u32,
    /// The descriptors' names, in order (`unix`, `broker`, `remote`).
    fds: Vec<String>,
    /// The build handing over.
    from: String,
}

/// Removes the offer's socket file however `give` ends.
struct Unlink<'a>(&'a Path);

impl Drop for Unlink<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// A connected peer's pid and uid.
fn peer(s: &UnixStream) -> Result<(u32, u32)> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred of the right size.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut c).cast(), &mut len) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok((c.pid as u32, c.uid))
}

fn uid() -> u32 {
    // SAFETY: no failure modes.
    unsafe { libc::getuid() }
}

/// Offer `fds` at `path` and have the unit come for them: `start` asks systemd to start it
/// (without waiting), `peer_ok` says whether a connecting process (by pid) is the unit's.
/// Returns the pid that took them; this process must exit at once then. On any error the
/// sockets are still this process's alone, and it carries on.
pub(crate) fn give(path: &Path, fds: &[(&str, RawFd)], from: &str, start: impl FnOnce() -> Result<()>, peer_ok: impl Fn(u32) -> bool, within: Duration) -> Result<u32> {
    if fds.len() > MAX_FDS {
        bail!("too many sockets to hand over");
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    let _unlink = Unlink(path);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    listener.set_nonblocking(true)?;
    start()?;
    let deadline = Instant::now() + within;
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => {
                let (pid, from_uid) = peer(&s)?;
                if from_uid != uid() || !peer_ok(pid) {
                    tracing::warn!(pid, "handover: a process that isn't the unit's connected; ignored");
                    continue;
                }
                break s;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("the unit didn't come for the sockets within {}s", within.as_secs());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("accept on the handover socket"),
        }
    };
    let (pid, _) = peer(&stream)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(ACK_WITHIN))?;
    let offer = Offer { v: 1, fds: fds.iter().map(|(n, _)| (*n).to_owned()).collect(), from: from.to_owned() };
    let mut text = serde_json::to_vec(&offer)?;
    text.push(b'\n');
    let raw: Vec<RawFd> = fds.iter().map(|(_, fd)| *fd).collect();
    send_fds(&stream, &text, &raw).context("sending the sockets")?;
    let mut ack = String::new();
    BufReader::new(&stream).read_line(&mut ack).context("waiting for the unit to take the sockets")?;
    if ack.trim() != "ok" {
        bail!("the unit didn't take the sockets ({:?})", ack.trim());
    }
    Ok(pid)
}

/// Sockets taken from a daemon that moved into the unit.
pub(crate) struct Taken {
    pub fds: Vec<(String, RawFd)>,
    pub from: String,
    pub pid: u32,
}

/// Take the sockets offered at `path`, if a daemon outside the unit offers them, then wait
/// for it to exit. `None`: none offered (or it went wrong; the caller starts as usual and
/// finds the old daemon still serving, if it is).
pub(crate) fn take(path: &Path, old_exits_within: Duration) -> Option<Taken> {
    if !path.exists() {
        return None;
    }
    let s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("handover socket {}: {e}", path.display());
            return None;
        }
    };
    match take_from(&s, old_exits_within) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("taking over from the daemon outside the unit: {e:#}");
            None
        }
    }
}

fn take_from(s: &UnixStream, old_exits_within: Duration) -> Result<Taken> {
    let (pid, from_uid) = peer(s)?;
    if from_uid != uid() {
        bail!("the offer came from uid {from_uid}");
    }
    s.set_read_timeout(Some(ACK_WITHIN))?;
    let (mut data, fds) = recv_fds(s).context("receiving the sockets")?;
    // Close what was received unless it is kept.
    struct Close(Vec<RawFd>);
    impl Drop for Close {
        fn drop(&mut self) {
            for fd in &self.0 {
                // SAFETY: descriptors received for us, not yet owned by anything else.
                unsafe { libc::close(*fd) };
            }
        }
    }
    let mut guard = Close(fds);
    while !data.contains(&b'\n') {
        let mut b = [0u8; 1024];
        let n = (&*s).read(&mut b)?;
        if n == 0 {
            bail!("the offer ended early");
        }
        data.extend_from_slice(&b[..n]);
    }
    let line = &data[..data.iter().position(|b| *b == b'\n').unwrap_or(data.len())];
    let offer: Offer = serde_json::from_slice(line).context("reading the offer")?;
    if offer.v != 1 || offer.fds.len() != guard.0.len() {
        bail!("an offer of {} names for {} sockets (v{})", offer.fds.len(), guard.0.len(), offer.v);
    }
    (&*s).write_all(b"ok\n")?;
    // From here the old daemon exits; serve only once it is gone.
    if !wait_exit(pid, old_exits_within) {
        bail!("the daemon that offered its sockets (pid {pid}) is still running after {}s", old_exits_within.as_secs());
    }
    let fds = std::mem::take(&mut guard.0);
    Ok(Taken { fds: offer.fds.into_iter().zip(fds).collect(), from: offer.from, pid })
}

/// Wait up to `within` for `pid` (not our child) to exit. True once it is gone.
fn wait_exit(pid: u32, within: Duration) -> bool {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as RawFd;
    if fd < 0 {
        // ESRCH: already gone. Anything else: fall back to looking.
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        let deadline = Instant::now() + within;
        while Path::new(&format!("/proc/{pid}")).exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        return true;
    }
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: poll on one pollfd we own; then close it.
    let r = unsafe { libc::poll(&mut p, 1, within.as_millis().min(i32::MAX as u128) as i32) };
    unsafe { libc::close(fd) };
    r > 0
}

fn send_fds(s: &UnixStream, data: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    let bytes = std::mem::size_of_val(fds);
    // SAFETY: CMSG_* compute sizes; the buffer is u64-aligned and as large as CMSG_SPACE
    // says; sendmsg reads `data` and the control buffer, both live for the call.
    unsafe {
        let space = libc::CMSG_SPACE(bytes as u32) as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(c), bytes);
        }
        let n = libc::sendmsg(s.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (n as usize) < data.len() {
            // The descriptors went with the first byte; the rest is plain data.
            (&*s).write_all(&data[n as usize..])?;
        }
    }
    Ok(())
}

fn recv_fds(s: &UnixStream) -> std::io::Result<(Vec<u8>, Vec<RawFd>)> {
    let mut data = vec![0u8; 4096];
    // SAFETY: as in send_fds; recvmsg fills `data` and the control buffer, then the
    // CMSG_* walk stays within what the kernel reported (msg_controllen).
    unsafe {
        let space = libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let n = libc::recvmsg(s.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = len / std::mem::size_of::<RawFd>();
                let p = libc::CMSG_DATA(c).cast::<RawFd>();
                fds.extend((0..count).map(|i| std::ptr::read_unaligned(p.add(i))));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            for fd in &fds {
                libc::close(*fd);
            }
            return Err(std::io::Error::other("more descriptors than expected"));
        }
        data.truncate(n as usize);
        Ok((data, fds))
    }
}

// ---------------------------------------------------------------------------------------
// Boot

/// Whether the daemon starts at boot here; `None` without systemd.
pub(crate) fn boot_status(sd: &Systemd, in_unit: bool) -> Option<BootStatus> {
    let unit = service::unit_name();
    let state = sd.unit_state(&unit)?;
    let user = service::passwd().map(|(u, _)| u)?;
    Some(boot_from(&state, service::linger(&user), in_unit))
}

fn boot_from(state: &UnitState, linger: bool, in_unit: bool) -> BootStatus {
    BootStatus {
        installed: state.loaded() && !matches!(state.file.as_str(), "" | "transient" | "generated"),
        enabled: state.enabled(),
        linger,
        in_unit,
        ..Default::default()
    }
}

/// Enable (`enabled`) or disable the unit; enabling also turns lingering on when it is off
/// and the person may. `linger` reads lingering's state.
pub(crate) fn set_boot(sd: &Systemd, unit: &str, user: &str, linger: impl Fn() -> bool, enabled: bool, in_unit: bool) -> Result<BootStatus> {
    let state = sd.unit_state(unit).context("the systemd user manager can't be reached")?;
    let before = boot_from(&state, linger(), in_unit);
    if !before.installed {
        bail!("{unit} isn't installed (tools/install-user.sh or the quiver package installs it)");
    }
    sd.systemctl(if enabled { "enable" } else { "disable" }, &["--quiet"], unit)?;
    let mut note = None;
    let mut linger_command = None;
    if enabled && !linger() {
        if let Err(e) = sd.enable_linger(user) {
            tracing::info!("lingering not turned on: {e:#}");
            linger_command = Some(service::linger_command(user));
        }
    } else if !enabled && linger() {
        note = Some("lingering stays on (other services may need it); `loginctl disable-linger` turns it off".into());
    }
    if enabled && !in_unit {
        note = Some(format!("this quiverd runs outside {unit}; it moves in once nothing needs it"));
    }
    let after = sd.unit_state(unit).map(|s| boot_from(&s, linger(), in_unit)).unwrap_or(before);
    Ok(BootStatus { linger_command: linger_command.filter(|_| !after.linger), note, ..after })
}

impl crate::Daemon {
    /// `Request::RemoteBoot`.
    pub(crate) async fn handle_boot(self: &Arc<Self>, source: &str, enabled: bool) -> Result<serde_json::Value> {
        let in_unit = self.lifecycle.in_unit;
        let unit = service::unit_name();
        let b = {
            let unit = unit.clone();
            tokio::task::spawn_blocking(move || {
                let user = service::passwd().map(|(u, _)| u).context("no password database entry for this user")?;
                let u = user.clone();
                set_boot(&Systemd::from_env(), &unit, &user, move || service::linger(&u), enabled, in_unit)
            })
            .await??
        };
        let what = if enabled { format!("quiverd starts at boot: {}", b.describe()) } else { "quiverd no longer starts at boot".to_owned() };
        self.log_activity(source, what, None);
        if enabled && !in_unit {
            self.lifecycle.want_relocation(unit);
        }
        self.emit(quiver_api::Event::RemoteChanged);
        Ok(serde_json::json!(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> Option<UnitState> {
        Some(UnitState { load: "loaded".into(), active: "inactive".into(), file: "disabled".into() })
    }

    #[test]
    fn whether_to_move_into_the_unit() {
        let ok = || Ok(());
        assert_eq!(relocation_target(false, true, ok(), None, |u| { assert_eq!(u, "quiverd.service"); loaded() }), Ok("quiverd.service".into()));
        // Already there, or restarts off, or someone's scratch daemon: never.
        assert!(relocation_target(true, true, ok(), None, |_| loaded()).is_err());
        assert!(relocation_target(false, false, ok(), None, |_| loaded()).is_err());
        let scratch = relocation_target(false, true, Err("HOME is /tmp/x".into()), None, |_| panic!("a scratch daemon never asks about the unit"));
        assert!(scratch.unwrap_err().contains("HOME is /tmp/x"));
        // The unit missing, or no manager.
        let missing = Some(UnitState { load: "not-found".into(), ..Default::default() });
        assert!(relocation_target(false, true, ok(), None, |_| missing.clone()).unwrap_err().contains("isn't installed"));
        assert!(relocation_target(false, true, ok(), None, |_| None).is_err());
        // A live test's scratch unit stands in for the person's.
        assert_eq!(relocation_target(false, true, Err("scratch".into()), Some("quiver-test-1.service".into()), |_| loaded()), Ok("quiver-test-1.service".into()));
    }

    /// A listening Unix socket and a listening TCP socket.
    fn listeners(dir: &Path) -> (UnixListener, std::net::TcpListener) {
        (UnixListener::bind(dir.join("q.sock")).unwrap(), std::net::TcpListener::bind("127.0.0.1:0").unwrap())
    }

    fn listening(fd: RawFd) -> bool {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN, (&raw mut v).cast(), &mut len) };
        r == 0 && v == 1
    }

    /// The whole handover in one process: the "unit" side is a thread, so the old daemon's
    /// pid (ours) never exits, and the taker gives up waiting for it, closing what it took.
    /// With a pid that exits (a child), it keeps them.
    #[test]
    fn handover_moves_the_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let (unix, tcp) = listeners(dir.path());
        let path = handover_path(&dir.path().join("q.sock"));
        assert_eq!(path, dir.path().join("handover.sock"));
        let fds = [("unix", unix.as_raw_fd()), ("broker", tcp.as_raw_fd())];
        let started = std::sync::atomic::AtomicBool::new(false);
        let taker = {
            let path = path.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                while !path.exists() {
                    assert!(t.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(5));
                }
                let s = UnixStream::connect(&path).unwrap();
                // The offerer is this very process, which won't exit: a short wait.
                take_from(&s, Duration::from_millis(200))
            })
        };
        let pid = give(&path, &fds, "0.1.0.test", || { started.store(true, std::sync::atomic::Ordering::SeqCst); Ok(()) }, |p| p == std::process::id(), Duration::from_secs(5)).unwrap();
        assert!(started.load(std::sync::atomic::Ordering::SeqCst), "the unit was asked to start");
        assert_eq!(pid, std::process::id());
        assert!(!path.exists(), "the offer's socket is removed");
        let err = taker.join().unwrap().err().unwrap();
        assert!(format!("{err:#}").contains("still running"), "{err:#}");
        // Ours are still ours and still listening.
        assert!(listening(unix.as_raw_fd()) && listening(tcp.as_raw_fd()));
    }

    #[test]
    fn handover_received_fds_listen() {
        // Receive side alone over a socketpair: names and descriptors arrive together.
        let dir = tempfile::tempdir().unwrap();
        let (unix, tcp) = listeners(dir.path());
        let (a, b) = UnixStream::pair().unwrap();
        let offer = Offer { v: 1, fds: vec!["unix".into(), "broker".into()], from: "x".into() };
        let mut text = serde_json::to_vec(&offer).unwrap();
        text.push(b'\n');
        send_fds(&a, &text, &[unix.as_raw_fd(), tcp.as_raw_fd()]).unwrap();
        let (data, fds) = recv_fds(&b).unwrap();
        assert_eq!(data, text);
        assert_eq!(fds.len(), 2);
        assert!(fds.iter().all(|fd| listening(*fd)));
        // A connection to the original arrives on the received copy.
        let got = unsafe { std::os::fd::FromRawFd::from_raw_fd(fds[0]) };
        let got: UnixListener = got;
        let _c = UnixStream::connect(dir.path().join("q.sock")).unwrap();
        got.set_nonblocking(true).unwrap();
        assert!(got.accept().is_ok());
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn a_stranger_cannot_take_them() {
        let dir = tempfile::tempdir().unwrap();
        let (unix, _tcp) = listeners(dir.path());
        let path = handover_path(&dir.path().join("q.sock"));
        let p = path.clone();
        let stranger = std::thread::spawn(move || {
            while !p.exists() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _s = UnixStream::connect(&p).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        // Nobody passes `peer_ok`: it times out and keeps its sockets.
        let err = give(&path, &[("unix", unix.as_raw_fd())], "x", || Ok(()), |_| false, Duration::from_millis(400)).unwrap_err();
        assert!(format!("{err:#}").contains("didn't come"), "{err:#}");
        assert!(!path.exists());
        stranger.join().unwrap();
        // A failed start leaves nothing behind either.
        assert!(give(&path, &[("unix", unix.as_raw_fd())], "x", || bail!("no manager"), |_| true, Duration::from_secs(1)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn take_waits_for_a_real_exit() {
        let mut child = std::process::Command::new("sleep").arg("0.3").spawn().unwrap();
        let t = Instant::now();
        assert!(wait_exit(child.id(), Duration::from_secs(5)));
        assert!(t.elapsed() >= Duration::from_millis(200));
        let _ = child.wait();
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(!wait_exit(child.id(), Duration::from_millis(100)));
        let _ = child.kill();
        let _ = child.wait();
    }

    fn fake(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Fake systemctl and loginctl on a PATH; lingering is a file the fake loginctl makes
    /// (or refuses to).
    fn fakes(file: &str, loginctl_ok: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let linger = dir.path().join("linger");
        let (l, g) = (log.display(), linger.display());
        let state = dir.path().join("file-state");
        std::fs::write(&state, file).unwrap();
        let s = state.display();
        fake(
            dir.path(),
            "systemctl",
            &format!(
                "echo \"systemctl $*\" >> {l}\ncase \"$2\" in\n  show) read -r st < {s}; printf 'LoadState=loaded\\nActiveState=active\\nUnitFileState=%s\\n' \"$st\" ;;\n  enable) echo enabled > {s} ;;\n  disable) echo disabled > {s} ;;\nesac"
            ),
        );
        let body = if loginctl_ok { format!(": > {g}") } else { "echo 'Access denied' >&2; exit 1".into() };
        fake(dir.path(), "loginctl", &format!("echo \"loginctl $*\" >> {l}\n{body}"));
        (dir, log, linger)
    }

    #[test]
    fn boot_on_and_off_with_fake_tools() {
        let (dir, log, linger) = fakes("disabled", true);
        let sd = Systemd::with_path(dir.path());
        let l = linger.clone();
        let b = set_boot(&sd, "quiverd.service", "p", move || l.exists(), true, true).unwrap();
        assert!(b.enabled && b.linger && b.at_boot() && b.linger_command.is_none(), "{b:?}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("systemctl --user enable --quiet -- quiverd.service"), "{calls}");
        assert!(calls.contains("loginctl --no-ask-password enable-linger -- p"), "{calls}");

        // Off: the unit only; lingering is left on.
        let l = linger.clone();
        let b = set_boot(&sd, "quiverd.service", "p", move || l.exists(), false, true).unwrap();
        assert!(!b.enabled && b.linger && b.note.as_deref().unwrap().contains("lingering stays on"), "{b:?}");

        // Where lingering needs root: the command to run, no sudo.
        let (dir, log, linger) = fakes("disabled", false);
        let sd = Systemd::with_path(dir.path());
        let b = set_boot(&sd, "quiverd.service", "p", move || linger.exists(), true, false).unwrap();
        assert!(b.enabled && !b.linger, "{b:?}");
        assert_eq!(b.linger_command.as_deref(), Some("sudo loginctl enable-linger p"));
        assert!(b.note.as_deref().unwrap().contains("moves in once nothing needs it"));
        assert!(!std::fs::read_to_string(&log).unwrap().contains("sudo"));

        // A transient quiverd.service isn't an installed unit.
        let (dir, _, linger) = fakes("transient", true);
        let err = set_boot(&Systemd::with_path(dir.path()), "quiverd.service", "p", move || linger.exists(), true, true).unwrap_err();
        assert!(err.to_string().contains("isn't installed"), "{err}");
    }
}
