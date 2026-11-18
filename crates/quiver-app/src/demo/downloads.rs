//! Demo answers for Downloads and first-run setup: a live queue (one download progressing,
//! with an occasional retry), install and update previews, verify and repair, download
//! settings, the existing-install scan with imports, and the first-run record. Shapes
//! follow the daemon (`crates/quiverd/src/downloads.rs`, `setup.rs`).
//!
//! `QUIVER_DEMO_GLOBAL` flags: `first-run` (no first-run record: the welcome flow starts on
//! its own), `fixes-undecided` (setup done, fixes left at "ask me later"), `welcome-fresh`
//! (nothing installed yet, so the folders can change), `low-space` (installs don't fit),
//! `retrying` (the download starts out retrying), `empty-queue`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use anyhow::{Result, anyhow, bail};
use chrono::{Duration as Span, Utc};
use serde_json::{Value, json};
use quiver_api::downloads::{
    CorruptFile, DownloadSettings, InstallPreview, LanguageSize, QueueEntry, QueueKind, QueueMove, QueueState, UpdatePreview, VerifyReport,
};
use quiver_api::mods::{ModPrefs, ModsRequest};
use quiver_api::setup::{FirstRun, FoundInstall, InstallSource, MatchedBy, SetupState};
use quiver_api::{ActivityEntry, Event, Job, JobKind, Request};

use super::{Demo, GB, State, clean, game_for, home};

fn flag(name: &str) -> bool {
    crate::ui::global::demo_flag(name).is_some()
}

/// Download sizes (bytes) for the demo library.
fn size_of(id: u32) -> u64 {
    let gb = |g: f64| (g * GB as f64) as u64;
    match id {
        6100 => gb(48.7),
        5595 => gb(6.2),
        420 => gb(40.1),
        5059 => gb(112.4),
        2688 => gb(47.6),
        5183 => gb(48.2),
        1875 => gb(45.7),
        1847 | 1848 => gb(3.9),
        5100 => gb(23.4),
        5404 => gb(30.8),
        5405 => gb(31.0),
        277 => gb(1.2),
        5408 => gb(0.4),
        5277 => gb(22.6),
        11784 => gb(0.9),
        3600 => gb(9.8),
        3353 => gb(47.5),
        3539 => gb(61.8),
        13504 => gb(131.0),
        720 => gb(51.2),
        1803 => gb(44.9),
        1651 => gb(3.4),
        _ => gb(20.0),
    }
}

const FREE: u64 = 1_210_000_000_000;
const TOTAL: u64 = 2_000_000_000_000;
const LANGS: [(&str, f64); 6] = [("en-US", 2.1), ("fr-FR", 1.9), ("de-DE", 1.9), ("es-ES", 1.8), ("it-IT", 1.8), ("ja-JP", 2.3)];

struct Import {
    job: u64,
    product_id: u32,
    done: u32,
    total: u32,
    extras: Vec<String>,
}

struct Dl {
    queue: Vec<QueueEntry>,
    settings: DownloadSettings,
    reports: BTreeMap<u32, VerifyReport>,
    /// A verify running: product, job, bytes read, total.
    verifying: Option<(u32, u64, u64, u64)>,
    imports: Vec<Import>,
    first_run: Option<FirstRun>,
    fixes: Option<bool>,
    library: PathBuf,
    prefixes: PathBuf,
    ticks: u64,
    /// The running download retries until this tick.
    retry_until: u64,
}

static DL: LazyLock<Mutex<HashMap<usize, Dl>>> = LazyLock::new(Mutex::default);

fn job(id: u64, kind: JobKind, product_id: u32, name: &str, status: &str, downloaded: u64, total: u64) -> Job {
    Job { id, kind, game: quiver_api::GameRef::ubisoft(product_id), name: name.into(), status: status.into(), downloaded, total, done: false, error: None, detail: vec![] }
}

fn entry(st: &State, id: u32, kind: QueueKind, state: QueueState, job_id: u64, source: &str, mins_ago: i64) -> QueueEntry {
    QueueEntry {
        product_id: id,
        game: None,
        name: super::name_of(st, id).unwrap_or_default(),
        kind,
        state,
        position: 0,
        job_id: Some(job_id),
        language: Some("en-US".into()),
        account: None,
        source: source.into(),
        added: Utc::now() - Span::minutes(mins_ago),
        error: None,
        done_bytes: 0,
        total_bytes: 0,
        speed: 0,
        retries: 0,
    }
}

