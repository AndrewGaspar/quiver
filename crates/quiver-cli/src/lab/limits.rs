//! Lab limits (design/LAB.md §15; the model is `quiver_core::lab::limits`): `quiver lab
//! limits` shows and sets them, and every instance's supervisor enforces them on its own
//! container, quietly, following the person's play as quiverd sees it. Nothing here ever
//! touches the person's games: only the instance's container, its cgroup, its threads'
//! affinity and its game's Sight layer page.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use clap::Subcommand;
use quiver_api::{GameRef, LibraryEntry, Request};
use quiver_core::lab::limits::{self, Governor, GpuSample, GpuStatus, LabLimits, Plan, Playing, Status, Which};
use quiver_core::lab::nvml::Nvml;
use quiver_core::lab::{self, Inst, Lab, procs};
use quiver_perf::labgpu::{self, Ctl};
use quiver_perf::shm::Mapping;

use super::ops;

#[derive(Subcommand)]
pub enum LimitsAction {
    /// The limits, where they come from, and what each running instance enforces now.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Change a profile: `set while_playing gpu=20 cpu=6`, `set always memory_max=24G`,
    /// `set always cpu_quota=8`; `KEY=off` lifts one. Keys: cpu (whole cores), cpu_quota
    /// (CPUs), memory_high, memory_max, gpu (percent).
    Set {
        /// `always` or `while_playing`.
        profile: String,
        #[arg(required = true)]
        changes: Vec<String>,
    },
    /// Back to the defaults (`always`: none; `while_playing`: gpu 20%, cpu 6 cores), one
    /// profile or both.
    Reset { profile: Option<String> },
}

/// Test hook: the supervisor acts as if these games (comma-separated titles) were the
/// person's, running. It can only make `while_playing` apply, never lift a limit.
pub const ENV_PRETEND_PLAYING: &str = "QUIVER_LAB_PRETEND_PLAYING";

pub async fn run(lab: &Lab, action: LimitsAction) -> Result<()> {
    match action {
        LimitsAction::Show { json } => show(lab, json).await,
        LimitsAction::Set { profile, changes } => {
            let (mut l, _, err) = LabLimits::load();
            if let Some(e) = err {
                bail!("the limits can't be read: {e}; fix it or `quiver lab limits reset`");
            }
            let p = l.profile_mut(&profile)?;
            for c in &changes {
                let Some((k, v)) = c.split_once('=') else { bail!("{c:?}: KEY=VALUE (e.g. gpu=20, cpu=6, memory_max=24G, gpu=off)") };
                p.set(k.trim(), v)?;
            }
            let at = l.save()?;
            println!("lab limits saved to {at}:");
            print_profiles(&l);
            println!("running instances pick this up within a few seconds (quiver lab limits)");
            Ok(())
        }
        LimitsAction::Reset { profile } => {
            let (mut l, _, _) = LabLimits::load();
            let d = LabLimits::default();
            match profile.as_deref() {
                None => l = d,
                Some(p) => *l.profile_mut(p)? = d.clone().profile_mut(p)?.clone(),
            }
            let at = l.save()?;
            println!("lab limits saved to {at}:");
            print_profiles(&l);
            Ok(())
        }
    }
}

fn print_profiles(l: &LabLimits) {
    println!("  always:        {}", l.always.describe());
    println!("  while playing: {}  (on top of always, while one of your games launched by Quiver or Steam runs)", l.while_playing.describe());
}

async fn show(lab: &Lab, json: bool) -> Result<()> {
    let (l, src, err) = LabLimits::load();
    let playing = playing_now(&mut Titles::default()).await;
    let statuses: Vec<(String, Option<Status>)> = lab.list().into_iter().map(|n| {
        let st = Status::read(&lab.instance(&n).dir).filter(|s| s.fresh(Utc::now()));
        (n, st)
    }).collect();
    if json {
        let inst: BTreeMap<&str, &Option<Status>> = statuses.iter().map(|(n, s)| (n.as_str(), s)).collect();
        return super::print_json(&serde_json::json!({ "limits": l, "source": src, "error": err, "playing": playing, "effective": l.effective(playing.is_yes()), "instances": inst }));
    }
    println!("lab limits ({src}):");
    if let Some(e) = &err {
        println!("  can't be read: {e}");
    }
    print_profiles(&l);
    let (which, eff) = l.effective(playing.is_yes());
    println!("now: {}: {} ({})", which.label(), eff.describe(), playing.describe());
    for (n, st) in &statuses {
        if let Some(s) = st {
            println!("  {n}: {}", s.line());
        }
    }
    if which != Which::None {
        println!("Wall-clock performance in instances is throttled while a limit is in force; fixed-step virtual-clock results (clock=virtual:HZ) are unaffected.");
    }
    Ok(())
}

