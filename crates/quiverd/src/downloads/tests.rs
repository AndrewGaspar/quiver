//! The queue end to end: a fake CDN (slices gated by a semaphore, so tests decide when
//! each download makes progress), fixture plans and the real installer in temp dirs.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize};

use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;
use quiver_core::install::{self, Plan, PlannedFile, SliceError, SliceRef, SliceSource};

use super::*;
use std::path::Path;

const SLICE: usize = 2048;

struct Cdn {
    slices: Mutex<HashMap<String, Vec<u8>>>,
    owner: HashMap<String, u32>,
    gets: Mutex<HashMap<u32, usize>>,
    gate: Semaphore,
    /// Slice hash -> GETs that still fail as timeouts.
    flaky: Mutex<HashMap<String, usize>>,
}

impl Default for Cdn {
    fn default() -> Self {
        Self { slices: Mutex::default(), owner: HashMap::new(), gets: Mutex::default(), gate: Semaphore::new(0), flaky: Mutex::default() }
    }
}

impl Cdn {
    fn gets(&self, product_id: u32) -> usize {
        self.gets.lock().unwrap().get(&product_id).copied().unwrap_or(0)
    }
    fn open(&self) {
        self.gate.add_permits(1 << 20);
    }
}

struct Src(Arc<Cdn>);

impl SliceSource for Src {
    async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        Ok(paths.iter().map(|p| vec![p.rsplit('/').next().unwrap().to_owned()]).collect())
    }
    async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
        self.0.gate.acquire().await.map_err(|e| SliceError::Fatal(e.into()))?.forget();
        if let Some(&p) = self.0.owner.get(&mirrors[0]) {
            *self.0.gets.lock().unwrap().entry(p).or_default() += 1;
        }
        if let Some(n) = self.0.flaky.lock().unwrap().get_mut(&mirrors[0]).filter(|n| **n > 0) {
            *n -= 1;
            return Err(SliceError::Retry(anyhow::anyhow!("connection timed out")));
        }
        self.0.slices.lock().unwrap().get(&mirrors[0]).cloned().ok_or_else(|| SliceError::Fatal(anyhow::anyhow!("404")))
    }
}

fn bytes(seed: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i as u32).wrapping_mul(2654435761).wrapping_add(seed * 7919) >> 13) as u8).collect()
}

/// Raw (uncompressed) slices: the CDN hash is the data's hash.
fn fixture(product_id: u32, files: usize, slices_per_file: usize, cdn: &mut Cdn) -> (Plan, Vec<Vec<u8>>) {
    let mut plan = Plan::default();
    let mut contents = Vec::new();
    for f in 0..files {
        let data = bytes(product_id * 100 + f as u32, SLICE * slices_per_file);
        let slices = data
            .chunks(SLICE)
            .enumerate()
            .map(|(i, part)| {
                let sha1 = Sha1::digest(part).to_vec();
                let key = hex::encode_upper(&sha1);
                cdn.slices.get_mut().unwrap().insert(key.clone(), part.to_vec());
                cdn.owner.insert(key, product_id);
                SliceRef { sha1: sha1.clone(), data_sha1: sha1, size: part.len() as u64, download_size: part.len() as u64, offset: (i * SLICE) as u64 }
            })
            .collect();
        plan.install_bytes += data.len() as u64;
        plan.download_bytes += data.len() as u64;
        plan.files.push(PlannedFile { path: format!("data/f{f}.bin"), size: data.len() as u64, slices });
        contents.push(data);
    }
    (plan, contents)
}

struct TestHost {
    jobs: Mutex<BTreeMap<u64, Job>>,
    next: AtomicU64,
    playing: Mutex<HashSet<u32>>,
    plans: HashMap<u32, Plan>,
    cdn: Arc<Cdn>,
    root: PathBuf,
    runs: AtomicUsize,
}

impl TestHost {
    fn new(plans: HashMap<u32, Plan>, cdn: Arc<Cdn>, root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::default(),
            next: AtomicU64::new(1),
            playing: Mutex::default(),
            plans,
            cdn,
            root,
            runs: AtomicUsize::new(0),
        })
    }
    fn job(&self, id: Option<u64>) -> Job {
        self.jobs.lock().unwrap()[&id.unwrap()].clone()
    }
}

