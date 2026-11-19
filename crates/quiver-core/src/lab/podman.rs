//! podman command lines for an instance, as pure functions (the CLI runs them).
//!
//! The container (lessons from the Pharos harness, design/LAB.md "Why it is built this way"):
//! - **gamescope `--backend headless`** is the display: GPU-side presentation, so frame rates
//!   are representative (on Xvfb, NVIDIA's Vulkan present costs a ~70 ms CPU copy a frame).
//! - **Its own network namespace** (pasta), with the broker's port forwarded to the host's
//!   loopback (`-T`). With `--network host` the container shared the host's abstract X
//!   socket namespace and once took `:0`, capturing the person's desktop clients. Displays
//!   `:0`–`:9` are reserved inside the container too ([`boot_script`]).
//! - **`--init`**: gamescope doesn't reap orphaned Wine processes.
//! - **CDI** (`nvidia.com/gpu=all`) injects only the 64-bit driver; the 32-bit libraries are
//!   mounted from the host and lib32 Vulkan is in the image.

use std::path::{Path, PathBuf};

use super::manifest::Manifest;
use super::{Inst, LAB_MOUNT};

/// Every lab container is named `quiver-lab-<instance>`.
pub const CONTAINER_PREFIX: &str = "quiver-lab-";

pub fn container_name(instance: &str) -> String {
    format!("{CONTAINER_PREFIX}{instance}")
}

/// Where the container's X display name is written once gamescope is up.
pub const DISPLAY_FILE: &str = "/tmp/display";

/// What the container runs: reserve `:0`–`:9` (lock files naming a live process, this
/// shell, which becomes gamescope), then gamescope headless at the manifest's size, its
/// child writing the display it got and sleeping. Nothing else runs until a launch.
pub fn boot_script() -> &'static str {
    r#"set -u
mkdir -p /tmp/xdg /tmp/steam "$HOME"; chmod 700 /tmp/xdg
echo "boot $(date -Is) ${QUIVER_LAB_W}x${QUIVER_LAB_H}@${QUIVER_LAB_HZ}" >> /lab/boot.log
# No sound device in the container: ALSA's default is a null sink, or a game that opens
# audio at start crashes (Alan Wake Remastered: a null read at Game_f_x64_EOS.exe+0x338489).
[ -e "$HOME/.asoundrc" ] || printf 'pcm.!default { type null }\n' > "$HOME/.asoundrc"
# Displays :0-:9 are never ours. Xwayland skips a display whose lock file names a live
# process; this shell's pid becomes gamescope's with the exec below.
for d in 0 1 2 3 4 5 6 7 8 9; do printf "%10d\n" $$ > /tmp/.X$d-lock; done
exec gamescope --backend headless -W "$QUIVER_LAB_W" -H "$QUIVER_LAB_H" -w "$QUIVER_LAB_W" -h "$QUIVER_LAB_H" -r "$QUIVER_LAB_HZ" -- \
  bash -c 'case "$DISPLAY" in :[0-9]|:[0-9].*) echo "refusing display $DISPLAY" >> /lab/boot.log; exit 1;; esac; echo "$DISPLAY" > /tmp/display; exec sleep infinity' \
  >> /lab/gamescope.log 2>&1"#
}

/// What the host gives the container besides the manifest.
#[derive(Debug, Clone, Default)]
pub struct Host {
    pub uid: u32,
    pub gid: u32,
    /// The host's name (Ubisoft's and Denuvo's view of the machine stays the same).
    pub hostname: String,
    /// The broker's TCP port on the host's loopback, forwarded into the container.
    pub broker_port: Option<u16>,
    /// The instance's EOS proxy on the host's loopback ([`super::eosproxy`]): the container's
    /// port 443 is forwarded to it, and Epic's names resolve to the container's 127.0.0.1.
    pub eos_proxy_port: Option<u16>,
    /// Host files mounted read-only at the same path: 32-bit NVIDIA libraries, NVIDIA's Wine
    /// DLLs (DLSS), `/etc/machine-id`.
    pub ro_mounts: Vec<PathBuf>,
    /// For a `sight = true` instance: the host's Sight pieces and the instance's shared-memory
    /// root ([`super::sight`]).
    pub sight: Option<(super::sight::HostSight, PathBuf)>,
}