/// Game titles, from quiverd's library (asked once per new game).
#[derive(Default)]
struct Titles(BTreeMap<GameRef, String>);

/// Whether the person is playing, as quiverd tracks it.
async fn playing_now(titles: &mut Titles) -> Playing {
    if let Some(p) = std::env::var(ENV_PRETEND_PLAYING).ok().filter(|v| !v.trim().is_empty()) {
        return Playing::Yes { games: p.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect() };
    }
    let client = match quiver_api::Client::connect_running("lab:limits").await {
        Ok(c) => c,
        Err(_) => return Playing::Unknown { why: "quiverd isn't running".into() },
    };
    let sessions: Vec<quiver_api::ActiveSession> = match client.call(Request::Sessions).await {
        Ok(s) => s,
        Err(e) => return Playing::Unknown { why: format!("quiverd: {e:#}") },
    };
    let games = limits::playing_games(&sessions);
    if games.is_empty() {
        return Playing::No;
    }
    if games.iter().any(|g| !titles.0.contains_key(g)) {
        let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: true }).await.unwrap_or_default();
        for g in &games {
            let name = lib.iter().find(|e| e.game == *g).map(|e| e.name.clone()).unwrap_or_else(|| g.to_string());
            titles.0.insert(*g, name);
        }
    }
    Playing::Yes { games: games.iter().map(|g| titles.0.get(g).cloned().unwrap_or_else(|| g.to_string())).collect() }
}

// ---- The supervisor's side.

/// What the enforcer needs from its supervisor.
pub struct Hooks {
    /// The run in progress (not exited), if any.
    pub run: Box<dyn Fn() -> Option<u32> + Send + Sync>,
    /// An event line (`events.jsonl` and the supervisor's log).
    pub event: Box<dyn Fn(Option<u32>, String) + Send + Sync>,
}

/// The marker `up` leaves when it mounted the Sight layer in a container without Sight, for
/// the GPU limit (`x86_64` or `x86_64,i686`).
const LAYER_MARKER: &str = "gpu-layer";

/// The Sight layer's name (its manifest's).
const LAYER_NAME: &str = "VK_LAYER_QUIVER_sight";

/// At `up`, for an instance without Sight: the host's Sight layer libraries, mounted read-only
/// at the Sight mount so a GPU limit can reach the game (low priority, pacing). `None` (and no
/// marker) when they aren't installed: GPU limits then don't apply to this instance.
pub fn up(inst: &Inst, m: &quiver_core::lab::Manifest) -> Option<PathBuf> {
    let _ = std::fs::remove_file(inst.dir.join(LAYER_MARKER));
    if m.sight {
        return None; // Sight mounts them
    }
    let dir = match std::env::var_os(lab::sight::ENV_SIGHT_DIR) {
        Some(d) => PathBuf::from(d),
        None => {
            let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
            quiver_core::sightenv::installed_sight_dir(&exe_dir, |p| p.is_file())?
        }
    };
    let so = quiver_core::sightenv::LAYER_SO;
    if !dir.join("x86_64").join(so).is_file() {
        return None;
    }
    let arches = if dir.join("i686").join(so).is_file() { "x86_64,i686" } else { "x86_64" };
    std::fs::write(inst.dir.join(LAYER_MARKER), arches).ok()?;
    Some(dir)
}