impl Host for TestHost {
    fn new_job(&self, kind: JobKind, product_id: u32, name: String) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let job = Job { id, kind, game: quiver_api::GameRef::ubisoft(product_id), name, status: "Queued".into(), downloaded: 0, total: 0, done: false, error: None, detail: Vec::new() };
        self.jobs.lock().unwrap().insert(id, job);
        id
    }
    fn update_job(&self, id: u64, f: &mut dyn FnMut(&mut Job)) {
        if let Some(j) = self.jobs.lock().unwrap().get_mut(&id) {
            f(j);
        }
    }
    fn log(&self, _: &str, _: String, _: Option<u32>) {}
    fn playing(&self, product_id: u32) -> bool {
        self.playing.lock().unwrap().contains(&product_id)
    }
    fn library_cache(&self) -> Option<Vec<LibraryEntry>> {
        None
    }
    fn changed(self: Arc<Self>) {}
    fn discard(&self, _: u32, _: &str) -> bool {
        false
    }
    fn run(self: Arc<Self>, item: QueueEntry, progress: Arc<Progress>, phase: PhaseFn) -> BoxFut<Result<Outcome>> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::Relaxed);
            let plan = self.plans[&item.product_id].clone();
            let dest = self.root.join(item.product_id.to_string());
            let cdn = self.cdn.clone();
            phase(Phase::Status("Downloading…".into()));
            progress.total.store(plan.download_bytes, Ordering::Relaxed);
            run_local(move || async move { install::install(&mut Src(cdn), &plan, &dest, false, &progress).await }).await?;
            Ok(Outcome { status: "Installed".into(), detail: Vec::new() })
        })
    }
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn order(d: &Downloads) -> Vec<(u32, QueueState)> {
    d.list().into_iter().map(|e| (e.product_id, e.state)).collect()
}

fn state(d: &Downloads, id: u32) -> Option<QueueState> {
    d.get(id).map(|e| e.state)
}

fn files_ok(root: &Path, id: u32, contents: &[Vec<u8>]) -> bool {
    contents.iter().enumerate().all(|(f, c)| std::fs::read(root.join(id.to_string()).join(format!("data/f{f}.bin"))).ok().as_ref() == Some(c))
}

const A: u32 = 101;
const B: u32 = 102;
const C: u32 = 103;

