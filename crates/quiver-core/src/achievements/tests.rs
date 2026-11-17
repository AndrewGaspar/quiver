use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

// ---------------------------------------------------------------------------------------
// Protocol

/// The blob for AC Origins' "Namaste" (sync id ACE, achievement 59), hand-encoded from
/// `Uplay.proto` (field numbers and wire types), independent of prost.
const NAMASTE_BLOB: &str = "0a170a090a0341434512025043120a0a08083b1080cbe9c806";

#[test]
fn achievement_blob_shape() {
    let p = AchProduct::new("ACE");
    let b = blob(&p, &[(59, 1_763_337_600)]);
    assert_eq!(hex::encode(b.encode_to_vec()), NAMASTE_BLOB);
    // One product, one achievement, the timestamp present.
    assert_eq!(b.product_achievements.len(), 1);
    let pa = &b.product_achievements[0];
    assert_eq!((pa.product.product_id.as_str(), pa.product.platform_id.as_str()), ("ACE", "PC"));
    assert_eq!(pa.achievements.achievements, vec![uplay::Achievement { achievement_id: 59, timestamp: Some(1_763_337_600) }]);

    // The full request: Req{request_id = 1, write_achievements_req = 4 {achievement_blob = 1}}.
    let mut req = write_req(&p, &[(59, 1_763_337_600)]);
    req.request_id = 7;
    assert_eq!(hex::encode(req.encode_to_vec()), format!("0807221b0a19{NAMASTE_BLOB}"));
    // Raw framing puts exactly that on the wire; wrapped adds Upstream{request = 1}.
    assert_eq!(encode_frame(&req, Framing::Raw), req.encode_to_vec());
    let wrapped = encode_frame(&req, Framing::Wrapped);
    assert_eq!(wrapped[0], 0x0a);
    assert_eq!(crate::demux::AchUpstream::decode(wrapped.as_slice()).unwrap().request.unwrap(), req);

    // Decodes back.
    let back = uplay::Req::decode(req.encode_to_vec().as_slice()).unwrap();
    assert_eq!(back, req);
    let d = describe_req(&req);
    assert_eq!(d["write_achievements_req"]["achievement_blob"][0]["product"]["product_id"], "ACE");
    assert_eq!(d["write_achievements_req"]["achievement_blob"][0]["achievements"][0]["achievement_id"], 59);
}

#[test]
fn read_request_shape() {
    let r = read_req("u-1", Some(&AchProduct::new("ACE")));
    let rr = r.read_achievements_req.as_ref().unwrap();
    assert_eq!(rr.user_id.as_deref(), Some("u-1"));
    assert_eq!(rr.product.as_ref().unwrap().product_id, "ACE");
    // Req field 3, holding user_id (1) and product (2).
    let bytes = r.encode_to_vec();
    assert_eq!(&bytes[..2], &[0x08, 0x00]);
    assert_eq!(bytes[2], 0x1a);
    assert!(read_req("u-1", None).read_achievements_req.unwrap().product.is_none());
    // The auth request never shows the token in descriptions.
    let a = auth_req("secret-ticket");
    assert_eq!(a.authenticate_req.as_ref().unwrap().orbit_token, "secret-ticket");
    assert!(!describe_req(&a).to_string().contains("secret"));
}

type BlobEntry<'a> = (&'a str, &'a str, &'a [(u32, Option<u64>)]);

fn rsp_blob(entries: &[BlobEntry]) -> uplay::AchievementBlob {
    uplay::AchievementBlob {
        product_achievements: entries
            .iter()
            .map(|(id, plat, items)| uplay::ProductAchievements {
                product: uplay::Product { product_id: id.to_string(), platform_id: plat.to_string() },
                achievements: uplay::Achievements {
                    achievements: items.iter().map(|&(a, t)| uplay::Achievement { achievement_id: a, timestamp: t }).collect(),
                },
            })
            .collect(),
    }
}

