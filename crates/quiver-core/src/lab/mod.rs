//! `quiver lab`: disposable headless instances of an installed game that an agent drives
//! (design/LAB.md). This module is the part with no daemon and no container in it: the
//! instance model (manifest, folders), the podman command lines, the host state (memory,
//! load, who uses the GPU), readiness, image metrics, config edits, A/B aggregation and the
//! facts a brief is written from. `quiver-cli`'s `lab` module drives
//! podman and the per-instance supervisor with these.
//!
//! **Invariants** (each has a test or a check at the point of use):
//! - An instance never sees the person's real prefix or install: both are reflink clones
//!   ([`clone_tree`]), mounted in the container at the original paths.
//! - Ubisoft Connect can't run in an instance: its executables are renamed away in the
//!   gold prefix and checked again before every launch, its credentials are deleted, its
//!   DLLs are disabled in `WINEDLLOVERRIDES`, and a Connect process seen in an instance
//!   kills the run ([`guard`]).
//! - An EOS game's requests to Epic pass an allowlist (sign-in, ownership): every instance's
//!   container resolves Epic's names to the instance's EOS proxy, which trusts nothing but
//!   the allowlist and answers the rest 403; a game with the EOS SDK doesn't launch unless
//!   the proxy answers from inside the container ([`eos`], [`eosproxy`], [`wincert`]).
//! - Displays `:0`–`:9` are reserved inside every container ([`podman::boot_script`]), and
//!   the container has a network namespace of its own (the broker port is forwarded), so it
//!   can't reach the host's X servers by their abstract sockets.

pub mod bench;
pub mod brief;
pub mod configset;
pub mod eos;
pub mod eosproxy;
pub mod facts;
pub mod guard;
pub mod host;
pub mod image;
pub mod manifest;
pub mod podman;
pub mod procs;
pub mod ready;
pub mod wincert;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub use manifest::Manifest;

/// The image `quiver lab image build` makes (bumped when the Containerfile changes).
pub const IMAGE: &str = "localhost/quiver-lab:1";

/// The Containerfile the image is built from.
pub const CONTAINERFILE: &str = include_str!("../../../../contrib/lab/Containerfile");

/// Where the instance's lab folder (`<instance>/lab`) is mounted in the container, and the
/// same folder as the game sees it (`QUIVER_LAB`).
pub const LAB_MOUNT: &str = "/lab";
pub const LAB_WIN: &str = r"Z:\lab";

/// Every lab folder: `$QUIVER_LAB_ROOT`, else `quiver-lab` next to Quiver's prefixes
/// folder (the same filesystem as the installs, so clones are reflinks).
pub fn root(prefixes: &Path) -> PathBuf {
    if let Some(r) = std::env::var_os("QUIVER_LAB_ROOT").filter(|r| !r.is_empty()) {
        return PathBuf::from(r);
    }
    prefixes.parent().unwrap_or(prefixes).join("quiver-lab")
}

/// Instance names: short, `[a-z0-9][a-z0-9_-]{0,23}` (they name a container, a folder and a
/// session id).
pub fn check_name(name: &str) -> Result<()> {
    let ok = (1..=24).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ok {
        bail!("instance names are 1-24 characters: lowercase letters, digits, - and _ (got '{name}')");
    }
    Ok(())
}

/// The folders under the lab root.
#[derive(Debug, Clone)]
pub struct Lab {
    pub root: PathBuf,
}

impl Lab {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }
    pub fn instance(&self, name: &str) -> Inst {
        Inst { name: name.to_owned(), dir: self.instances().join(name) }
    }
    /// A game's gold copy (prefix and install), cloned into each new instance.
    pub fn gold(&self, game_dir: &str) -> PathBuf {
        self.root.join("gold").join(game_dir)
    }
    /// A game's save fixtures (`<fixture>/` folders, read-only once saved).
    pub fn fixtures(&self, game_dir: &str) -> PathBuf {
        self.root.join("fixtures").join(game_dir)
    }
    /// A game's graphics profiles (`<name>.set`: config-set lines).
    pub fn profiles(&self, game_dir: &str) -> PathBuf {
        self.root.join("profiles").join(game_dir)
    }
    /// A game's named locations (`locations.txt`: `name x y z [yaw [pitch]]`).
    pub fn locations(&self, game_dir: &str) -> PathBuf {
        self.root.join("fixtures").join(game_dir).join("locations.txt")
    }
    /// The GPU-exclusive lock and its holder note.
    pub fn gpu_lock(&self) -> PathBuf {
        self.root.join("gpu.lock")
    }
    pub fn jobs(&self) -> PathBuf {
        self.root.join("jobs")
    }
    /// Names of existing instances, sorted.
    pub fn list(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.instances())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join(manifest::FILE).is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect();
        out.sort();
        out
    }
}

