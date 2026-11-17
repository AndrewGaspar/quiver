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
pub use quiver_api::info::{Achievement, Achievements, Challenge, ConnectProfile, Progress, Reward, Stat};

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
// Rewards: definitions per space, joined with the account's status

#[derive(Debug, Default, Deserialize)]
pub struct RewardsResponse<T> {
    #[serde(default = "Vec::new")]
    pub rewards: Vec<T>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RewardDef {
    pub reward_id: String,
    pub display_name: Option<String>,
    pub display_description: Option<String>,
    pub image_urls: Option<ImageUrls>,
    pub units: Option<u64>,
    pub rarity: Option<Rarity>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ImageUrls {
    pub image: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rarity {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RewardStatus {
    pub reward_id: String,
    /// `LimitReached` (redeemed up to its limit), `Unlocked` (can be redeemed), ...
    pub status: Option<String>,
    pub purchase_quantity: Option<u64>,
    pub purchased_at: Option<chrono::DateTime<chrono::Utc>>,
    pub current_units_price: Option<u64>,
}

pub fn rewards(defs: Vec<RewardDef>, mine: Vec<RewardStatus>) -> Vec<Reward> {
    defs.into_iter()
        .filter(|d| !d.reward_id.is_empty())
        .map(|d| {
            let st = mine.iter().find(|s| s.reward_id == d.reward_id);
            let owned = st.is_some_and(|s| s.purchase_quantity.unwrap_or(0) > 0 || s.status.as_deref() == Some("LimitReached"));
            Reward {
                name: d.display_name,
                description: d.display_description,
                image: d.image_urls.and_then(|i| i.image),
                owned,
                redeemed_at: st.and_then(|s| s.purchased_at),
                cost: st.and_then(|s| s.current_units_price).or(d.units),
                rarity: d.rarity.and_then(|r| r.display_name.or(r.id)),
                status: st.and_then(|s| s.status.clone()),
                id: d.reward_id,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Challenges. Every game in this library answers `{"challenges": []}`, so element shapes are
// unseen: fields are read by the names the rewards API uses, with common alternatives.

#[derive(Debug, Default, Deserialize)]
pub struct ChallengesResponse {
    #[serde(default)]
    pub challenges: Vec<serde_json::Value>,
}

fn pick<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|k| v.get(*k).filter(|x| !x.is_null()))
}

fn pick_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    pick(v, keys).and_then(|x| x.as_str()).map(str::to_owned).filter(|s| !s.is_empty())
}

fn pick_f64(v: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    pick(v, keys).and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
}

pub fn challenges(defs: Vec<serde_json::Value>, mine: Vec<serde_json::Value>) -> Vec<Challenge> {
    const ID: &[&str] = &["challengeId", "id", "legacyChallengeId"];
    let id_of = |v: &serde_json::Value| pick(v, ID).map(|x| x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string()));
    let mut ids: Vec<String> = defs.iter().chain(&mine).filter_map(id_of).collect();
    let mut seen = std::collections::HashSet::new();
    ids.retain(|i| seen.insert(i.clone()));
    ids.into_iter()
        .map(|id| {
            let def = defs.iter().find(|d| id_of(d).as_deref() == Some(&id)).cloned().unwrap_or_default();
            let me = mine.iter().find(|d| id_of(d).as_deref() == Some(&id)).cloned().unwrap_or_default();
            let both = |keys: &[&str]| pick_str(&me, keys).or_else(|| pick_str(&def, keys));
            let image = pick(&def, &["imageUrls"]).and_then(|i| pick_str(i, &["image"])).or_else(|| both(&["imageUrl", "image"]));
            let status = both(&["status", "state"]).unwrap_or_default().to_ascii_lowercase();
            let completed = pick(&me, &["isCompleted", "completed"]).and_then(|x| x.as_bool()).unwrap_or(false)
                || status.contains("complete")
                || status.contains("claimed");
            Challenge {
                name: both(&["displayName", "name", "title"]),
                description: both(&["displayDescription", "description"]),
                image,
                completed,
                progress: pick_f64(&me, &["progress", "currentValue", "value"]),
                target: pick_f64(&def, &["target", "targetValue", "threshold"]).or_else(|| pick_f64(&me, &["target", "targetValue"])),
                reward: pick_f64(&def, &["units", "xp"]).map(|n| format!("{n}")),
                id,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Ubisoft Connect level and units (account-wide)

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Units {
    pub units: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MetaProgression {
    pub level: Option<u32>,
    pub xp: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Levels {
    pub levels: Vec<Level>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Level {
    pub level: u32,
    /// XP at which this level starts.
    pub xp: Option<u64>,
}

pub fn connect_profile(units: Option<Units>, meta: Option<MetaProgression>, levels: Option<Levels>) -> ConnectProfile {
    let meta = meta.unwrap_or_default();
    let levels = levels.unwrap_or_default();
    let start = |l: Option<u32>| l.and_then(|l| levels.levels.iter().find(|x| x.level == l)?.xp);
    ConnectProfile {
        level: meta.level,
        xp: meta.xp,
        level_start_xp: start(meta.level),
        next_level_xp: start(meta.level.map(|l| l + 1)),
        units: units.and_then(|u| u.units),
    }
}

// ---------------------------------------------------------------------------------------
// Achievement definitions: the product config's `uplay.achievements` names a zip on the
// asset CDN with `<locale>_loc.txt` (`id<TAB>name<TAB>description` lines) and `<n>.png` icons.

/// Parse a `<locale>_loc.txt`.
pub fn parse_achievement_names(text: &str) -> Vec<Achievement> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|l| {
            let mut f = l.trim_end_matches('\r').splitn(3, '\t');
            let id = f.next()?.trim();
            if id.is_empty() {
                return None;
            }
            Some(Achievement {
                id: id.to_owned(),
                name: f.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
                description: f.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
                ..Default::default()
            })
        })
        .collect()
}

/// Unpack an achievement-set zip into `dir` (icons and texts) and read the definitions in
/// `locale` (falling back to en-US). Icon paths are files in `dir`.
pub fn unpack_achievements(zip: &[u8], dir: &std::path::Path, locale: &str) -> anyhow::Result<Achievements> {
    std::fs::create_dir_all(dir)?;
    for (name, data) in crate::zipread::entries(zip)? {
        let safe = !name.is_empty() && !name.contains(['/', '\\']) && !name.starts_with('.');
        if safe {
            std::fs::write(dir.join(&name), data)?;
        }
    }
    read_achievements(dir, locale)
}

/// Definitions from an unpacked set (see [`unpack_achievements`]).
pub fn read_achievements(dir: &std::path::Path, locale: &str) -> anyhow::Result<Achievements> {
    let text = std::fs::read_to_string(dir.join(format!("{locale}_loc.txt")))
        .or_else(|_| std::fs::read_to_string(dir.join("en-US_loc.txt")))
        .map_err(|e| anyhow::anyhow!("achievement set has no names: {e}"))?;
    let mut items = parse_achievement_names(&text);
    for a in &mut items {
        let icon = a.id.parse::<u32>().ok().map(|n| dir.join(format!("{n}.png"))).filter(|p| p.is_file());
        a.image = icon.map(|p| p.to_string_lossy().into_owned());
    }
    Ok(Achievements { unlocked_known: false, unlocked: 0, total: items.len() as u32, items, source: None })
}

/// The achievement set `asset` (a file name on the asset CDN), cached under `cache/<asset>/`.
pub async fn achievement_definitions(asset: &str, cache: &std::path::Path, locale: &str) -> anyhow::Result<Achievements> {
    anyhow::ensure!(!asset.is_empty() && asset.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'), "bad asset name {asset:?}");
    let dir = cache.join(asset);
    if let Ok(a) = read_achievements(&dir, locale) {
        return Ok(a);
    }
    let url = format!("{}{asset}", crate::library::ART_BASE_URL);
    let rsp = reqwest::get(&url).await?;
    anyhow::ensure!(rsp.status().is_success(), "GET {url}: HTTP {}", rsp.status());
    let zip = rsp.bytes().await?;
    unpack_achievements(&zip, &dir, locale)
}

// ---------------------------------------------------------------------------------------
// Fetching

/// What the daemon knows about the game besides its space.
#[derive(Debug, Default, Clone)]
pub struct GameRefs {
    pub space_id: Option<String>,
    /// `uplay.achievements` from the product config.
    pub achievements_asset: Option<String>,
}

async fn try_get<T: serde::de::DeserializeOwned>(auth: &Auth, session: &Session, path: &str, what: &str, errors: &mut Vec<String>) -> Option<T> {
    match get_json::<T>(auth, session, path).await {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!("{what}: {e:#}"));
            None
        }
    }
}

/// Everything for a game's progress page. Failing parts are listed in `errors`.
pub async fn fetch(auth: &Auth, session: &Session, game: &GameRefs, cache: &std::path::Path) -> Progress {
    let mut p = Progress { space_id: game.space_id.clone(), fetched: chrono::Utc::now(), ..Default::default() };
    let user = &session.user_id;
    let mut errors = Vec::new();
    if let Some(space) = &game.space_id {
        let stats_path = format!("/v1/profiles/{user}/statsCard?spaceId={space}");
        if let Some(r) = try_get::<StatsCardResponse>(auth, session, &stats_path, "stats", &mut errors).await {
            p.stats = stats(r);
        }
        let defs = format!("/v1/spaces/{space}/global/ubiconnect/rewards/api");
        let mine = format!("/v1/profiles/{user}/global/ubiconnect/rewards/api?spaceId={space}");
        if let Some(d) = try_get::<RewardsResponse<RewardDef>>(auth, session, &defs, "rewards", &mut errors).await {
            let m = try_get::<RewardsResponse<RewardStatus>>(auth, session, &mine, "reward status", &mut errors).await;
            p.rewards = rewards(d.rewards, m.map(|m| m.rewards).unwrap_or_default());
        }
        let defs = format!("/v1/spaces/{space}/global/ubiconnect/challenges/api");
        let mine = format!("/v1/profiles/{user}/global/ubiconnect/challenges/api?spaceId={space}");
        if let Some(d) = try_get::<ChallengesResponse>(auth, session, &defs, "challenges", &mut errors).await {
            let m = try_get::<ChallengesResponse>(auth, session, &mine, "challenge progress", &mut errors).await;
            p.challenges = challenges(d.challenges, m.map(|m| m.challenges).unwrap_or_default());
        }
    } else {
        errors.push("no UbiServices space id for this game".into());
    }
    if let Some(asset) = &game.achievements_asset {
        match achievement_definitions(asset, cache, "en-US").await {
            Ok(a) => p.achievements = Some(a),
            Err(e) => errors.push(format!("achievements: {e:#}")),
        }
    }
    p.connect = Some(connect(auth, session, &mut errors).await);
    p.errors = errors;
    p
}

/// The account's Ubisoft Connect level, XP and units.
pub async fn connect(auth: &Auth, session: &Session, errors: &mut Vec<String>) -> ConnectProfile {
    let user = &session.user_id;
    let base = format!("/v1/profiles/{user}/global/ubiconnect/economy/api");
    let units = try_get::<Units>(auth, session, &format!("{base}/units"), "units", errors).await;
    let meta = try_get::<MetaProgression>(auth, session, &format!("{base}/metaprogression"), "Connect level", errors).await;
    let levels = match meta.as_ref().and_then(|m| m.level) {
        Some(l) => {
            let path = format!("/v1/spaces/global/ubiconnect/economy/api/metaprogression?levels={l},{}", l + 1);
            try_get::<Levels>(auth, session, &path, "Connect levels", errors).await
        }
        None => None,
    };
    connect_profile(units, meta, levels)
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
    fn rewards_joined_with_status() {
        let defs: RewardsResponse<RewardDef> = serde_json::from_str(&fixture("rewards-space.json")).unwrap();
        let mine: RewardsResponse<RewardStatus> = serde_json::from_str(&fixture("rewards-profile.json")).unwrap();
        let r = rewards(defs.rewards, mine.rewards);
        assert_eq!(r.len(), 4);
        let aguilar = r.iter().find(|r| r.name.as_deref() == Some("Aguilar's Outfit")).unwrap();
        assert!(aguilar.owned);
        assert_eq!((aguilar.cost, aguilar.rarity.as_deref(), aguilar.status.as_deref()), (Some(40), Some("Legendary"), Some("LimitReached")));
        assert!(aguilar.image.as_deref().unwrap().starts_with("https://ubiservices.cdn.ubi.com/"));
        assert!(aguilar.redeemed_at.is_some());
        let kit = r.iter().find(|r| r.name.as_deref() == Some("Assassin's Creed Origins fan kit")).unwrap();
        assert!(!kit.owned);
        assert_eq!((kit.cost, kit.status.as_deref()), (Some(0), Some("Unlocked")));
        // Definitions without any status for this account: not owned.
        let r = rewards(serde_json::from_str::<RewardsResponse<RewardDef>>(&fixture("rewards-space.json")).unwrap().rewards, vec![]);
        assert!(r.iter().all(|r| !r.owned && r.status.is_none()));
    }

    #[test]
    fn connect_level_and_units() {
        let units: Units = serde_json::from_str(&fixture("units.json")).unwrap();
        let meta: MetaProgression = serde_json::from_str(&fixture("metaprogression.json")).unwrap();
        let levels: Levels = serde_json::from_str(&fixture("metaprogression-levels.json")).unwrap();
        let c = connect_profile(Some(units), Some(meta), Some(levels));
        assert_eq!(c, ConnectProfile { level: Some(84), xp: Some(41975), level_start_xp: Some(41500), next_level_xp: Some(42000), units: Some(0) });
        assert_eq!(connect_profile(None, None, None), ConnectProfile::default());
    }

    #[test]
    fn challenges_lenient() {
        let empty: ChallengesResponse = serde_json::from_str(&fixture("challenges-empty.json")).unwrap();
        assert!(challenges(empty.challenges, vec![]).is_empty());
        // Guessed shapes (none seen live yet).
        let defs = vec![serde_json::json!({"challengeId": "c1", "displayName": "Win", "units": 10, "target": 5, "imageUrls": {"image": "https://x/i.png"}})];
        let mine = vec![
            serde_json::json!({"challengeId": "c1", "progress": "3", "status": "InProgress"}),
            serde_json::json!({"id": 7, "name": "Old", "isCompleted": true}),
        ];
        let c = challenges(defs, mine);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].name.as_deref(), c[0].progress, c[0].target, c[0].completed), (Some("Win"), Some(3.0), Some(5.0), false));
        assert_eq!(c[0].image.as_deref(), Some("https://x/i.png"));
        assert_eq!((c[1].id.as_str(), c[1].completed), ("7", true));
    }

    #[test]
    fn achievement_set() {
        let zip = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/assets/achievements-3539.zip")).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let a = unpack_achievements(&zip, tmp.path(), "fr-FR").unwrap();
        assert_eq!(a.total, a.items.len() as u32);
        assert!(a.total >= 30 && !a.unlocked_known);
        assert_eq!(a.items[0].id, "01");
        assert_eq!(a.items[0].name.as_deref(), Some("Premiers pas"));
        assert!(a.items[0].image.as_deref().unwrap().ends_with("/1.png"));
        assert!(a.items[2].image.is_none(), "no 3.png in the fixture");
        // Cached: read back without the zip; unknown locales fall back to English.
        let a = read_achievements(tmp.path(), "xx-XX").unwrap();
        assert_eq!(a.items[0].name.as_deref(), Some("First Steps"));
        assert_eq!(a.items[0].description.as_deref(), Some("Complete the Prologue."));
        assert_eq!(std::fs::read(tmp.path().join("2.png")).unwrap(), b"\x89PNG fake icon 2 (stored)");
    }

    #[test]
    fn tolerant() {
        let r: StatsCardResponse = serde_json::from_str(r#"{"Statscards":[{"statName":"X","value":"1.5","new":1}],"more":true}"#).unwrap();
        assert_eq!(stats(r)[0].value.as_deref(), Some("1.50"));
        let r: StatsCardResponse = serde_json::from_str("{}").unwrap();
        assert!(stats(r).is_empty());
    }
}