#[test]
fn reading_a_blob() {
    let b = rsp_blob(&[
        ("WD2", "PC", &[(1, Some(1))]),
        ("ace", "PCUPLAY", &[(59, Some(1_763_337_600)), (2, None)]),
        ("ACE", "STEAM", &[(3, Some(1_763_337_600))]),
    ]);
    // Wanted platform absent: the first other platform's entry, learned.
    let r = read_blob(&b, "ACE", Some("PC"));
    assert!(r.found);
    assert_eq!(r.platform_id.as_deref(), Some("PCUPLAY"));
    assert_eq!(r.ts_unit, Some(TsUnit::Seconds));
    assert_eq!(r.earned.keys().copied().collect::<Vec<_>>(), vec![2, 59]);
    assert_eq!(r.earned[&59].unwrap().to_rfc3339(), "2025-11-17T00:00:00+00:00");
    assert_eq!(r.earned[&2], None);
    // Wanted platform present: only its entry.
    let r = read_blob(&b, "ACE", Some("STEAM"));
    assert_eq!(r.earned.keys().copied().collect::<Vec<_>>(), vec![3]);
    assert!(!read_blob(&b, "FCC", None).found);
}

#[test]
fn timestamp_units() {
    let t: DateTime<Utc> = "2026-11-17T01:02:03Z".parse().unwrap();
    for unit in [TsUnit::Seconds, TsUnit::Millis, TsUnit::FileTime] {
        let v = unit.encode(t);
        assert_eq!(TsUnit::detect(v), Some(unit), "{unit:?} {v}");
        assert_eq!(unit.decode(v), Some(t));
    }
    assert_eq!(TsUnit::detect(0), None);
}

#[test]
fn legacy_response() {
    let rsp: LegacyResponse = serde_json::from_str(
        r#"{"legacyPlayerAchievements":[
            {"achievementLegacyId":59,"spaceId":"s","profileId":"p","completed":true,"displayName":"Namaste"},
            {"achievementLegacyId":"7","completed":true},
            {"achievementLegacyId":8,"completed":false},
            {"completed":true}
        ],"extra":1}"#,
    )
    .unwrap();
    assert_eq!(legacy_earned(&rsp), BTreeSet::from([7, 59]));
    assert!(legacy_earned(&serde_json::from_str("{}").unwrap()).is_empty());
    assert_eq!(legacy_path("u", "s"), "/v1/profiles/u/legacyPlayerAchievements?spaceId=s");
}

#[test]
fn refs_from_config() {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/product-config/3539.yaml")).unwrap();
    let cfg = crate::library::ProductConfig::parse(&text).unwrap();
    let r = AchRefs::from_config(&cfg, Some("space-1".into()));
    assert_eq!(r.sync_id.as_deref(), Some("ACE"));
    assert_eq!(r.asset.as_deref(), Some("3483afa6b8475717dabfe0f88025a465"));
    assert_eq!(r.space_id.as_deref(), Some("space-1"));
    // A numeric sync id (5183: `achievements_sync_id: 5183`).
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/product-config/5183.yaml")).unwrap();
    let r = AchRefs::from_config(&crate::library::ProductConfig::parse(&text).unwrap(), None);
    assert_eq!(r.sync_id.as_deref(), Some("5183"));
}

// ---------------------------------------------------------------------------------------
// Journal

fn t(s: &str) -> DateTime<Utc> {
    format!("2026-11-17T{s}Z").parse().unwrap()
}

#[test]
fn dedupe_against_ubisoft_and_the_journal() {
    let mut j = Journal { product_id: 3539, account: "u".into(), ..Default::default() };
    j.apply_read(BTreeMap::from([(59, Some(t("00:00:00"))), (1, None)]), "ach_frontend", t("01:00:00"));
    // Origins re-sends its whole set at every load: nothing recorded.
    assert_eq!(j.record_earn(59, t("01:01:00"), ReportMode::On), EarnOutcome::AlreadyEarned);
    assert_eq!(j.record_earn(1, t("01:01:00"), ReportMode::On), EarnOutcome::AlreadyEarned);
    assert!(j.entries.is_empty());
    // A new one once, then never again.
    assert_eq!(j.record_earn(60, t("01:02:00"), ReportMode::On), EarnOutcome::New { state: ReportState::Pending, unverified: false });
    assert_eq!(j.record_earn(60, t("01:03:00"), ReportMode::On), EarnOutcome::AlreadyJournaled(ReportState::Pending));
    assert_eq!(j.sendable(), vec![(60, t("01:02:00"))]);
    j.mark_reported(&[60], t("01:04:00"));
    assert_eq!(j.entry(60).unwrap().state, ReportState::Reported);
    assert!(j.known.contains_key(&60), "reported ones count as known");
    assert_eq!(j.record_earn(60, t("01:05:00"), ReportMode::On), EarnOutcome::AlreadyJournaled(ReportState::Reported));
    assert!(j.sendable().is_empty());
    // Reporting off / dry run: journaled, nothing to send.
    assert_eq!(j.record_earn(61, t("01:06:00"), ReportMode::Off), EarnOutcome::New { state: ReportState::Off, unverified: false });
    assert_eq!(j.record_earn(62, t("01:06:00"), ReportMode::DryRun), EarnOutcome::New { state: ReportState::DryRun, unverified: false });
    assert!(j.sendable().is_empty());
}