/// One instance's folders.
#[derive(Debug, Clone)]
pub struct Inst {
    pub name: String,
    pub dir: PathBuf,
}

impl Inst {
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(manifest::FILE)
    }
    pub fn load(&self) -> Result<Manifest> {
        Manifest::load(&self.manifest_path()).with_context(|| format!("instance '{}'", self.name))
    }
    /// The clone of the game's prefix (mounted at the original prefix path).
    pub fn prefix(&self) -> PathBuf {
        self.dir.join("prefix")
    }
    /// The clone of the game's install (mounted at the original install path).
    pub fn install(&self) -> PathBuf {
        self.dir.join("install")
    }
    /// Mounted at [`LAB_MOUNT`]: the lab channel (state, requests), runs, shots, tools.
    pub fn lab(&self) -> PathBuf {
        self.dir.join("lab")
    }
    pub fn runs(&self) -> PathBuf {
        self.lab().join("runs")
    }
    pub fn run_dir(&self, run: u32) -> PathBuf {
        self.runs().join(format!("r{run}"))
    }
    pub fn shots(&self) -> PathBuf {
        self.lab().join("shots")
    }
    /// Originals of install files a mod replaced (restored before the next launch).
    pub fn originals(&self) -> PathBuf {
        self.dir.join("orig")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }
    /// The supervisor's socket: in the instance's folder, or (when that path is too long for
    /// a Unix socket) `$XDG_RUNTIME_DIR/quiver-lab/<name>-<hash of the folder>.sock`.
    pub fn socket(&self) -> PathBuf {
        let here = self.dir.join("supervisor.sock");
        if here.as_os_str().len() < 100 {
            return here;
        }
        let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("quiver-lab");
        let _ = std::fs::create_dir_all(&run);
        run.join(format!("{}-{:08x}.sock", self.name, crc32fast::hash(self.dir.as_os_str().as_encoded_bytes())))
    }
    pub fn supervisor_lock(&self) -> PathBuf {
        self.dir.join("supervisor.lock")
    }
    pub fn supervisor_log(&self) -> PathBuf {
        self.dir.join("supervisor.log")
    }
    pub fn container(&self) -> String {
        podman::container_name(&self.name)
    }
}

/// Copy a folder tree as reflinks (`cp -a --reflink=always`): free on btrfs and XFS, and
/// refused elsewhere rather than silently copying a whole game.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=always")
        .arg("--")
        .arg(src)
        .arg(dst)
        .output()
        .context("run cp")?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dst);
        bail!(
            "cloning {} to {} failed (the lab needs a filesystem with reflinks, e.g. btrfs, on the same filesystem as the game): {}",
            src.display(),
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Write `bytes` to `path` through a temporary file and a rename (readers never see half).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["t1", "origins-dlss", "a_b", "9"] {
            check_name(ok).unwrap();
        }
        for bad in ["", "T1", "-x", "a/b", "a b", &"x".repeat(25)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn folders() {
        let lab = Lab::new("/l".into());
        let i = lab.instance("t1");
        assert_eq!(i.prefix(), Path::new("/l/instances/t1/prefix"));
        assert_eq!(i.run_dir(3), Path::new("/l/instances/t1/lab/runs/r3"));
        assert_eq!(lab.gold("3539"), Path::new("/l/gold/3539"));
        assert_eq!(i.container(), "quiver-lab-t1");
    }

    #[test]
    fn clone_and_atomic() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::write(src.join("a/f"), b"x").unwrap();
        // tmpfs has no reflinks: refused, and nothing is left behind.
        let dst = t.path().join("dst");
        match clone_tree(&src, &dst) {
            Ok(()) => assert_eq!(std::fs::read(dst.join("a/f")).unwrap(), b"x"),
            Err(e) => {
                assert!(e.to_string().contains("reflinks"), "{e}");
                assert!(!dst.exists());
            }
        }
        let p = t.path().join("s.json");
        write_atomic(&p, b"{}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{}");
    }
}
