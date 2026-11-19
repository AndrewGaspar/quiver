//! Sight in a lab instance (design/LAB.md §12, design/SIGHT.md §14): with `sight = true` in
//! the manifest (`quiver lab create … --sight`, `quiver lab edit N sight=on`), launches carry
//! the Sight layer and the input preload, and a `quiver-sight` runs **inside the container**
//! for the run, in its lab mode.
//!
//! Why inside: the container has its own pid namespace and its own `/dev/shm`. Inside, the
//! game, the layer, the preload and `quiver-sight` share both, so the preload's "the panel's
//! owner still exists" failsafe works as on the desktop, and the session dir needs no special
//! path. It renders off-screen (GPUI's headless platform on the container's GPU, through CDI).
//!
//! Its session dir is in a per-instance tmpfs folder on the host (`/dev/shm/quiver-lab-<uid>/
//! <instance>`), bind-mounted at the same path, so the supervisor makes it (and `hud.image`)
//! before the launch and `quiver lab sight` reads `panel.input` and the layer's ring from the
//! host.
//!
//! **Nothing reaches the person's settings.** The lab `quiver-sight` has no quiverd
//! connection at all (none is reachable from the container, and its lab mode makes none):
//! its HUD model and panel data come from the layer's ring and the instance; every panel
//! action that would change settings (mod options, Tweaks, HUD settings, quitting) is refused
//! and logged; the HUD's shown/hidden and preset only change the instance's own `quiver-sight`.
//! The session stays `lab-<instance>-r<run>`, which quiverd never reports.
//!
//! Mounted read-only: the Sight libraries at [`SIGHT_MOUNT`] (`{x86_64,i686}/…`), the
//! `quiver-sight` binary at [`SIGHT_BIN`], and libraries the image lacks for it at
//! [`SIGHT_LIBS`] (`libxkbcommon-x11`, image `quiver-lab:1`). The layer's manifests are written
//! into the lab folder ([`LAYER_DIR`]) at each launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::sightenv::{self, ENV_SIGHT, INPUT_SO, LAYER_SO};
use quiver_api::perf::Captures;

/// The Sight libraries in the container (`x86_64/`, `i686/`).
pub const SIGHT_MOUNT: &str = "/opt/quiver/sight";
/// The `quiver-sight` binary in the container.
pub const SIGHT_BIN: &str = "/opt/quiver/bin/quiver-sight";
/// Extra shared libraries for `quiver-sight` (its `LD_LIBRARY_PATH` only, not the game's).
pub const SIGHT_LIBS: &str = "/opt/quiver/lib";
/// The layer's manifests, in the lab folder (`/lab/sight/layer` in the container).
pub const LAYER_DIR: &str = "sight/layer";
/// Libraries `quiver-sight` needs that the lab image may lack, from the host.
pub const EXTRA_LIBS: [&str; 1] = ["libxkbcommon-x11.so.0"];
/// Overrides for where the host's Sight pieces are (tests, dev builds): a folder laid out
/// `{x86_64,i686}/lib*.so`, and the binary.
pub const ENV_SIGHT_DIR: &str = "QUIVER_LAB_SIGHT_DIR";
pub const ENV_SIGHT_BIN: &str = "QUIVER_LAB_SIGHT_BIN";

/// The host's Sight pieces a `sight = true` instance mounts.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSight {
    /// `{x86_64,i686}/libquiver_sight_{layer,input}.so`.
    pub lib_dir: PathBuf,
    pub binary: PathBuf,
    /// Host libraries mounted under [`SIGHT_LIBS`] (their real files, by soname).
    pub extra_libs: Vec<(PathBuf, String)>,
    pub i686: bool,
}

impl HostSight {
    /// The installed pieces next to the running `quiver` (`<prefix>/bin/quiver-sight`,
    /// `<prefix>/lib/quiver/sight`), or the overrides. `Err`: what's missing.
    pub fn discover() -> Result<HostSight, String> {
        let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).ok_or("no executable path")?;
        let lib_dir = match std::env::var_os(ENV_SIGHT_DIR) {
            Some(d) => PathBuf::from(d),
            None => sightenv::installed_sight_dir(&exe_dir, |p| p.is_file()).ok_or_else(|| format!("no Sight libraries next to {} (lib/quiver/sight/x86_64/{INPUT_SO}): install Quiver (tools/install-user.sh), or set {ENV_SIGHT_DIR}", exe_dir.display()))?,
        };
        for so in [LAYER_SO, INPUT_SO] {
            if !lib_dir.join("x86_64").join(so).is_file() {
                return Err(format!("{} missing", lib_dir.join("x86_64").join(so).display()));
            }
        }
        let binary = std::env::var_os(ENV_SIGHT_BIN).map(PathBuf::from).unwrap_or_else(|| exe_dir.join("quiver-sight"));
        if !binary.is_file() {
            return Err(format!("{} missing (quiver-sight is installed next to quiver)", binary.display()));
        }
        let i686 = [LAYER_SO, INPUT_SO].iter().all(|so| lib_dir.join("i686").join(so).is_file());
        Ok(HostSight { lib_dir, binary, extra_libs: extra_libs(Path::new("/usr/lib")), i686 })
    }
}