#[test]
fn unverified_earns_are_checked_first() {
    let mut j = Journal { product_id: 3539, account: "u".into(), ..Default::default() };
    // Ubisoft's set unknown (offline at the first load): kept, not sendable yet.
    assert_eq!(j.record_earn(59, t("01:00:00"), ReportMode::On), EarnOutcome::New { state: ReportState::Pending, unverified: true });
    assert_eq!(j.record_earn(60, t("01:00:00"), ReportMode::On), EarnOutcome::New { state: ReportState::Pending, unverified: true });
    assert!(j.sendable().is_empty() && j.needs_check());
    // A read shows 59 was already there: only 60 is new.
    let new = j.apply_read(BTreeMap::from([(59, None)]), "ach_frontend", t("02:00:00"));
    assert_eq!(new, vec![60]);
    assert_eq!(j.entry(59).unwrap().state, ReportState::AlreadyEarned);
    assert_eq!(j.sendable(), vec![(60, t("01:00:00"))]);
    assert!(!j.needs_check());
}

#[test]
fn failures_and_read_backs() {
    let mut j = Journal { product_id: 1, account: "u".into(), ..Default::default() };
    j.apply_read(BTreeMap::new(), "ach_frontend", t("00:00:00"));
    j.record_earn(5, t("00:01:00"), ReportMode::On);
    // Offline attempts don't count.
    for _ in 0..20 {
        j.mark_failed(&[5], "offline: no network", false);
    }
    assert_eq!((j.entry(5).unwrap().state, j.entry(5).unwrap().attempts), (ReportState::Pending, 0));
    for _ in 0..MAX_ATTEMPTS {
        j.mark_failed(&[5], "ach_frontend request 2 timed out", true);
    }
    let e = j.entry(5).unwrap();
    assert_eq!((e.state, e.attempts), (ReportState::Failed, MAX_ATTEMPTS));
    assert!(e.last_error.as_deref().unwrap().contains("timed out"));
    assert!(j.sendable().is_empty());

    // A write that a read-back doesn't show: confirmed false (a silent ignore).
    j.record_earn(6, t("00:02:00"), ReportMode::On);
    j.mark_reported(&[6], t("00:03:00"));
    j.apply_read(BTreeMap::new(), "ach_frontend", t("00:04:00"));
    assert_eq!(j.entry(6).unwrap().confirmed, Some(false));
    j.apply_read(BTreeMap::from([(6, None)]), "ach_frontend", t("00:05:00"));
    assert_eq!(j.entry(6).unwrap().confirmed, Some(true));
    // Someone else reported a failed one.
    j.apply_read(BTreeMap::from([(5, None), (6, None)]), "ach_frontend", t("00:06:00"));
    assert_eq!(j.entry(5).unwrap().state, ReportState::AlreadyEarned);
}

#[test]
fn journal_persists() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = Journal::load(dir.path(), "user-1", 3539).unwrap();
    assert!(j.entries.is_empty() && !j.known_valid());
    j.product = Some(AchProduct::new("ACE"));
    j.apply_read(BTreeMap::from([(59, Some(t("00:00:00")))]), "ach_frontend", t("00:00:01"));
    j.record_earn(60, t("00:01:00"), ReportMode::On);
    j.save(dir.path()).unwrap();
    assert!(dir.path().join("user-1/3539.json").is_file());
    assert_eq!(Journal::load(dir.path(), "user-1", 3539).unwrap(), j);
    assert!(Journal::load(dir.path(), "../x", 1).is_err());
}

