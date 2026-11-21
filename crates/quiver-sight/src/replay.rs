//! A rolling recorder owned by one game session. Prefer its display connector,
//! falling back to the Wayland portal. An IPC save returns the finished clip.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

pub const DEFAULT_SECONDS: u32 = 60;
pub const LENGTHS: [u32; 3] = [30, DEFAULT_SECONDS, 120];

#[derive(Clone, Copy, Debug)]
pub enum State {
    Off,
    Starting,
    Stopping,
    Active(u32),
    External { pid: u32, seconds: u32 },
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Start(u32),
    Save,
    SaveExternal(u32),
    Stop,
}

impl State {
    pub fn primary(self) -> Option<(&'static str, Action)> {
        Some(match self {
            State::Off => ("Start replay buffer", Action::Start(DEFAULT_SECONDS)),
            State::Active(_) => ("Save replay", Action::Save),
            State::Starting => ("Cancel replay buffer", Action::Stop),
            State::External { pid, .. } => ("Save replay", Action::SaveExternal(pid)),
            State::Hidden | State::Stopping => return None,
        })
    }
}

pub struct Recorder {
    child: Option<Child>,
    private: TempDir,
    log: File,
    pub seconds: u32,
    pub ready: bool,
    stopping: Option<Instant>,
}

fn non_nvidia_connector(monitor: &str) -> bool {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    cards.flatten().any(|entry| {
        let name = entry.file_name();
        let Some((card, output)) = name.to_str().and_then(|n| n.split_once('-')) else {
            return false;
        };
        output == monitor
            && std::fs::read_to_string(entry.path().join("status")).is_ok_and(|s| s.trim() == "connected")
            && std::fs::read_to_string(Path::new("/sys/class/drm").join(card).join("device/vendor"))
                .is_ok_and(|vendor| matches!(vendor.trim(), "0x1002" | "0x8086"))
    })
}

impl Recorder {
    pub fn start(game: &str, monitor: Option<&str>, seconds: u32) -> Result<Self> {
        let videos = std::env::var_os("XDG_VIDEOS_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos")))
            .context("no videos directory")?;
        let target = quiver_api::sight::session::captures_dir(&videos, game);
        let source = monitor.unwrap_or_else(|| if std::env::var_os("WAYLAND_DISPLAY").is_some() { "portal" } else { "focused" });
        let mut command = Command::new("gpu-screen-recorder");
        // A hybrid desktop can force NVIDIA VA-API globally even when KMS
        // captures a connector on the integrated GPU. Let libva select its driver.
        if std::env::var("LIBVA_DRIVER_NAME").as_deref() == Ok("nvidia") && monitor.is_some_and(non_nvidia_connector) {
            command.env_remove("LIBVA_DRIVER_NAME").env_remove("LIBVA_DRIVERS_PATH");
        }
        Self::spawn(command, source, &target, seconds)
    }

    fn spawn(mut cmd: Command, source: &str, target: &Path, seconds: u32) -> Result<Self> {
        std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
        let private = tempfile::Builder::new().prefix("quiver-replay-").tempdir()?;
        let log = tempfile::tempfile()?;
        cmd.args(["-w", source, "-f", "60", "-a", "default_output", "-v", "no", "-c", "mp4", "-r"])
            .arg(seconds.to_string())
            .arg("-o")
            .arg(target)
            .arg("-ipc")
            .arg(private.path().join("recorder.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.try_clone()?);
        let parent = std::process::id();
        // SAFETY: only async-signal-safe syscalls between fork and exec. A
        // crashed Sight must also release the portal capture.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent as i32 {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("starting gpu-screen-recorder (is it installed?)")?;
        Ok(Self { child: Some(child), private, log, seconds, ready: false, stopping: None })
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
    pub fn socket(&self) -> PathBuf {
        self.private.path().join("recorder.sock")
    }

    pub fn stopping(&self) -> bool {
        self.stopping.is_some()
    }

    pub fn stop(&mut self) {
        if let Some(child) = self.child.as_ref()
            && self.stopping.is_none()
        {
            // An unreaped child retains its pid.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            self.stopping = Some(Instant::now());
        }
    }

    /// Check startup and detect a cancelled source picker or recorder failure.
    pub fn poll(&mut self) -> Result<bool> {
        let child = self.child.as_mut().context("recorder stopped")?;
        if self.stopping.is_some_and(|start| start.elapsed() >= Duration::from_secs(3)) {
            child.kill()?;
        }
        if let Some(status) = child.try_wait()? {
            self.child = None;
            if self.stopping.is_some() {
                return Ok(false);
            }
            let end = self.log.seek(SeekFrom::End(0))?;
            self.log.seek(SeekFrom::Start(end.saturating_sub(2048)))?;
            let mut tail = Vec::new();
            self.log.read_to_end(&mut tail)?;
            let tail = String::from_utf8_lossy(&tail);
            let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("capture ended");
            bail!("{last} ({status})");
        }
        self.ready = !self.stopping() && self.socket().exists();
        Ok(true)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // This child has not been reaped, so its pid cannot be reused.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            std::thread::spawn(move || {
                // Some versions wait inside the portal picker during startup.
                for _ in 0..30 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

/// Pin the target process before rechecking it. A stale tile cannot signal a
/// process which later inherited the recorder's pid.
pub fn save_external(pid: u32) -> Result<()> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("opening the external recorder");
    }
    // SAFETY: pidfd_open returned a new descriptor owned by this call.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).context("the external recorder has stopped")?;
    if crate::quick::replay_args(&cmd).is_none() {
        bail!("the external replay buffer has stopped");
    }
    let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), libc::SIGUSR1, std::ptr::null::<libc::siginfo_t>(), 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("requesting a replay");
    }
    Ok(())
}