impl Dl {
    /// The mock-up's queue: Mirage downloading, an auto-update and an agent's install
    /// waiting, a repair paused, a failed install; and a few finished jobs.
    fn seed(st: &mut State) -> Dl {
        let games = home().join("Games");
        let mut d = Dl {
            queue: vec![],
            settings: DownloadSettings::default(),
            reports: BTreeMap::new(),
            verifying: None,
            imports: vec![],
            first_run: (!flag("first-run")).then(|| FirstRun { at: Utc::now() - Span::days(12), source: "gui".into(), skipped: false, steam_on_install: true }),
            fixes: (!flag("fixes-undecided") && !flag("first-run")).then_some(true),
            library: games.join("quiver-library"),
            prefixes: games.join("quiver-prefixes"),
            ticks: 0,
            retry_until: if flag("retrying") { 8 } else { 0 },
        };
        let name = |st: &State, id| super::name_of(st, id).unwrap_or_default();
        if flag("empty-queue") {
            st.jobs.retain(|j| j.kind != JobKind::Install);
        } else {
            let (mirage, odyssey, trackmania, wd2, fc4) = (6100, 5059, 5595, 2688, 420);
            let mut m = entry(st, mirage, QueueKind::Install, QueueState::Downloading, 41, "gui", 9);
            m.done_bytes = 31_200_000_000;
            m.total_bytes = size_of(mirage);
            m.speed = 84_600_000;
            m.retries = 2;
            let mut o = entry(st, odyssey, QueueKind::Update, QueueState::Queued, 43, "auto-update", 90);
            o.language = Some("en-US".into());
            let t = entry(st, trackmania, QueueKind::Install, QueueState::Queued, 42, "claude-code", 6);
            let mut w = entry(st, wd2, QueueKind::Repair, QueueState::Paused, 44, "cli", 60 * 3);
            w.done_bytes = 21_300_000_000;
            w.total_bytes = size_of(wd2);
            let mut f = entry(st, fc4, QueueKind::Install, QueueState::Failed, 45, "gui", 60 * 26);
            f.done_bytes = 12_000_000_000;
            f.total_bytes = size_of(fc4);
            f.error = Some("slice 1f3a9c… failed 6 times: connection reset by peer (Retry continues from 12.0 GB)".into());
            d.queue = vec![m, o, t, w, f];
            st.jobs.push(job(43, JobKind::Install, odyssey, &name(st, odyssey), "Queued · #2", 0, 0));
            st.jobs.push(job(44, JobKind::Install, wd2, &name(st, wd2), "Paused", 21_300_000_000, size_of(wd2)));
            let mut fj = job(45, JobKind::Install, fc4, &name(st, fc4), "Failed", 12_000_000_000, size_of(fc4));
            fj.done = true;
            fj.error = d.queue[4].error.clone();
            st.jobs.push(fj);
        }
        // Finished earlier.
        let mut done = |id: u64, kind: JobKind, pid: u32, status: &str, bytes: u64, detail: Vec<String>| {
            let mut j = job(id, kind, pid, &name(st, pid), status, bytes, bytes);
            j.done = true;
            j.detail = detail;
            st.jobs.push(j);
        };
        done(36, JobKind::Install, 1651, "Installed Assassin's Creed Chronicles China", size_of(1651), vec![]);
        done(37, JobKind::Verify, 13504, "All 1,204 files OK", size_of(13504), vec![]);
        done(38, JobKind::Install, 3539, "Updated Assassin's Creed Origins", 2_300_000_000, vec![]);
        d.reports.insert(
            3539,
            VerifyReport {
                product_id: 3539,
                game: None,
                at: Some(Utc::now() - Span::days(2)),
                version: Some("1.6.0".into()),
                files: 1_412,
                bytes: size_of(3539),
                ok: 1_410,
                missing: vec!["videos/en/intro_ubisoft.bk2".into()],
                corrupt: vec![CorruptFile {
                    path: "DataPC_ACE_Egypt.forge".into(),
                    size: 4_831_838_208,
                    expected_size: 4_831_838_208,
                    bad_slices: 3,
                    bad_bytes: 1_572_864,
                }],
                unverified: vec![],
                repaired_files: 0,
                repaired_bytes: 0,
            },
        );
        d.reports.insert(13504, VerifyReport { product_id: 13504, at: Some(Utc::now() - Span::hours(5)), version: Some("1.7.0".into()), files: 1_204, bytes: size_of(13504), ok: 1_204, ..Default::default() });
        d.renumber(st);
        d
    }

