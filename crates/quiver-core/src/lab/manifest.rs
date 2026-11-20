//! The instance manifest, `<instance>/lab.toml`: everything that decides what a launch
//! runs, pinned when the instance is made, so measurements in it are comparable. Every
//! tool reads the display size from here (never from a script's constant).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::GameRef;
use serde::{Deserialize, Serialize};

pub const FILE: &str = "lab.toml";

/// How an instance's launches get the user's licence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Licence {
    /// Ubisoft: `quiver-bridge` in the prefix with a launch context from quiverd per
    /// launch; Denuvo through quiverd's broker. Connect never runs.
    UbisoftBridge,
    /// Epic: a fresh exchange code (and ownership token) from quiverd per launch.
    EpicExchangeCode,
    /// DRM-free: nothing to mint (GOG, DRM-free Steam and Epic titles, a folder you own).
    None,
}

impl Licence {
    pub fn describe(self) -> &'static str {
        match self {
            Licence::UbisoftBridge => "Ubisoft bridge + quiverd broker (Connect never runs)",
            Licence::EpicExchangeCode => "Epic exchange code minted by quiverd per launch",
            Licence::None => "DRM-free (no licence)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    /// gamescope's refresh rate (`-r`), Hz.
    pub refresh: u32,
    /// HDR (design/LAB.md §13): gamescope's headless output forced to HDR10 PQ, and the
    /// game's environment for HDR ([`super::hdr`]). `WxH@HZ+hdr`, `display.hdr=on`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hdr: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self { width: 1920, height: 1080, refresh: 60, hdr: false }
    }
}

impl Display {
    /// `WxH[@HZ][+hdr]`.
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        let lower = t.to_ascii_lowercase();
        let (t, hdr) = match ["+hdr", " hdr"].iter().find(|x| lower.ends_with(*x)) {
            Some(x) => (&t[..t.len() - x.len()], true),
            None => (t, false),
        };
        let (res, hz) = match t.split_once('@') {
            Some((r, h)) => (r, Some(h)),
            None => (t, None),
        };
        let (w, h) = res.split_once(['x', 'X']).with_context(|| format!("'{s}': expected WIDTHxHEIGHT[@HZ]"))?;
        let d = Self {
            width: w.trim().parse().with_context(|| format!("'{s}': bad width"))?,
            height: h.trim().parse().with_context(|| format!("'{s}': bad height"))?,
            refresh: match hz {
                Some(h) => h.trim().trim_end_matches("Hz").parse().with_context(|| format!("'{s}': bad refresh"))?,
                None => 60,
            },
            hdr,
        };
        d.check()?;
        Ok(d)
    }

    pub fn check(&self) -> Result<()> {
        if !(320..=7680).contains(&self.width) || !(200..=4320).contains(&self.height) || !(1..=500).contains(&self.refresh) {
            bail!("display {}x{}@{} is out of range", self.width, self.height, self.refresh);
        }
        Ok(())
    }

    /// A point given as fractions of the window, in pixels (clamped to the display).
    pub fn at(&self, fx: f64, fy: f64) -> (u32, u32) {
        let px = |f: f64, n: u32| ((f.clamp(0.0, 1.0) * f64::from(n)).round() as u32).min(n.saturating_sub(1));
        (px(fx, self.width), px(fy, self.height))
    }
}

impl std::fmt::Display for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}{}", self.width, self.height, self.refresh, if self.hdr { "+hdr" } else { "" })
    }
}

/// The Wine runtime, pinned: the Proton build (by default the one the person plays the game
/// with) and the translation layers it ships, recorded so a later change shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// The build's folder name (`GE-Proton11-12-x86_64`).
    pub proton: String,
    pub proton_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dxvk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vkd3d: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvapi: Option<String>,
    /// The container image.
    pub image: String,
}

