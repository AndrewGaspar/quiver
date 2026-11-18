//! Achievement unlock status and reporting.
//!
//! Two upstream mechanisms exist (`tools/recon/achievements-report-path.md`):
//!
//! - **demux `ach_frontend`** (`mg.protocol.uplay`): `ReadAchievementsReq{user_id,
//!   Product{product_id, platform_id}}` reads the account's unlocks with timestamps;
//!   `WriteAchievementsReq{AchievementBlob}` reports new ones. The connection authenticates
//!   with `AuthenticateReq{orbit_token}`; the product id is the config's
//!   `uplay.achievements_sync_id` (`ACE`, `WD2`, `5183`). No request signing. Used for
//!   both reading and reporting.
//! - **UbiServices REST `legacyPlayerAchievements`**: `GET …?spaceId=` (`completed` per
//!   achievement, no timestamps) is read as a second source. Its POST (`/completions`) is
//!   signed with an unknown `Ubi-Signature` scheme and is not used.
//!
//! The [`Engine`] keeps a **journal** per account and game (`<data>/achievements/<user>/
//! <product>.json`): the last unlock set read from Ubisoft and every achievement a game
//! earned through a bridge that Ubisoft didn't already have, with its report state.
//! Games re-send their whole unlocked set (Origins does at every save load), so an earn is
//! checked against Ubisoft's set and the journal before anything is sent; new ones are
//! written (batched per game), confirmed with a read-back, and retried while pending.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use futures::future::BoxFuture;
use prost::Message;
use serde::{Deserialize, Serialize};
use quiver_api::achievements::{JournalEntry, ReportState};
use quiver_api::info::{Achievement, Achievements};
use quiver_proto::uplay;

use crate::demux::{AchFrontend, AchFrontendWrapped, Demux, Service, ServiceConnection};

/// `Product.platform_id` until a read shows the one Ubisoft uses for the product.
pub const DEFAULT_PLATFORM: &str = "PC";
/// How long a read of Ubisoft's unlock set is reused for deduplicating and listing.
pub const READ_TTL: Duration = Duration::from_secs(10 * 60);
/// Attempts that reached Ubisoft before a pending report is marked failed.
pub const MAX_ATTEMPTS: u32 = 8;

// ---------------------------------------------------------------------------------------
// Protocol

/// An `ach_frontend` product: `uplay.achievements_sync_id` and the platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AchProduct {
    pub sync_id: String,
    pub platform_id: String,
}

impl AchProduct {
    pub fn new(sync_id: &str) -> Self {
        Self { sync_id: sync_id.to_owned(), platform_id: DEFAULT_PLATFORM.to_owned() }
    }

    fn proto(&self) -> uplay::Product {
        uplay::Product { product_id: self.sync_id.clone(), platform_id: self.platform_id.clone() }
    }
}

/// How `Achievement.timestamp` counts. Unknown upstream; learned from the magnitude of the
/// timestamps a read returns, Unix seconds until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsUnit {
    #[default]
    Seconds,
    Millis,
    /// Windows FILETIME: 100 ns since 1601.
    FileTime,
}

const FILETIME_UNIX: u64 = 116_444_736_000_000_000;

impl TsUnit {
    pub fn detect(ts: u64) -> Option<Self> {
        match ts {
            0 => None,
            t if t >= 10_000_000_000_000_000 => Some(Self::FileTime),
            t if t >= 100_000_000_000 => Some(Self::Millis),
            _ => Some(Self::Seconds),
        }
    }

    pub fn decode(self, ts: u64) -> Option<DateTime<Utc>> {
        let ms = match self {
            Self::Seconds => ts.checked_mul(1000)?,
            Self::Millis => ts,
            Self::FileTime => ts.checked_sub(FILETIME_UNIX)? / 10_000,
        };
        Utc.timestamp_millis_opt(i64::try_from(ms).ok()?).single()
    }

    pub fn encode(self, t: DateTime<Utc>) -> u64 {
        let ms = t.timestamp_millis().max(0) as u64;
        match self {
            Self::Seconds => ms / 1000,
            Self::Millis => ms,
            Self::FileTime => ms * 10_000 + FILETIME_UNIX,
        }
    }
}

/// `AchievementBlob{ ProductAchievements{ Product, Achievements{ Achievement{id, ts} } } }`
/// for one product.
pub fn blob(product: &AchProduct, items: &[(u32, u64)]) -> uplay::AchievementBlob {
    uplay::AchievementBlob {
        product_achievements: vec![uplay::ProductAchievements {
            product: product.proto(),
            achievements: uplay::Achievements {
                achievements: items.iter().map(|&(id, ts)| uplay::Achievement { achievement_id: id, timestamp: Some(ts) }).collect(),
            },
        }],
    }
}

pub fn write_req(product: &AchProduct, items: &[(u32, u64)]) -> uplay::Req {
    uplay::Req {
        write_achievements_req: Some(uplay::WriteAchievementsReq { achievement_blob: Some(blob(product, items)) }),
        ..Default::default()
    }
}

/// `product: None` asks for every product of the account.
pub fn read_req(user_id: &str, product: Option<&AchProduct>) -> uplay::Req {
    uplay::Req {
        read_achievements_req: Some(uplay::ReadAchievementsReq { user_id: Some(user_id.to_owned()), product: product.map(AchProduct::proto) }),
        ..Default::default()
    }
}

pub fn auth_req(orbit_token: &str) -> uplay::Req {
    uplay::Req { authenticate_req: Some(uplay::AuthenticateReq { orbit_token: orbit_token.to_owned() }), ..Default::default() }
}