    fn renumber(&mut self, st: &mut State) {
        for (i, e) in self.queue.iter_mut().enumerate() {
            e.position = i as u32 + 1;
            let line = match e.state {
                QueueState::Queued => Some(format!("Queued · #{}", e.position)),
                QueueState::Paused => Some("Paused".to_owned()),
                _ => None,
            };
            if let (Some(line), Some(j)) = (line, st.jobs.iter_mut().find(|j| Some(j.id) == e.job_id)) {
                j.status = line;
            }
        }
    }

    /// Start queued items while fewer than `max_active` run.
    fn pump(&mut self, st: &mut State) -> Vec<Event> {
        let mut out = vec![];
        let max = self.settings.max_active.max(1);
        let mut active = self.queue.iter().filter(|e| e.state.is_active()).count();
        for e in self.queue.iter_mut() {
            if active >= max {
                break;
            }
            if e.state != QueueState::Queued {
                continue;
            }
            e.state = if e.kind == QueueKind::Repair { QueueState::Verifying } else { QueueState::Downloading };
            e.total_bytes = size_of(e.product_id);
            if e.kind == QueueKind::Update {
                e.total_bytes = 4_100_000_000;
            }
            active += 1;
            if let Some(j) = st.jobs.iter_mut().find(|j| Some(j.id) == e.job_id) {
                j.status = if e.kind == QueueKind::Repair { "Verifying…".into() } else { "Downloading…".into() };
                j.total = e.total_bytes;
                j.downloaded = e.done_bytes;
                out.push(Event::Job(j.clone()));
            }
        }
        self.renumber(st);
        out
    }

    fn new_job(st: &mut State, kind: JobKind, product_id: u32, status: &str) -> Job {
        let id = st.next_job;
        st.next_job += 1;
        let j = job(id, kind, product_id, &super::name_of(st, product_id).unwrap_or_default(), status, 0, 0);
        st.jobs.push(j.clone());
        j
    }

