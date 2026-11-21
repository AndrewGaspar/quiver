//! The virtual display: headless gamescope at the client's size and rate, running the
//! stream's program (design/LAB.md §1, §13).
//!
//! **Never :0–:9.** gamescope's Xwayland takes the first free X display, which on a desktop is
//! :1. Before it starts, the host holds every free display from :0 to :9 with a lock file
//! naming its own (live) pid, so Xwayland skips them; the locks go once gamescope has said
//! which display it took. The command runs through a small shell prologue that refuses a
//! single-digit display (as the lab's does) and writes the display it got to the stream's
//! folder; the host refuses it too.
//!
//! **Its own process group**, so stopping the stream stops everything in it, and a death
//! signal, so a host that crashes takes gamescope (and with it the program's X display) along.
//!
//! **The NVIDIA GPU.** On a hybrid laptop gamescope would pick whichever Vulkan device comes
//! first; it is pointed at the NVIDIA one (`--prefer-vk-device`), where NVENC is.
//!
//! gamescope announces its PipeWire screencast node on stderr ("stream available on node ID:
//! N"); capture connects to that node.

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// What to run, and how big.
#[derive(Debug, Clone)]
pub struct DisplaySpec {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    /// HDR10 output (forced on, as headless has no real output to ask).
    pub hdr: bool,
    /// The program, run inside gamescope.
    pub command: Vec<String>,
    /// Added to the program's environment.
    pub env: Vec<(String, String)>,
    /// The stream's folder: gamescope's log, the display file.
    pub dir: PathBuf,
    /// A command gamescope itself runs under (the pad namespace, [`crate::sandbox::wrap`]);
    /// it must exec gamescope in the same process, so the pid stays gamescope's.
    pub wrapper: Vec<String>,
}

/// A running headless gamescope.
pub struct Display {
    child: Child,
    /// The X display the program got (`:10`).
    pub x_display: String,
    /// gamescope's own Wayland socket (`gamescope-0`).
    pub wayland_display: String,
    /// The PipeWire node of its screencast stream.
    pub node_id: u32,
    pub spec: DisplaySpec,
}

/// The first NVIDIA GPU's PCI ids as gamescope's `--prefer-vk-device` wants them
/// (`10de:2d58`), from sysfs.
pub fn nvidia_vk_device() -> Option<String> {
    let mut found = Vec::new();
    for e in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.starts_with("card") || n.contains('-') {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(e.path().join("device").join(f)).ok().map(|s| s.trim().trim_start_matches("0x").to_owned());
        if let (Some(v), Some(d)) = (read("vendor"), read("device"))
            && v == "10de"
        {
            found.push(format!("{v}:{d}"));
        }
    }
    found.sort();
    found.into_iter().next()
}

/// The arguments gamescope runs with (before `--`).
pub fn gamescope_args(spec: &DisplaySpec, vk_device: Option<&str>) -> Vec<String> {
    let hz = (spec.refresh_mhz as f64 / 1000.0).round().max(1.0) as u32;
    let mut a: Vec<String> = ["--backend", "headless"].map(String::from).to_vec();
    for (k, v) in [("-W", spec.width), ("-H", spec.height), ("-w", spec.width), ("-h", spec.height), ("-r", hz)] {
        a.push(k.into());
        a.push(v.to_string());
    }
    a.push("--force-windows-fullscreen".into());
    if let Some(d) = vk_device {
        a.push("--prefer-vk-device".into());
        a.push(d.into());
    }
    if spec.hdr {
        for f in ["--hdr-enabled", "--hdr-debug-force-support", "--hdr-debug-force-output"] {
            a.push(f.into());
        }
    }
    a
}

/// The prologue the program runs through: refuse :0–:9, record the displays, exec.
pub const PROLOGUE: &str = r#"case "$DISPLAY" in :[0-9]|:[0-9].*|'') echo "quiver-stream: refusing display '$DISPLAY'" >&2; exit 97;; esac
printf '%s\n%s\n' "$DISPLAY" "${GAMESCOPE_WAYLAND_DISPLAY:-}" > "$QUIVER_STREAM_DIR/display.tmp" && mv "$QUIVER_STREAM_DIR/display.tmp" "$QUIVER_STREAM_DIR/display"
exec "$@""#;

/// Whether an X display name is one the stream may never use.
pub fn reserved_display(d: &str) -> bool {
    let n = d.trim_start_matches(':').split('.').next().unwrap_or("");
    n.is_empty() || n.parse::<u32>().map(|n| n < 10).unwrap_or(true)
}

/// Lock files for the free displays :1–:9, naming this process, removed on drop.
struct DisplayLocks(Vec<PathBuf>);

impl DisplayLocks {
    fn take() -> DisplayLocks {
        let pid = std::process::id();
        let mut made = Vec::new();
        for d in 0..10 {
            let p = PathBuf::from(format!("/tmp/.X{d}-lock"));
            let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p);
            if let Ok(mut f) = f {
                use std::io::Write;
                let _ = writeln!(f, "{pid:>10}");
                made.push(p);
            }
        }
        DisplayLocks(made)
    }
}

