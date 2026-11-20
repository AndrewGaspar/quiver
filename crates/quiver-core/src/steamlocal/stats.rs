//! Achievements from the client's stats cache in `appcache/stats/`. Both files are binary
//! KeyValues with inline keys (see [`crate::exports::vdf`]); the client writes them for
//! apps it has fetched stats for, so a game never started on this machine usually has
//! neither.
//!
//! `UserGameStatsSchema_<appid>.bin`, the definitions (the same for every user):
//!
//! ```text
//! "<appid>" {
//!     "gamename" "…"   "version" 12
//!     "stats" {
//!         "<stat id>" {
//!             "type" "ACHIEVEMENTS"        // or "4", or int 4; often also "type_int" 4
//!             "id" "<stat id>"             // sometimes
//!             "bits" {
//!                 "<n>" {
//!                     "name" "API_NAME"
//!                     "bit" <n>            // sometimes; else the key is the bit
//!                     "display" {
//!                         "name" { "english" "…" "german" "…" "token" "…" }   // or a plain string
//!                         "desc" { … }                                        // likewise
//!                         "hidden" 0       // int, or the string "0"/"1"
//!                         "icon" "<sha1>.jpg"   "icon_gray" "<sha1>.jpg"
//!                     }
//!                     "progress" { … }     // optional
//!                 }
//!             }
//!         }
//!         "<stat id>" { "type" "INT" … }   // ordinary stats, ignored here
//!     }
//! }
//! ```
//!
//! `UserGameStats_<accountid>_<appid>.bin`, one user's state:
//!
//! ```text
//! "cache" {
//!     "crc" …   "PendingChanges" 0
//!     "<stat id>" {
//!         "data" <int32>                       // achievement stats: bit n set = unlocked
//!         "AchievementTimes" { "<n>" <unix secs> }
//!     }
//! }
//! ```
//!
//! The bit is the truth. A time without its bit (seen 3 times on this machine, likely a
//! reset achievement) is ignored, and a set bit without a time (8 of 1223 unlocked here)
//! is unlocked at an unknown time. `data` often has bits set that the schema doesn't
//! define; those are ignored. Icons are the file names the
//! client stores, served from
//! `https://cdn.steamstatic.com/steamcommunity/public/images/apps/<appid>/<icon>`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use super::kv::{self, Kv, key_str};

/// One achievement, joined with the user's state.
#[derive(Debug, Clone, PartialEq)]
pub struct Achievement {
    /// The developer's API name (`ACH_WIN_ONE_GAME`).
    pub api_name: String,
    /// English, else the first language in the file, else the API name.
    pub name: String,
    pub description: String,
    pub hidden: bool,
    /// The file name as stored (`<sha1>.jpg`).
    pub icon: Option<String>,
    pub icon_gray: Option<String>,
    /// The user's bit is set.
    pub unlocked: bool,
    /// When it was unlocked, unix seconds; `None` when locked or when the client kept no
    /// time for it.
    pub unlocked_at: Option<i64>,
    /// A locked one's progress, (current, max), when the schema ties it to a stat and the
    /// user's file has the stat.
    pub progress: Option<(f64, f64)>,
}

/// An achievement's `progress` block: `statvalue` of a stat, from `min` to `max`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressDef {
    /// The stat's name, and its id once found among the schema's stats.
    pub stat: String,
    pub stat_id: Option<u32>,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Achievements {
    pub total: usize,
    pub unlocked: usize,
    /// In schema order.
    pub list: Vec<Achievement>,
}

/// An achievement definition from the schema, with where its bit lives.
#[derive(Debug, Clone, PartialEq)]
pub struct AchievementDef {
    pub stat_id: u32,
    pub bit: u32,
    pub api_name: String,
    pub name: String,
    pub description: String,
    pub hidden: bool,
    pub icon: Option<String>,
    pub icon_gray: Option<String>,
    pub progress: Option<ProgressDef>,
}

