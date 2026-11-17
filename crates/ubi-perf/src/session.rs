//! The session dir (SIGHT-HUD §3.7): `/dev/shm/quiver-<uid>/<session>/`, mode 0700 under a
//! 0700 per-user parent, holding `layer-<pid>.ring`, `mod-<surface>.ring` and `hud.image`.
//! quiverd creates it and passes it to the game as `QUIVER_PERF_DIR`.
//!
//! Everything in it can be written by the game process (same uid), so openers check names,
//! file types, owners and sizes, and the ring and image readers validate every byte.

#[cfg(unix)]
use std::fs;
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use crate::hud::{HudReader, HudWriter, hud_file_bytes, init_hud};
use crate::layout::{HUD_MAX_BUFFER_BYTES, RING_MAX_SLOTS, RING_MIN_SLOTS};
use crate::ring::{RingParams, RingReader, RingWriter, ring_bytes};
use crate::shm::Mapping;

/// The session dir, for the layer (Linux code in the game process).
pub const ENV_PERF_DIR: &str = "QUIVER_PERF_DIR";
/// The session id.
pub const ENV_SESSION: &str = "QUIVER_SESSION";
/// Where session dirs live, normally.
pub const SHM_ROOT: &str = "/dev/shm";
pub const HUD_IMAGE_FILE: &str = "hud.image";
const MAX_SESSION_ID: usize = 48;
const MAX_SURFACE_ID: usize = 32;

#[cfg(unix)]
fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// A session id (`s-4kq2`): 1–48 of `[a-z0-9_-]`, starting with a letter or digit.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(s: &str) -> Option<SessionId> {
        let ok = !s.is_empty()
            && s.len() <= MAX_SESSION_ID
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        ok.then(|| SessionId(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A mod surface id as it appears in ring file names and metric namespaces: 1–32 of
/// `[a-z0-9_-]`, starting with a letter or digit.
pub fn valid_surface_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SURFACE_ID
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// What a file in the session dir is, from its name. Anything else (temp files, junk the game
/// left) is ignored by discovery.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// `layer-<pid>.ring`: the Sight layer in process `pid` (Linux pid).
    Layer { pid: u32 },
    /// `mod-<surface>.ring` (connection 0) or `mod-<surface>.<conn>.ring`.
    Mod { surface: String, conn: u32 },
    /// `hud.image`.
    HudImage,
}

impl FileKind {
    pub fn file_name(&self) -> String {
        match self {
            FileKind::Layer { pid } => format!("layer-{pid}.ring"),
            FileKind::Mod { surface, conn: 0 } => format!("mod-{surface}.ring"),
            FileKind::Mod { surface, conn } => format!("mod-{surface}.{conn}.ring"),
            FileKind::HudImage => HUD_IMAGE_FILE.to_owned(),
        }
    }

    pub fn parse(name: &str) -> Option<FileKind> {
        if name == HUD_IMAGE_FILE {
            return Some(FileKind::HudImage);
        }
        let stem = name.strip_suffix(".ring")?;
        if let Some(pid) = stem.strip_prefix("layer-") {
            let canonical = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && !(pid.len() > 1 && pid.starts_with('0'));
            return canonical.then(|| pid.parse().ok().map(|pid| FileKind::Layer { pid })).flatten();
        }
        let rest = stem.strip_prefix("mod-")?;
        let (surface, conn) = match rest.split_once('.') {
            Some((s, c)) => {
                let canonical = !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) && !c.starts_with('0');
                (s, if canonical { c.parse().ok()? } else { return None })
            }
            None => (rest, 0),
        };
        valid_surface_id(surface).then(|| FileKind::Mod { surface: surface.to_owned(), conn })
    }

    /// Size bounds for opening this kind of file.
    fn size_bounds(&self) -> (usize, usize) {
        match self {
            FileKind::HudImage => (hud_file_bytes(4096), hud_file_bytes(HUD_MAX_BUFFER_BYTES)),
            _ => (ring_bytes(RING_MIN_SLOTS), ring_bytes(RING_MAX_SLOTS)),
        }
    }
}

/// `Z:\dev\shm\quiver-1000\s-4kq2\…`: an absolute Unix path as the game's Wine sees it
/// through Proton's `Z:` → `/` drive. `None` for relative or non-UTF-8 paths.
pub fn to_windows_path(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    s.starts_with('/').then(|| format!("Z:{}", s.replace('/', "\\")))
}

