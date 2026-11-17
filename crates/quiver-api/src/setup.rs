//! First-run setup: the record of it, storage paths, existing installs found elsewhere
//! (Ubisoft Connect under Lutris or another Wine prefix), and the app-level choice about
//! recommended fixes on unmodded games.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The first-run record (`config.json` → `first_run`). Front ends show their welcome flow
/// while there is none, so every front end agrees on whether setup happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirstRun {
    pub at: DateTime<Utc>,
    /// Who finished it (gui, cli, …).
    pub source: String,
    /// Closed with "Skip setup" rather than finished.
    #[serde(default)]
    pub skipped: bool,
    /// Offer "Add to Steam" (ticked) when an install finishes.
    #[serde(default = "yes")]
    pub steam_on_install: bool,
}

fn yes() -> bool {
    true
}

/// How `Request::Setup` finishes setup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FinishSetup {
    #[serde(default)]
    pub skipped: bool,
    #[serde(default = "yes")]
    pub steam_on_install: bool,
}

/// Reply to `Request::Setup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupState {
    pub first_run: Option<FirstRun>,
    /// Where games install, and where their Wine prefixes go.
    pub library: PathBuf,
    pub prefixes: PathBuf,
    /// Free and total bytes on each one's filesystem (`None`: couldn't tell).
    #[serde(default)]
    pub library_free: Option<u64>,
    #[serde(default)]
    pub library_total: Option<u64>,
    #[serde(default)]
    pub prefixes_free: Option<u64>,
    #[serde(default)]
    pub prefixes_total: Option<u64>,
    /// Games Quiver has installed or imported.
    #[serde(default)]
    pub installed: usize,
    /// Paths can only change before anything is installed, queued or importing.
    #[serde(default)]
    pub paths_locked: bool,
    /// The app-level choice about recommended fixes on unmodded games (`None`: not
    /// decided, ask again).
    #[serde(default)]
    pub fixes_on_vanilla: Option<bool>,
    /// Set when the reply changed the paths: quiverd restarts to use them (clients
    /// reconnect on their own).
    #[serde(default)]
    pub restarting: bool,
}

/// Where an existing install was found.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallSource {
    /// A Wine prefix a Lutris game config points at.
    Lutris,
    /// A Wine or Proton prefix under `~/Games`.
    Wine,
    /// A Steam (Proton) prefix under `steamapps/compatdata`.
    Steam,
    #[serde(other)]
    Other,
}

/// How a found install was matched to a product.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// Connect's own record in the prefix's registry (`Launcher\Installs\<id>`).
    Registry,
    /// The folder's name against the owned games' names.
    Name,
    #[serde(other)]
    Unknown,
}

/// An existing Ubisoft Connect install found by `Request::ScanInstalls`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FoundInstall {
    /// The game's folder (what `Request::Import` takes).
    pub path: PathBuf,
    /// The Wine prefix it is in.
    pub prefix: PathBuf,
    pub source: InstallSource,
    /// "Lutris · Ubisoft Connect", "Wine · ~/Games/ubisoft-connect", "Connect inside Steam · Far Cry 5"…
    pub source_label: String,
    /// Lutris games (names) that launch through this prefix.
    #[serde(default)]
    pub lutris_games: Vec<String>,
    pub product_id: Option<u32>,
    #[serde(default)]
    pub matched_by: Option<MatchedBy>,
    /// The owned game's name if matched, else the folder's.
    pub name: String,
    /// Install language Connect recorded.
    #[serde(default)]
    pub language: Option<String>,
    /// Version from the install's manifest.
    #[serde(default)]
    pub version: Option<String>,
    /// Bytes and files the manifest lists (what an import copies).
    pub size_bytes: u64,
    pub files: usize,
    /// `None`: ownership unknown (not signed in, or the library couldn't be read).
    #[serde(default)]
    pub owned: Option<bool>,
    /// Quiver already has this game installed.
    #[serde(default)]
    pub installed: bool,
    /// Can be imported now (matched, owned, not installed, has a manifest).
    pub importable: bool,
    /// Why not, when not.
    #[serde(default)]
    pub note: Option<String>,
}

/// The three answers front ends offer for `fixes_on_vanilla`; [`FixesChoice::value`] is what
/// goes into `ModsRequest::Prefs` ('later' leaves it undecided, so nothing is sent).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixesChoice {
    Yes,
    No,
    /// Not decided: ask again.
    Later,
}

impl FixesChoice {
    pub fn value(self) -> Option<bool> {
        match self {
            Self::Yes => Some(true),
            Self::No => Some(false),
            Self::Later => None,
        }
    }

    pub fn of(v: Option<bool>) -> Self {
        match v {
            Some(true) => Self::Yes,
            Some(false) => Self::No,
            None => Self::Later,
        }
    }
}

impl std::str::FromStr for FixesChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "yes" | "y" | "on" | "true" => Ok(Self::Yes),
            "no" | "n" | "off" | "false" => Ok(Self::No),
            "later" | "l" | "ask" => Ok(Self::Later),
            _ => Err(format!("expected yes, no or later, got '{s}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire() {
        // Older and newer daemons: minimal objects decode.
        let s: SetupState = serde_json::from_str(r#"{"first_run":null,"library":"/g","prefixes":"/p"}"#).unwrap();
        assert!(!s.paths_locked && s.fixes_on_vanilla.is_none() && !s.restarting);
        let f: FirstRun = serde_json::from_str(r#"{"at":"2026-11-16T00:00:00Z","source":"gui"}"#).unwrap();
        assert!(f.steam_on_install && !f.skipped);
        let i: FoundInstall = serde_json::from_str(
            r#"{"path":"/a","prefix":"/p","source":"bottles","source_label":"x","product_id":null,"name":"x",
                "size_bytes":1,"files":1,"importable":false,"matched_by":"magic"}"#,
        )
        .unwrap();
        assert_eq!(i.source, InstallSource::Other);
        assert_eq!(i.matched_by, Some(MatchedBy::Unknown));
        assert_eq!("later".parse::<FixesChoice>().unwrap().value(), None);
        assert_eq!(FixesChoice::of(Some(false)), FixesChoice::No);
        assert_eq!(serde_json::to_value(FixesChoice::Yes).unwrap(), "yes");
    }
}