/// What a read says about one product.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlobRead {
    pub earned: BTreeMap<u32, Option<DateTime<Utc>>>,
    /// The product's entry was in the blob.
    pub found: bool,
    /// The platform id the blob used for it.
    pub platform_id: Option<String>,
    pub ts_unit: Option<TsUnit>,
}

/// The unlocks for `sync_id` in a blob (product ids compared case-insensitively; an entry
/// on `platform_id` preferred over one on another platform).
pub fn read_blob(blob: &uplay::AchievementBlob, sync_id: &str, platform_id: Option<&str>) -> BlobRead {
    let mut mine: Vec<&uplay::ProductAchievements> =
        blob.product_achievements.iter().filter(|p| p.product.product_id.eq_ignore_ascii_case(sync_id)).collect();
    mine.sort_by_key(|p| Some(p.product.platform_id.as_str()) != platform_id);
    let Some(first) = mine.first() else { return BlobRead::default() };
    let ts_unit = mine
        .iter()
        .flat_map(|p| &p.achievements.achievements)
        .filter_map(|a| a.timestamp)
        .max()
        .and_then(TsUnit::detect);
    let unit = ts_unit.unwrap_or_default();
    let mut earned = BTreeMap::new();
    // Only the preferred platform's entries, if it has any; else every platform's.
    let platform = first.product.platform_id.clone();
    for p in mine.iter().filter(|p| p.product.platform_id == platform) {
        for a in &p.achievements.achievements {
            let at = a.timestamp.filter(|&t| t > 0).and_then(|t| unit.decode(t));
            earned.entry(a.achievement_id).and_modify(|x: &mut Option<DateTime<Utc>>| *x = x.or(at)).or_insert(at);
        }
    }
    BlobRead { earned, found: true, platform_id: Some(platform), ts_unit }
}

/// A protobuf message as JSON-ish text for logs and dry runs.
pub fn describe_req(req: &uplay::Req) -> serde_json::Value {
    let product = |p: &uplay::Product| serde_json::json!({ "product_id": p.product_id, "platform_id": p.platform_id });
    let blob = |b: &uplay::AchievementBlob| {
        serde_json::Value::Array(
            b.product_achievements
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "product": product(&p.product),
                        "achievements": p.achievements.achievements.iter().map(|a| serde_json::json!({ "achievement_id": a.achievement_id, "timestamp": a.timestamp })).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    };
    let mut v = serde_json::json!({ "request_id": req.request_id });
    if let Some(w) = &req.write_achievements_req {
        v["write_achievements_req"] = serde_json::json!({ "achievement_blob": w.achievement_blob.as_ref().map(blob) });
    }
    if let Some(r) = &req.read_achievements_req {
        v["read_achievements_req"] = serde_json::json!({ "user_id": r.user_id, "product": r.product.as_ref().map(product) });
    }
    if req.authenticate_req.is_some() {
        v["authenticate_req"] = serde_json::json!({ "orbit_token": "(hidden)" });
    }
    v
}

pub fn describe_blob(b: &uplay::AchievementBlob) -> serde_json::Value {
    describe_req(&uplay::Req { write_achievements_req: Some(uplay::WriteAchievementsReq { achievement_blob: Some(b.clone()) }), ..Default::default() })
        ["write_achievements_req"]["achievement_blob"]
        .clone()
}

/// Envelope of `ach_frontend` service frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// Bare `Req`/`Rsp` (the vendored proto has no envelope for this package).
    #[default]
    Raw,
    /// `Upstream{request}`/`Downstream{response}`, like every other service.
    Wrapped,
}

impl Framing {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "raw" => Ok(Self::Raw),
            "wrapped" => Ok(Self::Wrapped),
            other => bail!("framing: expected raw or wrapped, got {other:?}"),
        }
    }
}

/// An authenticated `ach_frontend` connection.
pub enum AchConn {
    Raw(ServiceConnection<AchFrontend>),
    Wrapped(ServiceConnection<AchFrontendWrapped>),
}

impl AchConn {
    /// Open the service and authenticate it with `orbit_token` (the account's UbiServices
    /// ticket, which also authenticates the demux socket).
    pub async fn open(demux: &Demux, orbit_token: &str, framing: Framing) -> Result<Self> {
        let mut c = match framing {
            Framing::Raw => Self::Raw(demux.open::<AchFrontend>().await?),
            Framing::Wrapped => Self::Wrapped(demux.open::<AchFrontendWrapped>().await?),
        };
        let rsp = c.request(auth_req(orbit_token)).await.context("ach_frontend AuthenticateReq")?;
        let ok = rsp.authenticate_rsp.context("ach_frontend: no authenticate_rsp")?.success;
        if !ok {
            bail!("ach_frontend refused the orbit token");
        }
        Ok(c)
    }

    pub async fn request(&mut self, req: uplay::Req) -> Result<uplay::Rsp> {
        match self {
            Self::Raw(c) => c.request(req).await,
            Self::Wrapped(c) => c.request(req).await,
        }
    }

    pub async fn read(&mut self, user_id: &str, product: Option<&AchProduct>) -> Result<uplay::ReadAchievementsRsp> {
        let rsp = self.request(read_req(user_id, product)).await?;
        rsp.read_achievements_rsp.context("ach_frontend: no read_achievements_rsp")
    }

    /// A write counts as accepted when the server answers it with `write_achievements_rsp`.
    pub async fn write(&mut self, req: uplay::Req) -> Result<()> {
        let rsp = self.request(req).await?;
        rsp.write_achievements_rsp.context("ach_frontend answered the write without write_achievements_rsp")?;
        Ok(())
    }
}

