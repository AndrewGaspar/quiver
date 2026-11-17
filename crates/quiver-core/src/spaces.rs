//! UbiServices' games API: what Ubisoft Connect shows for a game's space (name,
//! description, release date, art, links), for any game, including those bought on Steam
//! whose product configs carry no art.
//!
//! ```text
//! GET /v1/spaces/global/ubiconnect/games/api?spaceIds=<spaceId>[,<spaceId>…]
//!   -> {"games":[{"spaceId", "displayName", "displayDescription", "releaseAt",
//!                 "imageUrls": {"lowBoxArt", "highBoxArt", "background", "gameLogoColor",
//!                               "gameLogoFlat", "gameAvatar", …},
//!                 "linkUrls": {"website", "facebook", "twitter", …}, …}]}
//! ```
//!
//! (from Connect's own HTTP cache; URLs are on `ubiservices.cdn.ubi.com`, file names differ
//! per game). Every field is optional here; unknown ones are ignored. Answers are kept on
//! disk ([`SpaceCache`]) for a day, including "no such game", so the library asks at most
//! once a day per space.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use quiver_api::art::SpaceArt;

use crate::auth::{Auth, Session};

/// How long an answer is used before asking again.
pub const TTL: chrono::TimeDelta = chrono::TimeDelta::days(1);
/// Space ids per request.
pub const BATCH: usize = 20;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageUrls {
    #[serde(default)]
    pub low_thumbnail: Option<String>,
    #[serde(default)]
    pub high_thumbnail: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub low_box_art: Option<String>,
    #[serde(default)]
    pub high_box_art: Option<String>,
    #[serde(default)]
    pub game_logo_flat: Option<String>,
    #[serde(default)]
    pub game_logo_color: Option<String>,
    #[serde(default)]
    pub game_avatar: Option<String>,
}

/// One game of the answer (the fields Quiver uses).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceGame {
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub display_description: Option<String>,
    /// RFC 3339, e.g. `2020-11-10T00:00:00Z`.
    #[serde(default)]
    pub release_at: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub image_urls: ImageUrls,
    /// `website`, `facebook`, `twitter`, `instagram`, `reddit`, … (values may be null).
    #[serde(default, deserialize_with = "lenient")]
    pub link_urls: BTreeMap<String, Option<String>>,
}

/// A field of the wrong shape (null, a string where an object was expected) is empty rather
/// than an error that loses the whole answer.
fn lenient<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

fn https(u: &Option<String>) -> Option<String> {
    u.as_deref().map(str::trim).filter(|u| u.starts_with("https://") && u.len() > 8).map(str::to_owned)
}

impl SpaceGame {
    /// Its art: box art for tiles, the background for heroes, the color logo (else the
    /// flat one), the avatar as icon. Only `https` URLs.
    pub fn art(&self) -> SpaceArt {
        let i = &self.image_urls;
        SpaceArt {
            tile: https(&i.high_box_art).or_else(|| https(&i.low_box_art)),
            hero: https(&i.background),
            logo: https(&i.game_logo_color).or_else(|| https(&i.game_logo_flat)),
            icon: https(&i.game_avatar),
        }
    }

    /// The release day, `YYYY-MM-DD`.
    pub fn release_date(&self) -> Option<String> {
        let r = self.release_at.as_deref()?;
        let d = DateTime::parse_from_rfc3339(r).ok()?;
        Some(d.date_naive().format("%Y-%m-%d").to_string())
    }

