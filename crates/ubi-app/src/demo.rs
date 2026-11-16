//! Demo data (`QUIVER_DEMO=1`): a stand-in for `quiverd` that answers the same requests
//! from fixtures. The library and art come from `design/art/art.json` (the real product
//! configs for this library); install state, play time, settings history and activity
//! follow the mock-ups in `design/mockup/src`. One install job progresses live, and Play
//! starts a fake session until Stop, so event handling can be exercised too.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration as Span, Local, NaiveTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
pub mod detail;
pub mod downloads;

use ubi_api::{
    ActiveSession, ActivityEntry, Art, Event, Game, GameSettings, Job, JobKind, LibraryEntry, PlaySession, Playtime,
    Request, Revision, Status,
};

/// Accounts, exports, Steam and more Activity for the global screens.
#[path = "demo_global.rs"]
mod global;

/// The product configs the mock-ups were made from.
const ART_JSON: &str = include_str!("../../../design/art/art.json");

#[derive(Deserialize)]
struct ArtRecord {
    product_id: u32,
    lib_name: String,
    #[serde(default)]
    thumb_image: Option<String>,
    #[serde(default)]
    background_image: Option<String>,
    #[serde(default)]
    logo_image: Option<String>,
    #[serde(default)]
    exes: Vec<String>,
}

const GB: u64 = 1_000_000_000;

/// Per-game demo state, from the mock-up's `ST` table (app1.js).
struct Fixture {
    id: u32,
    installed: bool,
    hours: u64,
    /// Days since the last session.
    last_days: i64,
    update: bool,
}

const fn f(id: u32, installed: bool, hours: u64, last_days: i64, update: bool) -> Fixture {
    Fixture { id, installed, hours, last_days, update }
}

/// Installed games, most recently played first (mock: "Recent and installed").
const FIXTURES: &[Fixture] = &[
    f(3539, true, 87, 1, false),
    f(1651, true, 2, 6, false),
    f(5059, true, 142, 16, true),
    f(13504, true, 64, 64, false),
    f(720, true, 31, 105, false),
    f(1803, true, 22, 119, false),
    f(2688, true, 18, 165, false),
    f(277, true, 9, 198, false),
];

const INSTALLING: u32 = 6100; // AC Mirage, 64%
const QUEUED: u32 = 5595; // Trackmania

struct State {
    entries: Vec<LibraryEntry>,
    playtime: BTreeMap<u32, Playtime>,
    settings: BTreeMap<u32, GameSettings>,
    history: BTreeMap<u32, Vec<Revision>>,
    jobs: Vec<Job>,
    activity: Vec<ActivityEntry>,
    sessions: Vec<ActiveSession>,
    next_job: u64,
}

pub struct Demo {
    state: Mutex<State>,
    events: mpsc::UnboundedSender<Event>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/home/you".into()))
}