/// `S::wrap` and encode, as [`ServiceConnection::request`] puts it on the wire (for logs
/// and dry runs; request id 0).
pub fn encode_frame(req: &uplay::Req, framing: Framing) -> Vec<u8> {
    match framing {
        Framing::Raw => AchFrontend::wrap(req.clone()).encode_to_vec(),
        Framing::Wrapped => AchFrontendWrapped::wrap(req.clone()).encode_to_vec(),
    }
}

// ---------------------------------------------------------------------------------------
// REST legacyPlayerAchievements (read only)

/// The exact prefix is unconfirmed (see the recon note); this is the profiles base the
/// other per-profile reads use.
pub fn legacy_path(user_id: &str, space_id: &str) -> String {
    format!("/v1/profiles/{user_id}/legacyPlayerAchievements?spaceId={space_id}")
}

#[derive(Debug, Default, Deserialize)]
pub struct LegacyResponse {
    #[serde(rename = "legacyPlayerAchievements", default)]
    pub items: Vec<LegacyItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LegacyItem {
    pub achievement_legacy_id: Option<serde_json::Value>,
    pub completed: bool,
    pub display_name: Option<String>,
}

pub fn legacy_earned(rsp: &LegacyResponse) -> BTreeSet<u32> {
    rsp.items
        .iter()
        .filter(|i| i.completed)
        .filter_map(|i| match i.achievement_legacy_id.as_ref()? {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Journal

/// Reporting mode for a game (`achievements` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportMode {
    #[default]
    On,
    Off,
    DryRun,
}

impl ReportMode {
    pub fn from_setting(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("off") => Self::Off,
            Some("dry-run") | Some("dry_run") => Self::DryRun,
            _ => Self::On,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::DryRun => "dry-run",
        }
    }
}

/// One account's record for one game.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub product_id: u32,
    pub account: String,
    /// The `ach_frontend` product, with the platform learned from reads.
    #[serde(default)]
    pub product: Option<AchProduct>,
    #[serde(default)]
    pub ts_unit: Option<TsUnit>,
    /// Ubisoft's unlock set at the last successful read (with unlock times when known).
    #[serde(default)]
    pub known: BTreeMap<u32, Option<DateTime<Utc>>>,
    #[serde(default)]
    pub known_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub known_source: Option<String>,
    #[serde(default)]
    pub entries: Vec<JournalEntry>,
}

/// What an earn did to the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EarnOutcome {
    /// Ubisoft already has it: nothing recorded.
    AlreadyEarned,
    /// Already in the journal (in this state): nothing new.
    AlreadyJournaled(ReportState),
    /// Recorded in this state. `unverified`: Ubisoft's set was unknown.
    New { state: ReportState, unverified: bool },
}

fn valid_account(account: &str) -> Result<()> {
    anyhow::ensure!(
        !account.is_empty() && account.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "bad account id {account:?}"
    );
    Ok(())
}

impl Journal {
    pub fn path(dir: &Path, account: &str, product_id: u32) -> PathBuf {
        dir.join(account).join(format!("{product_id}.json"))
    }

    pub fn load(dir: &Path, account: &str, product_id: u32) -> Result<Self> {
        valid_account(account)?;
        let path = Self::path(dir, account, product_id);
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self { product_id, account: account.to_owned(), ..Default::default() })
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        valid_account(&self.account)?;
        let path = Self::path(dir, &self.account, self.product_id);
        std::fs::create_dir_all(path.parent().context("journal path")?)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Whether Ubisoft's set is known (some read worked).
    pub fn known_valid(&self) -> bool {
        self.known_at.is_some()
    }

    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        self.known_at.is_some_and(|t| (now - t).to_std().is_ok_and(|d| d < READ_TTL))
    }

    pub fn entry(&self, id: u32) -> Option<&JournalEntry> {
        self.entries.iter().find(|e| e.achievement_id == id)
    }

    fn entry_mut(&mut self, id: u32) -> Option<&mut JournalEntry> {
        self.entries.iter_mut().find(|e| e.achievement_id == id)
    }

    /// The game earned `id` at `now`: deduplicated against Ubisoft's set and the journal.
    pub fn record_earn(&mut self, id: u32, now: DateTime<Utc>, mode: ReportMode) -> EarnOutcome {
        if let Some(e) = self.entry(id) {
            return EarnOutcome::AlreadyJournaled(e.state);
        }
        if self.known.contains_key(&id) {
            return EarnOutcome::AlreadyEarned;
        }
        let state = match mode {
            ReportMode::On => ReportState::Pending,
            ReportMode::Off => ReportState::Off,
            ReportMode::DryRun => ReportState::DryRun,
        };
        let unverified = !self.known_valid();
        self.entries.push(JournalEntry {
            achievement_id: id,
            state,
            earned_at: now,
            reported_at: None,
            attempts: 0,
            last_error: None,
            confirmed: None,
            unverified,
            via: "game".into(),
        });
        EarnOutcome::New { state, unverified }
    }

    /// Take in a successful read. Returns the unverified entries it showed to be new (to
    /// announce now).
    pub fn apply_read(&mut self, earned: BTreeMap<u32, Option<DateTime<Utc>>>, source: &str, now: DateTime<Utc>) -> Vec<u32> {
        let mut confirmed_new = Vec::new();
        for e in &mut self.entries {
            let upstream = earned.contains_key(&e.achievement_id);
            if e.unverified {
                e.unverified = false;
                if upstream {
                    e.state = ReportState::AlreadyEarned;
                } else {
                    confirmed_new.push(e.achievement_id);
                }
                continue;
            }
            match e.state {
                ReportState::Reported => {
                    if e.confirmed != Some(true) {
                        e.confirmed = Some(upstream);
                    }
                }
                // Someone else (Connect, another machine) reported it meanwhile.
                ReportState::Pending | ReportState::Failed if upstream => {
                    e.state = ReportState::AlreadyEarned;
                    e.last_error = None;
                }
                _ => {}
            }
        }
        self.known = earned;
        self.known_at = Some(now);
        self.known_source = Some(source.to_owned());
        confirmed_new
    }

    /// Pending reports that can be sent (Ubisoft's set was checked for them).
    pub fn sendable(&self) -> Vec<(u32, DateTime<Utc>)> {
        self.entries.iter().filter(|e| e.state == ReportState::Pending && !e.unverified).map(|e| (e.achievement_id, e.earned_at)).collect()
    }

    pub fn needs_check(&self) -> bool {
        self.entries.iter().any(|e| e.unverified)
    }

    pub fn mark_reported(&mut self, ids: &[u32], now: DateTime<Utc>) {
        for &id in ids {
            if let Some(e) = self.entry_mut(id) {
                e.state = ReportState::Reported;
                e.reported_at = Some(now);
                e.attempts += 1;
                e.last_error = None;
                e.confirmed = None;
                let at = e.earned_at;
                self.known.entry(id).or_insert(Some(at));
            }
        }
    }

    /// A write failed. `reached`: it got to Ubisoft (an unanswered or refused write), not
    /// just offline; only those count towards giving up.
    pub fn mark_failed(&mut self, ids: &[u32], error: &str, reached: bool) {
        for &id in ids {
            if let Some(e) = self.entry_mut(id) {
                e.last_error = Some(error.to_owned());
                if reached {
                    e.attempts += 1;
                    if e.attempts >= MAX_ATTEMPTS && e.state == ReportState::Pending {
                        e.state = ReportState::Failed;
                    }
                }
            }
        }
    }

    pub fn has_sendable(&self) -> bool {
        self.entries.iter().any(|e| e.state == ReportState::Pending)
    }
}