/// One user's achievement bits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserStats {
    /// Stat id -> its `data` bit field.
    pub data: HashMap<u32, u32>,
    /// Stat id -> its `data` as a number (ordinary INT and FLOAT stats, for progress).
    pub values: HashMap<u32, f64>,
    /// (stat id, bit) -> unlock time.
    pub times: HashMap<(u32, u32), i64>,
}

impl UserStats {
    fn unlocked(&self, stat_id: u32, bit: u32) -> bool {
        bit < 32 && self.data.get(&stat_id).is_some_and(|d| d >> bit & 1 == 1)
    }
}

/// A display string: `{english, …, token}` or a plain string.
fn localized(node: Option<&Kv>) -> Option<String> {
    let node = node?;
    if let Some(s) = node.text() {
        return Some(s.into_owned());
    }
    if let Some(s) = node.str_at("english") {
        return Some(s.into_owned());
    }
    node.entries()
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(b"token"))
        .find_map(|(_, v)| v.text().map(|s| s.into_owned()))
}

fn is_achievement_stat(stat: &Kv) -> bool {
    let ty = stat.get("type");
    stat.int_at("type_int") == Some(4)
        || ty.and_then(Kv::int) == Some(4)
        || ty.and_then(Kv::text).is_some_and(|t| t.eq_ignore_ascii_case("ACHIEVEMENTS"))
}

/// The achievement definitions in a schema file (empty when the app has none).
pub fn parse_schema(buf: &[u8], app_id: u32) -> Result<Vec<AchievementDef>> {
    let root = kv::parse(buf).context("achievement schema")?;
    // The root holds one map keyed by the app id.
    let Some(app) = root.get(&app_id.to_string()).or_else(|| root.entries().first().map(|(_, v)| v)) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let number = |k: Option<&Kv>| k.and_then(Kv::text).and_then(|t| t.trim().parse::<f64>().ok());
    // Ordinary stats by name, for achievements whose progress is one of them.
    let mut stat_ids: HashMap<String, u32> = HashMap::new();
    for (sid, stat) in app.get("stats").map(Kv::entries).unwrap_or_default() {
        if !is_achievement_stat(stat)
            && let (Ok(id), Some(name)) = (key_str(sid).parse::<u32>(), stat.str_at("name"))
        {
            stat_ids.insert(name.into_owned(), id);
        }
    }
    for (sid, stat) in app.get("stats").map(Kv::entries).unwrap_or_default() {
        if !is_achievement_stat(stat) {
            continue;
        }
        let stat_id = key_str(sid).parse().ok().or_else(|| stat.int_at("id").and_then(|i| u32::try_from(i).ok()));
        let Some(stat_id) = stat_id else { continue };
        for (key, def) in stat.get("bits").map(Kv::entries).unwrap_or_default() {
            let bit = def.int_at("bit").and_then(|b| u32::try_from(b).ok()).or_else(|| key_str(key).parse().ok());
            let (Some(bit), Some(api_name)) = (bit, def.str_at("name")) else { continue };
            let display = def.get("display");
            let field = |k: &str| display.and_then(|d| d.str_at(k)).map(|s| s.into_owned()).filter(|s| !s.is_empty());
            let progress = def.get("progress").and_then(|p| {
                let v = p.get("value")?;
                if !v.str_at("operation").is_some_and(|o| o.eq_ignore_ascii_case("statvalue")) {
                    return None;
                }
                let stat = v.str_at("operand1")?.into_owned();
                let max = number(p.get("max_val"))?;
                let min = number(p.get("min_val")).unwrap_or(0.0);
                (max > min).then(|| ProgressDef { stat_id: stat_ids.get(&stat).copied(), stat, min, max })
            });
            out.push(AchievementDef {
                progress,
                stat_id,
                bit,
                name: localized(display.and_then(|d| d.get("name"))).unwrap_or_else(|| api_name.to_string()),
                description: localized(display.and_then(|d| d.get("desc"))).unwrap_or_default(),
                hidden: display.and_then(|d| d.int_at("hidden")).is_some_and(|h| h != 0),
                icon: field("icon"),
                icon_gray: field("icon_gray"),
                api_name: api_name.into_owned(),
            });
        }
    }
    Ok(out)
}