impl Drop for DisplayLocks {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Display {
    /// Start gamescope and wait (up to `timeout`) for the program's display and the
    /// screencast node.
    pub fn start(spec: DisplaySpec, timeout: Duration) -> Result<Display> {
        if spec.command.is_empty() {
            bail!("no command to run in the display");
        }
        std::fs::create_dir_all(&spec.dir).with_context(|| format!("create {}", spec.dir.display()))?;
        let display_file = spec.dir.join("display");
        let _ = std::fs::remove_file(&display_file);
        let log_path = spec.dir.join("gamescope.log");
        let vk = nvidia_vk_device();
        let mut cmd = match spec.wrapper.split_first() {
            Some((first, rest)) => {
                let mut c = Command::new(first);
                c.args(rest).arg("gamescope");
                c
            }
            None => Command::new("gamescope"),
        };
        cmd.args(gamescope_args(&spec, vk.as_deref()));
        cmd.arg("--").arg("sh").arg("-c").arg(PROLOGUE).arg("quiver-stream").args(&spec.command);
        cmd.env("QUIVER_STREAM_DIR", &spec.dir);
        // Headless: nothing of the desktop's. (gamescope would connect to the person's
        // compositor when it finds WAYLAND_DISPLAY, and fails without one in its runtime dir.)
        cmd.env_remove("WAYLAND_DISPLAY").env_remove("DISPLAY");
        // Nor an inherited QUIVER_SIGHT, which would load the Sight layer into gamescope itself;
        // a program that wants Sight gets it from the spec's own environment.
        cmd.env_remove("QUIVER_SIGHT");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // SAFETY: setsid and prctl are async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let locks = DisplayLocks::take();
        let mut child = cmd.spawn().context("start gamescope")?;
        let node = Arc::new(Mutex::new(None::<u32>));
        let log = Arc::new(Mutex::new(std::fs::File::create(&log_path).with_context(|| format!("create {}", log_path.display()))?));
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
            let node = node.clone();
            let log = log.clone();
            std::thread::Builder::new().name("gamescope-log".into()).spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if let Some(n) = line.split("stream available on node ID: ").nth(1).and_then(|s| s.trim().parse().ok()) {
                        *node.lock().unwrap() = Some(n);
                    }
                    use std::io::Write;
                    let _ = writeln!(log.lock().unwrap(), "{line}");
                }
            })?;
        }
        let start = Instant::now();
        let mut d = Display { child, x_display: String::new(), wayland_display: String::new(), node_id: 0, spec };
        loop {
            if let Some(status) = d.child.try_wait()? {
                bail!("gamescope exited ({status}) before the display was up; see {}", log_path.display());
            }
            if d.x_display.is_empty()
                && let Ok(s) = std::fs::read_to_string(&display_file)
            {
                let mut l = s.lines();
                d.x_display = l.next().unwrap_or("").to_owned();
                d.wayland_display = l.next().unwrap_or("").to_owned();
                if reserved_display(&d.x_display) {
                    d.stop();
                    bail!("gamescope's Xwayland took {}, which a stream never uses", d.x_display);
                }
            }
            if let Some(n) = *node.lock().unwrap() {
                d.node_id = n;
            }
            if !d.x_display.is_empty() && d.node_id != 0 {
                break;
            }
            if start.elapsed() > timeout {
                d.stop();
                bail!("gamescope didn't bring up its display and screencast within {timeout:?}; see {}", log_path.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(locks);
        tracing::info!("display {} (wayland {}), screencast node {}", d.x_display, d.wayland_display, d.node_id);
        Ok(d)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether gamescope still runs.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop gamescope and everything in its process group: SIGTERM, then SIGKILL after 3 s.
    pub fn stop(&mut self) {
        let pgid = self.child.id() as i32;
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if !matches!(self.child.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }

    pub fn log_path(&self) -> PathBuf {
        self.spec.dir.join("gamescope.log")
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The stream's folder under `root` (`$XDG_RUNTIME_DIR/quiver-stream/<id>`).
pub fn stream_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_digit_displays_are_reserved() {
        for d in [":0", ":1", ":9", ":9.0", "", ":", ":x"] {
            assert!(reserved_display(d), "{d}");
        }
        for d in [":10", ":10.0", ":47"] {
            assert!(!reserved_display(d), "{d}");
        }
    }

    #[test]
    fn gamescope_args_for_hdr_and_rate() {
        let spec = DisplaySpec { width: 2560, height: 1440, refresh_mhz: 119_880, hdr: true, command: vec!["x".into()], env: vec![], dir: "/tmp/x".into(), wrapper: vec![] };
        let a = gamescope_args(&spec, Some("10de:2d58")).join(" ");
        assert_eq!(a, "--backend headless -W 2560 -H 1440 -w 2560 -h 1440 -r 120 --force-windows-fullscreen --prefer-vk-device 10de:2d58 --hdr-enabled --hdr-debug-force-support --hdr-debug-force-output");
    }

    #[test]
    fn the_prologue_refuses_single_digit_displays() {
        let t = tempfile::tempdir().unwrap();
        let run = |display: &str| {
            Command::new("sh").arg("-c").arg(PROLOGUE).arg("quiver-stream").arg("true").env("DISPLAY", display).env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0").env("QUIVER_STREAM_DIR", t.path()).status().unwrap()
        };
        assert_eq!(run(":1").code(), Some(97));
        assert_eq!(run(":0.0").code(), Some(97));
        assert!(!t.path().join("display").exists());
        assert!(run(":12").success());
        assert_eq!(std::fs::read_to_string(t.path().join("display")).unwrap(), ":12\ngamescope-0\n");
    }
}