/// Merge Ubisoft's set and the journal into the definitions. Achievements Ubisoft or the
/// journal has that the definitions lack are added (no name).
pub fn merge(a: &mut Achievements, j: &Journal) {
    let local: HashMap<u32, &JournalEntry> = j.entries.iter().map(|e| (e.achievement_id, e)).collect();
    let mut seen = BTreeSet::new();
    for item in &mut a.items {
        let Ok(id) = item.id.trim().parse::<u32>() else { continue };
        seen.insert(id);
        apply(item, id, j, local.get(&id).copied());
    }
    let extra: BTreeSet<u32> = j.known.keys().chain(local.keys()).copied().filter(|id| !seen.contains(id)).collect();
    for id in extra {
        let mut item = Achievement { id: id.to_string(), ..Default::default() };
        apply(&mut item, id, j, local.get(&id).copied());
        a.items.push(item);
    }
    a.unlocked_known = j.known_valid();
    a.unlocked = a.items.iter().filter(|x| x.unlocked).count() as u32;
    a.total = a.total.max(a.items.len() as u32);
    a.source = j.known_source.clone();
}

fn apply(item: &mut Achievement, id: u32, j: &Journal, local: Option<&JournalEntry>) {
    let upstream = j.known.get(&id);
    item.unlocked = upstream.is_some() || local.is_some_and(|e| e.state != ReportState::AlreadyEarned);
    item.unlocked_at = upstream.copied().flatten().or(local.map(|e| e.earned_at)).filter(|_| item.unlocked);
    item.report = local.map(|e| e.state);
}

// ---------------------------------------------------------------------------------------
// Upstream (what the engine needs from Ubisoft; faked in tests)

/// The game's achievement facts from its product config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AchRefs {
    /// `uplay.achievements_sync_id`.
    pub sync_id: Option<String>,
    /// `uplay.achievements`: the definition set on the asset CDN.
    pub asset: Option<String>,
    pub space_id: Option<String>,
}

impl AchRefs {
    pub fn from_config(cfg: &crate::library::ProductConfig, space_id: Option<String>) -> Self {
        let uplay = &cfg.root()["uplay"];
        let s = |v: &serde_yaml::Value| match v {
            serde_yaml::Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
            serde_yaml::Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Self {
            sync_id: s(&uplay["achievements_sync_id"]),
            asset: s(&uplay["achievements"]),
            space_id: space_id.filter(|s| !s.is_empty()).or_else(|| s(&cfg.root()["space_id"])),
        }
    }

    pub fn from_product(p: &crate::library::Product) -> Self {
        let cfg = p.configuration.as_deref().and_then(|c| crate::library::ProductConfig::parse(c).ok());
        match cfg {
            Some(cfg) => Self::from_config(&cfg, p.space_id.clone()),
            None => Self { space_id: p.space_id.clone(), ..Default::default() },
        }
    }
}

/// Couldn't reach Ubisoft at all (not signed in, no connection): a report attempt that
/// doesn't count towards giving up.
#[derive(Debug)]
pub struct Offline(pub String);

impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline: {}", self.0)
    }
}

impl std::error::Error for Offline {}

pub fn is_offline(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Offline>().is_some())
}

pub trait Upstream: Send + Sync {
    /// The user id of `account` (user id; `None`: the default account).
    fn user_id<'a>(&'a self, account: Option<&'a str>) -> BoxFuture<'a, Result<String>>;
    fn refs<'a>(&'a self, user: &'a str, product_id: u32) -> BoxFuture<'a, Result<AchRefs>>;
    fn read<'a>(&'a self, user: &'a str, product: Option<&'a AchProduct>, framing: Framing) -> BoxFuture<'a, Result<uplay::ReadAchievementsRsp>>;
    fn write<'a>(&'a self, user: &'a str, req: uplay::Req, framing: Framing) -> BoxFuture<'a, Result<()>>;
    fn read_legacy<'a>(&'a self, user: &'a str, space_id: &'a str) -> BoxFuture<'a, Result<BTreeSet<u32>>>;
    fn definitions<'a>(&'a self, asset: &'a str, locale: &'a str) -> BoxFuture<'a, Result<Achievements>>;
}