/// The host's copies of [`EXTRA_LIBS`] under `dir` (the files the sonames point at).
pub fn extra_libs(dir: &Path) -> Vec<(PathBuf, String)> {
    EXTRA_LIBS.iter().filter_map(|so| std::fs::canonicalize(dir.join(so)).ok().map(|real| (real, (*so).to_owned()))).collect()
}

/// The instance's shared-memory root on the host, mounted at the same path in its container:
/// `<base>/quiver-lab-<uid>/<instance>` (`base` is `/dev/shm`, a tmpfs).
pub fn shm_root(base: &Path, uid: u32, instance: &str) -> PathBuf {
    base.join(format!("quiver-lab-{uid}")).join(instance)
}

/// `-v` arguments (`host:container[:opts]`) for a `sight = true` instance's container.
pub fn mounts(h: &HostSight, shm: &Path) -> Vec<String> {
    let mut v = vec![format!("{}:{SIGHT_MOUNT}:ro", h.lib_dir.display()), format!("{}:{SIGHT_BIN}:ro", h.binary.display())];
    for (real, soname) in &h.extra_libs {
        v.push(format!("{}:{SIGHT_LIBS}/{soname}:ro", real.display()));
    }
    v.push(format!("{}:{}", shm.display(), shm.display()));
    v
}

/// The layer's manifests for the container (file name, JSON): the mounted libraries.
pub fn layer_manifests(i686: bool) -> Vec<(String, String)> {
    let mut v = vec![("VK_LAYER_QUIVER_sight.x86_64.json".to_owned(), sightenv::layer_manifest_json(&Path::new(SIGHT_MOUNT).join("x86_64").join(LAYER_SO), true))];
    if i686 {
        v.push(("VK_LAYER_QUIVER_sight.i686.json".to_owned(), sightenv::layer_manifest_json(&Path::new(SIGHT_MOUNT).join("i686").join(LAYER_SO), false)));
    }
    v
}

/// The game's environment for Sight: the session dir, the layer (its manifests in the lab
/// folder, and the layer order quiverd's launches use), the input preload after the
/// manifest's own `LD_PRELOAD`, and the layer's and preload's logs on (they go to the run's
/// `game.log`). `env` is the manifest's environment.
pub fn launch_env(session_dir: &Path, env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = vec![
        (quiver_perf::session::ENV_PERF_DIR.to_owned(), session_dir.display().to_string()),
        (ENV_SIGHT.to_owned(), "1".to_owned()),
    ];
    let layer_dir = format!("{}/{LAYER_DIR}", super::LAB_MOUNT);
    out.push(("VK_ADD_IMPLICIT_LAYER_PATH".into(), match env.get("VK_ADD_IMPLICIT_LAYER_PATH").filter(|v| !v.is_empty()) {
        Some(v) => format!("{layer_dir}:{v}"),
        None => layer_dir,
    }));
    let obs = sightenv::uses_obs_capture(env, &[]);
    out.push(("VK_INSTANCE_LAYERS".into(), sightenv::layer_order(Captures::Shown, env, obs).join(":")));
    if env.get(sightenv::ENV_INPUT).is_none_or(|v| v != "0") {
        out.push(("LD_PRELOAD".into(), sightenv::join_preload(env.get("LD_PRELOAD").map(String::as_str), &sightenv::preload_entry(Path::new(SIGHT_MOUNT)))));
    }
    if !env.contains_key("QUIVER_SIGHT_LOG") {
        out.push(("QUIVER_SIGHT_LOG".into(), "1".into()));
    }
    out
}