/// Before a launch, when any profile has a GPU share: the run's GPU page (the layer creates the
/// game's devices at low priority, and paces presents by what the governor writes there), and
/// the environment that loads the layer for it. `sight`: the launch already has the layer.
pub fn prepare_launch(inst: &Inst, m: &quiver_core::lab::Manifest, sight: bool, run_dir: &Path, run_mount: &str) -> (Vec<(String, String)>, Vec<String>) {
    let (l, _, _) = LabLimits::load();
    if !l.any_gpu() {
        return (Vec::new(), Vec::new());
    }
    let mut env = Vec::new();
    if !sight {
        let Ok(arches) = std::fs::read_to_string(inst.dir.join(LAYER_MARKER)) else {
            return (Vec::new(), vec![format!(
                "lab limits: no GPU limit for this run: its container was started without the layer (quiver lab down {0} && quiver lab up {0}, with Quiver's Sight libraries installed)",
                inst.name
            )]);
        };
        let layer_dir = inst.lab().join(lab::sight::LAYER_DIR);
        if std::fs::create_dir_all(&layer_dir).is_err() {
            return (Vec::new(), vec!["lab limits: no GPU limit for this run (the layer's folder can't be made)".into()]);
        }
        for (name, json) in lab::sight::layer_manifests(arches.contains("i686")) {
            if lab::write_atomic(&layer_dir.join(name), json.as_bytes()).is_err() {
                return (Vec::new(), vec!["lab limits: no GPU limit for this run (the layer's manifest can't be written)".into()]);
            }
        }
        let mount = format!("{}/{}", lab::LAB_MOUNT, lab::sight::LAYER_DIR);
        let join = |k: &str, ours: &str, first: bool| match m.env.get(k).filter(|v| !v.is_empty()) {
            Some(v) if first => format!("{ours}:{v}"),
            Some(v) => format!("{v}:{ours}"),
            None => ours.to_owned(),
        };
        env.push(("VK_ADD_IMPLICIT_LAYER_PATH".to_owned(), join("VK_ADD_IMPLICIT_LAYER_PATH", &mount, true)));
        // Named, so the loader enables it without QUIVER_SIGHT; with no QUIVER_PERF_DIR the
        // rest of the layer passes everything through.
        env.push(("VK_INSTANCE_LAYERS".to_owned(), join("VK_INSTANCE_LAYERS", LAYER_NAME, false)));
    }
    let path = run_dir.join(labgpu::FILE);
    let _ = std::fs::remove_file(&path);
    if Mapping::create(&path, labgpu::LEN).ok().and_then(|m| Ctl::init(m, labgpu::PRIORITY_LOW)).is_none() {
        return (Vec::new(), vec!["lab limits: no GPU limit for this run (its gpu.ctl can't be made)".into()]);
    }
    env.push((labgpu::ENV.to_owned(), format!("{run_mount}/{}", labgpu::FILE)));
    (env, vec!["lab limits: the game's Vulkan devices are made at low queue priority, and its presents are paced while a GPU share applies (gpu.ctl)".into()])
}