fn clean(name: &str) -> String {
    name.replace(['®', '™'], "").replace('’', "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Local time `days` ago at `h:m`, as UTC.
fn at(days: i64, h: u32, m: u32) -> DateTime<Utc> {
    let date = (Local::now() - Span::days(days)).date_naive();
    let t = date.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap());
    Local.from_local_datetime(&t).earliest().unwrap_or_else(|| Local::now()).with_timezone(&Utc)
}

fn session(days: i64, (h, m): (u32, u32), minutes: u64, rev: u32) -> PlaySession {
    let started = at(days, h, m);
    PlaySession { started, ended: started + Span::minutes(minutes as i64), seconds: minutes * 60, settings_revision: rev, profile: None, profile_revision: None, mods: Vec::new() }
}

fn rev(revision: u32, days: i64, source: &str, message: &str, settings: GameSettings) -> Revision {
    Revision { revision, at: at(days, 20, 10), source: source.into(), message: Some(message.into()), settings }
}

fn origins_history() -> (GameSettings, Vec<Revision>) {
    let base = GameSettings::default();
    let mut s = base.clone();
    let mut out = vec![rev(1, 44, "system", "Created from global defaults", s.clone())];
    s.wrappers = vec!["gamemoderun".into(), "mangohud".into()];
    out.push(rev(10, 25, "gui", "MangoHud preset FPS + frame time", s.clone()));
    out.push(rev(11, 11, "system", "Before update 1.5.1 to 1.6.0", s.clone()));
    s.env.insert("PROTON_USE_NTSYNC".into(), "1".into());
    out.push(rev(12, 5, "cli", "Sync: fsync to NTSync", s.clone()));
    s.env.remove("PROTON_USE_NTSYNC");
    s.proton = Some("GE-Proton10-25".into());
    out.push(rev(13, 3, "claude-code", "Pin GE-Proton10-25, back to fsync", s.clone()));
    s.wrappers = vec!["gamemoderun".into(), "gamescope".into(), "mangohud".into()];
    s.env.insert("DXVK_HDR".into(), "1".into());
    s.env.insert("ENABLE_HDR_WSI".into(), "1".into());
    s.args = vec!["-W".into(), "3440".into(), "-H".into(), "1440".into()];
    out.push(rev(14, 1, "gui", "Turn on gamescope with HDR", s.clone()));
    // Play runs the DLSS profile (demo/detail.rs) unless told otherwise.
    s.profile = Some(detail::ORIGINS_DEFAULT.into());
    out.push(rev(15, 0, "cli", "Default for Play: DLSS", s.clone()));
    out.reverse(); // newest first, like the daemon
    (s, out)
}

impl Demo {
    pub fn start() -> (Arc<Demo>, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let demo = Arc::new(Demo { state: Mutex::new(build_state()), events: tx });
        let weak = Arc::downgrade(&demo);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                let Some(demo) = weak.upgrade() else { break };
                demo.tick();
            }
        });
        (demo, rx)
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// Advance the download queue, verifies and imports (`demo/downloads.rs`).
    fn tick(&self) {
        let events = downloads::tick(self, &mut self.state.lock().unwrap());
        for e in events {
            self.emit(e);
        }
    }

    pub fn handle(&self, req: Request) -> Result<Value> {
        let mut st = self.state.lock().unwrap();
        let req = match downloads::handle(self, &mut st, req)? { Ok(v) => return Ok(v), Err(req) => req };
        let req = match detail::handle(self, &mut st, req)? { Ok(v) => return Ok(v), Err(req) => req };
        if let Some(v) = global::handle(self, &mut st, &req)? {
            return Ok(v);
        }
        Ok(match req {
            Request::Status => json!(Status {
                version: format!("{} (demo)", env!("CARGO_PKG_VERSION")),
                signed_in: Some("demo".into()),
                awaiting_2fa: false,
                running: st.sessions.iter().map(|s| s.product_id).collect(),
                jobs: st.jobs.clone(),
                proton_default: Some("GE-Proton10-26".into()),
                broker: String::new(),
                accounts: vec![ubi_api::Account { user_id: "demo".into(), name: "demo".into(), default: true }],
            }),
            Request::Library { .. } => json!(st.entries),
            Request::Playtime { product_id } => json!(st.playtime.get(&product_id).cloned().unwrap_or_default()),
            Request::SettingsGet { product_id } => json!(st.settings.get(&product_id).cloned().unwrap_or_default()),
            Request::SettingsHistory { product_id } => json!(st.history.get(&product_id).cloned().unwrap_or_default()),
            Request::Activity { limit } => json!(st.activity.iter().take(limit).collect::<Vec<_>>()),
            Request::Sessions => json!(st.sessions),
            Request::ProtonList => json!(["GE-Proton10-26", "GE-Proton10-25", "Proton 9.0 (Beta)"]),
            Request::Subscribe => Value::Null,
            Request::Install { product_id, .. } => {
                let name = name_of(&st, product_id)?;
                let id = st.next_job;
                st.next_job += 1;
                let queued = st.jobs.iter().any(|j| j.kind == JobKind::Install && !j.done);
                let job = Job {
                    id,
                    kind: JobKind::Install,
                    product_id,
                    name,
                    status: if queued { "Queued".into() } else { "Downloading · 0%".into() },
                    downloaded: 0,
                    total: 20 * GB,
                    done: false,
                    error: None,
                    detail: vec![],
                };
                st.jobs.push(job.clone());
                drop(st);
                self.emit(Event::Job(job));
                json!(id)
            }
            Request::Play { product_id, .. } => {
                let name = name_of(&st, product_id)?;
                if st.entries.iter().all(|e| e.product_id != product_id || e.installed.is_none()) {
                    bail!("{name} is not installed");
                }
                let id = st.next_job;
                st.next_job += 1;
                let job = Job {
                    id,
                    kind: JobKind::Play,
                    product_id,
                    name,
                    status: "Running".into(),
                    downloaded: 0,
                    total: 0,
                    done: false,
                    error: None,
                    detail: vec![],
                };
                st.jobs.push(job.clone());
                st.sessions.push(ActiveSession { product_id, account: "demo".into(), started: Utc::now(), holders: vec!["launcher".into()], reported: true, steam_app_id: None });
                if let Some(e) = st.entries.iter_mut().find(|e| e.product_id == product_id) {
                    e.running = true;
                }
                drop(st);
                self.emit(Event::Job(job));
                self.emit(Event::GameStarted { product_id });
                json!(id)
            }
            Request::Stop { product_id } => {
                st.sessions.retain(|s| s.product_id != product_id);
                let mut ended = None;
                for j in st.jobs.iter_mut().filter(|j| j.kind == JobKind::Play && j.product_id == product_id && !j.done) {
                    j.done = true;
                    j.status = "Exited".into();
                    ended = Some(j.clone());
                }
                if let Some(e) = st.entries.iter_mut().find(|e| e.product_id == product_id) {
                    e.running = false;
                }
                drop(st);
                if let Some(j) = ended {
                    self.emit(Event::Job(j));
                }
                self.emit(Event::GameStopped { product_id });
                Value::Null
            }
            Request::Cancel { job_id, .. } => {
                let job = st.jobs.iter_mut().find(|j| j.id == job_id).ok_or_else(|| anyhow!("no job {job_id}"))?;
                job.done = true;
                job.status = "Cancelled".into();
                let job = job.clone();
                drop(st);
                self.emit(Event::Job(job));
                Value::Null
            }
            Request::Login { .. } | Request::Login2fa { .. } | Request::Logout { .. } => bail!("demo mode has no Ubisoft account"),
            other => bail!("not available in demo mode: {}", method_name(&other)),
        })
    }
}

