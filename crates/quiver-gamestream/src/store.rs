//! [`FileClientStore`]: paired Moonlight clients in a JSON file the caller names.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::host::{ClientStore, PairedClient};
use crate::identity::write_private;

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    clients: Vec<PairedClient>,
}

/// Paired clients in one JSON file (`{"clients": [...]}`, 0600), rewritten atomically on
/// every change.
pub struct FileClientStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileClientStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_owned(), lock: Mutex::new(()) }
    }

    fn read(&self) -> Result<File> {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parse {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    fn write(&self, f: &File) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(&self.path, &serde_json::to_vec_pretty(f)?)
    }
}

impl ClientStore for FileClientStore {
    fn list(&self) -> Result<Vec<PairedClient>> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read()?.clients)
    }

    fn add(&self, client: PairedClient) -> Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = self.read()?;
        f.clients.retain(|c| c.fingerprint != client.fingerprint);
        f.clients.push(client);
        self.write(&f)
    }

    fn remove(&self, fingerprint: &str) -> Result<bool> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = self.read()?;
        let before = f.clients.len();
        f.clients.retain(|c| c.fingerprint != fingerprint);
        let removed = f.clients.len() != before;
        if removed {
            self.write(&f)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn client(fp: &str) -> PairedClient {
        PairedClient { name: "tv".into(), address: "10.0.0.9".into(), fingerprint: fp.into(), cert_pem: "pem".into(), paired_at: 1 }
    }

    #[test]
    fn add_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/moonlight-clients.json");
        let s = FileClientStore::new(&path);
        assert!(s.list().unwrap().is_empty());
        s.add(client("aa")).unwrap();
        s.add(client("bb")).unwrap();
        s.add(PairedClient { name: "phone".into(), ..client("aa") }).unwrap();
        let l = s.list().unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(s.find("aa").unwrap().unwrap().name, "phone");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(s.remove("aa").unwrap());
        assert!(!s.remove("aa").unwrap());
        assert_eq!(FileClientStore::new(&path).list().unwrap(), vec![client("bb")]);
    }
}