/// The versions a Proton build ships (`files/lib/wine/<layer>/version`).
pub fn layer_versions(proton: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let read = |layer: &str| {
        let text = std::fs::read_to_string(proton.join("files/lib/wine").join(layer).join("version")).ok()?;
        // "<sha> dxvk (v3.1.1-174-gf86377680)" -> "v3.1.1-174-gf86377680"
        let t = text.trim();
        Some(t.rsplit_once('(').and_then(|(_, v)| v.strip_suffix(')')).unwrap_or(t).to_owned())
    };
    (read("dxvk"), read("vkd3d-proton"), read("nvapi"))
}

/// Where the game lives. `install` and `prefix` are the *original* paths: the instance's
/// clones are mounted there in the container, so paths the game or its registry recorded
/// keep working. The originals themselves are never mounted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamePaths {
    pub install: PathBuf,
    pub prefix: PathBuf,
    /// The executable, relative to `install`.
    pub exe: String,
    /// Its arguments (the store's, then yours).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub game: GameRef,
    pub title: String,
    pub licence: Licence,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub created_by: String,
    pub display: Display,
    pub runtime: Runtime,
    pub paths: GamePaths,
    /// Extra environment for the game (the game's Quiver settings when made, then yours).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dll_overrides: BTreeMap<String, String>,
    /// A folder of files copied over the install before each launch (the originals are
    /// kept and put back first): the mod build under test. `--mod` overrides it per launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_dir: Option<PathBuf>,
    /// The graphics profile applied before each launch (`<lab>/profiles/<game>/<name>.set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphics: Option<String>,
    /// The save fixture installed when the instance was made, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<String>,
    /// Save folders, relative to the prefix (what fixtures copy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saves: Vec<String>,
    /// Short names for config files, for `quiver lab set` (`pharos = "install:pharos.ini"`,
    /// `aco = "drive_c/users/steamuser/Documents/…/ACO.ini"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// Config-set lines applied to this instance while another one holds the GPU
    /// (`quiver lab gpu`), and undone after: e.g. `pharos:[reflex]fps_limit=5`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub throttle: Vec<String>,
    /// A log line that means a live config reload happened (a regex), for `quiver lab set
    /// --wait` and A/B runs; the mod's log is `log_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_pattern: Option<String>,
    /// The mod's log: `run:<file>` in the current run's folder (`lab/runs/r<N>/`, where a mod
    /// that writes to `QUIVER_LAB_RUN_DIR` puts it), `lab:<rel>`, `install:<rel>`, or a path
    /// relative to the prefix ([`Manifest::resolve_file`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Launches carry Sight: the layer (HUD, the panel drawn in the game), the input preload,
    /// and a lab `quiver-sight` in the container (`lab::sight`). Off by default; changing it
    /// needs `quiver lab down`/`up` (the container's mounts).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sight: bool,
    /// The fixed-step virtual clock (`[clock]`, [`super::vclock`]); absent: real time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<super::vclock::VirtualClock>,
    /// Virtual game controllers (`[pad]`, design/LAB.md §16): uinput pads made on the host for
    /// this instance only. Off by default; changing it needs `quiver lab down`/`up`.
    #[serde(default, skip_serializing_if = "PadConfig::is_off")]
    pub pad: PadConfig,
}

/// `[pad]`: how many virtual pads the instance's container gets, and how they're driven.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PadConfig {
    /// Pads made at `up` (0: none). Player 1 is the first.
    #[serde(default)]
    pub count: u8,
    /// A hyprpad user's grip map (`r4=a,r5=b,l4=up,l5=down`): what the Steam Controller's
    /// grips are on an Xbox pad, so scripts can say `r4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grips: Option<String>,
    /// How long, in real ms, input settles while the game is held on the virtual clock before
    /// the next step (default 40: evdev, SDL, winebus and XInput's own thread).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u32>,
}