/// A user stats file.
pub fn parse_user(buf: &[u8]) -> Result<UserStats> {
    let root = kv::parse(buf).context("user stats")?;
    let mut out = UserStats::default();
    for (sid, stat) in root.get("cache").map(Kv::entries).unwrap_or_default() {
        let Ok(stat_id) = key_str(sid).parse::<u32>() else { continue };
        if let Some(d) = stat.int_at("data") {
            out.data.insert(stat_id, d as u32);
        }
        if let Some(v) = stat.get("data").and_then(Kv::text).and_then(|t| t.trim().parse::<f64>().ok()) {
            out.values.insert(stat_id, v);
        }
        for (bit, t) in stat.get("AchievementTimes").map(Kv::entries).unwrap_or_default() {
            if let (Ok(bit), Some(t)) = (key_str(bit).parse::<u32>(), t.int()) {
                // Times are int32 holding unsigned seconds.
                out.times.insert((stat_id, bit), t as u32 as i64);
            }
        }
    }
    Ok(out)
}

/// Join definitions with a user's state (`None`: no user file, all locked).
pub fn join(defs: Vec<AchievementDef>, user: Option<&UserStats>) -> Achievements {
    let list: Vec<Achievement> = defs
        .into_iter()
        .map(|d| {
            let unlocked = user.is_some_and(|u| u.unlocked(d.stat_id, d.bit));
            let unlocked_at = user
                .filter(|_| unlocked)
                .and_then(|u| u.times.get(&(d.stat_id, d.bit)).copied())
                .filter(|&t| t > 0);
            let progress = d.progress.as_ref().filter(|_| !unlocked).and_then(|p| {
                let v = user?.values.get(&p.stat_id?)?;
                Some(((v - p.min).clamp(0.0, p.max - p.min), p.max - p.min))
            });
            Achievement {
                progress,
                api_name: d.api_name,
                name: d.name,
                description: d.description,
                hidden: d.hidden,
                icon: d.icon,
                icon_gray: d.icon_gray,
                unlocked,
                unlocked_at,
            }
        })
        .collect();
    Achievements { total: list.len(), unlocked: list.iter().filter(|a| a.unlocked).count(), list }
}