    fn enqueue(&mut self, st: &mut State, product_id: u32, kind: QueueKind, language: Option<String>) -> Result<(u64, Vec<Event>)> {
        if let Some(e) = self.queue.iter().find(|e| e.product_id == product_id) {
            let id = e.job_id.unwrap_or(0);
            if matches!(e.state, QueueState::Paused | QueueState::Failed) {
                return self.resume(st, product_id);
            }
            return Ok((id, vec![]));
        }
        let j = Self::new_job(st, JobKind::Install, product_id, "Queued");
        let mut e = entry(st, product_id, kind, QueueState::Queued, j.id, "gui", 0);
        e.language = language.or(Some("en-US".into()));
        self.queue.push(e);
        let what = match kind {
            QueueKind::Install => "install",
            QueueKind::Update => "update",
            QueueKind::Repair => "repair",
        };
        st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: format!("queued {what} of {}", j.name), game: Some(quiver_api::GameRef::ubisoft(product_id)) });
        let mut ev = vec![Event::Job(j.clone())];
        ev.extend(self.pump(st));
        ev.extend(self.queue.iter().filter_map(|e| st.jobs.iter().find(|j| Some(j.id) == e.job_id)).map(|j| Event::Job(j.clone())));
        Ok((j.id, ev))
    }

    fn resume(&mut self, st: &mut State, product_id: u32) -> Result<(u64, Vec<Event>)> {
        let e = self.queue.iter_mut().find(|e| e.product_id == product_id).ok_or_else(|| anyhow!("not in the queue"))?;
        if e.state == QueueState::Failed {
            e.error = None;
            let j = Self::new_job(st, JobKind::Install, product_id, "Queued");
            e.job_id = Some(j.id);
        }
        if matches!(e.state, QueueState::Paused | QueueState::Failed) {
            e.state = QueueState::Queued;
        }
        let id = e.job_id.unwrap_or(0);
        let mut ev = self.pump(st);
        ev.extend(st.jobs.iter().filter(|j| j.id == id).map(|j| Event::Job(j.clone())));
        Ok((id, ev))
    }

    fn state(&self, st: &State) -> SetupState {
        let fresh = flag("welcome-fresh");
        let installed = if fresh { 0 } else { st.entries.iter().filter(|e| e.installed.is_some()).count() };
        SetupState {
            first_run: self.first_run.clone(),
            library: self.library.clone(),
            prefixes: self.prefixes.clone(),
            library_free: Some(if flag("low-space") { 31_400_000_000 } else { FREE }),
            library_total: Some(TOTAL),
            prefixes_free: Some(if flag("low-space") { 31_400_000_000 } else { FREE }),
            prefixes_total: Some(TOTAL),
            installed,
            paths_locked: !fresh && (installed > 0 || !self.queue.is_empty()),
            fixes_on_vanilla: self.fixes,
            restarting: false,
        }
    }

    fn preview(&self, st: &State, product_id: u32, language: Option<String>) -> Result<InstallPreview> {
        let e = st.entries.iter().find(|e| e.ubisoft_id() == Some(product_id)).ok_or_else(|| anyhow!("unknown product {product_id}"))?;
        let lang = language.unwrap_or_else(|| "en-US".into());
        let base = size_of(product_id);
        let extra = |code: &str| LANGS.iter().find(|(c, _)| *c == code).map(|(_, g)| (g * GB as f64) as u64).unwrap_or(0);
        let download = base + extra(&lang) - extra("en-US");
        let install = download + download / 12;
        let done = self.queue.iter().find(|q| q.product_id == product_id).map(|q| q.done_bytes).unwrap_or(0);
        let free = if flag("low-space") { 31_400_000_000 } else { FREE };
        let required = install.saturating_sub(done);
        let margin = (1u64 << 30).max(required / 20);
        Ok(InstallPreview {
            product_id,
            game: None,
            name: e.name.clone(),
            dest: self.library.join(clean(&e.name)),
            version: Some(match product_id {
                6100 => "1.0.7",
                5183 => "1.03",
                5595 => "2025-11-04",
                _ => "1.0.2",
            }
            .into()),
            language: lang,
            languages: LANGS.iter().map(|(c, g)| LanguageSize { code: (*c).into(), download_bytes: (g * GB as f64) as u64, install_bytes: (g * 1.1 * GB as f64) as u64 }).collect(),
            download_bytes: download,
            install_bytes: install,
            remaining_download_bytes: download.saturating_sub(done),
            required_bytes: required,
            free_bytes: free,
            margin_bytes: margin,
            fits: required + margin <= free,
            update: None,
        })
    }

    fn found(&self, st: &State) -> Vec<FoundInstall> {
        let games = home().join("Games");
        let connect = games.join("ubisoft-connect");
        let in_connect = |folder: &str| connect.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games").join(folder);
        let installed = |id: u32| st.entries.iter().any(|e| e.ubisoft_id() == Some(id) && e.installed.is_some());
        let name = |id: u32| super::name_of(st, id).unwrap_or_default();
        let lutris = vec!["Ubisoft Connect".to_owned(), "Assassin's Creed Origins (DLSS)".to_owned(), "Assassin's Creed III Remastered".to_owned()];
        let mk = |path: PathBuf, prefix: &PathBuf, source: InstallSource, label: &str, id: Option<u32>, by: Option<MatchedBy>, size: f64, files: usize, version: Option<&str>, note: Option<String>| {
            let inst = id.is_some_and(installed);
            let note = if inst { Some(format!("Already in Quiver (~/Games/quiver-library/{})", clean(&name(id.unwrap())))) } else { note };
            FoundInstall {
                path,
                prefix: prefix.clone(),
                source,
                source_label: label.into(),
                lutris_games: if source == InstallSource::Lutris { lutris.clone() } else { vec![] },
                product_id: id,
                matched_by: by,
                name: id.map(name).unwrap_or_else(|| "Assassin's Creed Syndicate".into()),
                language: Some("en-US".into()),
                version: version.map(str::to_owned),
                size_bytes: (size * GB as f64) as u64,
                files,
                owned: Some(true),
                installed: inst,
                importable: note.is_none() && !inst,
                note,
            }
        };
        let steam_pfx = home().join(".local/share/Steam/steamapps/compatdata/2195250/pfx");
        let mut v = vec![
            mk(in_connect("Assassin's Creed III Remastered"), &connect, InstallSource::Lutris, "Lutris · Ubisoft Connect", Some(5183), Some(MatchedBy::Registry), 48.2, 3_412, Some("1.03"), None),
            mk(in_connect("Immortals Fenyx Rising"), &connect, InstallSource::Lutris, "Lutris · Ubisoft Connect", Some(5405), Some(MatchedBy::Registry), 31.0, 2_087, Some("1.3.4"), None),
            mk(
                steam_pfx.join("drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Watch Dogs Legion"),
                &steam_pfx,
                InstallSource::Steam,
                "Connect inside Steam · Riders Republic",
                Some(3353),
                Some(MatchedBy::Name),
                47.5,
                1_904,
                Some("4.6"),
                None,
            ),
            mk(in_connect("Assassin's Creed Origins"), &connect, InstallSource::Lutris, "Lutris · Ubisoft Connect", Some(3539), Some(MatchedBy::Registry), 76.4, 411, Some("1.6.0"), None),
            mk(
                games.join("ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Assassin's Creed Syndicate"),
                &games.join("ubisoft-connect"),
                InstallSource::Wine,
                "Wine · ~/Games/ubisoft-connect",
                None,
                None,
                0.06,
                2,
                None,
                Some("No uplay_install.manifest: Connect didn't finish installing it, or uninstalled it".into()),
            ),
        ];
        v.sort_by(|a, b| b.importable.cmp(&a.importable));
        v
    }

    fn update_preview(&self, st: &State, product_id: u32) -> Result<UpdatePreview> {
        let e = st.entries.iter().find(|e| e.ubisoft_id() == Some(product_id)).ok_or_else(|| anyhow!("unknown product {product_id}"))?;
        if e.installed.is_none() {
            bail!("not installed");
        }
        if !e.update_available {
            return Ok(UpdatePreview {
                product_id,
                game: None,
                name: e.name.clone(),
                installed_version: Some("1.6.0".into()),
                latest_version: Some("1.6.0".into()),
                up_to_date: true,
                download_bytes: 0,
                write_bytes: 0,
                changed: vec![],
                added: vec![],
                removed: vec![],
                unchanged: 1_412,
                notes: vec![],
            });
        }
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Ok(UpdatePreview {
            product_id,
            game: None,
            name: e.name.clone(),
            installed_version: Some("1.5.6".into()),
            latest_version: Some("1.6.1".into()),
            up_to_date: false,
            download_bytes: 4_100_000_000,
            write_bytes: 6_300_000_000,
            changed: s(&["ACOdyssey.exe", "DataPC_patch_01.forge", "DataPC_patch_02.forge", "DataPC_Greece_ext.forge", "scimitar_engine.dll", "uplay_install.manifest", "DataPC_LowRes.forge"]),
            added: s(&["DataPC_patch_03.forge", "videos/en/legacy_s2.bk2"]),
            removed: s(&["DataPC_hotfix_1_5_4.forge"]),
            unchanged: 1_312,
            notes: s(&["1.9 GB of the changed files' slices is already on disk and isn't downloaded again"]),
        })
    }
}