#[test]
fn merged_into_definitions() {
    let mut a = Achievements {
        total: 3,
        items: ["01", "59", "60"].iter().map(|id| Achievement { id: id.to_string(), name: Some(format!("A{id}")), ..Default::default() }).collect(),
        ..Default::default()
    };
    let mut j = Journal { product_id: 3539, account: "u".into(), ..Default::default() };
    j.apply_read(BTreeMap::from([(59, Some(t("00:00:00"))), (77, None)]), "ach_frontend+legacyPlayerAchievements", t("00:00:01"));
    j.record_earn(60, t("00:01:00"), ReportMode::On);
    merge(&mut a, &j);
    assert!(a.unlocked_known);
    assert_eq!(a.source.as_deref(), Some("ach_frontend+legacyPlayerAchievements"));
    let get = |id: &str| a.items.iter().find(|x| x.id == id).unwrap();
    assert!(!get("01").unlocked && get("01").unlocked_at.is_none());
    assert!(get("59").unlocked);
    assert_eq!(get("59").unlocked_at, Some(t("00:00:00")));
    assert_eq!((get("60").unlocked, get("60").report), (true, Some(ReportState::Pending)));
    assert_eq!(get("60").unlocked_at, Some(t("00:01:00")));
    // Unknown to the definitions: added without a name.
    assert!(get("77").unlocked && get("77").name.is_none());
    assert_eq!((a.unlocked, a.total), (3, 4));
}

// ---------------------------------------------------------------------------------------
// Engine, against a fake Ubisoft

#[derive(Default)]
struct Fake {
    /// (sync id, platform) -> achievement -> timestamp.
    upstream: Mutex<BTreeMap<(String, String), BTreeMap<u32, u64>>>,
    writes: Mutex<Vec<uplay::Req>>,
    reads: AtomicUsize,
    offline: AtomicBool,
    /// Accept writes but don't store them (an `ach_frontend` that ignores the title).
    ignore_writes: AtomicBool,
    /// Leave writes unanswered (the demux way of refusing).
    refuse_writes: AtomicBool,
    legacy: Mutex<Option<BTreeSet<u32>>>,
}

impl Fake {
    #[allow(clippy::type_complexity)]
    fn with(entries: &[(&str, &str, &[(u32, u64)])]) -> Arc<Self> {
        let f = Self::default();
        for (s, p, items) in entries {
            f.upstream.lock().unwrap().insert((s.to_string(), p.to_string()), items.iter().copied().collect());
        }
        Arc::new(f)
    }

    fn offline(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(Offline("no network".into())));
        }
        Ok(())
    }

    fn written(&self) -> Vec<Vec<(String, u32)>> {
        self.writes
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                r.write_achievements_req
                    .as_ref()
                    .unwrap()
                    .achievement_blob
                    .as_ref()
                    .unwrap()
                    .product_achievements
                    .iter()
                    .flat_map(|p| p.achievements.achievements.iter().map(|a| (p.product.product_id.clone(), a.achievement_id)))
                    .collect()
            })
            .collect()
    }
}