#[test]
fn queue_order_pause_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    let queue_file = tmp.path().join("queue.json");
    let mut cdn = Cdn::default();
    let mut plans = HashMap::new();
    let mut contents = HashMap::new();
    for id in [A, B, C] {
        let (p, c) = fixture(id, 3, 40, &mut cdn);
        plans.insert(id, p);
        contents.insert(id, c);
    }
    let total_a: usize = plans[&A].files.iter().map(|f| f.slices.len()).sum();
    let data = std::mem::take(cdn.slices.get_mut().unwrap());
    let owner = cdn.owner.clone();
    let cdn = Arc::new(cdn);
    *cdn.slices.lock().unwrap() = data.clone();

    // First daemon run.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let a_gets_before_restart = rt.block_on(async {
        let host = TestHost::new(plans.clone(), cdn.clone(), root.clone());
        let d = Downloads::load(queue_file.clone(), DownloadSettings::default());
        d.start(&host);
        for (id, name) in [(A, "A"), (B, "B"), (C, "C")] {
            d.enqueue(&host, "cli", id, name.into(), QueueKind::Install, None, None).unwrap();
        }
        // The same game again: the same job.
        let ja = d.get(A).unwrap().job_id;
        assert_eq!(d.enqueue(&host, "gui", A, "A".into(), QueueKind::Install, None, None).unwrap(), ja.unwrap());
        assert_eq!(order(&d), [(A, QueueState::Downloading), (B, QueueState::Queued), (C, QueueState::Queued)]);
        assert_eq!(host.job(d.get(C).unwrap().job_id).status, "Queued · #3");

        // Reorder.
        d.move_item(&host, C, QueueMove::Up).unwrap();
        assert_eq!(order(&d).iter().map(|o| o.0).collect::<Vec<_>>(), [A, C, B]);
        d.move_item(&host, B, QueueMove::Top).unwrap();
        assert_eq!(order(&d).iter().map(|o| o.0).collect::<Vec<_>>(), [B, A, C]);
        d.move_item(&host, B, QueueMove::Down).unwrap();
        assert_eq!(order(&d).iter().map(|o| o.0).collect::<Vec<_>>(), [A, B, C]);
        d.move_item(&host, C, QueueMove::Up).unwrap();
        assert_eq!(d.list().iter().map(|e| (e.product_id, e.position)).collect::<Vec<_>>(), [(A, 1), (C, 2), (B, 3)]);
        assert_eq!(host.job(d.get(B).unwrap().job_id).status, "Queued · #3");

        // Let A make some progress, then pause it: it stops at a slice boundary and C starts.
        cdn.gate.add_permits(10);
        until("A's first slices", || cdn.gets(A) >= 10).await;
        d.pause(&host, "cli", A).unwrap();
        cdn.gate.add_permits(30); // let the slices in flight finish
        until("A paused", || state(&d, A) == Some(QueueState::Paused)).await;
        until("C started", || state(&d, C) == Some(QueueState::Downloading)).await;
        assert_eq!(host.job(d.get(A).unwrap().job_id).status, "Paused");
        assert!(!host.job(d.get(A).unwrap().job_id).done);
        let (left, _) = install::remaining(&plans[&A], &root.join(A.to_string()));
        assert!(left < plans[&A].download_bytes, "A's progress is on disk");

        // A paused item can't run, a queued one can be paused without running.
        d.pause(&host, "cli", B).unwrap();
        assert_eq!(state(&d, B), Some(QueueState::Paused));
        d.resume(&host, "cli", B).unwrap();
        assert_eq!(state(&d, B), Some(QueueState::Queued));

        let saved: Saved = serde_json::from_str(&std::fs::read_to_string(&queue_file).unwrap()).unwrap();
        let s: Vec<(u32, QueueState)> = saved.items.iter().map(|e| (e.product_id, e.state)).collect();
        assert_eq!(s, [(A, QueueState::Paused), (C, QueueState::Downloading), (B, QueueState::Queued)]);
        cdn.gets(A)
    });
    // The daemon dies mid-download of C.
    rt.shutdown_background();
    cdn.gate.close();

    // Second run: C (was running) resumes, then B; A stays paused until resumed.
    let cdn2 = Arc::new(Cdn { slices: Mutex::new(data), owner, ..Cdn::default() });
    cdn2.open();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let host = TestHost::new(plans.clone(), cdn2.clone(), root.clone());
        let d = Downloads::load(queue_file.clone(), DownloadSettings::default());
        assert_eq!(order(&d), [(A, QueueState::Paused), (C, QueueState::Queued), (B, QueueState::Queued)]);
        d.start(&host);
        until("C and B done", || d.list().len() == 1).await;
        assert_eq!(order(&d), [(A, QueueState::Paused)]);
        assert!(files_ok(&root, C, &contents[&C]) && files_ok(&root, B, &contents[&B]));
        assert_eq!(cdn2.gets(A), 0, "paused stays paused");
        assert_eq!(host.job(d.get(A).unwrap().job_id).status, "Paused");

        let job = d.resume(&host, "cli", A).unwrap();
        until("A done", || d.list().is_empty()).await;
        assert!(files_ok(&root, A, &contents[&A]));
        assert!(cdn2.gets(A) <= total_a - a_gets_before_restart.min(10), "A resumed: {} more of {total_a}", cdn2.gets(A));
        let j = host.job(Some(job));
        assert!(j.done && j.status == "Installed" && j.error.is_none(), "{j:?}");
        let saved: Saved = serde_json::from_str(&std::fs::read_to_string(&queue_file).unwrap()).unwrap();
        assert!(saved.items.is_empty());
    });
}