/// An app's achievements for one user (`account_id` is the `userdata/` directory name).
/// `None` when the client has no schema cached or the app has no achievements; a schema
/// without the user's file means everything is locked.
pub fn read(steam_root: &Path, account_id: &str, app_id: u32) -> Result<Option<Achievements>> {
    let dir = steam_root.join("appcache/stats");
    let schema = match std::fs::read(dir.join(format!("UserGameStatsSchema_{app_id}.bin"))) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read achievement schema"),
    };
    let defs = parse_schema(&schema, app_id)?;
    if defs.is_empty() {
        return Ok(None);
    }
    let user = if account_id.is_empty() || !account_id.bytes().all(|b| b.is_ascii_digit()) {
        None
    } else {
        match std::fs::read(dir.join(format!("UserGameStats_{account_id}_{app_id}.bin"))) {
            Ok(b) => Some(parse_user(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("read user stats"),
        }
    };
    Ok(Some(join(defs, user.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::super::kv::build::Kb;
    use super::*;

    fn ach(b: &mut Kb, key: &str, api: &str, english: Option<&str>, hidden: bool, bit: Option<u32>) {
        b.map(key, |b| {
            b.str("name", api);
            if let Some(bit) = bit {
                b.int("bit", bit);
            }
            b.map("display", |b| {
                b.map("name", |b| {
                    if let Some(e) = english {
                        b.str("english", e);
                    }
                    b.str("token", "NEW_ACHIEVEMENT_1_0_NAME").str("german", &format!("{api} (de)"));
                });
                b.map("desc", |b| {
                    b.str("english", "Do the thing.");
                });
                if hidden {
                    b.str("hidden", "1");
                } else {
                    b.int("hidden", 0);
                }
                b.str("icon", "aaaa.jpg").str("icon_gray", "bbbb.jpg");
            });
        });
    }

    /// Stat 1 holds bits 0..2 (type as a string), stat 2 holds bits 0 and 5 (type_int
    /// only, explicit `bit`); stat 3 is an ordinary INT stat.
    fn schema() -> Vec<u8> {
        let mut b = Kb::default();
        b.map("620", |b| {
            b.str("gamename", "Portal 2").int("version", 3);
            b.map("stats", |b| {
                b.map("1", |b| {
                    b.str("type", "ACHIEVEMENTS");
                    b.map("bits", |b| {
                        ach(b, "0", "ACH_A", Some("First"), false, None);
                        ach(b, "1", "ACH_B", None, true, None);
                        ach(b, "2", "ACH_C", Some("Third"), false, None);
                    });
                });
                b.map("2", |b| {
                    b.int("type_int", 4).str("type", "4");
                    b.map("bits", |b| {
                        ach(b, "0", "ACH_D", Some("Fourth"), false, Some(0));
                        ach(b, "1", "ACH_E", Some("Fifth"), false, Some(5));
                    });
                });
                b.map("3", |b| {
                    b.str("type", "INT").str("name", "kills");
                });
            });
        });
        b.end();
        b.out
    }

    fn user() -> Vec<u8> {
        let mut b = Kb::default();
        b.map("cache", |b| {
            b.int("crc", 1234).int("PendingChanges", 0);
            b.map("1", |b| {
                // Bits 0 and 1 set; bit 2 has a stale time without its bit.
                b.int("data", 0b011);
                b.map("AchievementTimes", |b| {
                    b.int("0", 1_600_000_000).int("2", 1_600_000_100);
                });
            });
            b.map("2", |b| {
                b.int("data", 1 << 5);
                b.map("AchievementTimes", |b| {
                    b.int("5", 1_700_000_000);
                });
            });
        });
        b.end();
        b.out
    }

    /// An achievement whose progress is the `kills` stat (id 3) from 0 to 50, and one tied to
    /// a stat the user's file doesn't have.
    #[test]
    fn progress_from_a_stat() {
        let mut b = Kb::default();
        b.map("620", |b| {
            b.map("stats", |b| {
                b.map("1", |b| {
                    b.str("type", "ACHIEVEMENTS");
                    b.map("bits", |b| {
                        for (key, api, stat) in [("0", "ACH_KILLS", "kills"), ("1", "ACH_JUMPS", "jumps"), ("2", "ACH_DONE", "kills")] {
                            b.map(key, |b| {
                                b.str("name", api);
                                b.map("display", |b| {
                                    b.str("name", api);
                                });
                                b.map("progress", |b| {
                                    b.int("min_val", 0).int("max_val", 50);
                                    b.map("value", |b| {
                                        b.str("operation", "statvalue").str("operand1", stat);
                                    });
                                });
                            });
                        }
                    });
                });
                b.map("3", |b| {
                    b.str("type", "INT").str("name", "kills");
                });
                b.map("4", |b| {
                    b.str("type", "INT").str("name", "jumps");
                });
            });
        });
        b.end();
        let defs = parse_schema(&b.out, 620).unwrap();
        assert_eq!(defs[0].progress, Some(ProgressDef { stat: "kills".into(), stat_id: Some(3), min: 0.0, max: 50.0 }));
        let mut u = Kb::default();
        u.map("cache", |b| {
            b.map("1", |b| {
                b.int("data", 0b100);
            });
            b.map("3", |b| {
                b.int("data", 22);
            });
        });
        u.end();
        let user = parse_user(&u.out).unwrap();
        let a = join(defs, Some(&user));
        assert_eq!(a.list[0].progress, Some((22.0, 50.0)));
        assert_eq!(a.list[1].progress, None, "the user has no jumps stat");
        assert_eq!(a.list[2].progress, None, "unlocked: no progress shown");
    }

    #[test]
    fn schema_definitions() {
        let defs = parse_schema(&schema(), 620).unwrap();
        let names: Vec<_> = defs.iter().map(|d| (d.stat_id, d.bit, d.api_name.as_str())).collect();
        assert_eq!(names, [(1, 0, "ACH_A"), (1, 1, "ACH_B"), (1, 2, "ACH_C"), (2, 0, "ACH_D"), (2, 5, "ACH_E")]);
        assert_eq!(defs[0].name, "First");
        assert_eq!(defs[0].description, "Do the thing.");
        // No English: the first language that isn't the token.
        assert_eq!(defs[1].name, "ACH_B (de)");
        assert!(defs[1].hidden && !defs[0].hidden);
        assert_eq!(defs[0].icon.as_deref(), Some("aaaa.jpg"));
        assert_eq!(defs[0].icon_gray.as_deref(), Some("bbbb.jpg"));
    }

    #[test]
    fn join_across_two_stats() {
        let a = join(parse_schema(&schema(), 620).unwrap(), Some(&parse_user(&user()).unwrap()));
        assert_eq!((a.total, a.unlocked), (5, 3));
        let state: Vec<_> = a.list.iter().map(|x| (x.api_name.as_str(), x.unlocked, x.unlocked_at)).collect();
        assert_eq!(
            state,
            [
                ("ACH_A", true, Some(1_600_000_000)),
                ("ACH_B", true, None), // bit set, no time kept
                ("ACH_C", false, None), // time without the bit
                ("ACH_D", false, None),
                ("ACH_E", true, Some(1_700_000_000)),
            ]
        );
    }

    #[test]
    fn plain_string_display_and_missing_user() {
        let mut b = Kb::default();
        b.map("10", |b| {
            b.map("stats", |b| {
                b.map("7", |b| {
                    b.int("type", 4);
                    b.map("bits", |b| {
                        b.map("3", |b| {
                            b.str("name", "WIN");
                            b.map("display", |b| {
                                b.str("name", "Winner").str("desc", "Win.");
                            });
                        });
                        // No name: skipped.
                        b.map("4", |b| {
                            b.map("display", |_| {});
                        });
                    });
                });
            });
        });
        b.end();
        let a = join(parse_schema(&b.out, 10).unwrap(), None);
        assert_eq!((a.total, a.unlocked), (1, 0));
        assert_eq!((a.list[0].name.as_str(), a.list[0].description.as_str()), ("Winner", "Win."));
        assert_eq!(a.list[0].icon, None);
    }

    #[test]
    fn read_from_a_steam_root() {
        let root = std::env::temp_dir().join(format!("quiver-steamlocal-stats-{}", std::process::id()));
        let dir = root.join("appcache/stats");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("UserGameStatsSchema_620.bin"), schema()).unwrap();
        // No schema: None.
        assert_eq!(read(&root, "1", 400).unwrap(), None);
        // Schema, no user file: all locked.
        let a = read(&root, "1", 620).unwrap().unwrap();
        assert_eq!((a.total, a.unlocked), (5, 0));
        std::fs::write(dir.join("UserGameStats_1_620.bin"), user()).unwrap();
        assert_eq!(read(&root, "1", 620).unwrap().unwrap().unlocked, 3);
        // Not an account id: never joined into a path.
        assert_eq!(read(&root, "../1", 620).unwrap().unwrap().unlocked, 0);
        // A schema with no achievements: None.
        let mut b = Kb::default();
        b.map("5", |b| {
            b.map("stats", |b| {
                b.map("1", |b| {
                    b.str("type", "ACHIEVEMENTS");
                    b.map("bits", |_| {});
                });
            });
        });
        std::fs::write(dir.join("UserGameStatsSchema_5.bin"), &b.out).unwrap();
        assert_eq!(read(&root, "1", 5).unwrap(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn garbled_files_fail() {
        let s = schema();
        for n in [1, 5, s.len() / 2, s.len() - 3] {
            assert!(parse_schema(&s[..n], 620).is_err(), "{n}");
        }
        let u = user();
        assert!(parse_user(&u[..u.len() / 2]).is_err());
        assert!(parse_user(b"\x09cache\x00").is_err());
    }
}