/// `\\?\unix\dev\shm\…`: Wine's name for a Unix path when `Z:` is missing.
pub fn to_wine_unix_path(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    s.starts_with('/').then(|| format!("\\\\?\\unix{}", s.replace('/', "\\")))
}

/// Creates a ring file at `path` and attaches a writer. On Unix the file is built under a
/// temporary name and renamed into place, so a reader (or quiverd's inotify) never sees a
/// half-made ring; under Wine (the bridge) it is created in place, magic last.
pub fn create_ring_file(path: &Path, p: &RingParams) -> io::Result<RingWriter<Mapping>> {
    #[cfg(unix)]
    {
        let tmp = temp_name(path)?;
        let w = Mapping::create(&tmp, p.file_bytes()).and_then(|m| Ok(RingWriter::create(m, p)?));
        let w = w.and_then(|w| fs::rename(&tmp, path).map(|_| w));
        if w.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        w
    }
    #[cfg(not(unix))]
    {
        let m = Mapping::create(path, p.file_bytes())?;
        Ok(RingWriter::create(m, p)?)
    }
}

/// Opens a ring read-only.
pub fn open_ring_reader(path: &Path) -> io::Result<RingReader<Mapping>> {
    let m = Mapping::open_read(path, ring_bytes(RING_MIN_SLOTS), ring_bytes(RING_MAX_SLOTS))?;
    Ok(RingReader::new(m)?)
}

/// Attaches a writer to an existing ring (the SDK side of a bridge-made ring, or a restart).
pub fn open_ring_writer(path: &Path) -> io::Result<RingWriter<Mapping>> {
    let m = Mapping::open_write(path, ring_bytes(RING_MIN_SLOTS), ring_bytes(RING_MAX_SLOTS))?;
    Ok(RingWriter::new(m)?)
}

/// Creates `hud.image` at `path` with buffers of `buffer_bytes` (quiverd).
pub fn create_hud_file(path: &Path, buffer_bytes: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let tmp = temp_name(path)?;
        let r = Mapping::create(&tmp, hud_file_bytes(buffer_bytes)).and_then(|m| Ok(init_hud(&m, buffer_bytes).map(|_| ())?));
        let r = r.and_then(|_| fs::rename(&tmp, path));
        if r.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        r
    }
    #[cfg(not(unix))]
    {
        let m = Mapping::create(path, hud_file_bytes(buffer_bytes))?;
        init_hud(&m, buffer_bytes)?;
        Ok(())
    }
}

pub fn open_hud_writer(path: &Path) -> io::Result<HudWriter<Mapping>> {
    let (lo, hi) = FileKind::HudImage.size_bounds();
    Ok(HudWriter::new(Mapping::open_write(path, lo, hi)?)?)
}

pub fn open_hud_reader(path: &Path) -> io::Result<HudReader<Mapping>> {
    let (lo, hi) = FileKind::HudImage.size_bounds();
    Ok(HudReader::new(Mapping::open_read(path, lo, hi)?)?)
}