/// Answer a Downloads or setup request, or hand it back for `demo.rs` to answer.
pub(super) fn handle(demo: &Demo, st: &mut State, req: Request) -> Result<std::result::Result<Value, Request>> {
    let key = demo as *const Demo as usize;
    let mut all = DL.lock().unwrap();
    let d = all.entry(key).or_insert_with(|| Dl::seed(st));
    let mut events = vec![];
    let out = match req {
        Request::Queue => json!(d.queue),
        Request::QueueMove { game, to } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let q = &mut d.queue;
            let i = q.iter().position(|e| e.product_id == product_id).ok_or_else(|| anyhow!("not in the queue"))?;
            match to {
                QueueMove::Up if i > 0 => q.swap(i, i - 1),
                QueueMove::Down if i + 1 < q.len() => q.swap(i, i + 1),
                QueueMove::Top => {
                    let e = q.remove(i);
                    q.insert(0, e);
                }
                _ => {}
            }
            d.renumber(st);
            events.extend(d.queue.iter().filter_map(|e| st.jobs.iter().find(|j| Some(j.id) == e.job_id)).map(|j| Event::Job(j.clone())));
            json!(d.queue)
        }
        Request::QueuePause { game } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let e = d.queue.iter_mut().find(|e| e.product_id == product_id).ok_or_else(|| anyhow!("not in the queue"))?;
            if e.state == QueueState::Failed {
                bail!("{} can't be paused (Failed)", e.name);
            }
            e.state = QueueState::Paused;
            e.speed = 0;
            let id = e.job_id;
            d.renumber(st);
            events.extend(st.jobs.iter().filter(|j| Some(j.id) == id).map(|j| Event::Job(j.clone())));
            events.extend(d.pump(st));
            Value::Null
        }
        Request::QueueResume { game } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let (id, ev) = d.resume(st, product_id)?;
            events.extend(ev);
            json!(id)
        }
        Request::QueueRemove { game } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let i = d.queue.iter().position(|e| e.product_id == product_id).ok_or_else(|| anyhow!("not in the queue"))?;
            let e = d.queue.remove(i);
            if let Some(j) = st.jobs.iter_mut().find(|j| Some(j.id) == e.job_id) {
                if !j.done {
                    j.done = true;
                    j.status = "Removed from queue".into();
                }
                events.push(Event::Job(j.clone()));
            }
            events.extend(d.pump(st));
            Value::Null
        }
        Request::Install { game, language } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let kind = if st.entries.iter().any(|e| e.ubisoft_id() == Some(product_id) && e.installed.is_some()) { QueueKind::Update } else { QueueKind::Install };
            if kind == QueueKind::Install && flag("low-space") {
                bail!("not enough space on ~/Games: needs {} plus a margin", crate::model::gb(size_of(product_id)));
            }
            let (id, ev) = d.enqueue(st, product_id, kind, language)?;
            events.extend(ev);
            json!(id)
        }
        Request::Update { game } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let (id, ev) = d.enqueue(st, product_id, QueueKind::Update, None)?;
            events.extend(ev);
            json!(id)
        }
        Request::Cancel { job_id, .. } if d.queue.iter().any(|e| e.job_id == Some(job_id)) => {
            let pid = d.queue.iter().find(|e| e.job_id == Some(job_id)).map(|e| e.product_id).unwrap_or(0);
            d.queue.retain(|e| e.product_id != pid);
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job_id) {
                j.done = true;
                j.status = "Removed from queue (install again to resume)".into();
                events.push(Event::Job(j.clone()));
            }
            events.extend(d.pump(st));
            Value::Null
        }
        Request::InstallPreview { game, language } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let mut p = d.preview(st, product_id, language)?;
            if st.entries.iter().any(|e| e.ubisoft_id() == Some(product_id) && e.installed.is_some()) {
                p.update = Some(d.update_preview(st, product_id)?);
            }
            json!(p)
        }
        Request::UpdatePreview { game } => { let product_id = game.as_ubisoft().unwrap_or(0); json!(d.update_preview(st, product_id)?) }
        Request::Verify { game, repair: true } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let (id, ev) = d.enqueue(st, product_id, QueueKind::Repair, None)?;
            events.extend(ev);
            json!(id)
        }
        Request::Verify { game, repair: false } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            if d.verifying.is_some() {
                bail!("another game is being verified (demo: one at a time)");
            }
            let j = Dl::new_job(st, JobKind::Verify, product_id, "Verifying…");
            d.verifying = Some((product_id, j.id, 0, size_of(product_id)));
            events.push(Event::Job(j.clone()));
            st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: format!("verify {}", j.name), game: Some(quiver_api::GameRef::ubisoft(product_id)) });
            json!(j.id)
        }
        Request::VerifyResult { game } => { let product_id = game.as_ubisoft().unwrap_or(0); json!(d.reports.get(&product_id)) }
        Request::DownloadSettings { max_active, bandwidth_limit, auto_update } => {
            if let Some(n) = max_active {
                if n == 0 {
                    bail!("max_active must be at least 1");
                }
                d.settings.max_active = n;
            }
            if let Some(b) = bandwidth_limit {
                d.settings.bandwidth_limit = (b > 0).then_some(b);
            }
            if let Some(a) = auto_update {
                d.settings.auto_update = a;
            }
            events.extend(d.pump(st));
            json!(d.settings)
        }
        Request::Setup { library, prefixes, finish } => {
            let s = d.state(st);
            if library.is_some() || prefixes.is_some() {
                if s.paths_locked {
                    bail!("games are installed, queued or importing, so the library and compatibility folders can't change now");
                }
                if let Some(p) = library {
                    d.library = p;
                }
                if let Some(p) = prefixes {
                    d.prefixes = p;
                }
            }
            if let Some(f) = finish {
                d.first_run = Some(FirstRun { at: Utc::now(), source: "gui".into(), skipped: f.skipped, steam_on_install: f.steam_on_install });
            }
            json!(d.state(st))
        }
        Request::ScanInstalls => json!(d.found(st)),
        // Mods v2's app-level preference, kept here so the welcome flow's choice sticks.
        Request::Mods(ModsRequest::Prefs { fixes_on_vanilla }) => {
            if fixes_on_vanilla.is_some() {
                d.fixes = fixes_on_vanilla;
            }
            json!(ModPrefs { fixes_on_vanilla: d.fixes })
        }
        Request::Import { game, path, .. } => {
            let product_id = game.as_ubisoft().unwrap_or(0);
            let found = d.found(st).into_iter().find(|f| f.path == path && f.importable).ok_or_else(|| anyhow!("{} has no uplay_install.manifest", path.display()))?;
            let j = Dl::new_job(st, JobKind::Import, product_id, "Reading the manifest…");
            let extras = if product_id == 5183 {
                vec!["ACIII.exe.orig".to_owned(), "d3d11.dll".into(), "reshade-shaders/".into(), "ReShade.ini".into()]
            } else {
                vec![]
            };
            d.imports.push(Import { job: j.id, product_id, done: 0, total: found.files as u32, extras });
            events.push(Event::Job(j.clone()));
            json!(j.id)
        }
        other => return Ok(Err(other)),
    };
    drop(all);
    for e in events {
        demo.emit(e);
    }
    Ok(Ok(out))
}