/// The real thing: the account's shared demux socket (`Online`), UbiServices REST and the
/// asset CDN.
pub struct LiveUpstream {
    pub accounts: Arc<crate::online::Accounts>,
    /// Achievement-set cache (`<cache>/achievements`).
    pub cache: PathBuf,
}

impl LiveUpstream {
    fn online(&self, user: &str) -> Result<Arc<crate::online::Online>> {
        self.accounts.get(Some(user)).map_err(|e| anyhow::Error::new(Offline(format!("{e:#}"))))
    }

    async fn conn(&self, user: &str, framing: Framing) -> Result<AchConn> {
        let online = self.online(user)?;
        let session = online.session().await.map_err(|e| Offline(format!("{e:#}")))?;
        let demux = online.demux().await.map_err(|e| Offline(format!("{e:#}")))?;
        AchConn::open(&demux, &session.ticket, framing).await
    }
}

impl Upstream for LiveUpstream {
    fn user_id<'a>(&'a self, account: Option<&'a str>) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let online = self.accounts.get(account)?;
            match online.user_id() {
                Some(id) => Ok(id.to_owned()),
                None => Ok(online.session().await?.user_id),
            }
        })
    }

    fn refs<'a>(&'a self, user: &'a str, product_id: u32) -> BoxFuture<'a, Result<AchRefs>> {
        Box::pin(async move {
            let online = self.online(user)?;
            let demux = online.demux().await.map_err(|e| Offline(format!("{e:#}")))?;
            let lib = crate::library::Library::fetch(&demux).await?;
            let p = lib.get(product_id).with_context(|| format!("product {product_id} is not owned by {user}"))?;
            Ok(AchRefs::from_product(p))
        })
    }

    fn read<'a>(&'a self, user: &'a str, product: Option<&'a AchProduct>, framing: Framing) -> BoxFuture<'a, Result<uplay::ReadAchievementsRsp>> {
        Box::pin(async move { self.conn(user, framing).await?.read(user, product).await })
    }

    fn write<'a>(&'a self, user: &'a str, req: uplay::Req, framing: Framing) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.conn(user, framing).await?.write(req).await })
    }

    fn read_legacy<'a>(&'a self, user: &'a str, space_id: &'a str) -> BoxFuture<'a, Result<BTreeSet<u32>>> {
        Box::pin(async move {
            let online = self.online(user)?;
            let session = online.session().await?;
            let rsp: LegacyResponse = crate::ubiservices::get_json(&online.auth, &session, &legacy_path(&session.user_id, space_id)).await?;
            Ok(legacy_earned(&rsp))
        })
    }

    fn definitions<'a>(&'a self, asset: &'a str, locale: &'a str) -> BoxFuture<'a, Result<Achievements>> {
        Box::pin(crate::progress::achievement_definitions(asset, &self.cache, locale))
    }
}

// ---------------------------------------------------------------------------------------
// Engine

/// [`Engine::status`].
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub user: String,
    pub journal: Journal,
    /// Definitions merged with the unlocks (`None`: no definitions and nothing earned).
    pub achievements: Option<Achievements>,
    pub mode: ReportMode,
    pub errors: Vec<String>,
}

type ModeFn = Box<dyn Fn(u32) -> ReportMode + Send + Sync>;
type EventFn = Box<dyn Fn(quiver_api::Event) + Send + Sync>;
/// One lock per (account, product): journal read-modify-write and its upstream calls.
type Locks = Mutex<HashMap<(String, u32), Arc<tokio::sync::Mutex<()>>>>;

pub struct Engine {
    up: Arc<dyn Upstream>,
    dir: PathBuf,
    mode: ModeFn,
    events: EventFn,
    framing: Framing,
    locks: Locks,
    refs: Mutex<HashMap<u32, AchRefs>>,
    defs: Mutex<HashMap<(u32, String), Achievements>>,
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

impl Engine {
    /// `dir`: the journal root; `mode`: a game's reporting mode (its setting); `events`:
    /// where `AchievementUnlocked` goes.
    pub fn new(up: Arc<dyn Upstream>, dir: PathBuf, mode: ModeFn, events: EventFn) -> Arc<Self> {
        let framing = std::env::var("QUIVER_ACH_FRAMING").ok().and_then(|f| Framing::parse(&f).ok()).unwrap_or_default();
        Arc::new(Self {
            up,
            dir,
            mode,
            events,
            framing,
            locks: Mutex::default(),
            refs: Mutex::default(),
            defs: Mutex::default(),
        })
    }

    pub fn journal_dir(&self) -> &Path {
        &self.dir
    }

    /// Remember a game's config facts (callers that have the config already).
    pub fn set_refs(&self, product_id: u32, refs: AchRefs) {
        self.refs.lock().unwrap().insert(product_id, refs);
    }

    fn lock_for(&self, user: &str, product_id: u32) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry((user.to_owned(), product_id)).or_default().clone()
    }

    async fn refs_for(&self, user: &str, product_id: u32) -> Result<AchRefs> {
        if let Some(r) = self.refs.lock().unwrap().get(&product_id).cloned() {
            return Ok(r);
        }
        let r = self.up.refs(user, product_id).await?;
        self.refs.lock().unwrap().insert(product_id, r.clone());
        Ok(r)
    }

    async fn definitions(&self, product_id: u32, refs: &AchRefs, locale: &str) -> Result<Option<Achievements>> {
        let Some(asset) = &refs.asset else { return Ok(None) };
        let key = (product_id, locale.to_owned());
        if let Some(d) = self.defs.lock().unwrap().get(&key).cloned() {
            return Ok(Some(d));
        }
        let d = self.up.definitions(asset, locale).await?;
        self.defs.lock().unwrap().insert(key, d.clone());
        Ok(Some(d))
    }