impl Upstream for Fake {
    fn user_id<'a>(&'a self, account: Option<&'a str>) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move { Ok(account.unwrap_or("user-1").to_owned()) })
    }

    fn refs<'a>(&'a self, _user: &'a str, product_id: u32) -> BoxFuture<'a, Result<AchRefs>> {
        Box::pin(async move {
            anyhow::ensure!(product_id == 3539, "unknown product");
            Ok(AchRefs { sync_id: Some("ACE".into()), asset: Some("set".into()), space_id: Some("space".into()) })
        })
    }

    fn read<'a>(&'a self, user: &'a str, product: Option<&'a AchProduct>, _framing: Framing) -> BoxFuture<'a, Result<uplay::ReadAchievementsRsp>> {
        Box::pin(async move {
            self.offline()?;
            self.reads.fetch_add(1, Ordering::SeqCst);
            let up = self.upstream.lock().unwrap();
            let entries = up
                .iter()
                .filter(|((s, p), _)| product.is_none_or(|q| q.sync_id == *s && q.platform_id == *p))
                .map(|((s, p), items)| uplay::ProductAchievements {
                    product: uplay::Product { product_id: s.clone(), platform_id: p.clone() },
                    achievements: uplay::Achievements {
                        achievements: items.iter().map(|(&a, &t)| uplay::Achievement { achievement_id: a, timestamp: Some(t) }).collect(),
                    },
                })
                .collect();
            Ok(uplay::ReadAchievementsRsp { user_id: user.to_owned(), achievement_blob: uplay::AchievementBlob { product_achievements: entries } })
        })
    }

    fn write<'a>(&'a self, _user: &'a str, req: uplay::Req, _framing: Framing) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.offline()?;
            self.writes.lock().unwrap().push(req.clone());
            if self.refuse_writes.load(Ordering::SeqCst) {
                bail!("ach_frontend request 2 timed out (likely rejected)");
            }
            if !self.ignore_writes.load(Ordering::SeqCst) {
                let mut up = self.upstream.lock().unwrap();
                for p in req.write_achievements_req.unwrap().achievement_blob.unwrap().product_achievements {
                    let e = up.entry((p.product.product_id, p.product.platform_id)).or_default();
                    for a in p.achievements.achievements {
                        e.insert(a.achievement_id, a.timestamp.unwrap_or(0));
                    }
                }
            }
            Ok(())
        })
    }

    fn read_legacy<'a>(&'a self, _user: &'a str, _space_id: &'a str) -> BoxFuture<'a, Result<BTreeSet<u32>>> {
        Box::pin(async move {
            self.offline()?;
            self.legacy.lock().unwrap().clone().context("GET /v1/profiles/legacyPlayerAchievements returned 404")
        })
    }

    fn definitions<'a>(&'a self, _asset: &'a str, _locale: &'a str) -> BoxFuture<'a, Result<Achievements>> {
        Box::pin(async move {
            let items: Vec<Achievement> = (1..=60)
                .map(|n| Achievement { id: format!("{n:02}"), name: Some(if n == 59 { "Namaste".into() } else { format!("Ach {n}") }), ..Default::default() })
                .collect();
            Ok(Achievements { total: items.len() as u32, items, ..Default::default() })
        })
    }
}

struct Rig {
    fake: Arc<Fake>,
    engine: Arc<Engine>,
    events: Arc<Mutex<Vec<quiver_api::Event>>>,
    mode: Arc<Mutex<ReportMode>>,
    _dir: tempfile::TempDir,
}

fn rig(fake: Arc<Fake>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let events: Arc<Mutex<Vec<quiver_api::Event>>> = Arc::default();
    let mode: Arc<Mutex<ReportMode>> = Arc::default();
    let (ev, m) = (events.clone(), mode.clone());
    let engine = Engine::new(
        fake.clone(),
        dir.path().to_owned(),
        Box::new(move |_| *m.lock().unwrap()),
        Box::new(move |e| ev.lock().unwrap().push(e)),
    );
    Rig { fake, engine, events, mode, _dir: dir }
}

fn unlocked_ids(r: &Rig) -> Vec<u32> {
    r.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            quiver_api::Event::AchievementUnlocked { achievement_id, .. } => Some(*achievement_id),
            _ => None,
        })
        .collect()
}

/// Ground truth: Origins (3539, sync id ACE) has 59 "Namaste" unlocked through Connect.
fn origins() -> Arc<Fake> {
    Fake::with(&[("ACE", "PC", &[(59, 1_763_337_600)])])
}

#[tokio::test]
async fn status_shows_ubisoft_unlocks() {
    let r = rig(origins());
    let s = r.engine.status(None, 3539, false, "en-US").await.unwrap();
    let a = s.achievements.unwrap();
    assert!(a.unlocked_known);
    assert_eq!(a.source.as_deref(), Some("ach_frontend"), "legacy 404s, the frontend answers");
    let namaste = a.items.iter().find(|x| x.id == "59").unwrap();
    assert!(namaste.unlocked);
    assert_eq!(namaste.name.as_deref(), Some("Namaste"));
    assert_eq!(namaste.unlocked_at.unwrap().timestamp(), 1_763_337_600);
    assert_eq!((a.unlocked, a.total), (1, 60));
    assert!(s.errors.iter().any(|e| e.starts_with("legacyPlayerAchievements")));
    // Cached: no second read within the TTL.
    let reads = r.fake.reads.load(Ordering::SeqCst);
    r.engine.status(None, 3539, false, "en-US").await.unwrap();
    assert_eq!(r.fake.reads.load(Ordering::SeqCst), reads);
    r.engine.status(None, 3539, true, "en-US").await.unwrap();
    assert!(r.fake.reads.load(Ordering::SeqCst) > reads);

    // The REST source joins in when it answers.
    *r.fake.legacy.lock().unwrap() = Some(BTreeSet::from([2]));
    let a = r.engine.status(None, 3539, true, "en-US").await.unwrap().achievements.unwrap();
    assert_eq!(a.source.as_deref(), Some("ach_frontend+legacyPlayerAchievements"));
    assert_eq!(a.unlocked, 2);
}

