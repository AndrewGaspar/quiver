//! Epic achievements (design/STORE-EPIC.md §3 C11, STORES.md §3.8): read only. Games report
//! their unlocks to Epic themselves, through EOS, with the exchange-code sign-in the launch
//! gives them; Quiver never reports or unlocks anything.
//!
//! Both reads are the store's GraphQL (`POST launcher.store.epicgames.com/graphql`, the
//! session's bearer token, a launcher user agent), queries as the store and legendary send
//! them (read as a protocol reference):
//!
//! - definitions per sandbox (the game's catalog namespace):
//!   `Achievement.productAchievementsRecordBySandbox(sandboxId, locale)`, cached a day;
//! - this account's unlocks: `PlayerAchievement.playerAchievementGameRecordsBySandbox
//!   (epicAccountId, sandboxId)`, cached a few minutes.
//!
//! **Toasts.** During a session quiverd polls [`Epic::new_unlocks`] every few minutes and
//! once more when it ends. Unlocks already seen are kept per account and game
//! (`achievements/<account>/<app>.json`); the first read only records what's there, so
//! achievements earned before Quiver looked never toast, and each unlock toasts once.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::epicgame::EpicAchievements;
use quiver_api::stores::{Achievement, Achievements};
use serde::{Deserialize, Serialize};

use crate::Epic;
use crate::api::Api;
use crate::http::{Method, Request};
use crate::progress::{read_json, write_json};

pub const STORE_GRAPHQL: &str = "https://launcher.store.epicgames.com/graphql";
/// The store's GraphQL answers launcher clients (the user agent EGL sends it).
pub const STORE_USER_AGENT: &str = "EpicGamesLauncher/14.0.8-22004686+++Portal+Release-Live";
const DEFINITIONS_TTL: chrono::Duration = chrono::Duration::days(1);
const PLAYER_TTL: chrono::Duration = chrono::Duration::minutes(3);

const DEFINITIONS_QUERY: &str = "query Achievement($sandboxId: String!, $locale: String!) { Achievement { productAchievementsRecordBySandbox(sandboxId: $sandboxId, locale: $locale) { sandboxId totalAchievements totalProductXP achievementSets { achievementSetId isBase totalAchievements totalXP } platinumRarity { percent } achievements { achievement { name hidden isBase unlockedDisplayName lockedDisplayName unlockedDescription lockedDescription unlockedIconId lockedIconId XP flavorText unlockedIconLink lockedIconLink tier { name hexColor min max } rarity { percent } } } } } }";

const PLAYER_QUERY: &str = "query PlayerAchievement($epicAccountId: String!, $sandboxId: String!) { PlayerAchievement { playerAchievementGameRecordsBySandbox(epicAccountId: $epicAccountId, sandboxId: $sandboxId) { records { totalXP totalUnlocked playerAwards { awardType unlockedDateTime achievementSetId } achievementSets { achievementSetId isBase totalUnlocked totalXP } playerAchievements { playerAchievement { sandboxId epicAccountId unlocked progress XP unlockDate achievementName isBase achievementSetId } } } } } }";

/// An achievement as the store defines it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Definition {
    pub name: String,
    pub hidden: bool,
    pub unlocked_display_name: Option<String>,
    pub locked_display_name: Option<String>,
    pub unlocked_description: Option<String>,
    pub locked_description: Option<String>,
    pub unlocked_icon_link: Option<String>,
    pub locked_icon_link: Option<String>,
    #[serde(rename = "XP")]
    pub xp: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Definitions {
    pub total_achievements: u32,
    #[serde(rename = "totalProductXP")]
    pub total_xp: u32,
    pub achievements: Vec<Definition>,
}

/// One of the player's achievements.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayerAchievement {
    pub achievement_name: String,
    pub unlocked: bool,
    /// ISO time, or `N/A`.
    pub unlock_date: Option<String>,
    pub progress: Option<f64>,
}

impl PlayerAchievement {
    pub fn unlocked_at(&self) -> Option<DateTime<Utc>> {
        self.unlock_date.as_deref().and_then(|d| DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&Utc))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayerRecord {
    #[serde(rename = "totalXP")]
    pub total_xp: u32,
    pub total_unlocked: u32,
    pub achievements: Vec<PlayerAchievement>,
}

fn graphql_errors(v: &serde_json::Value) -> Option<String> {
    let errs = v.get("errors")?.as_array()?;
    let msgs: Vec<&str> = errs.iter().filter_map(|e| e.get("message").and_then(|m| m.as_str())).collect();
    (!errs.is_empty()).then(|| if msgs.is_empty() { "Epic's store answered with an error".to_owned() } else { msgs.join("; ") })
}

impl Api {
    async fn graphql(&self, label: &'static str, token: &str, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let req = Request::new(label, Method::Post, STORE_GRAPHQL).bearer(token).header("User-Agent", STORE_USER_AGENT).json(&body);
        let v: serde_json::Value = self.json(req).await?;
        if v.get("data").is_none_or(|d| d.is_null())
            && let Some(e) = graphql_errors(&v)
        {
            bail!("Epic {label}: {e}");
        }
        Ok(v)
    }