#[test]
fn concurrency_failures_and_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    let mut cdn = Cdn::default();
    let mut plans = HashMap::new();
    let mut contents = HashMap::new();
    for id in [A, B, C] {
        let (p, c) = fixture(id, 1, 5, &mut cdn);
        plans.insert(id, p);
        contents.insert(id, c);
    }
    // One of B's slices is missing from the CDN at first.
    let missing = hex::encode_upper(&plans[&B].files[0].slices[2].sha1);
    let body = cdn.slices.get_mut().unwrap().remove(&missing).unwrap();
    let cdn = Arc::new(cdn);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let host = TestHost::new(plans.clone(), cdn.clone(), root.clone());
        let d = Downloads::load(tmp.path().join("queue.json"), DownloadSettings { max_active: 2, ..Default::default() });
        d.start(&host);
        host.playing.lock().unwrap().insert(C);
        for id in [A, B, C] {
            d.enqueue(&host, "cli", id, id.to_string(), QueueKind::Install, None, None).unwrap();
        }
        assert_eq!(order(&d), [(A, QueueState::Downloading), (B, QueueState::Downloading), (C, QueueState::Queued)]);
        // Repair of a queued game is refused while it is queued for install.
        assert!(d.enqueue(&host, "cli", A, "A".into(), QueueKind::Repair, None, None).is_err());
        assert!(d.ensure_idle(A).is_err());
        cdn.open();
        until("B failed, A done", || order(&d) == [(B, QueueState::Failed), (C, QueueState::Queued)]).await;
        let jb = host.job(d.get(B).unwrap().job_id);
        assert!(jb.done && jb.error.as_deref().is_some_and(|e| e.contains("404")), "{jb:?}");
        // C waits for its game to exit.
        assert!(host.job(d.get(C).unwrap().job_id).status.contains("waiting for the game"));
        host.playing.lock().unwrap().clear();
        d.pump(&host);
        until("C done", || d.get(C).is_none()).await;

        // Retry B once the slice is there.
        cdn.slices.lock().unwrap().insert(missing.clone(), body.clone());
        let job = d.resume(&host, "cli", B).unwrap();
        assert_ne!(job, jb.id, "a retry gets a new job");
        until("B done", || d.list().is_empty()).await;
        for id in [A, B, C] {
            assert!(files_ok(&root, id, &contents[&id]), "{id}");
        }

        // Remove: queued at once; running ones stop first.
        cdn.gate.forget_permits(1 << 30);
        std::fs::remove_dir_all(root.join(A.to_string())).unwrap();
        std::fs::remove_dir_all(root.join(B.to_string())).unwrap();
        let ja = d.enqueue(&host, "cli", A, "A".into(), QueueKind::Install, None, None).unwrap();
        let jb = d.enqueue(&host, "cli", B, "B".into(), QueueKind::Install, None, None).unwrap();
        let jc = d.enqueue(&host, "cli", C, "C".into(), QueueKind::Repair, None, None).unwrap();
        d.remove(&host, "cli", C, false).unwrap();
        assert!(host.job(Some(jc)).done);
        d.remove(&host, "cli", A, false).unwrap();
        cdn.open();
        until("A removed, B done", || d.list().is_empty()).await;
        assert!(host.job(Some(ja)).status.starts_with("Removed from queue"));
        assert_eq!(host.job(Some(jb)).status, "Installed");
    });
}

#[test]
fn bad_queue_file_is_ignored_and_states_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("queue.json");
    std::fs::write(&path, "{not json").unwrap();
    assert!(Downloads::load(path.clone(), DownloadSettings::default()).list().is_empty());
    let entry = |id: u32, state| QueueEntry {
        product_id: id,
        game: None,
        name: id.to_string(),
        kind: QueueKind::Update,
        state,
        position: 9,
        job_id: Some(77),
        language: Some("fr-FR".into()),
        account: None,
        source: "gui".into(),
        added: chrono::Utc::now(),
        error: None,
        done_bytes: 5,
        total_bytes: 10,
        speed: 3,
        retries: 0,
    };
    let saved = Saved {
        items: vec![
            entry(1, QueueState::Verifying),
            entry(2, QueueState::Done),
            entry(3, QueueState::Paused),
            entry(4, QueueState::Failed),
            entry(5, QueueState::Downloading),
        ],
    };
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let d = Downloads::load(path, DownloadSettings::default());
    let l = d.list();
    assert_eq!(
        l.iter().map(|e| (e.product_id, e.state, e.position)).collect::<Vec<_>>(),
        [(1, QueueState::Queued, 1), (3, QueueState::Paused, 2), (4, QueueState::Failed, 3), (5, QueueState::Queued, 4)]
    );
    assert!(l.iter().all(|e| e.job_id.is_none() && e.speed == 0 && e.language.as_deref() == Some("fr-FR")));
}

#[test]
fn retries_show_in_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    let mut cdn = Cdn::default();
    let (plan, contents) = fixture(A, 1, 3, &mut cdn);
    cdn.flaky.get_mut().unwrap().insert(hex::encode_upper(&plan.files[0].slices[1].sha1), 2);
    let cdn = Arc::new(cdn);
    cdn.open();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let host = TestHost::new(HashMap::from([(A, plan)]), cdn.clone(), root.clone());
        let d = Downloads::load(tmp.path().join("queue.json"), DownloadSettings::default());
        d.start(&host);
        let job = d.enqueue(&host, "cli", A, "A".into(), QueueKind::Install, None, None).unwrap();
        until("retrying shown", || host.job(Some(job)).status.starts_with("Retrying…")).await;
        until("A done", || d.list().is_empty()).await;
        let j = host.job(Some(job));
        assert!(j.done && j.error.is_none() && j.status == "Installed", "{j:?}");
        assert!(files_ok(&root, A, &contents));
    });
}
