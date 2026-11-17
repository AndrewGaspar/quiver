//! Writing files that hold credentials (Steam's `localconfig.vdf` backups with app and
//! offline tickets, CDN tokens and the cloud key; launch contexts with Ubisoft tickets and
//! CD keys). They are only ever readable by the user: written to a fresh `0600` file next to
//! the target and renamed over it, so an existing file with looser permissions is replaced
//! rather than written through, and no reader sees a partial file.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

/// Mode of every file [`write_private`] creates.
pub const PRIVATE_MODE: u32 = 0o600;

/// Write `bytes` to `path` as a `0600` file, replacing whatever is there (including a
/// symlink, which is replaced, not followed). The parent directory must exist.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().with_context(|| format!("{} has no file name", path.display()))?;
    let tmp = dir.join(format!(".{}.quiver-private-{}", name.to_string_lossy(), std::process::id()));
    // A leftover from a crashed write by a process that had this pid.
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_MODE)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        // The umask can only narrow the mode; set it exactly anyway.
        f.set_permissions(std::fs::Permissions::from_mode(PRIVATE_MODE))?;
        f.write_all(bytes).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_file_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.vdf.quiver-backup");
        write_private(&p, b"secret").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"secret");
        assert_eq!(mode(&p), 0o600);
        // Only the target is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn existing_0644_file_is_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.vdf.quiver-backup");
        std::fs::write(&p, b"old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(mode(&p), 0o600);
    }

    #[test]
    fn symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, b"untouched").unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o644)).unwrap();
        let p = dir.path().join("a.vdf.quiver-backup");
        std::os::unix::fs::symlink(&elsewhere, &p).unwrap();
        write_private(&p, b"secret").unwrap();
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&p).unwrap().file_type().is_file());
        assert_eq!(mode(&p), 0o600);
    }

    #[test]
    fn missing_dir_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_private(&dir.path().join("nope/a"), b"x").is_err());
    }
}