    /// The game's achievements (`None`: the store has none for it).
    pub async fn achievement_definitions(&self, token: &str, sandbox: &str, locale: &str) -> Result<Option<Definitions>> {
        let v = self.graphql("achievements", token, DEFINITIONS_QUERY, serde_json::json!({ "sandboxId": sandbox, "locale": locale })).await?;
        let rec = &v["data"]["Achievement"]["productAchievementsRecordBySandbox"];
        if rec.is_null() {
            return Ok(None);
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrapped {
            achievement: Definition,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Raw {
            total_achievements: u32,
            #[serde(rename = "totalProductXP")]
            total_xp: u32,
            achievements: Vec<Wrapped>,
        }
        let raw: Raw = serde_json::from_value(without_nulls(rec.clone())).context("Epic achievements: unexpected reply")?;
        Ok(Some(Definitions { total_achievements: raw.total_achievements, total_xp: raw.total_xp, achievements: raw.achievements.into_iter().map(|w| w.achievement).collect() }))
    }

    /// The account's unlocks for the game (an empty record when it has none).
    pub async fn player_achievements(&self, token: &str, account: &str, sandbox: &str) -> Result<PlayerRecord> {
        let v = self.graphql("player achievements", token, PLAYER_QUERY, serde_json::json!({ "epicAccountId": account, "sandboxId": sandbox })).await?;
        let records = &v["data"]["PlayerAchievement"]["playerAchievementGameRecordsBySandbox"]["records"];
        let Some(first) = records.as_array().and_then(|r| r.first()) else { return Ok(PlayerRecord::default()) };
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Wrapped {
            player_achievement: PlayerAchievement,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Raw {
            #[serde(rename = "totalXP")]
            total_xp: u32,
            total_unlocked: u32,
            player_achievements: Vec<Wrapped>,
        }
        let raw: Raw = serde_json::from_value(without_nulls(first.clone())).context("Epic player achievements: unexpected reply")?;
        Ok(PlayerRecord { total_xp: raw.total_xp, total_unlocked: raw.total_unlocked, achievements: raw.player_achievements.into_iter().map(|w| w.player_achievement).collect() })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct CachedDefinitions {
    fetched_at: Option<DateTime<Utc>>,
    definitions: Option<Definitions>,
}

/// Unlocks already seen (toasted or there before), per account and game.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Seen {
    since: Option<DateTime<Utc>>,
    unlocked: BTreeSet<String>,
}

/// An unlock Quiver hadn't seen: what a toast shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Unlock {
    pub app: String,
    pub account_id: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub unlocked_at: DateTime<Utc>,
}

/// Definitions and the player's record as the page and Sight show them.
pub fn merge(defs: &Definitions, player: &PlayerRecord) -> Achievements {
    let mine: HashMap<&str, &PlayerAchievement> = player.achievements.iter().map(|p| (p.achievement_name.as_str(), p)).collect();
    let list: Vec<Achievement> = defs
        .achievements
        .iter()
        .map(|d| {
            let p = mine.get(d.name.as_str()).filter(|p| p.unlocked);
            let unlocked = p.is_some();
            let pick = |u: &Option<String>, l: &Option<String>| if unlocked { u.clone().or(l.clone()) } else { l.clone().or(u.clone()) };
            Achievement {
                id: d.name.clone(),
                name: pick(&d.unlocked_display_name, &d.locked_display_name).unwrap_or_else(|| d.name.clone()),
                description: pick(&d.unlocked_description, &d.locked_description).unwrap_or_default(),
                hidden: d.hidden,
                unlocked_at: p.and_then(|p| p.unlocked_at()),
                unlocked,
                icon: d.unlocked_icon_link.clone(),
                icon_gray: d.locked_icon_link.clone(),
            }
        })
        .collect();
    Achievements { unlocked: list.iter().filter(|a| a.unlocked).count(), total: list.len().max(defs.total_achievements as usize), list }
}

impl Epic {
    async fn definitions(&self, sandbox: &str, token: &str, refresh: bool) -> Result<Option<Definitions>> {
        let path = self.dir.join("cache").join(format!("achievements-{}.json", crate::api::urlencode(sandbox)));
        let cached: CachedDefinitions = read_json(&path).unwrap_or_default();
        if let Some(at) = cached.fetched_at
            && !refresh
            && Utc::now() - at < DEFINITIONS_TTL
        {
            return Ok(cached.definitions);
        }
        match self.api.achievement_definitions(token, sandbox, &self.api.config.locale).await {
            Ok(d) => {
                let _ = write_json(&path, &CachedDefinitions { fetched_at: Some(Utc::now()), definitions: d.clone() });
                Ok(d)
            }
            // A stale copy beats none.
            Err(e) if cached.fetched_at.is_some() => {
                tracing::info!(sandbox, "Epic achievements (using the cached set): {e:#}");
                Ok(cached.definitions)
            }
            Err(e) => Err(e),
        }
    }

    async fn player(&self, account: &str, sandbox: &str, token: &str, refresh: bool) -> Result<(DateTime<Utc>, PlayerRecord)> {
        let key = format!("{account}/{sandbox}");
        let mut cache = self.p3.players.lock().await;
        if let Some((at, r)) = cache.get(&key)
            && !refresh
            && Utc::now() - *at < PLAYER_TTL
        {
            return Ok((*at, r.clone()));
        }
        let r = self.api.player_achievements(token, account, sandbox).await?;
        let now = Utc::now();
        cache.insert(key, (now, r.clone()));
        Ok((now, r))
    }

    /// `EpicGameRequest::Achievements` (also the game page's and Sight's).
    pub async fn achievements(&self, app: &str, refresh: bool) -> Result<EpicAchievements> {
        let t = self.title_of(app).await?;
        let game = crate::provider::game_ref(app).context("not an Epic app name")?;
        let (account, name) = self.owner_of(&t)?;
        let token = self.access_token(&account).await?;
        let mut out = EpicAchievements { account: Some(name), ..EpicAchievements::new(game) };
        let Some(defs) = self.definitions(&t.namespace, token.expose(), refresh).await? else {
            out.achievements = Some(Achievements::default());
            return Ok(out);
        };
        match self.player(&account, &t.namespace, token.expose(), refresh).await {
            Ok((at, p)) => {
                out.fetched = Some(at);
                out.xp = Some((p.total_xp, defs.total_xp));
                out.achievements = Some(merge(&defs, &p));
            }
            Err(e) => {
                out.errors.push(format!("unlocks: {e:#}"));
                // Definitions without unlock state would read as "none unlocked": leave it unknown.
            }
        }
        Ok(out)
    }

    /// Unlocks since the last look, each once. The first look at a game only records
    /// what's there (nothing is new then).
    pub async fn new_unlocks(&self, app: &str, account: &str) -> Result<Vec<Unlock>> {
        let t = self.title_of(app).await?;
        let token = self.access_token(account).await?;
        let Some(defs) = self.definitions(&t.namespace, token.expose(), false).await? else { return Ok(Vec::new()) };
        let (_, p) = self.player(account, &t.namespace, token.expose(), true).await?;
        let _seen_lock = self.p3.unlocks.lock().await;
        let path = self.dir.join("achievements").join(crate::api::urlencode(account)).join(format!("{}.json", crate::api::urlencode(app)));
        let first = !path.exists();
        let mut seen: Seen = read_json(&path)?;
        let now_unlocked: Vec<&PlayerAchievement> = p.achievements.iter().filter(|a| a.unlocked).collect();
        let fresh: Vec<&PlayerAchievement> = now_unlocked.iter().copied().filter(|a| !seen.unlocked.contains(&a.achievement_name)).collect();
        if fresh.is_empty() && !first {
            return Ok(Vec::new());
        }
        seen.since.get_or_insert_with(Utc::now);
        seen.unlocked.extend(fresh.iter().map(|a| a.achievement_name.clone()));
        write_json(&path, &seen)?;
        if first {
            return Ok(Vec::new());
        }
        let by_name: HashMap<&str, &Definition> = defs.achievements.iter().map(|d| (d.name.as_str(), d)).collect();
        Ok(fresh
            .into_iter()
            .map(|a| {
                let d = by_name.get(a.achievement_name.as_str());
                Unlock {
                    app: app.to_owned(),
                    account_id: account.to_owned(),
                    id: a.achievement_name.clone(),
                    name: d.and_then(|d| d.unlocked_display_name.clone().or(d.locked_display_name.clone())).unwrap_or_else(|| a.achievement_name.clone()),
                    description: d.and_then(|d| d.unlocked_description.clone()),
                    icon_url: d.and_then(|d| d.unlocked_icon_link.clone()),
                    unlocked_at: a.unlocked_at().unwrap_or_else(Utc::now),
                }
            })
            .collect())
    }

    /// An achievement icon on this machine (downloaded once), for a toast.
    pub async fn icon_file(&self, url: &str) -> Option<std::path::PathBuf> {
        if !url.starts_with("https://") {
            return None;
        }
        let ext = url.rsplit('.').next().filter(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg")).unwrap_or("png");
        let path = self.dir.join("cache/achievement-icons").join(format!("{}.{ext}", hex::encode(crate::savefmt::sha1(url.as_bytes()))));
        if path.is_file() {
            return Some(path);
        }
        let data = self.api.fetch_link("achievement icon", url).await.ok()?;
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, data).ok()?;
        Some(path)
    }
}

/// The value with every `null` object member removed, so `#[serde(default)]` fills it:
/// Epic sends `"totalProductXP": null` (and the like) for games that award no XP.
fn without_nulls(mut v: serde_json::Value) -> serde_json::Value {
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.retain(|_, x| !x.is_null());
                m.values_mut().for_each(walk);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

#[cfg(test)]
mod null_tests {
    #[test]
    fn nulls_become_defaults() {
        let v = serde_json::json!({"totalAchievements": 3, "totalProductXP": null, "achievements": [{"achievement": null}]});
        let v = super::without_nulls(v);
        assert_eq!(v, serde_json::json!({"totalAchievements": 3, "achievements": [{}]}));
    }
}