/// Start the enforcer thread for an instance's container.
pub fn spawn(inst: Inst, hooks: Hooks) {
    let hooks = Arc::new(hooks);
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => return (hooks.event)(None, format!("lab limits: not enforced ({e})")),
        };
        let mut e = Enforcer::new(inst);
        loop {
            e.tick(&rt, &hooks);
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

struct GpuRun {
    run: u32,
    ctl: Ctl<Mapping>,
    last: (Instant, u64),
    governor: Governor,
}

struct Enforcer {
    inst: Inst,
    cores: Vec<limits::Core>,
    online: Vec<u32>,
    titles: Titles,
    limits: (LabLimits, String, Option<String>),
    playing: Playing,
    last_poll: Option<Instant>,
    cgroup: Option<(String, PathBuf)>,
    applied: Option<Plan>,
    last_assert: Instant,
    said: Option<(Which, limits::Profile, bool)>,
    nvml: Option<Nvml>,
    nvml_err: Option<String>,
    gpu: Option<GpuRun>,
    gpu_status: Option<GpuStatus>,
    containers: (Instant, BTreeMap<String, String>),
    errors: Vec<String>,
}

impl Enforcer {
    fn new(inst: Inst) -> Enforcer {
        let cores = limits::topology(Path::new("/sys/devices/system/cpu"));
        Enforcer {
            inst,
            cores,
            online: limits::online_cpus(),
            titles: Titles::default(),
            limits: (LabLimits::default(), String::new(), None),
            playing: Playing::No,
            last_poll: None,
            cgroup: None,
            applied: None,
            last_assert: Instant::now(),
            said: None,
            nvml: None,
            nvml_err: None,
            gpu: None,
            gpu_status: None,
            containers: (Instant::now() - Duration::from_secs(60), BTreeMap::new()),
            errors: Vec::new(),
        }
    }

    fn tick(&mut self, rt: &tokio::runtime::Runtime, hooks: &Hooks) {
        self.errors.clear();
        if self.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_secs(3)) {
            self.last_poll = Some(Instant::now());
            self.limits = LabLimits::load();
            // quiverd is only asked when it matters.
            self.playing = if self.limits.0.while_playing.is_empty() { Playing::No } else { rt.block_on(playing_now(&mut self.titles)) };
        }
        let (which, effective) = self.limits.0.effective(self.playing.is_yes());
        let plan = Plan::new(&effective, &self.cores);
        let Some(id) = ops::running_id(&self.inst.container()) else {
            self.cgroup = None;
            self.applied = None;
            return;
        };
        if self.cgroup.as_ref().is_none_or(|(i, _)| *i != id) {
            self.cgroup = cgroup_of(&id).map(|d| (id.clone(), d));
            self.applied = None;
        }
        let run = (hooks.run)();
        // The cgroup: on every change, and every few seconds again (affinity especially:
        // runc gives exec'd processes every CPU, and a game may set its own).
        if let Some((_, dir)) = &self.cgroup {
            let changed = self.applied.as_ref() != Some(&plan);
            if changed || self.last_assert.elapsed() >= Duration::from_secs(2) {
                self.last_assert = Instant::now();
                if let Err(e) = limits::apply_cgroup(dir, &plan) {
                    self.errors.push(format!("{e:#}"));
                }
                if plan.cpus.is_some() || changed {
                    let (_, failed) = limits::apply_affinity(&limits::cgroup_threads(dir), plan.cpus.as_deref(), &self.online);
                    if failed > 0 {
                        self.errors.push(format!("{failed} thread(s) refused their CPU affinity"));
                    }
                }
                self.applied = Some(plan.clone());
            }
        } else {
            self.errors.push(format!("the container's cgroup wasn't found (podman inspect {})", self.inst.container()));
        }
        self.gpu_tick(run, plan.gpu_percent);
        // Say so when what's in force changes.
        let now = (which, effective.clone(), self.playing.is_yes());
        if self.said.as_ref() != Some(&now) {
            let first = self.said.is_none();
            self.said = Some(now);
            let st = self.status(which, &effective, &plan);
            if !(first && which == Which::None) {
                (hooks.event)(run, if which == Which::None { format!("lab limits lifted ({})", self.playing.describe()) } else { st.line() });
            }
        }
        let st = self.status(which, &effective, &plan);
        if let Ok(b) = serde_json::to_vec_pretty(&st) {
            let _ = lab::write_atomic(&self.inst.dir.join(limits::STATUS_FILE), &b);
        }
    }

    fn status(&self, which: Which, effective: &limits::Profile, plan: &Plan) -> Status {
        Status {
            at: Utc::now(),
            source: self.limits.1.clone(),
            limits: self.limits.0.clone(),
            playing: self.playing.clone(),
            which,
            effective: effective.clone(),
            plan: plan.clone(),
            gpu: self.gpu_status.clone(),
            errors: self.errors.iter().cloned().chain(self.limits.2.clone()).collect(),
        }
    }

    /// The governor: measure this instance's GPU share, set the run's present interval.
    fn gpu_tick(&mut self, run: Option<u32>, share: Option<u32>) {
        // The run's page.
        if self.gpu.as_ref().map(|g| g.run) != run {
            self.gpu = run.and_then(|r| {
                let p = self.inst.run_dir(r).join(labgpu::FILE);
                let ctl = Mapping::open_write(&p, labgpu::LEN, labgpu::LEN).ok().and_then(Ctl::attach)?;
                let presents = ctl.snapshot().presents;
                Some(GpuRun { run: r, ctl, last: (Instant::now(), presents), governor: Governor::new(share.unwrap_or(100) as f64) })
            });
        }
        let Some(g) = self.gpu.as_mut() else {
            self.nvml = None; // lets the GPU sleep
            self.gpu_status = share.map(|p| GpuStatus {
                percent: p,
                target: p as f64,
                priority: "no run".into(),
                note: run.map(|_| "this run has no GPU page (launched before the GPU limit was set, or no layer): relaunch".into()),
                ..Default::default()
            });
            return;
        };
        let snap = g.ctl.snapshot();
        let dt = g.last.0.elapsed().as_secs_f64().max(0.001);
        let fps = (snap.presents.saturating_sub(g.last.1)) as f64 / dt;
        g.last = (Instant::now(), snap.presents);
        let Some(share) = share else {
            g.ctl.set_interval_ns(0);
            g.governor = Governor::new(100.0);
            self.nvml = None;
            self.gpu_status = None;
            return;
        };
        if (g.governor.share - share as f64).abs() > 0.01 {
            g.governor.share = share as f64;
        }
        if self.nvml.is_none() {
            match Nvml::open() {
                Ok(n) => {
                    self.nvml = Some(n);
                    self.nvml_err = None;
                }
                Err(e) => self.nvml_err = Some(e),
            }
        }
        let mut status = GpuStatus { percent: share, target: g.governor.target, priority: snap.priority.describe().into(), fps: Some(fps), ..Default::default() };
        if snap.priority == labgpu::Priority::NotYet && snap.presents == 0 {
            status.priority = "no Vulkan device yet".into();
        }
        match self.nvml.as_mut() {
            None => {
                status.note = Some(format!("not measured: {}", self.nvml_err.clone().unwrap_or_default()));
            }
            Some(n) => {
                let reading = n.read();
                if self.containers.0.elapsed() > Duration::from_secs(10) {
                    self.containers = (Instant::now(), ops::containers());
                }
                let mine = self.cgroup.as_ref().map(|(_, d)| limits::cgroup_pids(d)).unwrap_or_default();
                // Shares of the GPU at full clock (a light load clocks it down; nvml.rs).
                let own = reading.at_full_clock(reading.share_of(&mine));
                // Other lab instances' share, and how many use the GPU.
                let mut others: BTreeMap<String, f64> = BTreeMap::new();
                for (&pid, &u) in &reading.by_pid {
                    if mine.contains(&pid) {
                        continue;
                    }
                    let Some(p) = procs::read(Path::new("/proc"), pid) else { continue };
                    let owner = procs::owner(&p, &self.containers.1);
                    if owner.starts_with("lab:") {
                        *others.entry(owner).or_default() += reading.at_full_clock(u);
                    }
                }
                let busy = others.values().filter(|&&u| u >= 1.0).count() as u32;
                let interval = g.governor.step(GpuSample { own, others: others.values().sum(), instances: 1 + busy, fps });
                g.ctl.set_interval_ns(interval);
                status.target = g.governor.target;
                status.measured = g.governor.own;
                status.cap_fps = g.governor.cap;
                if snap.presents == 0 && own > 1.0 {
                    status.note = Some("the game uses the GPU but presents nothing through the layer (a loading screen, or not Vulkan): it can't be paced".into());
                }
            }
        }
        self.gpu_status = Some(status);
    }
}

