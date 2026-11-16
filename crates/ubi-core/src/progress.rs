//! Ubisoft progression over UbiServices REST, as Ubisoft Connect reads it for a game page
//! (captured from Connect 13416; shapes from live responses, see `fixtures/ubiservices/`):
//!
//! ```text
//! GET /v1/profiles/{userId}/statsCard?spaceId=S                                   player stats
//! GET /v1/profiles/{userId}/global/ubiconnect/challenges/api?spaceId=S            challenge progress
//! GET /v1/spaces/S/global/ubiconnect/challenges/api                               challenge definitions
//! GET /v1/profiles/{userId}/global/ubiconnect/rewards/api?spaceId=S               owned rewards
//! GET /v1/spaces/S/global/ubiconnect/rewards/api                                  reward definitions
//! GET /v1/profiles/{userId}/global/ubiconnect/economy/api/units                   units balance
//! GET /v1/profiles/{userId}/global/ubiconnect/economy/api/metaprogression         level and XP
//! GET /v1/spaces/global/ubiconnect/economy/api/metaprogression?levels=N,N+1       level thresholds
//! ```
//!
//! Every type tolerates unknown and missing fields; a failing endpoint becomes an entry in
//! `Progress::errors` rather than failing the whole page.

use serde::Deserialize;
pub use ubi_api::info::{Achievement, Achievements, Challenge, ConnectProfile, Progress, Reward, Stat};

use crate::auth::{Auth, Session};
use crate::ubiservices::get_json;

// ---------------------------------------------------------------------------------------
// statsCard

#[derive(Debug, Default, Deserialize)]
pub struct StatsCardResponse {
    #[serde(rename = "Statscards", alias = "statscards", alias = "statsCards", default)]
    pub cards: Vec<StatsCard>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatsCard {
    pub stat_name: String,
    pub display_name: Option<String>,
    /// `LongTimespan`, `Integer`, `Float`, ...
    pub format: Option<String>,
    /// `Seconds`, `%`, `m`, or empty.
    pub unit: Option<String>,
    /// `Cumulative`, `Tier`, ...
    pub semantic: Option<String>,
    /// A number as a string; empty when the player has no value.
    pub value: Option<String>,
    pub ordinal: Option<i64>,
    pub obj: Option<String>,
    pub last_modified: Option<String>,
}

/// Format a stat the way a game page shows it.
pub fn format_stat(value: f64, format: Option<&str>, unit: Option<&str>) -> String {
    let unit = unit.unwrap_or("").trim();
    if format.is_some_and(|f| f.contains("Timespan")) || unit.eq_ignore_ascii_case("seconds") {
        let s = value as u64;
        return match (s / 3600, s % 3600 / 60) {
            (0, m) => format!("{m} min"),
            (h, m) => format!("{h} h {m:02} min"),
        };
    }
    let n = if value.fract() == 0.0 { group(value as i64) } else { format!("{value:.2}") };
    match unit {
        "" => n,
        "%" => format!("{n}%"),
        u => format!("{n} {u}"),
    }
}

fn group(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn stats(rsp: StatsCardResponse) -> Vec<Stat> {
    let mut cards = rsp.cards;
    cards.sort_by_key(|c| c.ordinal.unwrap_or(i64::MAX));
    cards
        .into_iter()
        .filter(|c| !c.stat_name.is_empty())
        .map(|c| {
            let number = c.value.as_deref().and_then(|v| v.trim().parse::<f64>().ok());
            Stat {
                value: number.map(|n| format_stat(n, c.format.as_deref(), c.unit.as_deref())),
                number,
                name: c.display_name.filter(|n| !n.is_empty()),
                kind: c.format.filter(|f| !f.is_empty()),
                image: c.obj.filter(|o| o.starts_with("http")),
                id: c.stat_name,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Fetching

pub async fn fetch(auth: &Auth, session: &Session, space_id: Option<&str>) -> Progress {
    let mut p = Progress { space_id: space_id.map(str::to_owned), fetched: chrono::Utc::now(), ..Default::default() };
    let user = &session.user_id;
    if let Some(space) = space_id {
        match get_json::<StatsCardResponse>(auth, session, &format!("/v1/profiles/{user}/statsCard?spaceId={space}")).await {
            Ok(r) => p.stats = stats(r),
            Err(e) => p.errors.push(format!("stats: {e:#}")),
        }
    } else {
        p.errors.push("no UbiServices space id for this game".into());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/ubiservices").join(name)).unwrap()
    }

    #[test]
    fn stats_card() {
        let rsp: StatsCardResponse = serde_json::from_str(&fixture("statscard.json")).unwrap();
        let s = stats(rsp);
        assert_eq!(s[0].id, "TimePlayed");
        assert_eq!(s[0].name.as_deref(), Some("Playtime"));
        assert_eq!(s[0].number, Some(144076.0));
        assert_eq!(s[0].value.as_deref(), Some("40 h 01 min"));
        let level = s.iter().find(|s| s.id == "PlayerLevel").unwrap();
        assert_eq!(level.value.as_deref(), Some("39"));
        let prog = s.iter().find(|s| s.id == "TotalProgression").unwrap();
        assert_eq!(prog.value.as_deref(), Some("59%"));
        let dist = s.iter().find(|s| s.id == "DistanceTravelled").unwrap();
        assert_eq!(dist.value.as_deref(), Some("420,194 m"));
        // No value yet.
        let juno = s.iter().find(|s| s.id == "JunoBossDefeated").unwrap();
        assert_eq!((juno.number, juno.value.as_deref()), (None, None));
    }

    #[test]
    fn tolerant() {
        let r: StatsCardResponse = serde_json::from_str(r#"{"Statscards":[{"statName":"X","value":"1.5","new":1}],"more":true}"#).unwrap();
        assert_eq!(stats(r)[0].value.as_deref(), Some("1.50"));
        let r: StatsCardResponse = serde_json::from_str("{}").unwrap();
        assert!(stats(r).is_empty());
    }
}
