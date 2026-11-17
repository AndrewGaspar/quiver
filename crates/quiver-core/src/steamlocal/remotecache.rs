//! Steam Cloud state per app, from `userdata/<accountid>/<appid>/remotecache.vdf` (text
//! VDF), which the client keeps for every app with cloud files:
//!
//! ```text
//! "<appid>"
//! {
//!     "ChangeNumber"   "14"
//!     "ostype"         "0"
//!     "<file name, relative to its root>"
//!     {
//!         "root"              "0"
//!         "size"              "1024"
//!         "localtime"         "<unix secs>"
//!         "time"              "<unix secs>"
//!         "remotetime"        "<unix secs>"
//!         "sha"               "<hex SHA-1>"
//!         "syncstate"         "1"
//!         "persiststate"      "0"
//!         "platformstosync2"  "-1"
//!     }
//!     …
//! }
//! ```
//!
//! Observed on this machine (Nov 2026, 16 apps, 492 files): `syncstate` was always `1`
//! and `persiststate` always `0`, `localtime`/`time`/`remotetime` always set,
//! `platformstosync2` `-1` (all) or `1`. That `syncstate 1` means "in sync" and
//! `persiststate 0` means "persisted" is inferred from Steam showing those games as
//! synced, **not verified** against other values. `root` was `0` (the app's
//! `userdata/<accountid>/<appid>/remote/`, the Steam Cloud API folder; **verified**) or an
//! Auto-Cloud root id (`1`, `2`, `3`, `12` seen; the id → folder mapping is
//! **unverified**, it follows the app's `ufs` roots in appinfo).

use std::path::Path;

use crate::exports::textvdf::{self, Node};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudFile {
    /// The path relative to its root, as the client wrote it.
    pub name: String,
    pub root: u32,
    pub size: u64,
    /// Unix seconds.
    pub time: Option<i64>,
    pub remote_time: Option<i64>,
    pub sync_state: u32,
    pub persist_state: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudState {
    pub change_number: Option<u64>,
    pub files: Vec<CloudFile>,
}

impl CloudState {
    /// Bytes in all the files.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// `None` if the text isn't a remotecache.vdf.
pub fn parse(text: &str) -> Option<CloudState> {
    let root = textvdf::parse(text).ok()?;
    let (_, app) = root.entries().iter().find(|(k, v)| k.parse::<u32>().is_ok() && matches!(v, Node::Map(_)))?;
    let num = |n: &Node, k: &str| n.str(k).and_then(|v| v.trim().parse::<i64>().ok());
    let time = |n: &Node, k: &str| num(n, k).filter(|&t| t > 0);
    let unsigned = |n: &Node, k: &str| num(n, k).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let files = app
        .entries()
        .iter()
        .filter(|(_, v)| matches!(v, Node::Map(_)))
        .map(|(name, f)| CloudFile {
            name: name.clone(),
            root: unsigned(f, "root"),
            size: num(f, "size").and_then(|s| u64::try_from(s).ok()).unwrap_or(0),
            time: time(f, "time"),
            remote_time: time(f, "remotetime"),
            sync_state: unsigned(f, "syncstate"),
            persist_state: unsigned(f, "persiststate"),
        })
        .collect();
    let change_number = num(app, "ChangeNumber").and_then(|c| u64::try_from(c).ok());
    Some(CloudState { change_number, files })
}

/// An app's cloud state for one user (`None` without a remotecache.vdf).
pub fn read(steam_root: &Path, account_id: &str, app_id: u32) -> Option<CloudState> {
    if account_id.is_empty() || !account_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let path = steam_root.join("userdata").join(account_id).join(app_id.to_string()).join("remotecache.vdf");
    parse(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#""620"
{
	"ChangeNumber"		"14"
	"ostype"		"0"
	"save/slot1.sav"
	{
		"root"		"0"
		"size"		"1024"
		"localtime"		"1700000000"
		"time"		"1700000001"
		"remotetime"		"1700000002"
		"sha"		"0000000000000000000000000000000000000000"
		"syncstate"		"1"
		"persiststate"		"0"
		"platformstosync2"		"-1"
	}
	"config.cfg"
	{
		"root"		"2"
		"size"		"300"
		"time"		"0"
		"syncstate"		"3"
		"persiststate"		"1"
	}
}
"#;

    #[test]
    fn two_files() {
        let c = parse(TEXT).unwrap();
        assert_eq!(c.change_number, Some(14));
        assert_eq!(c.files.len(), 2);
        assert_eq!(
            c.files[0],
            CloudFile {
                name: "save/slot1.sav".into(),
                root: 0,
                size: 1024,
                time: Some(1700000001),
                remote_time: Some(1700000002),
                sync_state: 1,
                persist_state: 0,
            }
        );
        let f = &c.files[1];
        assert_eq!((f.root, f.time, f.remote_time, f.sync_state, f.persist_state), (2, None, None, 3, 1));
        assert_eq!(c.total_size(), 1324);
    }

    #[test]
    fn not_a_remotecache() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("\"620\" {"), None);
        assert_eq!(parse(r#""users" { "a" "b" }"#), None);
        assert_eq!(parse(r#""620" { "ChangeNumber" "-1" }"#), Some(CloudState::default()));
        assert_eq!(read(Path::new("/nonexistent"), "1", 620), None);
        assert_eq!(read(Path::new("/"), "..", 620), None);
    }
}