impl PadConfig {
    pub fn is_off(&self) -> bool {
        *self == PadConfig::default()
    }
    /// At most this many pads per instance (XInput has four slots).
    pub const MAX: u8 = 4;
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let m: Manifest = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        m.display.check()?;
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = format!(
            "# quiver lab instance (design/LAB.md). Edit with care: paths.install and paths.prefix are\n\
             # the ORIGINAL locations, where the instance's clones are mounted in its container.\n{}",
            toml::to_string_pretty(self)?
        );
        super::write_atomic(path, text.as_bytes())
    }

    /// The game's executable as the game sees it (a Windows path on the mounted install).
    pub fn exe_win(&self) -> String {
        crate::launcher::win_path(&self.paths.install.join(&self.paths.exe))
    }

    /// The exe's file name (what its process is called).
    pub fn exe_name(&self) -> String {
        Path::new(&self.paths.exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.paths.exe.clone())
    }

    /// The instance's install clone on the host: its own folder, or inside the prefix clone
    /// when the game is installed in its prefix (a Connect or Lutris layout).
    pub fn install_in(&self, inst: &super::Inst) -> PathBuf {
        match self.paths.install.strip_prefix(&self.paths.prefix) {
            Ok(rel) => inst.prefix().join(rel),
            Err(_) => inst.install(),
        }
    }

    /// A file reference (an alias from `files`, `install:<rel>`, `lab:<rel>`, `run:<rel>` for
    /// the current run's folder `lab/runs/r<N>/`, or a path relative to the prefix) as a host
    /// path inside the instance's folders. `run:` is resolved when called: after a relaunch,
    /// resolve again.
    pub fn resolve_file(&self, inst: &super::Inst, spec: &str) -> Result<PathBuf> {
        let spec = self.files.get(spec).map(String::as_str).unwrap_or(spec);
        let run_dir;
        let (base, rel) = if let Some(rel) = spec.strip_prefix("install:") {
            (self.install_in(inst), rel)
        } else if let Some(rel) = spec.strip_prefix("lab:") {
            (inst.lab(), rel)
        } else if let Some(rel) = spec.strip_prefix("run:") {
            let run = inst.current_run().with_context(|| format!("'{spec}': instance '{}' has no run yet (run: is the current run's folder)", inst.name))?;
            run_dir = inst.run_dir(run);
            (run_dir, rel)
        } else {
            (inst.prefix(), spec.strip_prefix("prefix:").unwrap_or(spec))
        };
        let rel = Path::new(rel.trim_start_matches('/'));
        if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            bail!("'{spec}': config paths are relative and may not contain '..'");
        }
        Ok(base.join(rel))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Manifest {
        Manifest {
            name: "t1".into(),
            game: GameRef::ubisoft(3539),
            title: "Assassin's Creed Origins".into(),
            licence: Licence::UbisoftBridge,
            created: "2026-11-18T10:00:00Z".parse().unwrap(),
            created_by: "agent".into(),
            display: Display { width: 2560, height: 1600, refresh: 165, hdr: false },
            runtime: Runtime {
                proton: "GE-Proton11-12-x86_64".into(),
                proton_path: "/p/GE-Proton11-12-x86_64".into(),
                dxvk: Some("v3.1.1".into()),
                vkd3d: None,
                nvapi: None,
                image: super::super::IMAGE.into(),
            },
            paths: GamePaths { install: "/g/origins".into(), prefix: "/pf/origins".into(), exe: "ACOrigins.exe".into(), args: vec![] },
            env: BTreeMap::from([("PROTON_ENABLE_NVAPI".into(), "1".into())]),
            dll_overrides: BTreeMap::new(),
            mod_dir: None,
            graphics: Some("ultra".into()),
            fixture: None,
            saves: vec!["drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames/u/3539".into()],
            files: BTreeMap::from([("pharos".into(), "install:pharos.ini".into())]),
            throttle: vec!["pharos:[reflex]fps_limit=5".into()],
            reload_pattern: Some("reloaded".into()),
            log_file: None,
            notes: String::new(),
            sight: false,
            clock: None,
            pad: Default::default(),
        }
    }

    #[test]
    fn round_trip() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(FILE);
        let m = sample();
        m.save(&p).unwrap();
        let back = Manifest::load(&p).unwrap();
        assert_eq!(back, m);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("game = \"ubisoft:3539\""), "{text}");
        // Typos fail loudly.
        std::fs::write(&p, text.replace("refresh = 165", "refesh = 165")).unwrap();
        assert!(Manifest::load(&p).is_err());
    }

    #[test]
    fn displays() {
        assert_eq!(Display::parse("2560x1600@165").unwrap(), Display { width: 2560, height: 1600, refresh: 165, hdr: false });
        let h = Display::parse("1920x1080@60+HDR").unwrap();
        assert!(h.hdr && h.refresh == 60);
        assert_eq!(h.to_string(), "1920x1080@60+hdr");
        assert_eq!(Display::parse(&h.to_string()).unwrap(), h);
        assert!(Display::parse("1280x720 hdr").unwrap().hdr);
        assert_eq!(Display::parse("1280x720").unwrap().refresh, 60);
        assert!(Display::parse("10x10").is_err() && Display::parse("axb").is_err());
        let d = Display { width: 1920, height: 1080, refresh: 60, hdr: false };
        assert_eq!(d.at(0.5, 0.5), (960, 540));
        assert_eq!(d.at(1.0, 2.0), (1919, 1079));
        assert_eq!(d.to_string(), "1920x1080@60");
    }

    #[test]
    fn files_resolve_inside_the_instance() {
        let m = sample();
        let inst = super::super::Lab::new("/l".into()).instance("t1");
        assert_eq!(m.resolve_file(&inst, "pharos").unwrap(), Path::new("/l/instances/t1/install/pharos.ini"));
        assert_eq!(m.resolve_file(&inst, "drive_c/x.ini").unwrap(), Path::new("/l/instances/t1/prefix/drive_c/x.ini"));
        assert_eq!(m.resolve_file(&inst, "lab:state").unwrap(), Path::new("/l/instances/t1/lab/state"));
        assert!(m.resolve_file(&inst, "install:../../etc/passwd").is_err());
    }

    #[test]
    fn run_files_follow_the_current_run() {
        let t = tempfile::tempdir().unwrap();
        let mut m = sample();
        let inst = super::super::Lab::new(t.path().into()).instance("t1");
        std::fs::create_dir_all(&inst.dir).unwrap();
        // No run yet: said so.
        let e = m.resolve_file(&inst, "run:pharos.log").unwrap_err().to_string();
        assert!(e.contains("no run yet"), "{e}");
        let state = |id: u32| {
            let st = super::super::ready::State {
                instance: "t1".into(),
                run: Some(super::super::ready::Run::new(id, format!("lab-t1-r{id}"), chrono::Utc::now())),
                ..Default::default()
            };
            std::fs::write(inst.state_path(), serde_json::to_vec(&st).unwrap()).unwrap();
        };
        state(3);
        assert_eq!(m.resolve_file(&inst, "run:pharos.log").unwrap(), inst.lab().join("runs/r3/pharos.log"));
        // The next launch: the same spec names the new run's file.
        state(4);
        assert_eq!(m.resolve_file(&inst, "run:logs/pharos.log").unwrap(), inst.lab().join("runs/r4/logs/pharos.log"));
        // Through an alias too, and never out of the run folder.
        m.files.insert("modlog".into(), "run:pharos.log".into());
        assert_eq!(m.resolve_file(&inst, "modlog").unwrap(), inst.lab().join("runs/r4/pharos.log"));
        assert!(m.resolve_file(&inst, "run:../r3/pharos.log").is_err());
    }

    #[test]
    fn layer_versions_from_proton() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("files/lib/wine/dxvk");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("version"), "f8637 dxvk (v3.1.1-174-gf86377680)\n").unwrap();
        assert_eq!(layer_versions(t.path()), (Some("v3.1.1-174-gf86377680".into()), None, None));
    }
}