#[cfg(unix)]
fn temp_name(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| invalid("no file name"))?;
    Ok(path.with_file_name(format!(".{name}.{}.{:x}.tmp", std::process::id(), crate::random_u64())))
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    /// `<shm_root>/quiver-<uid>`.
    pub fn user_dir(shm_root: &Path, uid: u32) -> PathBuf {
        shm_root.join(format!("quiver-{uid}"))
    }

    pub fn current_uid() -> u32 {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() }
    }

    /// Makes `path` a 0700 directory owned by us: creates it, or accepts an existing one that
    /// is a real directory (not a symlink) owned by us, tightening its mode if needed. A
    /// directory someone else owns (squatting the name in world-writable /dev/shm) is refused.
    fn ensure_private_dir(path: &Path) -> io::Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        check_private_dir(path, true)
    }

    fn check_private_dir(path: &Path, fix_mode: bool) -> io::Result<()> {
        let md = fs::symlink_metadata(path)?;
        if !md.file_type().is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a directory", path.display())));
        }
        if md.uid() != current_uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is owned by uid {}", path.display(), md.uid())));
        }
        if md.mode() & 0o777 != 0o700 {
            if !fix_mode {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} has mode {:o}", path.display(), md.mode() & 0o777)));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// A session's directory.
    #[derive(Clone, Debug)]
    pub struct SessionDir {
        path: PathBuf,
        id: SessionId,
    }

    impl SessionDir {
        /// Creates (or reuses, after a quiverd restart) `<shm_root>/quiver-<uid>/<id>/`.
        pub fn create(shm_root: &Path, id: &SessionId) -> io::Result<SessionDir> {
            let parent = user_dir(shm_root, current_uid());
            ensure_private_dir(&parent)?;
            let path = parent.join(id.as_str());
            ensure_private_dir(&path)?;
            Ok(SessionDir { path, id: id.clone() })
        }

        /// Opens an existing session dir, checking it is ours and private.
        pub fn open(path: &Path) -> io::Result<SessionDir> {
            if !path.is_absolute() {
                return Err(invalid("session dir must be absolute"));
            }
            let id = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(SessionId::parse)
                .ok_or_else(|| invalid(format!("{} is not a session dir", path.display())))?;
            check_private_dir(path, false)?;
            Ok(SessionDir { path: path.to_owned(), id })
        }

        /// `QUIVER_PERF_DIR`, if set (the layer's way in). `Ok(None)` when unset or empty.
        pub fn from_env() -> io::Result<Option<SessionDir>> {
            match std::env::var_os(ENV_PERF_DIR) {
                Some(p) if !p.is_empty() => Self::open(Path::new(&p)).map(Some),
                _ => Ok(None),
            }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn id(&self) -> &SessionId {
            &self.id
        }

        pub fn file(&self, kind: &FileKind) -> PathBuf {
            self.path.join(kind.file_name())
        }

        /// The dir as the SDK addresses it (`welcome.perf.session_dir_win`).
        pub fn windows_path(&self) -> Option<String> {
            to_windows_path(&self.path)
        }

        /// The environment a game launch gets (`QUIVER_SESSION`, `QUIVER_PERF_DIR`).
        pub fn env(&self) -> [(&'static str, String); 2] {
            [(ENV_SESSION, self.id.0.clone()), (ENV_PERF_DIR, self.path.to_string_lossy().into_owned())]
        }

        /// The files in it that are rings or the HUD image, by name. Discovery for quiverd
        /// (alongside inotify); temp files and anything unrecognized are skipped.
        pub fn list(&self) -> io::Result<Vec<(FileKind, PathBuf)>> {
            let mut out = Vec::new();
            for e in fs::read_dir(&self.path)? {
                let e = e?;
                let Some(kind) = e.file_name().to_str().and_then(FileKind::parse) else { continue };
                if e.file_type()?.is_file() {
                    out.push((kind, e.path()));
                }
            }
            out.sort_by(|a, b| a.1.cmp(&b.1));
            Ok(out)
        }

        /// The layer's ring for this process.
        pub fn create_layer_ring(&self, pid: u32, nonce: [u8; 16]) -> io::Result<RingWriter<Mapping>> {
            create_ring_file(&self.file(&FileKind::Layer { pid }), &RingParams::layer(pid, nonce))
        }

        pub fn create_ring(&self, kind: &FileKind, p: &RingParams) -> io::Result<RingWriter<Mapping>> {
            if *kind == FileKind::HudImage {
                return Err(invalid("hud.image is not a ring"));
            }
            create_ring_file(&self.file(kind), p)
        }

        pub fn open_ring(&self, kind: &FileKind) -> io::Result<RingReader<Mapping>> {
            if *kind == FileKind::HudImage {
                return Err(invalid("hud.image is not a ring"));
            }
            open_ring_reader(&self.file(kind))
        }

        pub fn create_hud_image(&self, buffer_bytes: u32) -> io::Result<PathBuf> {
            let p = self.file(&FileKind::HudImage);
            create_hud_file(&p, buffer_bytes)?;
            Ok(p)
        }

        pub fn open_hud_writer(&self) -> io::Result<HudWriter<Mapping>> {
            open_hud_writer(&self.file(&FileKind::HudImage))
        }

        pub fn open_hud_reader(&self) -> io::Result<HudReader<Mapping>> {
            open_hud_reader(&self.file(&FileKind::HudImage))
        }

        /// Removes the dir and everything in it (session end). Doesn't follow symlinks.
        pub fn remove(self) -> io::Result<()> {
            check_private_dir(&self.path, false)?;
            fs::remove_dir_all(&self.path)
        }
    }

    /// Removes session dirs under `<shm_root>/quiver-<uid>/` whose sessions aren't live
    /// (quiverd at start). Returns the ids removed. Names that aren't session ids are left.
    pub fn sweep_stale(shm_root: &Path, is_live: impl Fn(&SessionId) -> bool) -> io::Result<Vec<SessionId>> {
        let parent = user_dir(shm_root, current_uid());
        match check_private_dir(&parent, false) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
        let mut removed = Vec::new();
        for e in fs::read_dir(&parent)? {
            let e = e?;
            let Some(id) = e.file_name().to_str().and_then(SessionId::parse) else { continue };
            if is_live(&id) || !e.file_type()?.is_dir() {
                continue;
            }
            match fs::remove_dir_all(e.path()) {
                Ok(()) => removed.push(id),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        removed.sort();
        Ok(removed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::layout::*;
    use crate::shm::ShmRegion;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn shm() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("ubi-perf-test-").tempdir_in(SHM_ROOT).unwrap()
    }

    #[test]
    fn names() {
        for k in [
            FileKind::Layer { pid: 4242 },
            FileKind::Mod { surface: "aco-dlss".into(), conn: 0 },
            FileKind::Mod { surface: "aco-dlss".into(), conn: 3 },
            FileKind::HudImage,
        ] {
            assert_eq!(FileKind::parse(&k.file_name()), Some(k));
        }
        for bad in [
            "layer-.ring",
            "layer-01.ring",
            "layer-99999999999.ring",
            "layer-1.ring.tmp",
            ".layer-1.ring.1.2.tmp",
            "mod-.ring",
            "mod-Aco.ring",
            "mod-a/b.ring",
            "mod-a.0.ring",
            "mod-a.x.ring",
            "mod-a.1.2.ring",
            "mod-../x.ring",
            "hud.image2",
            "x.ring",
        ] {
            assert_eq!(FileKind::parse(bad), None, "{bad}");
        }
        assert!(SessionId::parse("s-4kq2").is_some());
        for bad in ["", "-s", "S-1", "a/b", "..", ".x", &"a".repeat(49)] {
            assert!(SessionId::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn windows_paths() {
        let p = Path::new("/dev/shm/quiver-1000/s-4kq2/mod-aco-dlss.ring");
        assert_eq!(to_windows_path(p).unwrap(), r"Z:\dev\shm\quiver-1000\s-4kq2\mod-aco-dlss.ring");
        assert_eq!(to_wine_unix_path(p).unwrap(), r"\\?\unix\dev\shm\quiver-1000\s-4kq2\mod-aco-dlss.ring");
        assert_eq!(to_windows_path(Path::new("rel")), None);
    }

    #[test]
    fn create_list_open_remove() {
        let root = shm();
        let id = SessionId::parse("s-test1").unwrap();
        let dir = SessionDir::create(root.path(), &id).unwrap();
        let parent = user_dir(root.path(), current_uid());
        assert_eq!(fs::metadata(&parent).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(dir.path()).unwrap().mode() & 0o777, 0o700);
        // Reuse is fine.
        SessionDir::create(root.path(), &id).unwrap();
        let w = dir.create_layer_ring(777, [1; 16]).unwrap();
        w.push(&Record::series(1, 5, 1.0));
        let ring_path = dir.file(&FileKind::Layer { pid: 777 });
        assert_eq!(fs::metadata(&ring_path).unwrap().mode() & 0o777, 0o600);
        dir.create_hud_image(8192).unwrap();
        fs::write(dir.path().join("junk.txt"), b"x").unwrap();
        fs::write(dir.path().join(".layer-1.ring.1.2.tmp"), b"x").unwrap();
        let kinds: Vec<_> = dir.list().unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![FileKind::HudImage, FileKind::Layer { pid: 777 }]);
        let mut r = dir.open_ring(&FileKind::Layer { pid: 777 }).unwrap();
        assert_eq!(r.info().writer_pid, 777);
        assert!(r.region().is_file_at(&ring_path));
        assert_eq!(r.poll(10, |_, rec| assert_eq!(rec.t, 5)).read, 1);
        dir.open_hud_writer().unwrap();
        dir.open_hud_reader().unwrap();
        // Read-only mapping.
        assert!(!r.region().writable());
        assert!(RingWriter::new(r.region()).is_err());
        // An existing ring name can't be created over by create_ring for a mod (O_EXCL on
        // the temp, rename replaces): the layer for a reused pid replaces its old ring.
        let w2 = dir.create_layer_ring(777, [2; 16]).unwrap();
        assert!(!r.region().is_file_at(&ring_path));
        drop(w2);
        let opened = SessionDir::open(dir.path()).unwrap();
        assert_eq!(opened.id(), &id);
        opened.remove().unwrap();
        assert!(!dir.path().exists());
    }

    #[test]
    fn refuses_bad_files_and_dirs() {
        let root = shm();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-bad").unwrap()).unwrap();
        // A symlink in place of a ring is not followed.
        let target = dir.path().join("target");
        fs::write(&target, vec![0u8; ring_bytes(16)]).unwrap();
        let link = dir.file(&FileKind::Layer { pid: 5 });
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_ring_reader(&link).is_err());
        // Wrong sizes, garbage, a directory.
        let small = dir.file(&FileKind::Layer { pid: 6 });
        fs::write(&small, [0u8; 100]).unwrap();
        assert!(open_ring_reader(&small).is_err());
        let huge = dir.file(&FileKind::Layer { pid: 7 });
        fs::File::create(&huge).unwrap().set_len(64 << 20).unwrap();
        assert!(open_ring_reader(&huge).is_err());
        let zeros = dir.file(&FileKind::Layer { pid: 8 });
        fs::write(&zeros, vec![0u8; ring_bytes(16)]).unwrap();
        assert_eq!(open_ring_reader(&zeros).err().unwrap().kind(), io::ErrorKind::WouldBlock);
        let garbage = dir.file(&FileKind::Layer { pid: 9 });
        fs::write(&garbage, (0..ring_bytes(16)).map(|_| fastrand::u8(..)).collect::<Vec<_>>()).unwrap();
        assert!(open_ring_reader(&garbage).is_err());
        fs::create_dir(dir.file(&FileKind::Layer { pid: 10 })).unwrap();
        assert!(open_ring_reader(&dir.file(&FileKind::Layer { pid: 10 })).is_err());
        // A session dir with a lax mode is refused when opened (not created) by the layer,
        // and fixed when quiverd creates it again.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SessionDir::open(dir.path()).is_err());
        SessionDir::create(root.path(), dir.id()).unwrap();
        assert!(SessionDir::open(dir.path()).is_ok());
        // Not a session dir name; relative.
        assert!(SessionDir::open(root.path()).is_err());
        assert!(SessionDir::open(Path::new("quiver-1000/s-bad")).is_err());
        // A file where the user dir should be.
        let root2 = shm();
        fs::write(user_dir(root2.path(), current_uid()), b"").unwrap();
        assert!(SessionDir::create(root2.path(), dir.id()).is_err());
        // A symlinked user dir.
        let root3 = shm();
        std::os::unix::fs::symlink(root.path(), user_dir(root3.path(), current_uid())).unwrap();
        assert!(SessionDir::create(root3.path(), dir.id()).is_err());
    }

    #[test]
    fn sweep() {
        let root = shm();
        for s in ["s-a", "s-b", "s-c"] {
            SessionDir::create(root.path(), &SessionId::parse(s).unwrap()).unwrap();
        }
        let parent = user_dir(root.path(), current_uid());
        fs::write(parent.join("NotASession"), b"").unwrap();
        let removed = sweep_stale(root.path(), |id| id.as_str() == "s-b").unwrap();
        assert_eq!(removed.iter().map(|s| s.as_str()).collect::<Vec<_>>(), ["s-a", "s-c"]);
        assert!(parent.join("s-b").exists() && parent.join("NotASession").exists());
        let empty = shm();
        assert!(sweep_stale(empty.path(), |_| false).unwrap().is_empty());
    }

    #[test]
    fn env() {
        let root = shm();
        let dir = SessionDir::create(root.path(), &SessionId::parse("s-env").unwrap()).unwrap();
        let env = dir.env();
        assert_eq!(env[0], (ENV_SESSION, "s-env".to_owned()));
        assert_eq!(PathBuf::from(&env[1].1), dir.path());
        assert!(dir.windows_path().unwrap().starts_with("Z:\\"));
        let _ = (RING_DEFAULT_SLOTS, PRODUCER_LAYER);
    }
}