#[tokio::test]
async fn platform_is_learned_from_an_all_products_read() {
    let r = rig(Fake::with(&[("ACE", "PCUPLAY", &[(59, 1_763_337_600_000)])]));
    let s = r.engine.status(None, 3539, false, "en-US").await.unwrap();
    assert_eq!(s.journal.product, Some(AchProduct { sync_id: "ACE".into(), platform_id: "PCUPLAY".into() }));
    assert_eq!(s.journal.ts_unit, Some(TsUnit::Millis));
    assert!(s.achievements.unwrap().items.iter().find(|x| x.id == "59").unwrap().unlocked);
    // Reports then use the learned platform and unit.
    r.engine.earned(None, 3539, 60).await.unwrap();
    let w = r.fake.writes.lock().unwrap()[0].clone();
    let pa = &w.write_achievements_req.unwrap().achievement_blob.unwrap().product_achievements[0];
    assert_eq!(pa.product.platform_id, "PCUPLAY");
    assert!(TsUnit::detect(pa.achievements.achievements[0].timestamp.unwrap()) == Some(TsUnit::Millis));
}

#[tokio::test]
async fn earn_resends_are_deduplicated_and_new_ones_reported() {
    let r = rig(origins());
    // The whole set again at a save load: nothing written, nothing announced.
    assert_eq!(r.engine.earned(None, 3539, 59).await.unwrap(), EarnOutcome::AlreadyEarned);
    assert!(r.fake.writes.lock().unwrap().is_empty());
    // A new unlock: journaled, announced once, written once (one blob for ACE).
    assert_eq!(r.engine.earned(None, 3539, 60).await.unwrap(), EarnOutcome::New { state: ReportState::Pending, unverified: false });
    assert_eq!(r.fake.written(), vec![vec![("ACE".to_string(), 60)]]);
    assert_eq!(unlocked_ids(&r), vec![60]);
    match &r.events.lock().unwrap()[0] {
        quiver_api::Event::AchievementUnlocked { name, report, product_id, account, .. } => {
            assert_eq!((name.as_deref(), *product_id, account.as_str()), (Some("Ach 60"), 3539, "user-1"));
            assert_eq!(*report, ReportState::Pending);
        }
        e => panic!("{e:?}"),
    }
    let j = Journal::load(r.engine.journal_dir(), "user-1", 3539).unwrap();
    assert_eq!((j.entry(60).unwrap().state, j.entry(60).unwrap().confirmed), (ReportState::Reported, Some(true)));
    // And again at the next load: nothing.
    for id in [59, 60, 59, 60] {
        let o = r.engine.earned(None, 3539, id).await.unwrap();
        assert!(matches!(o, EarnOutcome::AlreadyEarned | EarnOutcome::AlreadyJournaled(_)), "{o:?}");
    }
    assert_eq!(r.fake.writes.lock().unwrap().len(), 1);
    assert_eq!(unlocked_ids(&r), vec![60]);
}