/// 32-bit NVIDIA driver libraries and NVIDIA's Wine folder on this host.
pub fn host_ro_mounts() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["/usr/lib32"] {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut libs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ["libGLX_nvidia.so", "libnvidia-", "libEGL_nvidia.so", "libcuda.so"].iter().any(|pre| n.starts_with(pre))
            })
            .collect();
        libs.sort();
        out.extend(libs);
    }
    for p in ["/usr/lib/nvidia/wine", "/etc/machine-id"] {
        if Path::new(p).exists() {
            out.push(p.into());
        }
    }
    out
}

fn mount(host: &Path, at: &Path, opts: &str) -> String {
    let mut s = format!("{}:{}", host.display(), at.display());
    if !opts.is_empty() {
        s.push(':');
        s.push_str(opts);
    }
    s
}

/// `podman run …` (without `podman`) for an instance's container.
pub fn run_args(inst: &Inst, m: &Manifest, host: &Host) -> Vec<String> {
    let mut a: Vec<String> = ["run", "-d", "--rm", "--init"].map(String::from).to_vec();
    a.extend(["--name".into(), inst.container()]);
    a.extend(["--label".into(), format!("quiver.lab={}", inst.name)]);
    if !host.hostname.is_empty() {
        a.extend(["--hostname".into(), host.hostname.clone()]);
    }
    a.extend(["--device".into(), "nvidia.com/gpu=all".into()]);
    a.extend(["--userns=keep-id".into(), "--user".into(), format!("{}:{}", host.uid, host.gid)]);
    a.extend(["--shm-size=2g".into(), "--security-opt".into(), "label=disable".into()]);
    // Never `--network host` (module docs). `-T` forwards the container's loopback ports to
    // the host's: the broker's, and 443 to the EOS proxy.
    let mut fwd: Vec<String> = host.broker_port.map(|p| format!("-T,{p}")).into_iter().collect();
    if let Some(p) = host.eos_proxy_port {
        fwd.push(format!("-T,443:{p}"));
        for name in super::eos::redirected_hosts() {
            a.extend(["--add-host".into(), format!("{name}:127.0.0.1")]);
        }
    }
    a.push(if fwd.is_empty() { "--network=pasta".into() } else { format!("--network=pasta:{}", fwd.join(",")) });
    a.extend(["-v".into(), mount(&inst.prefix(), &m.paths.prefix, "")]);
    if !m.paths.install.starts_with(&m.paths.prefix) {
        a.extend(["-v".into(), mount(&inst.install(), &m.paths.install, "")]);
    }
    a.extend(["-v".into(), mount(&m.runtime.proton_path, &m.runtime.proton_path, "ro")]);
    a.extend(["-v".into(), mount(&inst.lab(), Path::new(LAB_MOUNT), "")]);
    for p in &host.ro_mounts {
        a.extend(["-v".into(), mount(p, p, "ro")]);
    }
    if m.sight
        && let Some((h, shm)) = &host.sight
    {
        for v in super::sight::mounts(h, shm) {
            a.extend(["-v".into(), v]);
        }
    }
    let env: [(&str, String); 12] = [
        ("HOME", "/tmp/home".into()),
        ("USER", "steamuser".into()),
        ("XDG_RUNTIME_DIR", "/tmp/xdg".into()),
        ("WINEPREFIX", m.paths.prefix.display().to_string()),
        ("STEAM_COMPAT_DATA_PATH", m.paths.prefix.display().to_string()),
        ("STEAM_COMPAT_CLIENT_INSTALL_PATH", "/tmp/steam".into()),
        ("STEAM_COMPAT_APP_ID", "0".into()),
        ("QUIVER_LAB_W", m.display.width.to_string()),
        ("QUIVER_LAB_H", m.display.height.to_string()),
        ("QUIVER_LAB_HZ", m.display.refresh.to_string()),
        ("QUIVER_LAB_INSTANCE", inst.name.clone()),
        ("QUIVER_GAME", m.game.to_string()),
    ];
    for (k, v) in env {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    a.push(m.runtime.image.clone());
    a.extend(["bash".into(), "-c".into(), boot_script().into()]);
    a
}

/// The prologue every `podman exec` into an instance runs: the instance's own display (as
/// gamescope reported it, never a default), then the command.
const WITH_DISPLAY: &str =
    r#"d=$(cat /tmp/display 2>/dev/null); [ -n "$d" ] || { echo "the instance's display isn't up" >&2; exit 3; }; export DISPLAY="$d"; exec "$@""#;

/// `podman exec …` (without `podman`) running `cmd` with the instance's display.
pub fn exec_args(container: &str, detach: bool, workdir: Option<&Path>, env: &[(String, String)], cmd: &[String]) -> Vec<String> {
    let mut a = vec!["exec".to_owned()];
    if detach {
        a.push("-d".into());
    }
    if let Some(w) = workdir {
        a.extend(["-w".into(), w.display().to_string()]);
    }
    for (k, v) in env {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    a.extend([container.to_owned(), "bash".into(), "-c".into(), WITH_DISPLAY.into(), "_".into()]);
    a.extend(cmd.iter().cloned());
    a
}

/// What a launch runs in the container: Proton's `run` (never `waitforexitandrun`, which
/// waits for every process in the prefix, the bridge included) of the bridge in launcher
/// mode (Ubisoft) or of the game, with output to the run's log.
pub fn launch_cmd(m: &Manifest, bridge: Option<(&str, &str, &str)>, extra_args: &[String], log: &str) -> Vec<String> {
    let proton = m.runtime.proton_path.join("proton").display().to_string();
    let mut argv = vec!["python3".to_owned(), proton, "run".into()];
    let exe = m.exe_win();
    match bridge {
        Some((bridge_win, ctx_win, bridge_log_win)) => {
            argv.push(bridge_win.into());
            argv.extend(quiver_ipc::run::BridgeArgs::run_command(ctx_win, Some(bridge_log_win), &exe, &game_args(m, extra_args)));
        }
        None => {
            argv.push(exe);
            argv.extend(game_args(m, extra_args));
        }
    }
    // Through bash for the redirect; the arguments stay separate words.
    let mut cmd = vec!["bash".to_owned(), "-c".into(), format!(r#"exec "$@" >> {} 2>&1"#, shell_quote(log)), "_".into()];
    cmd.extend(argv);
    cmd
}

fn game_args(m: &Manifest, extra: &[String]) -> Vec<String> {
    m.paths.args.iter().chain(extra).cloned().collect()
}

pub fn shell_quote(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// `podman build` arguments for the lab image from a context folder holding the
/// Containerfile.
pub fn build_args(tag: &str, context: &Path) -> Vec<String> {
    vec!["build".into(), "-t".into(), tag.into(), "-f".into(), context.join("Containerfile").display().to_string(), context.display().to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Inst, Manifest) {
        let m: Manifest = toml::from_str(
            r#"
name = "t1"
game = "ubisoft:3539"
title = "Origins"
licence = "ubisoft-bridge"
created = "2026-11-18T10:00:00Z"
[display]
width = 2560
height = 1600
refresh = 165
[runtime]
proton = "GE"
proton_path = "/p/GE"
image = "localhost/quiver-lab:1"
[paths]
install = "/g/origins"
prefix = "/pf/origins"
exe = "ACOrigins.exe"
"#,
        )
        .unwrap();
        (super::super::Lab::new("/l".into()).instance("t1"), m)
    }

    #[test]
    fn never_host_networking_and_mounts_the_clones() {
        let (inst, m) = sample();
        let a = run_args(&inst, &m, &Host { uid: 1000, gid: 1000, broker_port: Some(47810), ..Default::default() });
        assert!(!a.iter().any(|x| x.contains("network=host") || x == "host"), "{a:?}");
        assert!(a.contains(&"--network=pasta:-T,47810".to_owned()));
        assert!(a.contains(&"--init".to_owned()));
        assert!(a.contains(&"/l/instances/t1/prefix:/pf/origins".to_owned()));
        assert!(a.contains(&"/l/instances/t1/install:/g/origins".to_owned()));
        assert!(a.contains(&"/p/GE:/p/GE:ro".to_owned()));
        // The originals are never a mount source.
        assert!(!a.iter().any(|x| x.starts_with("/pf/origins:") || x.starts_with("/g/origins:")));
        assert!(a.contains(&"QUIVER_LAB_W=2560".to_owned()));
        assert!(!a.iter().any(|x| x.contains("epicgames")));
    }

    #[test]
    fn epic_names_go_to_the_eos_proxy() {
        let (inst, m) = sample();
        let a = run_args(&inst, &m, &Host { uid: 1000, gid: 1000, broker_port: Some(47810), eos_proxy_port: Some(40123), ..Default::default() });
        assert!(a.contains(&"--network=pasta:-T,47810,-T,443:40123".to_owned()), "{a:?}");
        let hosts: Vec<&String> = a.windows(2).filter(|w| w[0] == "--add-host").map(|w| &w[1]).collect();
        assert!(hosts.contains(&&"api.epicgames.dev:127.0.0.1".to_owned()), "{hosts:?}");
        assert!(hosts.contains(&&"api-gamedev.ol.epicgames.net:127.0.0.1".to_owned()));
        let a = run_args(&inst, &m, &Host { eos_proxy_port: Some(40123), ..Default::default() });
        assert!(a.contains(&"--network=pasta:-T,443:40123".to_owned()), "{a:?}");
    }

    #[test]
    fn sight_mounts_only_for_sight_instances() {
        let (inst, mut m) = sample();
        let h = super::super::sight::HostSight { lib_dir: "/h/sight".into(), binary: "/h/quiver-sight".into(), extra_libs: vec![], i686: false };
        let host = Host { sight: Some((h, "/dev/shm/quiver-lab-1000/t1".into())), ..Default::default() };
        let a = run_args(&inst, &m, &host);
        assert!(!a.iter().any(|x| x.contains("/opt/quiver") || x.contains("quiver-lab-1000")), "sight off: nothing of it ({a:?})");
        m.sight = true;
        let a = run_args(&inst, &m, &host);
        assert!(a.contains(&"/h/sight:/opt/quiver/sight:ro".to_owned()), "{a:?}");
        assert!(a.contains(&"/h/quiver-sight:/opt/quiver/bin/quiver-sight:ro".to_owned()));
        assert!(a.contains(&"/dev/shm/quiver-lab-1000/t1:/dev/shm/quiver-lab-1000/t1".to_owned()));
        // Still its own namespaces: no host pid, ipc or network namespace for Sight.
        assert!(!a.iter().any(|x| x.starts_with("--pid") || x.starts_with("--ipc") || x.contains("network=host")));
    }

    #[test]
    fn boot_reserves_low_displays() {
        let s = boot_script();
        assert!(s.contains("for d in 0 1 2 3 4 5 6 7 8 9"));
        assert!(s.contains("--backend headless"));
        assert!(s.contains("refusing display"));
        assert!(s.contains("pcm.!default { type null }"), "games without a sound device crash");
    }

    #[test]
    fn exec_uses_the_instances_display() {
        let a = exec_args("quiver-lab-t1", true, None, &[("A".into(), "b c".into())], &["xdotool".into(), "key".into(), "Return".into()]);
        assert_eq!(&a[..4], ["exec", "-d", "-e", "A=b c"]);
        assert!(a.iter().any(|x| x.contains("cat /tmp/display")));
        assert!(!a.iter().any(|x| x.contains("DISPLAY=:0")));
        assert_eq!(a.last().unwrap(), "Return");
    }

    #[test]
    fn launches_use_proton_run_and_the_bridge_launcher() {
        let (_, m) = sample();
        let c = launch_cmd(&m, Some((r"C:\quiver\quiver-bridge.exe", r"Z:\lab\runs\r1\context.json", r"Z:\lab\runs\r1\bridge.log")), &["-x".into()], "/lab/runs/r1/game.log");
        let joined = c.join(" ");
        assert!(joined.contains("/p/GE/proton run C:\\quiver\\quiver-bridge.exe --run"), "{joined}");
        assert!(!joined.contains("waitforexitandrun"));
        assert!(c.ends_with(&[r"Z:\g\origins\ACOrigins.exe".to_owned(), "-x".into()]));
        let plain = launch_cmd(&m, None, &[], "/lab/runs/r1/game.log");
        assert_eq!(plain.last().unwrap(), r"Z:\g\origins\ACOrigins.exe");
    }
}