/// Every 500 ms: the running download, a verify, imports.
pub(super) fn tick(demo: &Demo, st: &mut State) -> Vec<Event> {
    let key = demo as *const Demo as usize;
    let mut all = DL.lock().unwrap();
    let d = all.entry(key).or_insert_with(|| Dl::seed(st));
    d.ticks += 1;
    let t = d.ticks;
    let mut events = vec![];
    let mut finished = vec![];
    // Every 40 ticks a short stretch of retries.
    if t % 40 == 25 {
        d.retry_until = t + 6;
    }
    let retrying = t < d.retry_until;
    let limit = d.settings.bandwidth_limit;
    for e in d.queue.iter_mut().filter(|e| e.state.is_active()) {
        let wave = 84_600_000.0 + 9_000_000.0 * ((t as f64) * 0.45).sin() + 4_500_000.0 * ((t as f64) * 1.7).sin();
        let mut speed = if retrying { 0 } else { wave as u64 };
        if let Some(l) = limit {
            speed = speed.min(l);
        }
        if e.state == QueueState::Verifying {
            speed = 1_600_000_000;
        }
        if retrying && e.state == QueueState::Downloading && t + 5 == d.retry_until {
            e.retries += 1;
        }
        e.speed = speed;
        e.done_bytes = (e.done_bytes + speed / 2).min(e.total_bytes);
        let status = if retrying && e.state == QueueState::Downloading {
            format!("Retrying… ({} {})", e.retries, if e.retries == 1 { "retry" } else { "retries" })
        } else if e.state == QueueState::Verifying {
            "Verifying…".into()
        } else {
            "Downloading…".into()
        };
        if let Some(j) = st.jobs.iter_mut().find(|j| Some(j.id) == e.job_id) {
            j.downloaded = e.done_bytes;
            j.total = e.total_bytes;
            j.status = status;
            events.push(Event::Job(j.clone()));
        }
        if e.done_bytes >= e.total_bytes {
            if e.state == QueueState::Verifying {
                // The repair's verify found nothing wrong.
                e.state = QueueState::Downloading;
                e.done_bytes = e.total_bytes;
            }
            finished.push(e.product_id);
        }
    }
    for id in finished {
        let Some(i) = d.queue.iter().position(|e| e.product_id == id) else { continue };
        let e = d.queue.remove(i);
        let was_installed = st.entries.iter().any(|x| x.ubisoft_id() == Some(id) && x.installed.is_some());
        if let Some(x) = st.entries.iter_mut().find(|x| x.ubisoft_id() == Some(id)) {
            if x.installed.is_none() {
                x.installed = Some(game_for(id, &x.name, None));
            }
            x.update_available = false;
        }
        let status = match e.kind {
            QueueKind::Repair => "All 2,031 files OK".to_owned(),
            _ if was_installed => format!("Updated {}", clean(&e.name)),
            _ => format!("Installed {}", clean(&e.name)),
        };
        if let Some(j) = st.jobs.iter_mut().find(|j| Some(j.id) == e.job_id) {
            j.done = true;
            j.status = status;
            j.downloaded = j.total;
            events.push(Event::Job(j.clone()));
        }
        st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "system".into(), action: "install finished".into(), game: Some(quiver_api::GameRef::ubisoft(id)) });
        events.extend(d.pump(st));
        events.push(Event::LibraryChanged);
    }
    // A verify.
    if let Some((pid, jid, done, total)) = d.verifying {
        let done = (done + total / 10 + 1).min(total);
        let finished = done >= total;
        if let Some(j) = st.jobs.iter_mut().find(|j| j.id == jid) {
            j.downloaded = done;
            j.total = total;
            if finished {
                j.done = true;
                let bad = pid == 3539;
                j.status = if bad { "1 missing, 1 corrupt of 1412 files".into() } else { "All 1,204 files OK".into() };
                if bad {
                    j.detail = vec!["missing: videos/en/intro_ubisoft.bk2".into(), "corrupt: DataPC_ACE_Egypt.forge (3 bad slice(s))".into()];
                }
            }
            events.push(Event::Job(j.clone()));
        }
        if finished {
            let mut r = d.reports.get(&pid).cloned().unwrap_or(VerifyReport { product_id: pid, files: 1_204, ok: 1_204, bytes: total, ..Default::default() });
            r.at = Some(Utc::now());
            r.repaired_files = 0;
            d.reports.insert(pid, r);
            d.verifying = None;
        } else {
            d.verifying = Some((pid, jid, done, total));
        }
    }
    // Imports.
    let mut imported = vec![];
    for imp in d.imports.iter_mut() {
        if imp.done >= imp.total {
            continue;
        }
        imp.done = (imp.done + imp.total / 7 + 1).min(imp.total);
        if let Some(j) = st.jobs.iter_mut().find(|j| j.id == imp.job) {
            if imp.done >= imp.total {
                j.done = true;
                j.status = format!("Imported {} files", imp.total);
                j.detail = imp.extras.clone();
                imported.push(imp.product_id);
            } else {
                j.status = format!("Copying files {}/{}…", imp.done, imp.total);
            }
            events.push(Event::Job(j.clone()));
        }
    }
    for id in imported {
        if let Some(x) = st.entries.iter_mut().find(|x| x.ubisoft_id() == Some(id)) {
            x.installed = Some(game_for(id, &x.name, None));
        }
        st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action: "imported from Lutris".into(), game: Some(quiver_api::GameRef::ubisoft(id)) });
        events.push(Event::LibraryChanged);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn queue_previews_verify_and_setup() {
        let (demo, _events) = Demo::start();
        let q: Vec<QueueEntry> = serde_json::from_value(demo.handle(Request::Queue).unwrap()).unwrap();
        assert_eq!(q.iter().map(|e| e.product_id).collect::<Vec<_>>(), [6100, 5059, 5595, 2688, 420]);
        assert_eq!(q[0].state, QueueState::Downloading);
        assert_eq!(q[4].state, QueueState::Failed);
        // Reorder, pause, resume, retry, remove.
        let q: Vec<QueueEntry> = serde_json::from_value(demo.handle(Request::QueueMove { game: 5595.into(), to: QueueMove::Up }).unwrap()).unwrap();
        assert_eq!(q[1].product_id, 5595);
        assert_eq!(q[1].position, 2);
        demo.handle(Request::QueuePause { game: 6100.into()}).unwrap();
        let q: Vec<QueueEntry> = serde_json::from_value(demo.handle(Request::Queue).unwrap()).unwrap();
        assert_eq!(q[0].state, QueueState::Paused);
        assert!(q[1].state.is_active(), "the next one starts");
        demo.handle(Request::QueueResume { game: 420.into()}).unwrap();
        demo.handle(Request::QueueRemove { game: 2688.into()}).unwrap();
        let q: Vec<QueueEntry> = serde_json::from_value(demo.handle(Request::Queue).unwrap()).unwrap();
        assert!(q.iter().all(|e| e.product_id != 2688 && e.state != QueueState::Failed));
        // Previews.
        let p: InstallPreview = serde_json::from_value(demo.handle(Request::InstallPreview { game: 5183.into(), language: Some("fr-FR".into()) }).unwrap()).unwrap();
        assert!(p.fits && p.language == "fr-FR" && p.languages.len() > 1);
        let u: UpdatePreview = serde_json::from_value(demo.handle(Request::UpdatePreview { game: 5059.into()}).unwrap()).unwrap();
        assert!(!u.up_to_date && !u.changed.is_empty());
        // Verify runs and leaves a report.
        let r: Option<VerifyReport> = serde_json::from_value(demo.handle(Request::VerifyResult { game: 3539.into()}).unwrap()).unwrap();
        assert!(!r.unwrap().is_ok());
        let job: u64 = serde_json::from_value(demo.handle(Request::Verify { game: 13504.into(), repair: false }).unwrap()).unwrap();
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let s: quiver_api::Status = serde_json::from_value(demo.handle(Request::Status).unwrap()).unwrap();
            if s.jobs.iter().any(|j| j.id == job && j.done) {
                break;
            }
        }
        let s: quiver_api::Status = serde_json::from_value(demo.handle(Request::Status).unwrap()).unwrap();
        assert!(s.jobs.iter().any(|j| j.id == job && j.done && j.status.contains("OK")));
        // Setup and scan.
        let st: SetupState = serde_json::from_value(demo.handle(Request::Setup { library: None, prefixes: None, finish: None }).unwrap()).unwrap();
        assert!(st.paths_locked && st.first_run.is_some());
        let found: Vec<FoundInstall> = serde_json::from_value(demo.handle(Request::ScanInstalls).unwrap()).unwrap();
        assert!(found[0].importable && found.iter().any(|f| f.installed) && found.iter().any(|f| f.product_id.is_none()));
        let p: ModPrefs = serde_json::from_value(demo.handle(Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: Some(false) })).unwrap()).unwrap();
        let v = p.fixes_on_vanilla;
        assert_eq!(v, Some(false));
    }
}