    /// The journal's product, from the config's sync id (keeping a learned platform).
    fn product(j: &mut Journal, refs: &AchRefs) -> Option<AchProduct> {
        match (&j.product, &refs.sync_id) {
            (Some(p), Some(s)) if p.sync_id.eq_ignore_ascii_case(s) => {}
            (_, Some(s)) => j.product = Some(AchProduct::new(s)),
            (_, None) => {}
        }
        j.product.clone()
    }

    /// Read Ubisoft's unlock set into the journal: `ach_frontend` for the product (and, when
    /// that has no entry, once for every product to learn its platform), joined with REST
    /// `legacyPlayerAchievements`. Fails only if every source failed. Returns the unverified
    /// earns it confirmed new.
    async fn read_into(&self, user: &str, j: &mut Journal, refs: &AchRefs, errors: &mut Vec<String>) -> Result<Vec<u32>> {
        let mut earned: BTreeMap<u32, Option<DateTime<Utc>>> = BTreeMap::new();
        let mut sources = Vec::new();
        let mut offline = None;
        if let Some(product) = Self::product(j, refs) {
            match self.read_frontend(user, &product).await {
                Ok(r) => {
                    if let Some(p) = r.platform_id.filter(|p| *p != product.platform_id) {
                        tracing::info!(product_id = j.product_id, platform = %p, "ach_frontend: learned the product's platform id");
                        j.product = Some(AchProduct { platform_id: p, ..product.clone() });
                    }
                    if r.ts_unit.is_some() {
                        j.ts_unit = r.ts_unit;
                    }
                    earned.extend(r.earned);
                    sources.push("ach_frontend");
                }
                Err(e) => {
                    if is_offline(&e) {
                        offline = Some(format!("{e:#}"));
                    }
                    errors.push(format!("ach_frontend: {e:#}"));
                }
            }
        } else {
            errors.push("ach_frontend: the game's config has no achievements_sync_id".into());
        }
        if offline.is_none()
            && let Some(space) = &refs.space_id
        {
            match self.up.read_legacy(user, space).await {
                Ok(ids) => {
                    for id in ids {
                        earned.entry(id).or_insert(None);
                    }
                    sources.push("legacyPlayerAchievements");
                }
                Err(e) => errors.push(format!("legacyPlayerAchievements: {e:#}")),
            }
        }
        if sources.is_empty() {
            match offline {
                Some(o) => return Err(anyhow::Error::new(Offline(o))),
                None => bail!("no achievement unlock source answered"),
            }
        }
        Ok(j.apply_read(earned, &sources.join("+"), now()))
    }

    async fn read_frontend(&self, user: &str, product: &AchProduct) -> Result<BlobRead> {
        let rsp = self.up.read(user, Some(product), self.framing).await?;
        let r = read_blob(&rsp.achievement_blob, &product.sync_id, Some(&product.platform_id));
        if r.found {
            return Ok(r);
        }
        // Nothing for this product: maybe nothing unlocked, maybe another platform id. One
        // read of every product tells which.
        match self.up.read(user, None, self.framing).await {
            Ok(all) => {
                let r2 = read_blob(&all.achievement_blob, &product.sync_id, Some(&product.platform_id));
                Ok(if r2.found { r2 } else { r })
            }
            Err(e) => {
                tracing::debug!("ach_frontend read of every product: {e:#}");
                Ok(r)
            }
        }
    }

    fn announce(&self, j: &Journal, ids: &[u32], defs: Option<&Achievements>) {
        for &id in ids {
            let Some(e) = j.entry(id) else { continue };
            let def = defs.and_then(|d| d.items.iter().find(|x| x.id.trim().parse::<u32>().ok() == Some(id)));
            (self.events)(quiver_api::Event::AchievementUnlocked {
                product_id: j.product_id,
                account: j.account.clone(),
                achievement_id: id,
                name: def.and_then(|d| d.name.clone()),
                description: def.and_then(|d| d.description.clone()),
                image: def.and_then(|d| d.image.clone()),
                unlocked_at: e.earned_at,
                report: e.state,
                game: None,
                key: None,
            });
        }
    }