    /// Links with an `https`/`http` URL, by kind.
    pub fn links(&self) -> Vec<(String, String)> {
        self.link_urls
            .iter()
            .filter_map(|(k, v)| {
                let u = v.as_deref()?.trim();
                (u.starts_with("https://") || u.starts_with("http://")).then(|| (k.clone(), u.to_owned()))
            })
            .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
struct Answer {
    #[serde(default)]
    games: Vec<serde_json::Value>,
}

/// Parse an answer; games that don't parse are skipped.
pub fn parse(body: &str) -> Result<Vec<SpaceGame>> {
    let a: Answer = serde_json::from_str(body).context("games API answer isn't JSON")?;
    Ok(a.games
        .into_iter()
        .filter_map(|g| serde_json::from_value::<SpaceGame>(g).ok())
        .filter(|g| !g.space_id.is_empty())
        .collect())
}

/// Space ids are UUIDs; anything else never goes into a URL.
pub fn valid_space_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

pub fn path(ids: &[&str]) -> String {
    format!("/v1/spaces/global/ubiconnect/games/api?spaceIds={}", ids.join(","))
}

/// Ask for `ids` (valid ones, [`BATCH`] per request).
pub async fn fetch(auth: &Auth, session: &Session, ids: &[String]) -> Result<Vec<SpaceGame>> {
    let ids: Vec<&str> = ids.iter().map(String::as_str).filter(|s| valid_space_id(s)).collect();
    let mut out = Vec::new();
    for chunk in ids.chunks(BATCH) {
        let (status, body) = crate::ubiservices::get_raw(auth, session, &path(chunk)).await?;
        if !(200..300).contains(&status) {
            let short: String = body.chars().take(200).collect();
            anyhow::bail!("games API returned {status}: {short}");
        }
        out.extend(parse(&body)?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached {
    pub fetched: DateTime<Utc>,
    /// `None`: Ubisoft returned nothing for this space.
    #[serde(default)]
    pub game: Option<SpaceGame>,
}

/// Answers by space id, kept in a JSON file.
#[derive(Debug, Default)]
pub struct SpaceCache {
    path: PathBuf,
    pub entries: HashMap<String, Cached>,
}

impl SpaceCache {
    pub fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { path: path.to_owned(), entries }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec(&self.entries)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Of `ids`, those never asked for or asked for longer than [`TTL`] ago.
    pub fn stale<'a>(&self, ids: impl IntoIterator<Item = &'a str>, now: DateTime<Utc>) -> Vec<String> {
        let mut out: Vec<String> = ids
            .into_iter()
            .filter(|id| valid_space_id(id))
            .filter(|id| self.entries.get(*id).is_none_or(|c| now - c.fetched > TTL))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Record an answer to a request for `asked`: spaces it didn't return are recorded as
    /// having no game.
    pub fn record(&mut self, asked: &[String], games: Vec<SpaceGame>, now: DateTime<Utc>) {
        let mut by_id: HashMap<String, SpaceGame> = games.into_iter().map(|g| (g.space_id.clone(), g)).collect();
        for id in asked {
            let game = by_id.remove(id);
            self.entries.insert(id.clone(), Cached { fetched: now, game });
        }
        for (id, g) in by_id {
            self.entries.insert(id, Cached { fetched: now, game: Some(g) });
        }
    }

    pub fn get(&self, space_id: &str) -> Option<&SpaceGame> {
        self.entries.get(space_id)?.game.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/games-api/valhalla.json")).unwrap()
    }

    #[test]
    fn parses_connects_answer() {
        let games = parse(&fixture()).unwrap();
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.space_id, "c4f15d67-1300-4e9e-bbe3-12e11a148e81");
        assert_eq!(g.display_name.as_deref(), Some("Assassin's Creed® Valhalla"));
        assert_eq!(g.release_date().as_deref(), Some("2020-11-10"));
        let a = g.art();
        let base = "https://ubiservices.cdn.ubi.com/c4f15d67-1300-4e9e-bbe3-12e11a148e81/spaceCardAsset/";
        // highBoxArt is null here, so the low one; no flat logo, so the color one.
        assert_eq!(a.tile.as_deref(), Some(&*format!("{base}lowBoxArtAssetId.jpg")));
        assert_eq!(a.hero.as_deref(), Some(&*format!("{base}backgroundAssetIdv2.jpg")));
        assert_eq!(a.logo.as_deref(), Some(&*format!("{base}LogoColor.png")));
        assert_eq!(a.icon.as_deref(), Some(&*format!("{base}gameIconAssetId.png")));
        // "ttps://www.facebook.com/…" in the real answer is dropped.
        let links = g.links();
        assert!(links.iter().any(|(k, u)| k == "website" && u.starts_with("https://www.ubisoft.com/")));
        assert!(!links.iter().any(|(k, _)| k == "facebook"));
    }

    #[test]
    fn shaped_like_black_flag() {
        // The fields Connect's cache showed for AC IV; extra and missing fields are fine.
        let body = r#"{"games":[{"spaceId":"c8b3f1c6-a246-4ffe-a837-a49d6bb0ce52","installmentDefault":true,
            "imageUrls":{"lowThumbnail":"https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/gameThumbnail_mobile.jpg",
              "background":"https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/backgroundAssetId.jpg",
              "lowBoxArt":"https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/boxArt_mobile.jpg",
              "highBoxArt":"https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/boxArt.png",
              "gameLogoFlat":"https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/LogoFlat.png",
              "gameLogoColor":null,"gameAvatar":"https://ubiservices.cdn.ubi.com/c8b3/Club_Asset/Avatar.jpg"},
            "linkUrls":null,"siblingGames":[{"spaceId":"x"}],"releaseAt":"not a date","ageRating":{}},
            {"displayName":"no space id"}, 7]}"#;
        let games = parse(body).unwrap();
        assert_eq!(games.len(), 1);
        let a = games[0].art();
        assert_eq!(a.tile.as_deref(), Some("https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/boxArt.png"));
        assert_eq!(a.logo.as_deref(), Some("https://ubiservices.cdn.ubi.com/c8b3/spaceCardAsset/LogoFlat.png"));
        assert!(games[0].links().is_empty());
        assert_eq!(games[0].release_date(), None);
        // A wrong-shaped imageUrls loses the art, not the game.
        let games = parse(r#"{"games":[{"spaceId":"ab-12","imageUrls":"nope","releaseAt":"2013-11-19T00:00:00Z"}]}"#).unwrap();
        assert!(games[0].art().is_empty());
        assert_eq!(games[0].release_date().as_deref(), Some("2013-11-19"));
        assert!(parse("<html>").is_err());
        assert!(parse("{}").unwrap().is_empty());
    }

    #[test]
    fn cache_ttl_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("spaces.json");
        let now = Utc::now();
        let mut c = SpaceCache::load(&file);
        assert_eq!(c.stale(["b-1", "a-2", "a-2", "../x"], now), ["a-2", "b-1"]);
        let g = SpaceGame { space_id: "a-2".into(), release_at: Some("2013-11-19T00:00:00Z".into()), ..Default::default() };
        c.record(&["a-2".into(), "b-1".into()], vec![g], now);
        c.save().unwrap();
        let c = SpaceCache::load(&file);
        assert!(c.get("a-2").is_some() && c.get("b-1").is_none());
        // Both answered (b-1 with nothing), so neither is asked again until the TTL is up.
        assert!(c.stale(["a-2", "b-1"], now).is_empty());
        assert_eq!(c.stale(["a-2", "b-1"], now + TTL + chrono::TimeDelta::minutes(1)), ["a-2", "b-1"]);
        assert_eq!(path(&["a", "b"]), "/v1/spaces/global/ubiconnect/games/api?spaceIds=a,b");
    }
}
