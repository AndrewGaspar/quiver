//! Which OAuth client Quiver signs in to Epic as (design/STORE-EPIC.md §4.5, steer 2).
//!
//! Epic has no OAuth client for third-party launchers. Like legendary, Heroic and egs-api,
//! Quiver presents the Epic Games Launcher's public client id and secret, user agent and
//! build label (decided 2026-11-17: steer 2 (a), documented in the README). The values are
//! compiled in, and `<data>/epic-client.json` overrides any of them, so a change on Epic's
//! side needs no new build:
//!
//! ```json
//! { "user_agent": "UELauncher/…", "label": "Live" }
//! ```
//!
//! These are public values that ship inside EGL; they are not anyone's credentials.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// EGL's OAuth client (`launcherAppClient2`).
pub const EGL_CLIENT_ID: &str = "34a02cf8f4414e29b15921876da36f9a";
pub const EGL_CLIENT_SECRET: &str = "daafbccc737745039dffe53d94fc76cf";
/// The newest EGL user agent seen in a maintained client (egs-api 0.14, Feb 2026).
pub const EGL_USER_AGENT: &str = "UELauncher/17.0.1-37584233+++Portal+Release-Live Windows/10.0.19043.1.0.64bit";

/// The override file, under Quiver's data directory.
pub const OVERRIDE_FILE: &str = "epic-client.json";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: String,
    pub user_agent: String,
    /// The build label the launcher asks assets for (`Live`).
    pub label: String,
    /// The platform builds are for (`Windows`).
    pub platform: String,
    /// Catalog country and locale (titles, descriptions).
    pub country: String,
    pub locale: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            client_id: EGL_CLIENT_ID.into(),
            client_secret: EGL_CLIENT_SECRET.into(),
            user_agent: EGL_USER_AGENT.into(),
            label: "Live".into(),
            platform: "Windows".into(),
            country: "US".into(),
            locale: "en-US".into(),
        }
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<not printed>")
            .field("user_agent", &self.user_agent)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl ClientConfig {
    /// The compiled-in values with `<data>/epic-client.json` laid over them. A file that
    /// doesn't parse is reported and ignored. Returns the file used, if any.
    pub fn load(data_dir: &Path) -> (ClientConfig, Option<PathBuf>) {
        let path = data_dir.join(OVERRIDE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<ClientConfig>(&text) {
                Ok(c) => {
                    tracing::info!("Epic client config from {}", path.display());
                    (c, Some(path))
                }
                Err(e) => {
                    tracing::warn!("{}: {e}; using the built-in Epic client config", path.display());
                    (ClientConfig::default(), None)
                }
            },
            Err(_) => (ClientConfig::default(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_file_replaces_only_what_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(ClientConfig::load(tmp.path()), (ClientConfig::default(), None));
        std::fs::write(tmp.path().join(OVERRIDE_FILE), r#"{"user_agent":"UELauncher/99.0","later":1}"#).unwrap();
        let (c, from) = ClientConfig::load(tmp.path());
        assert_eq!((c.user_agent.as_str(), c.client_id.as_str(), c.label.as_str()), ("UELauncher/99.0", EGL_CLIENT_ID, "Live"));
        assert!(from.is_some());
        assert!(!format!("{c:?}").contains(EGL_CLIENT_SECRET));
        std::fs::write(tmp.path().join(OVERRIDE_FILE), "not json").unwrap();
        assert_eq!(ClientConfig::load(tmp.path()).0, ClientConfig::default());
    }
}