fn method_name(r: &Request) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned)).unwrap_or_default()
}

fn name_of(st: &State, id: u32) -> Result<String> {
    st.entries.iter().find(|e| e.product_id == id).map(|e| e.name.clone()).ok_or_else(|| anyhow!("unknown product {id}"))
}

fn game_for(id: u32, name: &str, exe: Option<&str>) -> Game {
    let games = home().join("Games");
    Game {
        product_id: id,
        name: name.to_owned(),
        install_dir: games.join("ubi-library").join(clean(name)),
        prefix: games.join("ubi-prefixes").join(id.to_string()),
        exe: exe.unwrap_or("game.exe").replace('\\', "/").trim_start_matches(['\'', '.', '/']).to_owned(),
        args: Some(format!("/appid:{id}")),
        language: Some("en-US".into()),
    }
}

/// The demo library: every product in art.json, in the mock-up's order (recent and
/// installed first, then the rest by name).
pub fn library() -> Vec<LibraryEntry> {
    let records: Vec<ArtRecord> = serde_json::from_str(ART_JSON).expect("design/art/art.json parses");
    let mut entries: Vec<(usize, LibraryEntry)> = records
        .into_iter()
        .map(|r| {
            let fx = FIXTURES.iter().position(|f| f.id == r.product_id);
            let installed = fx.map(|i| FIXTURES[i].installed).unwrap_or(false);
            let order = fx.unwrap_or(usize::MAX);
            let name = clean(&r.lib_name);
            let entry = LibraryEntry {
                product_id: r.product_id,
                installed: installed.then(|| game_for(r.product_id, &name, r.exes.first().map(String::as_str))),
                name,
                update_available: fx.is_some_and(|i| FIXTURES[i].update),
                running: false,
                art: Art { thumb: r.thumb_image, background: r.background_image, logo: r.logo_image, icon: None },
                owned_by: vec!["demo".into()],
                installable: true,
                steam: None,
            };
            (order, entry)
        })
        .collect();
    entries.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)));
    entries.into_iter().map(|(_, e)| e).collect()
}