/// The container's cgroup folder, from podman.
fn cgroup_of(id: &str) -> Option<PathBuf> {
    let out = ops::podman_cmd(&["inspect".into(), "-f".into(), "{{.State.CgroupPath}}".into(), id.into()]).stderr(std::process::Stdio::null()).output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let dir = limits::cgroup_dir(&p);
    (out.status.success() && !p.is_empty() && dir.join("cgroup.procs").is_file()).then_some(dir)
}

// ---- What the other commands say.

/// The instance's enforcer's status, when its supervisor is alive.
pub fn status_of(inst: &Inst) -> Option<Status> {
    Status::read(&inst.dir).filter(|s| s.fresh(Utc::now()))
}

/// The THROTTLED note for an instance's current run (`None`: no limit in force).
pub fn throttle_note(inst: &Inst, virtual_clock: bool) -> Option<String> {
    status_of(inst).and_then(|s| limits::throttle_note(&s, virtual_clock))
}

/// Whether the instance's current run has the virtual clock engaged (or asked for).
pub fn run_has_virtual_clock(inst: &Inst, run: Option<u32>) -> bool {
    if let Some(r) = run
        && lab::vclock::Report::read(&inst.run_dir(r)).is_some()
    {
        return true; // the run has the clock's DLL (engaged or about to be)
    }
    inst.load().is_ok_and(|m| m.clock.is_some())
}
