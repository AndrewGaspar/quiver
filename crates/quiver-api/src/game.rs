//! Game identity across stores (design/STORES.md §2).
//!
//! A game is named by a store-qualified [`GameRef`]: `steam:620`, `epic:Camellia`. A bare
//! number is a Ubisoft product, and every key Ubisoft data already uses stays bare, so
//! nothing on disk moves:
//!
//! | Form | Ubisoft | Other stores |
//! |---|---|---|
//! | display, CLI, JSON ([`fmt::Display`], serde) | `ubisoft:3539` (read: also `3539`, `ubi:3539`, a JSON number) | `steam:620` |
//! | key ([`GameRef::key`]: settings scopes, export keys) | `3539` | `steam:620` |
//! | data folder ([`GameRef::dir_name`]) | `3539` | `steam-620` |
//!
//! Types are `Copy`: an id is at most [`GameId::MAX`] bytes, kept inline, so a `GameRef`
//! goes wherever a `u32` product id went (view state, events, map keys).
//!
//! On the wire, requests take `game` (`#[serde(alias = "product_id")]`), and records that
//! used to carry `product_id` carry `game` too, with `product_id` still written for Ubisoft
//! ([`wire`], [`persisted`]).

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A store Quiver knows. Serialized lowercase (`"steam"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreId {
    Ubisoft,
    Steam,
    Epic,
    Gog,
    Ea,
    Meta,
}

impl StoreId {
    pub const ALL: [StoreId; 6] = [StoreId::Ubisoft, StoreId::Steam, StoreId::Epic, StoreId::Gog, StoreId::Ea, StoreId::Meta];

    /// `ubisoft`, `steam`, …: the prefix of a ref and the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            StoreId::Ubisoft => "ubisoft",
            StoreId::Steam => "steam",
            StoreId::Epic => "epic",
            StoreId::Gog => "gog",
            StoreId::Ea => "ea",
            StoreId::Meta => "meta",
        }
    }

    /// How a front end names the store: `Ubisoft`, `Steam`, `GOG`, …
    pub fn label(self) -> &'static str {
        match self {
            StoreId::Ubisoft => "Ubisoft",
            StoreId::Steam => "Steam",
            StoreId::Epic => "Epic",
            StoreId::Gog => "GOG",
            StoreId::Ea => "EA",
            StoreId::Meta => "Meta",
        }
    }

    /// A store name as typed: any case, `ubi` for Ubisoft.
    pub fn parse(s: &str) -> Option<StoreId> {
        let s = s.trim().to_ascii_lowercase();
        if s == "ubi" {
            return Some(StoreId::Ubisoft);
        }
        StoreId::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// Stores whose ids are numbers that fit a `u32` (checked when a ref is made).
    fn numeric(self) -> bool {
        matches!(self, StoreId::Ubisoft | StoreId::Steam)
    }
}

impl fmt::Display for StoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StoreId {
    type Err = GameRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StoreId::parse(s).ok_or_else(|| GameRefError(format!("unknown store '{s}' (ubisoft, steam, epic, gog, ea, meta)")))
    }
}

/// A store's own id for a game: `[A-Za-z0-9][A-Za-z0-9._-]{0,62}`, safe in paths, URLs,
/// VDF and environment values. Kept inline (no allocation), so it is `Copy`.
#[derive(Clone, Copy)]
pub struct GameId {
    len: u8,
    buf: [u8; GameId::MAX],
}

impl GameId {
    /// The longest id, in bytes. Store ids seen so far are under 40 (Epic app names, EA
    /// offer ids like `Origin.OFR.50.0002694`).
    pub const MAX: usize = 63;

    pub fn new(s: &str) -> Result<GameId, GameRefError> {
        let b = s.as_bytes();
        let ok_first = b.first().is_some_and(u8::is_ascii_alphanumeric);
        let ok_rest = b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        if !ok_first || !ok_rest || b.len() > Self::MAX {
            return Err(GameRefError(format!("bad game id '{s}' (letters, digits, '.', '_' and '-', at most {} long)", Self::MAX)));
        }
        let mut buf = [0u8; Self::MAX];
        buf[..b.len()].copy_from_slice(b);
        Ok(GameId { len: b.len() as u8, buf })
    }

    pub fn from_u32(n: u32) -> GameId {
        // A decimal number always passes the check.
        GameId::new(&n.to_string()).expect("digits are a valid id")
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII is ever stored.
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or_default()
    }
}

impl std::ops::Deref for GameId {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for GameId {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for GameId {}

impl std::hash::Hash for GameId {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.as_str().hash(h)
    }
}

impl PartialOrd for GameId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for GameId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Debug for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One store's record of a game (design/STORES.md §2.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameRef {
    pub store: StoreId,
    pub id: GameId,
}

/// A game reference that doesn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRefError(pub String);

impl fmt::Display for GameRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GameRefError {}

impl GameRef {
    pub fn new(store: StoreId, id: &str) -> Result<GameRef, GameRefError> {
        let id = GameId::new(id)?;
        if store.numeric() && id.parse::<u32>().is_err() {
            return Err(GameRefError(format!("{store} ids are numbers, got '{id}'")));
        }
        Ok(GameRef { store, id })
    }

    /// A Ubisoft product.
    pub fn ubisoft(product_id: u32) -> GameRef {
        GameRef { store: StoreId::Ubisoft, id: GameId::from_u32(product_id) }
    }

    /// A Steam app.
    pub fn steam(app_id: u32) -> GameRef {
        GameRef { store: StoreId::Steam, id: GameId::from_u32(app_id) }
    }

    /// The Ubisoft product id, for a Ubisoft ref.
    pub fn as_ubisoft(&self) -> Option<u32> {
        self.number_in(StoreId::Ubisoft)
    }

    /// The Steam app id, for a Steam ref.
    pub fn as_steam(&self) -> Option<u32> {
        self.number_in(StoreId::Steam)
    }

    fn number_in(&self, store: StoreId) -> Option<u32> {
        (self.store == store).then(|| self.id.parse().ok()).flatten()
    }

    pub fn is_ubisoft(&self) -> bool {
        self.store == StoreId::Ubisoft
    }

    /// `3539` | `steam:620`: export keys, settings scopes (`hud.game.<key>`).
    pub fn key(&self) -> String {
        match self.store {
            StoreId::Ubisoft => self.id.to_string(),
            s => format!("{s}:{}", self.id),
        }
    }

    /// `3539` | `steam-620`: the game's folder under `<data>/games/` and `<data>/mods/`.
    /// Ubisoft folders are all digits and no store name has a `-`, so they never collide.
    pub fn dir_name(&self) -> String {
        match self.store {
            StoreId::Ubisoft => self.id.to_string(),
            s => format!("{s}-{}", self.id),
        }
    }

    /// Parse a reference as typed or sent: `3539`, `ubi:3539`, `ubisoft:3539`, `steam:620`,
    /// `Steam:620`. A bare id is a Ubisoft product.
    pub fn parse(s: &str) -> Result<GameRef, GameRefError> {
        let s = s.trim();
        match s.split_once(':') {
            Some((store, id)) => GameRef::new(store.parse()?, id),
            None => GameRef::new(StoreId::Ubisoft, s),
        }
    }

    /// Like [`GameRef::parse`], but only for something that looks like a reference (a
    /// number, or `<store>:<id>` with a known store): a game name typed on the command line
    /// (`Far Cry 3: Blood Dragon`) is not one.
    pub fn parse_ref(s: &str) -> Option<GameRef> {
        let s = s.trim();
        if s.bytes().all(|b| b.is_ascii_digit()) {
            return GameRef::parse(s).ok();
        }
        let (store, _) = s.split_once(':')?;
        StoreId::parse(store)?;
        GameRef::parse(s).ok()
    }
}

impl fmt::Display for GameRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.store, self.id)
    }
}

impl fmt::Debug for GameRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GameRef({self})")
    }
}

impl FromStr for GameRef {
    type Err = GameRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        GameRef::parse(s)
    }
}

impl From<u32> for GameRef {
    /// A Ubisoft product id.
    fn from(product_id: u32) -> Self {
        GameRef::ubisoft(product_id)
    }
}

impl Serialize for GameRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A request's game, as a field that used to be `product_id: u32`: a Ubisoft game goes out
/// as `"product_id": <number>` (daemons from before game refs read it), any other as
/// `"game": "<store>:<id>"`; either key is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameField(pub GameRef);

impl From<GameRef> for GameField {
    fn from(g: GameRef) -> Self {
        GameField(g)
    }
}

impl PartialEq<GameRef> for GameField {
    fn eq(&self, o: &GameRef) -> bool {
        self.0 == *o
    }
}

impl std::ops::Deref for GameField {
    type Target = GameRef;
    fn deref(&self) -> &GameRef {
        &self.0
    }
}

impl Serialize for GameField {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(1))?;
        match self.0.as_ubisoft() {
            Some(n) => m.serialize_entry("product_id", &n)?,
            None => m.serialize_entry("game", &self.0)?,
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for GameField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            game: Option<GameRef>,
            #[serde(default)]
            product_id: Option<GameRef>,
        }
        let r = Raw::deserialize(d)?;
        r.game.or(r.product_id).map(GameField).ok_or_else(|| de::Error::missing_field("game"))
    }
}

impl<'de> Deserialize<'de> for GameRef {
    /// From a JSON number (a Ubisoft product, as `product_id` always was) or any string form.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = GameRef;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a game reference: a Ubisoft product id or \"<store>:<id>\"")
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<GameRef, E> {
                u32::try_from(n).map(GameRef::ubisoft).map_err(|_| E::custom(format!("product id {n} is too large")))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<GameRef, E> {
                u64::try_from(n).map_err(|_| E::custom(format!("product id {n} is negative"))).and_then(|n| self.visit_u64(n))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<GameRef, E> {
                GameRef::parse(s).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for GameId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GameId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = GameId;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a game id")
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<GameId, E> {
                GameId::new(&n.to_string()).map_err(E::custom)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<GameId, E> {
                GameId::new(s).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

/// Both keys, for both directions of a record that names its game (STORES.md §2.3).
#[derive(Serialize, Deserialize)]
struct Keys {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    game: Option<GameRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    product_id: Option<u32>,
}

impl Keys {
    fn into_ref<E: de::Error>(self) -> Result<GameRef, E> {
        self.game.or(self.product_id.map(GameRef::ubisoft)).ok_or_else(|| E::missing_field("game"))
    }
}

/// For responses and events: `#[serde(flatten, with = "game::wire")]` on a `GameRef`
/// field writes `"game": "<ref>"` and, for a Ubisoft game, the old `"product_id": <n>` too
/// (so `--json` consumers keep working). Reads either; `game` wins.
pub mod wire {
    use super::*;

    pub fn serialize<S: Serializer>(g: &GameRef, s: S) -> Result<S::Ok, S::Error> {
        Keys { game: Some(*g), product_id: g.as_ubisoft() }.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<GameRef, D::Error> {
        Keys::deserialize(d)?.into_ref()
    }
}

/// For files Quiver keeps (activity, records): `#[serde(flatten, with = "game::persisted")]`
/// writes a Ubisoft game exactly as before, `"product_id": <n>` alone, so an older quiverd
/// still reads it; any other store writes `"game": "<ref>"`. Reads either.
pub mod persisted {
    use super::*;

    pub fn serialize<S: Serializer>(g: &GameRef, s: S) -> Result<S::Ok, S::Error> {
        match g.as_ubisoft() {
            Some(id) => Keys { game: None, product_id: Some(id) }.serialize(s),
            None => Keys { game: Some(*g), product_id: None }.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<GameRef, D::Error> {
        Keys::deserialize(d)?.into_ref()
    }

    /// The same for an optional game: nothing is written for `None`.
    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(g: &Option<GameRef>, s: S) -> Result<S::Ok, S::Error> {
            match g {
                Some(g) => super::serialize(g, s),
                None => Keys { game: None, product_id: None }.serialize(s),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<GameRef>, D::Error> {
            let k = Keys::deserialize(d)?;
            Ok(k.game.or(k.product_id.map(GameRef::ubisoft)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forms() {
        let u = GameRef::ubisoft(3539);
        assert_eq!((u.to_string(), u.key(), u.dir_name()), ("ubisoft:3539".into(), "3539".into(), "3539".into()));
        let s = GameRef::steam(620);
        assert_eq!((s.to_string(), s.key(), s.dir_name()), ("steam:620".into(), "steam:620".into(), "steam-620".into()));
        let e = GameRef::new(StoreId::Epic, "Camellia").unwrap();
        assert_eq!((e.to_string(), e.key(), e.dir_name()), ("epic:Camellia".into(), "epic:Camellia".into(), "epic-Camellia".into()));
        assert_eq!(u.as_ubisoft(), Some(3539));
        assert_eq!(s.as_ubisoft(), None);
        assert_eq!(s.as_steam(), Some(620));
        assert_eq!(GameRef::from(5059), GameRef::ubisoft(5059));
    }

    #[test]
    fn parsing() {
        for s in ["3539", "ubi:3539", "ubisoft:3539", "Ubisoft:3539", " 3539 "] {
            assert_eq!(GameRef::parse(s).unwrap(), GameRef::ubisoft(3539), "{s}");
        }
        assert_eq!(GameRef::parse("steam:620").unwrap(), GameRef::steam(620));
        assert_eq!(GameRef::parse("STEAM:620").unwrap(), GameRef::steam(620));
        assert_eq!(GameRef::parse("ea:Origin.OFR.50.0002694").unwrap().id.as_str(), "Origin.OFR.50.0002694");
        for bad in ["", "steam:", "steam:abc", "ubisoft:-1", "nope:1", "epic:../x", "epic:a b", "3539x", &"a".repeat(64)] {
            assert!(GameRef::parse(bad).is_err(), "{bad}");
        }
        assert!(GameRef::parse(&format!("epic:{}", "a".repeat(63))).is_ok());
        // Names are not refs.
        assert_eq!(GameRef::parse_ref("Far Cry 3: Blood Dragon"), None);
        assert_eq!(GameRef::parse_ref("origins"), None);
        assert_eq!(GameRef::parse_ref("steam:620"), Some(GameRef::steam(620)));
        assert_eq!(GameRef::parse_ref("3539"), Some(GameRef::ubisoft(3539)));
    }

    #[test]
    fn serde_number_or_string() {
        let g: GameRef = serde_json::from_value(json!(3539)).unwrap();
        assert_eq!(g, GameRef::ubisoft(3539));
        let g: GameRef = serde_json::from_value(json!("ubi:3539")).unwrap();
        assert_eq!(g, GameRef::ubisoft(3539));
        let g: GameRef = serde_json::from_value(json!("steam:620")).unwrap();
        assert_eq!(g, GameRef::steam(620));
        assert!(serde_json::from_value::<GameRef>(json!(-1)).is_err());
        assert!(serde_json::from_value::<GameRef>(json!(5_000_000_000u64)).is_err());
        assert_eq!(serde_json::to_value(GameRef::steam(620)).unwrap(), json!("steam:620"));
        assert_eq!(serde_json::to_value(GameRef::ubisoft(1)).unwrap(), json!("ubisoft:1"));
        // As a map key (JSON object keys are strings).
        let m: std::collections::BTreeMap<GameRef, u8> = serde_json::from_value(json!({"3539": 1, "steam:620": 2})).unwrap();
        assert_eq!(m[&GameRef::ubisoft(3539)], 1);
        assert_eq!(StoreId::parse("ubi"), Some(StoreId::Ubisoft));
        assert_eq!(serde_json::to_value(StoreId::Gog).unwrap(), json!("gog"));
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct OnWire {
        #[serde(flatten, with = "wire")]
        game: GameRef,
        name: String,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct OnDisk {
        #[serde(flatten, with = "persisted")]
        game: GameRef,
        n: u32,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct MaybeOnDisk {
        #[serde(flatten, with = "persisted::option")]
        game: Option<GameRef>,
        n: u32,
    }

    #[test]
    fn keyed_records() {
        let w = OnWire { game: GameRef::ubisoft(3539), name: "x".into() };
        assert_eq!(serde_json::to_value(&w).unwrap(), json!({"game": "ubisoft:3539", "product_id": 3539, "name": "x"}));
        let w = OnWire { game: GameRef::steam(620), name: "x".into() };
        assert_eq!(serde_json::to_value(&w).unwrap(), json!({"game": "steam:620", "name": "x"}));
        // An older daemon's record, and a newer one's.
        let old: OnWire = serde_json::from_str(r#"{"product_id":3539,"name":"x"}"#).unwrap();
        assert_eq!(old.game, GameRef::ubisoft(3539));
        let new: OnWire = serde_json::from_str(r#"{"game":"steam:620","name":"x"}"#).unwrap();
        assert_eq!(new.game, GameRef::steam(620));
        assert!(serde_json::from_str::<OnWire>(r#"{"name":"x"}"#).is_err());

        // Files: Ubisoft records are byte-identical to before.
        let d = OnDisk { game: GameRef::ubisoft(3539), n: 1 };
        assert_eq!(serde_json::to_string(&d).unwrap(), r#"{"product_id":3539,"n":1}"#);
        assert_eq!(serde_json::from_str::<OnDisk>(r#"{"product_id":3539,"n":1}"#).unwrap(), d);
        let d = OnDisk { game: GameRef::steam(620), n: 1 };
        assert_eq!(serde_json::to_string(&d).unwrap(), r#"{"game":"steam:620","n":1}"#);
        assert_eq!(serde_json::from_str::<OnDisk>(r#"{"game":"steam:620","n":1}"#).unwrap(), d);

        let m = MaybeOnDisk { game: None, n: 2 };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"n":2}"#);
        assert_eq!(serde_json::from_str::<MaybeOnDisk>(r#"{"n":2}"#).unwrap(), m);
        assert_eq!(serde_json::from_str::<MaybeOnDisk>(r#"{"product_id":null,"n":2}"#).unwrap(), m);
        let m = MaybeOnDisk { game: Some(GameRef::ubisoft(7)), n: 2 };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"product_id":7,"n":2}"#);
    }

    #[test]
    fn copy_and_small() {
        fn is_copy<T: Copy>() {}
        is_copy::<GameRef>();
        assert!(std::mem::size_of::<GameRef>() <= 66);
    }
}