fn build_state() -> State {
    let entries = library();
    let mut playtime = BTreeMap::new();
    for fx in FIXTURES {
        let mut sessions = vec![];
        if fx.id == 3539 {
            sessions = vec![
                session(1, (19, 33), 134, 14),
                session(3, (20, 2), 65, 13),
                session(5, (19, 31), 4, 12),
                session(7, (18, 10), 221, 11),
                session(9, (21, 5), 96, 10),
                session(12, (19, 48), 152, 10),
            ];
        } else {
            sessions.push(session(fx.last_days, (20, 15), 70 + (fx.id as u64 % 90), 1));
        }
        let last = sessions.first().map(|s| s.ended); // newest first
        playtime.insert(fx.id, Playtime { total_seconds: fx.hours * 3600 + 25 * 60, last_played: last, sessions });
    }
    let (origins, history) = origins_history();
    let mut settings = BTreeMap::new();
    settings.insert(3539, origins);
    let mut histories = BTreeMap::new();
    histories.insert(3539, history);
    for fx in FIXTURES.iter().filter(|f| f.id != 3539) {
        let mut s = GameSettings::default();
        if fx.id == 5059 {
            s.wrappers = vec!["gamemoderun".into()];
        }
        histories.insert(fx.id, vec![rev(1, 40, "system", "Created from global defaults", GameSettings::default())]);
        settings.insert(fx.id, s);
    }
    let name = |id: u32| entries.iter().find(|e| e.product_id == id).map(|e| e.name.clone()).unwrap_or_default();
    let jobs = vec![
        Job {
            id: 41,
            kind: JobKind::Install,
            product_id: INSTALLING,
            name: name(INSTALLING),
            status: "Downloading · 64%".into(),
            downloaded: 31_200_000_000,
            total: 48_700_000_000,
            done: false,
            error: None,
            detail: vec![],
        },
        Job {
            id: 42,
            kind: JobKind::Install,
            product_id: QUEUED,
            name: name(QUEUED),
            status: "Queued".into(),
            downloaded: 0,
            total: 6_200_000_000,
            done: false,
            error: None,
            detail: vec![],
        },
    ];
    let act = |mins: i64, source: &str, action: &str, id: Option<u32>| ActivityEntry {
        at: Utc::now() - Span::minutes(mins),
        source: source.into(),
        action: action.into(),
        product_id: id,
    };
    let activity = vec![
        act(4, "gui", "install started", Some(INSTALLING)),
        act(6, "claude-code", "install queued", Some(QUEUED)),
        act(90, "system", "update found: 1.6.1 (4.1 GB)", Some(5059)),
        act(60 * 20, "gui", "settings r14: Turn on gamescope with HDR", Some(3539)),
        act(60 * 21, "system", "cloud saves pushed (3 slots)", Some(3539)),
        act(60 * 70, "claude-code", "settings r13: Pin GE-Proton10-25, back to fsync", Some(3539)),
        act(60 * 120, "cli", "settings r12: Sync: fsync to NTSync", Some(3539)),
        act(60 * 24 * 6, "gui", "played 1 h 21 m", Some(1651)),
    ];
    State {
        entries,
        playtime,
        settings,
        history: histories,
        jobs,
        activity,
        sessions: vec![],
        next_job: 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_covers_art_json_with_art_names() {
        let lib = library();
        assert_eq!(lib.len(), 23);
        assert_eq!(lib[0].product_id, 3539, "most recent first");
        assert!(lib.iter().all(|e| e.art.thumb.is_some() && e.art.background.is_some()));
        assert_eq!(lib.iter().filter(|e| e.installed.is_some()).count(), FIXTURES.len());
        assert!(lib.iter().all(|e| !e.name.contains('®')));
        assert!(lib.iter().find(|e| e.product_id == 5059).unwrap().update_available);
    }

    #[tokio::test]
    async fn answers_requests_like_the_daemon() {
        let (demo, mut events) = Demo::start();
        let status: Status = serde_json::from_value(demo.handle(Request::Status).unwrap()).unwrap();
        assert_eq!(status.signed_in.as_deref(), Some("demo"));
        assert!(status.jobs.iter().filter(|j| j.kind == JobKind::Install && !j.done).count() >= 2);
        let pt: Playtime = serde_json::from_value(demo.handle(Request::Playtime { product_id: 3539 }).unwrap()).unwrap();
        assert_eq!(pt.total_seconds / 3600, 87);
        assert!(pt.sessions[0].started > pt.sessions[1].started, "newest first");
        let hist: Vec<Revision> =
            serde_json::from_value(demo.handle(Request::SettingsHistory { product_id: 3539 }).unwrap()).unwrap();
        assert_eq!(hist[0].revision, 15);
        assert!(demo.handle(Request::Login { email: "a".into(), password: "b".into() }).is_err());
        // The install job advances on its own.
        let e = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
        assert!(matches!(e, Event::Job(j) if j.product_id == INSTALLING && j.downloaded > 31_200_000_000));
        // Play emits a job and GameStarted.
        demo.handle(Request::Play { product_id: 3539, profile: None }).unwrap();
        let mut started = false;
        for _ in 0..4 {
            if let Ok(Some(Event::GameStarted { product_id: 3539 })) =
                tokio::time::timeout(Duration::from_secs(1), events.recv()).await
            {
                started = true;
                break;
            }
        }
        assert!(started);
        assert!(demo.handle(Request::Play { product_id: 6100, profile: None }).is_err(), "not installed");
    }
}
