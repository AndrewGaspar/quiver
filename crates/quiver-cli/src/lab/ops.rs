//! What the lab does to a container: podman calls, screenshots, input, kill-all, and the
//! bridge check before a launch. Blocking; the supervisor and the commands call these from
//! their own threads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use quiver_core::lab::{Inst, Manifest, podman, procs};

pub fn podman_cmd(args: &[String]) -> Command {
    let mut c = Command::new("podman");
    c.args(args);
    c
}

/// Run podman, failing with its stderr.
pub fn podman_ok(args: &[String]) -> Result<Output> {
    let out = podman_cmd(args).stdin(Stdio::null()).output().context("run podman (is it installed?)")?;
    if !out.status.success() {
        bail!("podman {}: {}", args.first().map(String::as_str).unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out)
}

/// The running container's full id, or `None` when it isn't running.
pub fn running_id(container: &str) -> Option<String> {
    let out = podman_cmd(&["container".into(), "inspect".into(), "-f".into(), "{{.Id}} {{.State.Running}}".into(), container.into()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (id, running) = text.trim().split_once(' ')?;
    (out.status.success() && running == "true").then(|| id.to_owned())
}

/// Running containers: full id → name.
pub fn containers() -> BTreeMap<String, String> {
    let Ok(out) = podman_cmd(&["ps".into(), "--no-trunc".into(), "--format".into(), "{{.ID}} {{.Names}}".into()]).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_owned(), b.trim().to_owned()))).collect()
}

/// `podman exec` in the instance with its display; output captured.
pub fn exec(inst: &Inst, env: &[(String, String)], cmd: &[String]) -> Result<Output> {
    podman_ok(&podman::exec_args(&inst.container(), false, None, env, cmd))
}

pub fn exec_str(inst: &Inst, cmd: &[&str]) -> Result<Output> {
    exec(inst, &[], &cmd.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

/// The display gamescope reported in the container.
pub fn display(inst: &Inst) -> Option<String> {
    let out = podman_cmd(&["exec".into(), inst.container(), "cat".into(), podman::DISPLAY_FILE.into()]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let d = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !d.is_empty()).then_some(d)
}

/// gamescope's focused window (0: none), from its root-window property.
pub fn focused_window(inst: &Inst) -> Option<u64> {
    let out = exec_str(inst, &["xprop", "-root", "GAMESCOPE_FOCUSED_WINDOW"]).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split('=').nth(1)?.trim().split(',').next()?.trim().parse().ok()
}

/// The screenshot script: ask gamescope for one (it composites what the game presents; the
/// X root window doesn't hold GPU-presented frames), wait until the file stops growing.
const SHOT: &str = r#"rm -f /tmp/gamescope.png
xprop -root -f GAMESCOPECTRL_REQUEST_SCREENSHOT 32c -set GAMESCOPECTRL_REQUEST_SCREENSHOT 1 || exit 4
for i in $(seq 200); do
  if [ -s /tmp/gamescope.png ]; then s1=$(stat -c %s /tmp/gamescope.png); sleep 0.05
    [ "$s1" = "$(stat -c %s /tmp/gamescope.png)" ] && exec cat /tmp/gamescope.png; fi
  sleep 0.05
done
echo "gamescope wrote no screenshot" >&2; exit 5"#;

/// Take a full-resolution screenshot into `out` (PNG).
pub fn shot(inst: &Inst, out: &Path) -> Result<()> {
    let o = exec(inst, &[], &["bash".into(), "-c".into(), SHOT.into()]).context("screenshot")?;
    if o.stdout.len() < 8 || &o.stdout[1..4] != b"PNG" {
        bail!("screenshot failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(out, &o.stdout)?;
    Ok(())
}

/// A screenshot as grey levels (for metrics).
pub fn shot_gray(inst: &Inst, tmp: &Path) -> Result<image::GrayImage> {
    shot(inst, tmp)?;
    quiver_core::lab::image::load_gray(tmp)
}

/// xdotool in the instance.
pub fn xdotool(inst: &Inst, args: &[String]) -> Result<()> {
    let mut cmd = vec!["xdotool".to_owned()];
    cmd.extend(args.iter().cloned());
    exec(inst, &[], &cmd).map(|_| ())
}

/// Every Wine process of the instance, gone: `wineserver -k` first, then SIGKILL for what
/// is left. Processes are picked by the container's cgroup (never by name alone).
pub fn kill_all(inst: &Inst, m: &Manifest, container_id: &str) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let wineserver = m.runtime.proton_path.join("files/bin/wineserver").display().to_string();
    let _ = podman_cmd(&["exec".into(), inst.container(), wineserver, "-k".into()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let all = procs::scan();
        let left: Vec<&procs::Proc> = procs::instance_wine(&all, container_id);
        if left.is_empty() {
            break;
        }
        if Instant::now() > deadline {
            for p in &left {
                // Our own uid's processes, in the instance's cgroup.
                unsafe { libc::kill(p.pid as i32, libc::SIGKILL) };
                notes.push(format!("killed {} (pid {})", p.program(), p.pid));
            }
            std::thread::sleep(Duration::from_millis(300));
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // The run's quiver-sight (Sight instances) goes with it.
    notes.extend(super::sight::kill(container_id));
    // Epic ownership tokens are deleted once a run is over; a context left unread too.
    for dir in std::fs::read_dir(inst.runs()).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let n = f.file_name().to_string_lossy().into_owned();
            if n.ends_with(".ovt") || n == "context.json" {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    Ok(notes)
}

/// The bridge for an instance: the configured one, and only if it keeps lab sessions out
/// of the person's records (`quiver_ipc::LAB_PROTOCOL_MARKER`).
pub fn lab_bridge() -> Result<PathBuf> {
    let path = quiver_core::launcher::Config::load()?.bridge_exe()?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let marker = quiver_ipc::LAB_PROTOCOL_MARKER.as_bytes();
    if !bytes.windows(marker.len()).any(|w| w == marker) {
        bail!(
            "{} is a bridge from before lab sessions (it would report play time and achievements from the instance); install a current Quiver (tools/install-user.sh) first",
            path.display()
        );
    }
    Ok(path)
}
