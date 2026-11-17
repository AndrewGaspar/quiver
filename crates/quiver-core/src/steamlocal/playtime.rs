//! Play time the client recorded, from `userdata/<accountid>/config/localconfig.vdf`
//! (text VDF):
//!
//! ```text
//! "UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" {
//!     "<appid>" {
//!         "LastPlayed"    "<unix secs>"
//!         "Playtime"      "<minutes>"
//!         "Playtime2wks"  "<minutes>"     // only for apps played in the last two weeks
//!         …
//!     }
//! } } } } }
//! ```
//!
//! Keys are matched case-insensitively ([`crate::exports::textvdf::Node::get`]); this
//! machine writes them as above. An app appears here once the client has any per-app
//! setting for it, so entries without any of the three keys are left out.
//!
//! **This file holds the user's login tickets and tokens** (`apptickets`,
//! `Offline/Ticket`, `CloudKey`, depot CDN tokens, `nettickets`, streaming device keys;
//! see `design/STORE-STEAM.md` §4). This module reads only the three numbers above per
//! app: nothing else is returned, logged or kept, and a parse error yields an empty map
//! rather than an error that could quote the file.

use std::collections::HashMap;
use std::path::Path;

use crate::exports::textvdf::{self, Node};

const APPS: [&str; 5] = ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppPlaytime {
    /// Total, in minutes.
    pub minutes: u64,
    /// The last two weeks, in minutes.
    pub two_weeks_minutes: u64,
    /// Unix seconds.
    pub last_played: Option<i64>,
}

/// Play time per app id from a `localconfig.vdf` text (empty if it doesn't parse).
pub fn parse(text: &str) -> HashMap<u32, AppPlaytime> {
    let Ok(root) = textvdf::parse(text) else { return HashMap::new() };
    let Some(apps) = APPS.iter().try_fold(&root, |n, k| n.get(k)) else { return HashMap::new() };
    apps.entries()
        .iter()
        .filter_map(|(id, app)| {
            let id: u32 = id.parse().ok()?;
            let Node::Map(_) = app else { return None };
            let num = |k: &str| app.str(k).and_then(|v| v.trim().parse::<i64>().ok());
            let (total, two_weeks, last) = (num("Playtime"), num("Playtime2wks"), num("LastPlayed"));
            if total.is_none() && two_weeks.is_none() && last.is_none() {
                return None;
            }
            let minutes = |m: Option<i64>| m.and_then(|m| u64::try_from(m).ok()).unwrap_or(0);
            Some((
                id,
                AppPlaytime {
                    minutes: minutes(total),
                    two_weeks_minutes: minutes(two_weeks),
                    last_played: last.filter(|&t| t > 0),
                },
            ))
        })
        .collect()
}

/// One user's play time (`account_id` is the `userdata/` directory name; empty when the
/// file is missing or unreadable).
pub fn read(steam_root: &Path, account_id: &str) -> HashMap<u32, AppPlaytime> {
    if account_id.is_empty() || !account_id.bytes().all(|b| b.is_ascii_digit()) {
        return HashMap::new();
    }
    let path = steam_root.join("userdata").join(account_id).join("config/localconfig.vdf");
    std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#""UserLocalConfigStore"
{
	"Software"
	{
		"valve"
		{
			"Steam"
			{
				"Apps"
				{
					"292030"
					{
						"LastPlayed"		"1700000000"
						"Playtime2wks"		"95"
						"Playtime"		"1234"
						"cloud"
						{
							"last_sync_state"		"synchronized"
						}
					}
					"620"
					{
						"lastplayed"		"1600000000"
						"PLAYTIME"		"60"
					}
					"440"
					{
						"Playtime"		"7"
					}
					"570"
					{
						"LaunchOptions"		"-novid"
					}
					"730"
					{
						"LastPlayed"		"0"
						"Playtime"		"-3"
					}
					"notanid"
					{
						"Playtime"		"1"
					}
				}
			}
		}
	}
	"apptickets"
	{
		"292030"		"not-a-real-ticket"
	}
}
"#;

    #[test]
    fn mixed_case_and_missing_keys() {
        let p = parse(TEXT);
        assert_eq!(p.len(), 4);
        assert_eq!(p[&292030], AppPlaytime { minutes: 1234, two_weeks_minutes: 95, last_played: Some(1700000000) });
        assert_eq!(p[&620], AppPlaytime { minutes: 60, two_weeks_minutes: 0, last_played: Some(1600000000) });
        assert_eq!(p[&440], AppPlaytime { minutes: 7, two_weeks_minutes: 0, last_played: None });
        assert_eq!(p[&730], AppPlaytime::default());
        assert!(!p.contains_key(&570));
    }

    #[test]
    fn bad_input_is_empty() {
        assert!(parse("").is_empty());
        assert!(parse("\"UserLocalConfigStore\" {").is_empty());
        assert!(parse(r#""Other" { "apps" { "1" { "Playtime" "5" } } }"#).is_empty());
        assert!(read(Path::new("/nonexistent"), "1").is_empty());
        assert!(read(Path::new("/"), "../etc").is_empty());
    }
}