/// What starts the run's `quiver-sight` (through `podman exec -d`): its lab mode on the
/// session dir, its log appended to `log`, its status in `status` (both container paths).
pub fn sight_cmd(session_dir: &Path, status: &str, log: &str, title: &str, game: &str) -> Vec<String> {
    let mut cmd = vec![
        "bash".to_owned(),
        "-c".into(),
        format!(r#"export LD_LIBRARY_PATH={SIGHT_LIBS}; unset WAYLAND_DISPLAY; exec "$@" >> {} 2>&1"#, super::podman::shell_quote(log)),
        "_".into(),
        SIGHT_BIN.into(),
        "--headless".into(),
        "--lab".into(),
        session_dir.display().to_string(),
        "--lab-status".into(),
        status.into(),
    ];
    cmd.extend(["--title".into(), title.into(), "--game".into(), game.into()]);
    cmd
}

/// The `quiver-sight` lab mode's status file (`runs/r<N>/sight.json`), written on every change.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LabSightStatus {
    pub pid: u32,
    pub session: String,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// The HUD: shown, its preset.
    pub hud_on: bool,
    pub hud_layout: String,
    /// The layer's swapchain, when it has one.
    pub extent: Option<[u32; 2]>,
    /// Frames per second over the last second, from the layer's ring.
    pub fps: Option<f64>,
    /// The panel's last report (open, presenter and why, backdrop, …).
    #[serde(default)]
    pub panel: Option<quiver_api::sight::PanelReport>,
    /// Panel actions refused (read-only in a lab instance), newest last.
    #[serde(default)]
    pub refused: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn launch_env_like_quiverds() {
        let dir = Path::new("/dev/shm/quiver-lab-1000/awr/quiver-1000/lab-awr-r3");
        let e: BTreeMap<String, String> = launch_env(dir, &BTreeMap::new()).into_iter().collect();
        assert_eq!(e["QUIVER_PERF_DIR"], dir.display().to_string());
        assert_eq!(e["QUIVER_SIGHT"], "1");
        assert_eq!(e["VK_ADD_IMPLICIT_LAYER_PATH"], "/lab/sight/layer");
        assert_eq!(e["VK_INSTANCE_LAYERS"], "VK_LAYER_QUIVER_sight");
        assert_eq!(e["LD_PRELOAD"], "/opt/quiver/sight/$PLATFORM/libquiver_sight_input.so");
        assert_eq!(e["QUIVER_SIGHT_LOG"], "1");
        // The manifest's own: kept, ours after; frame generation above Sight; opt-out.
        let e: BTreeMap<String, String> =
            launch_env(dir, &env(&[("LD_PRELOAD", "/x/a.so"), ("ENABLE_VKBASALT", "1"), ("VK_ADD_IMPLICIT_LAYER_PATH", "/y"), ("QUIVER_SIGHT_LOG", "0")])).into_iter().collect();
        assert_eq!(e["LD_PRELOAD"], "/x/a.so:/opt/quiver/sight/$PLATFORM/libquiver_sight_input.so");
        assert_eq!(e["VK_INSTANCE_LAYERS"], "VK_LAYER_VKBASALT_post_processing:VK_LAYER_QUIVER_sight");
        assert_eq!(e["VK_ADD_IMPLICIT_LAYER_PATH"], "/lab/sight/layer:/y");
        assert!(!e.contains_key("QUIVER_SIGHT_LOG"), "the manifest's own value stays");
        let e: BTreeMap<String, String> = launch_env(dir, &env(&[("QUIVER_SIGHT_INPUT", "0")])).into_iter().collect();
        assert!(!e.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn mounts_and_manifests() {
        let h = HostSight { lib_dir: "/h/lib/quiver/sight".into(), binary: "/h/bin/quiver-sight".into(), extra_libs: vec![("/usr/lib/libxkbcommon-x11.so.0.13.2".into(), "libxkbcommon-x11.so.0".into())], i686: true };
        let shm = shm_root(Path::new("/dev/shm"), 1000, "awr");
        assert_eq!(shm, Path::new("/dev/shm/quiver-lab-1000/awr"));
        let m = mounts(&h, &shm);
        assert_eq!(m[0], "/h/lib/quiver/sight:/opt/quiver/sight:ro");
        assert_eq!(m[1], "/h/bin/quiver-sight:/opt/quiver/bin/quiver-sight:ro");
        assert_eq!(m[2], "/usr/lib/libxkbcommon-x11.so.0.13.2:/opt/quiver/lib/libxkbcommon-x11.so.0:ro");
        assert_eq!(m[3], "/dev/shm/quiver-lab-1000/awr:/dev/shm/quiver-lab-1000/awr", "the session dirs, read-write, at the same path");
        let man = layer_manifests(true);
        assert_eq!(man.len(), 2);
        let j: serde_json::Value = serde_json::from_str(&man[1].1).unwrap();
        assert_eq!(j["layer"]["library_path"], "/opt/quiver/sight/i686/libquiver_sight_layer.so");
        assert_eq!(j["layer"]["library_arch"], "32");
        assert_eq!(layer_manifests(false).len(), 1);
    }

    #[test]
    fn the_run_sight_command() {
        let c = sight_cmd(Path::new("/dev/shm/x/lab-a-r1"), "/lab/runs/r1/sight.json", "/lab/runs/r1/sight.log", "Alan Wake", "epic:x");
        assert!(c[2].contains("LD_LIBRARY_PATH=/opt/quiver/lib") && c[2].contains(">> /lab/runs/r1/sight.log"));
        assert_eq!(&c[4..8], [SIGHT_BIN, "--headless", "--lab", "/dev/shm/x/lab-a-r1"]);
        assert!(c.windows(2).any(|w| w == ["--title", "Alan Wake"]));
    }
}