#[tokio::test]
async fn offline_earns_wait_and_are_checked_then_sent() {
    let r = rig(origins());
    r.fake.offline.store(true, Ordering::SeqCst);
    // Offline before any read: kept unverified, not announced (it may be an old one).
    for id in [59, 60] {
        assert_eq!(r.engine.earned(None, 3539, id).await.unwrap(), EarnOutcome::New { state: ReportState::Pending, unverified: true });
    }
    assert!(unlocked_ids(&r).is_empty());
    r.engine.retry_all().await;
    let j = Journal::load(r.engine.journal_dir(), "user-1", 3539).unwrap();
    assert!(j.entries.iter().all(|e| e.attempts == 0 && e.state == ReportState::Pending), "offline doesn't count");
    // Back online: the retry reads first, so 59 (already on Ubisoft) isn't sent or announced.
    r.fake.offline.store(false, Ordering::SeqCst);
    r.engine.retry_all().await;
    assert_eq!(r.fake.written(), vec![vec![("ACE".to_string(), 60)]]);
    assert_eq!(unlocked_ids(&r), vec![60]);
    let j = Journal::load(r.engine.journal_dir(), "user-1", 3539).unwrap();
    assert_eq!(j.entry(59).unwrap().state, ReportState::AlreadyEarned);
    assert_eq!(j.entry(60).unwrap().state, ReportState::Reported);
    // Nothing left to retry.
    r.engine.retry_all().await;
    assert_eq!(r.fake.writes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refused_writes_are_retried_then_fail() {
    let r = rig(origins());
    r.fake.refuse_writes.store(true, Ordering::SeqCst);
    r.engine.earned(None, 3539, 60).await.unwrap();
    for _ in 0..MAX_ATTEMPTS {
        r.engine.retry_all().await;
    }
    let j = Journal::load(r.engine.journal_dir(), "user-1", 3539).unwrap();
    let e = j.entry(60).unwrap();
    assert_eq!(e.state, ReportState::Failed);
    assert!(e.last_error.as_deref().unwrap().contains("timed out"));
    let n = r.fake.writes.lock().unwrap().len();
    r.engine.retry_all().await;
    assert_eq!(r.fake.writes.lock().unwrap().len(), n, "failed ones aren't retried on their own");
    // A manual resend works once the server takes it.
    r.fake.refuse_writes.store(false, Ordering::SeqCst);
    let res = r.engine.report(None, 3539, 60, false).await.unwrap();
    assert!(res.sent);
    assert_eq!((res.entry.state, res.entry.via.as_str(), res.entry.confirmed), (ReportState::Reported, "manual", Some(true)));
}

#[tokio::test]
async fn silently_ignored_writes_show_as_unconfirmed() {
    let r = rig(origins());
    r.fake.ignore_writes.store(true, Ordering::SeqCst);
    r.engine.earned(None, 3539, 60).await.unwrap();
    let j = Journal::load(r.engine.journal_dir(), "user-1", 3539).unwrap();
    assert_eq!((j.entry(60).unwrap().state, j.entry(60).unwrap().confirmed), (ReportState::Reported, Some(false)));
    // Still shown as unlocked (the game earned it), with its journal state.
    let a = r.engine.status(None, 3539, true, "en-US").await.unwrap().achievements.unwrap();
    let x = a.items.iter().find(|x| x.id == "60").unwrap();
    assert_eq!((x.unlocked, x.report), (true, Some(ReportState::Reported)));
}

#[tokio::test]
async fn reporting_off_and_dry_run_send_nothing() {
    let r = rig(origins());
    *r.mode.lock().unwrap() = ReportMode::Off;
    assert_eq!(r.engine.earned(None, 3539, 60).await.unwrap(), EarnOutcome::New { state: ReportState::Off, unverified: false });
    *r.mode.lock().unwrap() = ReportMode::DryRun;
    assert_eq!(r.engine.earned(None, 3539, 58).await.unwrap(), EarnOutcome::New { state: ReportState::DryRun, unverified: false });
    r.engine.retry_all().await;
    assert!(r.fake.writes.lock().unwrap().is_empty());
    // Still announced (the player did earn them).
    assert_eq!(unlocked_ids(&r), vec![60, 58]);
    // A dry-run report shows the exact request without sending it.
    *r.mode.lock().unwrap() = ReportMode::On;
    let res = r.engine.report(None, 3539, 58, true).await.unwrap();
    assert!(!res.sent);
    assert!(r.fake.writes.lock().unwrap().is_empty());
    let req = uplay::Req::decode(hex::decode(&res.request_hex).unwrap().as_slice()).unwrap();
    let pa = &req.write_achievements_req.unwrap().achievement_blob.unwrap().product_achievements[0];
    assert_eq!((pa.product.product_id.as_str(), pa.achievements.achievements[0].achievement_id), ("ACE", 58));
}

#[tokio::test]
async fn only_journaled_achievements_can_be_resent() {
    let r = rig(origins());
    let err = r.engine.report(None, 3539, 59, false).await.unwrap_err();
    assert!(format!("{err:#}").contains("isn't in the journal"), "{err:#}");
    assert!(r.fake.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bridge_list() {
    let r = rig(origins());
    r.engine.earned(None, 3539, 60).await.unwrap();
    let (items, known) = r.engine.bridge_list(None, 3539, "en-US").await.unwrap();
    assert!(known);
    assert_eq!(items.len(), 60);
    let namaste = items.iter().find(|x| x.id == 59).unwrap();
    assert_eq!((namaste.name.as_str(), namaste.earned, namaste.earned_at), ("Namaste", true, 1_763_337_600));
    assert!(items.iter().find(|x| x.id == 60).unwrap().earned);
    assert!(!items.iter().find(|x| x.id == 1).unwrap().earned);
}

// ---------------------------------------------------------------------------------------
// ach_frontend over the (fake) demux socket

/// An `ach_frontend` that wants the token "ticket", holds ACE 59, and answers in `framing`.
fn ach_server(framing: Framing, writes: Arc<Mutex<Vec<uplay::Req>>>) -> crate::demux::fake::Handler {
    Box::new(move |service, _, body| {
        assert_eq!(service, "ach_frontend");
        let req = match framing {
            Framing::Raw => uplay::Req::decode(body).ok()?,
            Framing::Wrapped => crate::demux::AchUpstream::decode(body).ok()?.request?,
        };
        let mut rsp = uplay::Rsp { request_id: req.request_id, ..Default::default() };
        if let Some(a) = &req.authenticate_req {
            rsp.authenticate_rsp = Some(uplay::AuthenticateRsp { success: a.orbit_token == "ticket" });
        }
        if let Some(r) = &req.read_achievements_req {
            let p = r.product.clone().unwrap_or(uplay::Product { product_id: "ACE".into(), platform_id: "PC".into() });
            rsp.read_achievements_rsp = Some(uplay::ReadAchievementsRsp {
                user_id: r.user_id.clone().unwrap_or_default(),
                achievement_blob: rsp_blob(&[(p.product_id.as_str(), p.platform_id.as_str(), &[(59, Some(1_763_337_600))])]),
            });
        }
        if req.write_achievements_req.is_some() {
            writes.lock().unwrap().push(req.clone());
            rsp.write_achievements_rsp = Some(uplay::WriteAchievementsRsp {});
        }
        Some(match framing {
            Framing::Raw => rsp.encode_to_vec(),
            Framing::Wrapped => crate::demux::AchDownstream { response: Some(rsp) }.encode_to_vec(),
        })
    })
}

#[tokio::test]
async fn ach_frontend_over_demux() {
    for framing in [Framing::Raw, Framing::Wrapped] {
        let writes: Arc<Mutex<Vec<uplay::Req>>> = Arc::default();
        let server = crate::demux::fake::start(Duration::from_secs(5), ach_server(framing, writes.clone())).await;
        let mut c = AchConn::open(&server.demux, "ticket", framing).await.unwrap();
        let r = c.read("user-1", Some(&AchProduct::new("ACE"))).await.unwrap();
        assert_eq!(r.user_id, "user-1");
        let mine = read_blob(&r.achievement_blob, "ACE", Some("PC"));
        assert_eq!(mine.earned.keys().copied().collect::<Vec<_>>(), vec![59]);
        c.write(write_req(&AchProduct::new("ACE"), &[(60, 1_763_400_000)])).await.unwrap();
        let w = writes.lock().unwrap().clone();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].request_id, 3, "auth, read, write on one connection");
        let blob = w[0].write_achievements_req.as_ref().unwrap().achievement_blob.as_ref().unwrap();
        assert_eq!(blob, &super::blob(&AchProduct::new("ACE"), &[(60, 1_763_400_000)]));
    }
}

#[tokio::test]
async fn ach_frontend_refused_token_and_wrong_framing() {
    let writes: Arc<Mutex<Vec<uplay::Req>>> = Arc::default();
    let server = crate::demux::fake::start(Duration::from_millis(300), ach_server(Framing::Raw, writes.clone())).await;
    let err = AchConn::open(&server.demux, "other", Framing::Raw).await.err().unwrap();
    assert!(format!("{err:#}").contains("refused"), "{err:#}");
    // A server that doesn't understand the framing never answers: a timeout, not a hang.
    let err = AchConn::open(&server.demux, "ticket", Framing::Wrapped).await.err().unwrap();
    assert!(format!("{err:#}").contains("timed out"), "{err:#}");
}