    /// Unlock status for a game: the journal's set if fresh (or `refresh`), else read
    /// again; merged with the definitions in `locale`. Pending reports are sent afterwards
    /// when the read worked.
    pub async fn status(self: &Arc<Self>, account: Option<&str>, product_id: u32, refresh: bool, locale: &str) -> Result<Status> {
        let user = self.up.user_id(account).await?;
        let mut errors = Vec::new();
        let refs = match self.refs_for(&user, product_id).await {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("product config: {e:#}"));
                AchRefs::default()
            }
        };
        let lock = self.lock_for(&user, product_id);
        let guard = lock.lock().await;
        let mut j = Journal::load(&self.dir, &user, product_id)?;
        let mut read_ok = false;
        let mut new = Vec::new();
        if refresh || !j.fresh(now()) {
            match self.read_into(&user, &mut j, &refs, &mut errors).await {
                Ok(n) => {
                    read_ok = true;
                    new = n;
                }
                Err(e) => tracing::info!(product_id, "achievement unlocks unavailable: {e:#}"),
            }
            j.save(&self.dir)?;
        }
        drop(guard);
        let defs = match self.definitions(product_id, &refs, locale).await {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("achievement definitions: {e:#}"));
                None
            }
        };
        self.announce(&j, &new, defs.as_ref());
        let mut achievements = defs.clone();
        if achievements.is_none() && (!j.known.is_empty() || !j.entries.is_empty()) {
            achievements = Some(Achievements::default());
        }
        if let Some(a) = &mut achievements {
            merge(a, &j);
        }
        if read_ok && j.has_sendable() {
            let this = self.clone();
            let u = user.clone();
            tokio::spawn(async move {
                if let Err(e) = this.flush(&u, product_id).await {
                    tracing::info!(product_id, "achievement reports still pending: {e:#}");
                }
            });
        }
        Ok(Status { user, journal: j, achievements, mode: (self.mode)(product_id), errors })
    }

    /// The game earned `achievement_id`. Deduplicated against Ubisoft's set (read again
    /// when stale) and the journal; a new one is journaled, announced and (reporting on)
    /// sent before this returns.
    pub async fn earned(self: &Arc<Self>, account: Option<&str>, product_id: u32, achievement_id: u32) -> Result<EarnOutcome> {
        let user = self.up.user_id(account).await?;
        let refs = self.refs_for(&user, product_id).await.unwrap_or_else(|e| {
            tracing::info!(product_id, "product config for achievements: {e:#}");
            AchRefs::default()
        });
        let mode = (self.mode)(product_id);
        let lock = self.lock_for(&user, product_id);
        let guard = lock.lock().await;
        let mut j = Journal::load(&self.dir, &user, product_id)?;
        let mut confirmed = Vec::new();
        if j.entry(achievement_id).is_none() && !j.fresh(now()) {
            let mut errors = Vec::new();
            match self.read_into(&user, &mut j, &refs, &mut errors).await {
                Ok(n) => confirmed = n,
                Err(e) => tracing::info!(product_id, achievement_id, "can't check Ubisoft's unlocks before recording: {e:#}"),
            }
        }
        let outcome = j.record_earn(achievement_id, now(), mode);
        j.save(&self.dir)?;
        drop(guard);
        if let EarnOutcome::New { unverified: false, .. } = outcome {
            confirmed.push(achievement_id);
        }
        match &outcome {
            EarnOutcome::New { state, unverified } => {
                tracing::info!(product_id, achievement_id, ?state, unverified, "achievement earned")
            }
            o => tracing::debug!(product_id, achievement_id, ?o, "achievement earned again"),
        }
        if mode == ReportMode::DryRun
            && matches!(outcome, EarnOutcome::New { .. })
            && let Some(p) = &j.product
        {
            let req = write_req(p, &[(achievement_id, j.ts_unit.unwrap_or_default().encode(now()))]);
            tracing::info!(product_id, achievement_id, request = %describe_req(&req), hex = %hex::encode(encode_frame(&req, self.framing)), "achievements dry run: would send");
        }
        if !confirmed.is_empty() {
            let defs = self.definitions(product_id, &refs, "en-US").await.ok().flatten();
            self.announce(&j, &confirmed, defs.as_ref());
        }
        if mode == ReportMode::On
            && j.has_sendable()
            && let Err(e) = self.flush(&user, product_id).await
        {
            tracing::info!(product_id, achievement_id, "achievement report pending: {e:#}");
        }
        Ok(outcome)
    }

    /// Send a game's pending reports (one write), then read back to confirm. Unverified
    /// earns are checked against a fresh read first.
    pub async fn flush(self: &Arc<Self>, user: &str, product_id: u32) -> Result<Vec<u32>> {
        if (self.mode)(product_id) != ReportMode::On {
            return Ok(Vec::new());
        }
        let refs = self.refs_for(user, product_id).await.unwrap_or_default();
        let lock = self.lock_for(user, product_id);
        let _guard = lock.lock().await;
        let mut j = Journal::load(&self.dir, user, product_id)?;
        let mut errors = Vec::new();
        if j.needs_check() || !j.known_valid() {
            let new = self.read_into(user, &mut j, &refs, &mut errors).await;
            j.save(&self.dir)?;
            let new = new?;
            let defs = self.definitions(product_id, &refs, "en-US").await.ok().flatten();
            self.announce(&j, &new, defs.as_ref());
        }
        let items = j.sendable();
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let product = Self::product(&mut j, &refs).context("can't report: the game's config has no achievements_sync_id")?;
        let unit = j.ts_unit.unwrap_or_default();
        let wire: Vec<(u32, u64)> = items.iter().map(|&(id, at)| (id, unit.encode(at))).collect();
        let ids: Vec<u32> = items.iter().map(|&(id, _)| id).collect();
        let req = write_req(&product, &wire);
        tracing::info!(product_id, ?ids, request = %describe_req(&req), "reporting achievements to ach_frontend");
        let result = self.up.write(user, req, self.framing).await;
        match &result {
            Ok(()) => {
                j.mark_reported(&ids, now());
                // Read back: did it take? (`confirmed` on each entry.)
                if let Err(e) = self.read_into(user, &mut j, &refs, &mut errors).await {
                    tracing::info!(product_id, "achievement read-back after the write failed: {e:#}");
                }
                let unconfirmed: Vec<u32> =
                    ids.iter().copied().filter(|id| j.entry(*id).is_some_and(|e| e.confirmed == Some(false))).collect();
                if !unconfirmed.is_empty() {
                    tracing::warn!(product_id, ?unconfirmed, "ach_frontend accepted the write but a read-back doesn't show it");
                }
            }
            Err(e) => {
                let reached = !is_offline(e);
                j.mark_failed(&ids, &format!("{e:#}"), reached);
            }
        }
        j.save(&self.dir)?;
        result.map(|()| ids)
    }

    /// Flush every journal with pending reports (the next online opportunity).
    pub async fn retry_all(self: &Arc<Self>) {
        let Ok(users) = std::fs::read_dir(&self.dir) else { return };
        for u in users.flatten() {
            let user = u.file_name().to_string_lossy().into_owned();
            let Ok(files) = std::fs::read_dir(u.path()) else { continue };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let Some(product_id) = name.strip_suffix(".json").and_then(|n| n.parse::<u32>().ok()) else { continue };
                let pending = Journal::load(&self.dir, &user, product_id).is_ok_and(|j| j.has_sendable());
                if !pending {
                    continue;
                }
                match self.flush(&user, product_id).await {
                    Ok(ids) if !ids.is_empty() => tracing::info!(product_id, ?ids, "pending achievements reported"),
                    Ok(_) => {}
                    Err(e) => tracing::info!(product_id, "achievement reports still pending: {e:#}"),
                }
            }
        }
    }

    /// Send a journaled achievement again (or, `dry_run`, only build the request).
    pub async fn report(
        self: &Arc<Self>,
        account: Option<&str>,
        product_id: u32,
        achievement_id: u32,
        dry_run: bool,
    ) -> Result<quiver_api::achievements::ReportResult> {
        let user = self.up.user_id(account).await?;
        let refs = self.refs_for(&user, product_id).await.unwrap_or_default();
        let lock = self.lock_for(&user, product_id);
        let _guard = lock.lock().await;
        let mut j = Journal::load(&self.dir, &user, product_id)?;
        let entry = j.entry(achievement_id).cloned().with_context(|| {
            format!("achievement {achievement_id} isn't in the journal: Quiver only sends achievements a game earned through it")
        })?;
        let product = Self::product(&mut j, &refs).context("can't report: the game's config has no achievements_sync_id")?;
        let wire = [(achievement_id, j.ts_unit.unwrap_or_default().encode(entry.earned_at))];
        let req = write_req(&product, &wire);
        let request_hex = hex::encode(encode_frame(&req, self.framing));
        let request = describe_req(&req);
        if dry_run {
            return Ok(quiver_api::achievements::ReportResult { achievement_id, entry, request_hex, request, sent: false });
        }
        tracing::info!(product_id, achievement_id, request = %request, "resending an achievement to ach_frontend");
        let result = self.up.write(&user, req, self.framing).await;
        match &result {
            Ok(()) => {
                j.mark_reported(&[achievement_id], now());
                if let Some(e) = j.entry_mut(achievement_id) {
                    e.via = "manual".into();
                }
                let mut errors = Vec::new();
                if let Err(e) = self.read_into(&user, &mut j, &refs, &mut errors).await {
                    tracing::info!(product_id, "achievement read-back after the write failed: {e:#}");
                }
            }
            Err(e) => j.mark_failed(&[achievement_id], &format!("{e:#}"), false),
        }
        j.save(&self.dir)?;
        result?;
        let entry = j.entry(achievement_id).cloned().unwrap_or(entry);
        Ok(quiver_api::achievements::ReportResult { achievement_id, entry, request_hex, request, sent: true })
    }

    /// One raw read with overrides, for checking the protocol by hand. Read-only.
    pub async fn probe(
        self: &Arc<Self>,
        account: Option<&str>,
        product_id: u32,
        platform_id: Option<&str>,
        framing: Option<Framing>,
        all_products: bool,
    ) -> Result<serde_json::Value> {
        let user = self.up.user_id(account).await?;
        let refs = self.refs_for(&user, product_id).await?;
        let j = Journal::load(&self.dir, &user, product_id)?;
        let sync_id = refs.sync_id.clone().context("the game's config has no achievements_sync_id")?;
        let platform = platform_id
            .map(str::to_owned)
            .or_else(|| j.product.as_ref().filter(|p| p.sync_id.eq_ignore_ascii_case(&sync_id)).map(|p| p.platform_id.clone()))
            .unwrap_or_else(|| DEFAULT_PLATFORM.into());
        let product = AchProduct { sync_id, platform_id: platform };
        let framing = framing.unwrap_or(self.framing);
        let req = read_req(&user, (!all_products).then_some(&product));
        let sent = serde_json::json!({
            "framing": format!("{framing:?}"),
            "request": describe_req(&req),
            "hex": hex::encode(encode_frame(&req, framing)),
        });
        let rsp = self.up.read(&user, (!all_products).then_some(&product), framing).await;
        Ok(match rsp {
            Ok(r) => {
                let mine = read_blob(&r.achievement_blob, &product.sync_id, Some(&product.platform_id));
                serde_json::json!({
                    "sent": sent,
                    "user_id": r.user_id,
                    "blob": describe_blob(&r.achievement_blob),
                    "product": { "found": mine.found, "platform_id": mine.platform_id, "ts_unit": mine.ts_unit, "earned": mine.earned },
                })
            }
            Err(e) => serde_json::json!({ "sent": sent, "error": format!("{e:#}") }),
        })
    }

    /// The game's list for the SDK's `GetAchievements`, and whether Ubisoft's set is known.
    pub async fn bridge_list(self: &Arc<Self>, account: Option<&str>, product_id: u32, locale: &str) -> Result<(Vec<quiver_ipc::BridgeAchievement>, bool)> {
        let s = self.status(account, product_id, false, locale).await?;
        let known = s.journal.known_valid();
        let items = s
            .achievements
            .map(|a| a.items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|x| {
                let id = x.id.trim().parse::<u32>().ok()?;
                Some(quiver_ipc::BridgeAchievement {
                    id,
                    name: x.name.unwrap_or_default(),
                    description: x.description.unwrap_or_default(),
                    earned: x.unlocked,
                    earned_at: x.unlocked_at.map(|t| t.timestamp().max(0) as u64).unwrap_or(0),
                })
            })
            .collect();
        Ok((items, known))
    }
}

#[cfg(test)]
mod tests;