pub fn save(socket: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(socket).context("connecting to the replay buffer")?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"{\"id\":1,\"name\":\"save-replay\"}\n")?;
    let mut reply = String::new();
    BufReader::new(stream.take(65536)).read_line(&mut reply)?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("reading the recorder's reply")?;
    let data = reply["data"].as_str().unwrap_or("");
    if reply["result"] != "ok" || data.is_empty() {
        bail!("replay could not be saved: {data}");
    }
    Ok(data.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A recorder with the real IPC framing and process lifecycle and a synthetic
    // output path. Never opens a display, capture device, or audio source.
    #[test]
    fn starts_saves_and_stops_its_own_recorder() {
        let output = tempfile::tempdir().unwrap();
        let mut mock = Command::new("python3");
        mock.args([
            "-c",
            r#"
import json, os, signal, socket, sys
args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
assert [args[k] for k in ('-w', '-f', '-a', '-c', '-r')] == ['DP-3','60','default_output','mp4','30']
signal.signal(signal.SIGINT, lambda *unused: sys.exit(0))
s = socket.socket(socket.AF_UNIX)
s.bind(args['-ipc'])
s.listen(1)
while True:
    conn, _ = s.accept()
    with conn, conn.makefile('rb') as request:
        assert json.loads(request.readline()) == {'id':1,'name':'save-replay'}
        path = os.path.join(args['-o'], 'clip.mp4')
        open(path, 'wb').close()
        conn.sendall((json.dumps({'id':1,'result':'ok','data':path})+'\n').encode())
"#,
        ]);
        let mut r = Recorder::spawn(mock, "DP-3", output.path(), 30).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !r.ready {
            assert!(Instant::now() < deadline);
            assert!(r.poll().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        let path = save(&r.socket()).unwrap();
        assert_eq!(Path::new(&path), output.path().join("clip.mp4"));
        assert!(Path::new(&path).exists());
        r.stop();
        while r.poll().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(r.pid().is_none());
    }

    #[test]
    fn startup_error_surfaces_the_recorders_reason() {
        let output = tempfile::tempdir().unwrap();
        let mut failed = Command::new("sh");
        failed.args(["-c", "echo 'no hardware encoder' >&2; exit 1"]);
        let mut r = Recorder::spawn(failed, "DP-3", output.path(), 60).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Err(e) = r.poll() {
                assert!(e.to_string().contains("no hardware encoder"));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
