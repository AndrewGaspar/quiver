//! [`Secret`] lives in `quiver-api`, so requests that carry a password or a one-shot code can
//! hold one; the store crates use it from here as before.
//!
//! [`use_scratch_store_from_env`] is for scratch daemons in tests: with
//! [`SCRATCH_ENV`] set, every keyring entry (Ubisoft's `session:<userId>`, Epic's
//! `epic:<account id>`) goes to a 0600 file in that directory instead of the desktop's
//! Secret Service, so a test daemon can never read or write the person's real keyring, and
//! the test can check what each daemon stored.

use std::any::Any;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use quiver_api::secret::Secret;

/// The environment variable naming a scratch daemon's secrets directory.
pub const SCRATCH_ENV: &str = "QUIVER_SCRATCH_SECRETS";

/// With [`SCRATCH_ENV`] set (to an absolute path), send every keyring entry of this
/// process to files there. Returns the directory when it did. Call it first thing, before
/// any entry is opened.
pub fn use_scratch_store_from_env() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(SCRATCH_ENV)?);
    if !dir.is_absolute() {
        tracing::warn!("{SCRATCH_ENV} must be an absolute path; ignored");
        return None;
    }
    keyring::set_default_credential_builder(Box::new(ScratchStore { dir: dir.clone() }));
    tracing::warn!("secrets: a scratch store in {} ({SCRATCH_ENV}), not the keyring", dir.display());
    Some(dir)
}

/// The file a scratch store keeps `service`/`entry` in (for tests that check it).
pub fn scratch_file(dir: &Path, service: &str, entry: &str) -> PathBuf {
    dir.join(format!("{}.secret", hex::encode(format!("{service}\0{entry}"))))
}

struct ScratchStore {
    dir: PathBuf,
}

impl keyring::credential::CredentialBuilderApi for ScratchStore {
    fn build(&self, _target: Option<&str>, service: &str, user: &str) -> keyring::Result<Box<keyring::credential::Credential>> {
        Ok(Box::new(ScratchEntry { dir: self.dir.clone(), path: scratch_file(&self.dir, service, user) }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct ScratchEntry {
    dir: PathBuf,
    path: PathBuf,
}

fn failed(e: std::io::Error) -> keyring::Error {
    keyring::Error::PlatformFailure(Box::new(e))
}

impl keyring::credential::CredentialApi for ScratchEntry {
    fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir).map_err(failed)?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp).map_err(failed)?;
        f.write_all(secret).map_err(failed)?;
        std::fs::rename(&tmp, &self.path).map_err(failed)
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        match std::fs::read(&self.path) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(keyring::Error::NoEntry),
            Err(e) => Err(failed(e)),
        }
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(keyring::Error::NoEntry),
            Err(e) => Err(failed(e)),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::credential::CredentialBuilderApi;

    #[test]
    fn scratch_entries_are_private_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ScratchStore { dir: dir.clone() };
        let e = store.build(None, "quiver", "epic:abc").unwrap();
        assert!(matches!(e.get_password(), Err(keyring::Error::NoEntry)));
        e.set_password("{\"refresh_token\":\"x\"}").unwrap();
        assert_eq!(e.get_password().unwrap(), "{\"refresh_token\":\"x\"}");
        let f = scratch_file(&dir, "quiver", "epic:abc");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        // Another entry is another file.
        assert!(matches!(store.build(None, "quiver", "epic:abd").unwrap().get_password(), Err(keyring::Error::NoEntry)));
        e.delete_credential().unwrap();
        assert!(matches!(e.delete_credential(), Err(keyring::Error::NoEntry)));
        assert!(!f.exists());
    }
}
