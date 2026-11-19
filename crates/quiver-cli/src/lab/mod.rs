//! `quiver lab`: disposable headless instances of an installed game that an agent drives
//! (design/LAB.md). The model, the podman lines and the checks are `quiver_core::lab`;
//! this is the command line, the supervisor (`sup`) and the tools.

mod bench;
mod create;
mod jobs;
mod ops;
mod sup;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_api::Client;
use quiver_core::lab::manifest::{Display, layer_versions};
use quiver_core::lab::ready::{State, Want};
use quiver_core::lab::{self, Inst, Lab, Manifest, configset, eosfilter, facts, host, podman, procs};

use sup::SupRequest;

#[derive(Subcommand)]
pub enum LabAction {
    /// Check what the lab needs: podman, the NVIDIA CDI spec, the image, reflinks, quiverd,
    /// a bridge that keeps lab sessions out of your records.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// The container image: `build` it (from contrib/lab/Containerfile, built in) or `status`.
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Make an instance of an installed game: a reflink clone of its gold copy (made from
    /// your prefix and install the first time, with Connect's login and executables removed).
    Create {
        /// The game: a ref (`ubisoft:3539`, `epic:<app>`, `steam:<appid>`) or a name.
        game: String,
        /// Instance name (default: from the game's title).
        #[arg(long)]
        name: Option<String>,
        /// Display size and refresh, `WIDTHxHEIGHT[@HZ]` (default 1920x1080@60).
        #[arg(long, default_value = "1920x1080@60")]
        display: String,
        /// Proton build (default: what you play the game with).
        #[arg(long)]
        proton: Option<String>,
        /// The executable, relative to the install (default: the store's; required for Steam games).
        #[arg(long)]
        exe: Option<String>,
        /// A mod folder copied over the install before every launch.
        #[arg(long = "mod")]
        mod_dir: Option<PathBuf>,
        /// Graphics profile applied before every launch (`<lab>/profiles/<game>/<name>.set`).
        #[arg(long)]
        graphics: Option<String>,
        /// Make the game's gold copy again from your current prefix and install.
        #[arg(long)]
        regold: bool,
    },
    /// Instances, with their game, display and state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// An instance's manifest and state.
    Show {
        instance: String,
        #[arg(long)]
        json: bool,
    },
    /// Change an instance's manifest: display=WxH@HZ, proton=NAME, mod_dir=DIR, graphics=NAME,
    /// env.NAME=V, dll.NAME=V, file.ALIAS=PATH, throttle+=LINE, throttle=, reload_pattern=RE,
    /// log_file=PATH, args="…", notes="…" (key= clears).
    Edit {
        instance: String,
        #[arg(required = true)]
        changes: Vec<String>,
    },
    /// Boot the instance's container (gamescope headless) and its supervisor.
    Up { instance: String },
    /// Stop the instance's container (the game with it).
    Down { instance: String },
    /// Delete an instance (its clones and runs; fixtures and the gold copy stay).
    Rm { instance: String },
    /// Start the game: kills every Wine process in the instance, puts the mod in, gets a
    /// fresh licence from quiverd, starts it, optionally waits.
    Launch {
        instance: String,
        /// A mod folder for this launch (instead of the manifest's).
        #[arg(long = "mod")]
        mod_dir: Option<PathBuf>,
        /// Launch without the manifest's mod.
        #[arg(long)]
        no_mod: bool,
        /// Extra game arguments.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Wait for this state (process, window, frame, or a mod state).
        #[arg(long)]
        wait: Option<String>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Stop the game and every Wine process in the instance.
    Kill { instance: String },
    /// Readiness, the run, the mod's state, host load.
    Status {
        instance: String,
        #[arg(long)]
        json: bool,
    },
    /// Wait for a state: process, window, frame, exited, or one your mod reports.
    Wait {
        instance: String,
        state: String,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Then wait until consecutive frames stop changing (mean difference below this,
        /// twice in a row; 1.0 when given without a value).
        #[arg(long, num_args = 0..=1, default_missing_value = "1.0")]
        stable: Option<f64>,
    },
    /// A full-resolution screenshot (prints its path).
    Shot {
        instance: String,
        file: Option<PathBuf>,
        /// Seconds to let the frame settle first (temporal AA and upscalers converge).
        #[arg(long, default_value_t = 0.5)]
        settle: f64,
        /// Save only this crop (`WxH+X+Y` or fractions `x,y,w,h`).
        #[arg(long)]
        crop: Option<String>,
        /// Also print metrics (blank, sharpness, mean level).
        #[arg(long)]
        metrics: bool,
    },
    /// Press keys (xdotool names), each held for --hold ms.
    Key {
        instance: String,
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
    /// Type text.
    Type { instance: String, text: String },
    /// Relative mouse motion (mouse-look): DX DY pixels, in --steps steps.
    Mouse {
        instance: String,
        #[arg(allow_hyphen_values = true)]
        dx: i32,
        #[arg(allow_hyphen_values = true)]
        dy: i32,
        #[arg(long, default_value_t = 1)]
        steps: u32,
        /// Repeat N times, alternating direction, at random gaps (latency work), logging each
        /// send's CLOCK_MONOTONIC_RAW time to --log (a CSV in the lab folder).
        #[arg(long)]
        repeat: Option<u32>,
        #[arg(long, default_value = "0.4-0.7")]
        gap: String,
        #[arg(long)]
        log: Option<String>,
    },
    /// Click at a fraction of the window (0-1), e.g. `0.5 0.62`.
    Click {
        instance: String,
        fx: f64,
        fy: f64,
        #[arg(long, default_value_t = 1)]
        button: u8,
        /// Fail unless the screen changed afterwards (clicks can miss silently).
        #[arg(long)]
        expect_change: bool,
    },
    /// Run a command in the instance's container, with its display set.
    Exec {
        instance: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Edit the instance's config files: FILE:[section]key=value, FILE:section.key=value,
    /// FILE:<element/path>@attr=value (FILE: a manifest alias, install:REL, or prefix-relative).
    Set {
        instance: String,
        #[arg(required = true)]
        assigns: Vec<String>,
        /// Wait for the mod's reload (the manifest's reload_pattern in its log_file).
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
    /// Apply a graphics profile now (`<lab>/profiles/<game>/<NAME>.set`); without NAME, list them.
    Profile {
        instance: String,
        name: Option<String>,
        /// Also make it the instance's profile for every launch.
        #[arg(long)]
        default: bool,
    },
    /// Save fixtures: `save <i> NAME [--note]`, `install <i> NAME` (game stopped), `list <i>`.
    Fixture {
        #[command(subcommand)]
        action: FixtureAction,
    },
    /// A request your mod answers through the lab channel (`%QUIVER_LAB%\req`).
    Call {
        instance: String,
        verb: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
    /// The player's position, as your mod answers `where`.
    Where { instance: String },
    /// Move the player to a named location (`locations.txt`) or to `x y z [yaw [pitch]]`;
    /// `list` shows the locations.
    Teleport {
        instance: String,
        #[arg(allow_hyphen_values = true, required = true)]
        to: Vec<String>,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Run a measurement with the GPU to itself: takes the lab's GPU lock (others queue), and
    /// throttles other running instances through their manifests' `throttle` lines.
    Gpu {
        /// The instance being measured (not throttled).
        #[arg(long = "for")]
        for_instance: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// A/B runs: config variants applied live (or with a relaunch), settled, sampled N times,
    /// aggregated, host state recorded per variant.
    Bench(bench::BenchArgs),
    /// Free memory, load, the GPU and who uses it, and who holds the lab's GPU lock.
    Host {
        #[arg(long)]
        json: bool,
    },
    /// The run's folder (game.log, bridge.log, DXVK logs, your mod's log).
    Logs {
        instance: String,
        #[arg(long)]
        run: Option<u32>,
        /// Print the last N lines of game.log.
        #[arg(long)]
        tail: Option<usize>,
    },
    /// A ready-to-paste brief for an agent starting to mod this game.
    Brief {
        game: String,
        /// The instance the brief tells the agent to use.
        #[arg(long)]
        instance: Option<String>,
        #[arg(long, default_value = "1920x1080@60")]
        display: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// The executable, relative to the install (Steam doesn't say which one).
        #[arg(long)]
        exe: Option<String>,
    },
    /// Background jobs (`--bg`): list, show, wait.
    Job {
        #[command(subcommand)]
        action: jobs::JobAction,
    },
    /// (internal) The instance's supervisor.
    #[command(hide = true)]
    Supervise { instance: String },
}

#[derive(Subcommand)]
pub enum ImageAction {
    Build {
        #[arg(long, default_value = lab::IMAGE)]
        tag: String,
    },
    Status,
}

#[derive(Subcommand)]
pub enum FixtureAction {
    Save {
        instance: String,
        name: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    Install { instance: String, name: String },
    List { instance: String },
}

/// The lab root (`$QUIVER_LAB_ROOT`, else next to Quiver's prefixes).
pub fn the_lab() -> Result<Lab> {
    if let Some(r) = std::env::var_os("QUIVER_LAB_ROOT").filter(|r| !r.is_empty()) {
        return Ok(Lab::new(PathBuf::from(r)));
    }
    let cfg = quiver_core::launcher::Config::load()?;
    Ok(Lab::new(lab::root(&cfg.prefixes)))
}

fn inst_of(lab: &Lab, name: &str) -> Result<(Inst, Manifest)> {
    let inst = lab.instance(name);
    if !inst.manifest_path().is_file() {
        bail!("no instance '{name}' (quiver lab list)");
    }
    let m = inst.load()?;
    Ok((inst, m))
}

fn read_state(inst: &Inst) -> State {
    std::fs::read(inst.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Wait for `want`; the state reached, or an error saying why not.
fn wait_for(inst: &Inst, want: &Want, run: Option<u32>, timeout: Duration) -> Result<State> {
    let deadline = Instant::now() + timeout;
    loop {
        let st = read_state(inst);
        match quiver_core::lab::ready::reached(&st, want, run) {
            Some(true) => return Ok(st),
            Some(false) => {
                let why = st.run.as_ref().and_then(|r| r.exit.as_ref()).map(|e| format!("{}: {}", e.how, e.detail)).unwrap_or_else(|| "the instance is down".into());
                bail!("'{want}' can't be reached any more ({why})");
            }
            None if Instant::now() > deadline => {
                let at = st.run.as_ref().map(|r| format!("{}{}", r.phase.as_str(), r.mod_state.as_ref().map(|m| format!(", mod: {}", m.state)).unwrap_or_default())).unwrap_or_else(|| "no run".into());
                bail!("timed out after {}s waiting for '{want}' (at: {at})", timeout.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Wait until consecutive frames differ by less than `threshold` twice in a row.
fn wait_stable(inst: &Inst, threshold: f64, timeout: Duration) -> Result<f64> {
    let deadline = Instant::now() + timeout;
    let tmp = inst.dir.join(".stable.png");
    let mut prev = ops::shot_gray(inst, &tmp)?;
    let mut calm = 0;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let next = ops::shot_gray(inst, &tmp)?;
        let d = quiver_core::lab::image::mean_abs_diff(&prev, &next)?;
        calm = if d < threshold { calm + 1 } else { 0 };
        if calm >= 2 {
            return Ok(d);
        }
        if Instant::now() > deadline {
            bail!("frames kept changing (last difference {d:.2}, threshold {threshold})");
        }
        prev = next;
    }
}

/// The new log text since `from` bytes.
fn log_since(path: &Path, from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let _ = f.seek(SeekFrom::Start(from));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    s
}

fn log_len(path: &Path) -> u64 {
    path.metadata().map(|m| m.len()).unwrap_or(0)
}

/// Apply assignments and, with `wait`, wait for the mod's reload line.
pub(crate) fn set_and_wait(inst: &Inst, m: &Manifest, assigns: &[configset::Assign], wait: bool, timeout: Duration) -> Result<Vec<configset::Done>> {
    let log = match (&m.reload_pattern, &m.log_file) {
        (Some(p), Some(f)) if wait => Some((regex::Regex::new(p).context("reload_pattern")?, m.resolve_file(inst, f)?)),
        (_, _) if wait => bail!("--wait needs reload_pattern and log_file in the manifest (quiver lab edit {} reload_pattern=… log_file=…)", inst.name),
        _ => None,
    };
    let from = log.as_ref().map(|(_, p)| log_len(p)).unwrap_or(0);
    let done = configset::apply(inst, m, assigns)?;
    if let Some((re, path)) = log {
        let deadline = Instant::now() + timeout;
        while !log_since(&path, from).lines().any(|l| re.is_match(l)) {
            if Instant::now() > deadline {
                bail!("no reload line matching /{}/ in {} within {}s", re.as_str(), path.display(), timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    Ok(done)
}

pub async fn run(source: &str, action: LabAction, bg: bool) -> Result<()> {
    if bg && !matches!(action, LabAction::Job { .. } | LabAction::Supervise { .. }) {
        let lab = the_lab()?;
        return jobs::start_background(&lab);
    }
    let lab = the_lab()?;
    match action {
        LabAction::Supervise { instance } => sup::supervise(lab, &instance)?,
        LabAction::Job { action } => jobs::run(&lab, action)?,
        LabAction::Doctor { json } => doctor(&lab, json).await?,
        LabAction::Image { action } => match action {
            ImageAction::Build { tag } => {
                let dir = lab.root.join(".image-build");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("Containerfile"), lab::CONTAINERFILE)?;
                let st = ops::podman_cmd(&podman::build_args(&tag, &dir)).status()?;
                if !st.success() {
                    bail!("podman build failed");
                }
                println!("built {tag}");
            }
            ImageAction::Status => {
                let ok = ops::podman_cmd(&["image".into(), "exists".into(), lab::IMAGE.into()]).status()?.success();
                println!("{} {}", lab::IMAGE, if ok { "present" } else { "missing (quiver lab image build)" });
            }
        },
        LabAction::Create { game, name, display, proton, exe, mod_dir, graphics, regold } => {
            let client = Client::connect_running(source).await.context("quiverd isn't running (quiver status starts it)")?;
            let src = create::source(&client, &game, exe.as_deref(), true).await?;
            let name = name.unwrap_or_else(|| default_name(&src.title));
            let o = create::CreateOpts { name, display: Display::parse(&display)?, proton, mod_dir, graphics, created_by: source.to_owned(), regold };
            println!("cloning {} (reflinks; a big game takes a minute)…", src.title);
            let (inst, notes) = create::create(&lab, &src, &o)?;
            for n in notes {
                println!("  {n}");
            }
            let m = inst.load()?;
            println!("instance '{}' ({}, {}, {})", m.name, m.game, m.display, m.licence.describe());
            println!("next: quiver lab up {0} && quiver lab launch {0} --wait frame", m.name);
        }
        LabAction::List { json } => {
            let rows: Vec<serde_json::Value> = lab
                .list()
                .into_iter()
                .filter_map(|n| {
                    let i = lab.instance(&n);
                    let m = i.load().ok()?;
                    let st = read_state(&i);
                    let up = ops::running_id(&i.container()).is_some();
                    Some(serde_json::json!({ "name": n, "game": m.game, "title": m.title, "display": m.display.to_string(), "up": up,
                        "phase": st.run.as_ref().map(|r| r.phase.as_str()), "run": st.run.as_ref().map(|r| r.id) }))
                })
                .collect();
            if json {
                return print_json(&rows);
            }
            if rows.is_empty() {
                println!("no instances (quiver lab create <game>)");
            }
            for r in rows {
                println!(
                    "{:<16} {:<22} {:<14} {:<5} {}",
                    r["name"].as_str().unwrap_or(""),
                    r["game"].as_str().unwrap_or(""),
                    r["display"].as_str().unwrap_or(""),
                    if r["up"].as_bool() == Some(true) { "up" } else { "down" },
                    r["phase"].as_str().map(|p| format!("r{} {p}", r["run"])).unwrap_or_default()
                );
            }
        }
        LabAction::Show { instance, json } => {
            let (inst, m) = inst_of(&lab, &instance)?;
            let st = read_state(&inst);
            if json {
                return print_json(&serde_json::json!({ "manifest": m, "state": st, "dir": inst.dir }));
            }
            println!("{}", std::fs::read_to_string(inst.manifest_path())?);
            println!("# folder: {}\n# state:\n{}", inst.dir.display(), serde_json::to_string_pretty(&st)?);
        }
        LabAction::Edit { instance, changes } => {
            let (inst, mut m) = inst_of(&lab, &instance)?;
            for c in &changes {
                edit(&mut m, c)?;
            }
            m.save(&inst.manifest_path())?;
            println!("saved {}", inst.manifest_path().display());
            if changes.iter().any(|c| c.starts_with("display=")) && ops::running_id(&inst.container()).is_some() {
                println!("the display changes at the next `quiver lab down {0}; quiver lab up {0}`", inst.name);
            }
        }
        LabAction::Up { instance } => up(&lab, &instance)?,
        LabAction::Down { instance } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let _ = sup::request(&lab, &inst, &SupRequest::Kill { source: source.into() });
            let _ = std::os::unix::net::UnixStream::connect(inst.socket()).and_then(|mut c| {
                use std::io::Write;
                c.write_all(b"{\"op\":\"quit\"}\n")
            });
            let _ = ops::podman_cmd(&["rm".into(), "-f".into(), "-t".into(), "5".into(), inst.container()]).output();
            println!("{} is down", inst.name);
        }
        LabAction::Rm { instance } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            if ops::running_id(&inst.container()).is_some() {
                bail!("{0} is up; quiver lab down {0} first", inst.name);
            }
            std::fs::remove_dir_all(&inst.dir).with_context(|| format!("remove {}", inst.dir.display()))?;
            println!("removed {}", inst.name);
        }
        LabAction::Launch { instance, mod_dir, no_mod, args, wait, timeout } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let mod_dir = mod_dir.map(|d| d.canonicalize().with_context(|| format!("{} not found", d.display()))).transpose()?;
            let r = sup::request(&lab, &inst, &SupRequest::Launch { source: source.into(), mod_dir, no_mod, args })?;
            for n in r["notes"].as_array().into_iter().flatten() {
                println!("  {}", n.as_str().unwrap_or_default());
            }
            let run = r["run"].as_u64().map(|r| r as u32);
            println!("launched r{} ({}); logs in {}", run.unwrap_or(0), r["session"].as_str().unwrap_or(""), r["run_dir"].as_str().unwrap_or(""));
            if let Some(w) = wait {
                let st = wait_for(&inst, &Want::parse(&w), run, Duration::from_secs(timeout))?;
                let r = st.run.as_ref().unwrap();
                println!("{w} after {:.1}s", (chrono::Utc::now() - r.started).num_milliseconds() as f64 / 1000.0);
            }
        }
        LabAction::Kill { instance } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let r = sup::request(&lab, &inst, &SupRequest::Kill { source: source.into() })?;
            for n in r["notes"].as_array().into_iter().flatten() {
                println!("  {}", n.as_str().unwrap_or_default());
            }
            println!("nothing runs in {} now", inst.name);
        }
        LabAction::Status { instance, json } => {
            let (inst, m) = inst_of(&lab, &instance)?;
            let st = read_state(&inst);
            let hs = host::snapshot(&procs::scan(), &ops::containers());
            if json {
                return print_json(&serde_json::json!({ "state": st, "host": hs, "display": m.display }));
            }
            println!("{} ({}, {}): container {}", inst.name, m.game, m.display, if st.container_up { "up" } else { "down" });
            match &st.run {
                None => println!("  no run yet"),
                Some(r) => {
                    println!("  run r{} ({}): {}", r.id, r.session, r.phase.as_str());
                    for (p, t) in &r.reached {
                        println!("    {:<9} +{:.1}s", p.as_str(), (*t - r.started).num_milliseconds() as f64 / 1000.0);
                    }
                    if let Some(ms) = &r.mod_state {
                        println!("  mod: {} {:?}", ms.state, ms.detail);
                    }
                    if let Some(e) = &r.exit {
                        println!("  exit: {} {}", e.how, e.detail);
                    }
                }
            }
            for v in &st.violations {
                println!("  VIOLATION: {v}");
            }
            println!("  host: {}", hs.summary());
        }
        LabAction::Wait { instance, state, timeout, stable } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let t0 = Instant::now();
            wait_for(&inst, &Want::parse(&state), None, Duration::from_secs(timeout))?;
            print!("{state} ({:.1}s)", t0.elapsed().as_secs_f64());
            if let Some(th) = stable {
                let left = Duration::from_secs(timeout).saturating_sub(t0.elapsed());
                let d = wait_stable(&inst, th, left)?;
                print!(", stable ({:.1}s, difference {d:.2})", t0.elapsed().as_secs_f64());
            }
            println!();
        }
        LabAction::Shot { instance, file, settle, crop, metrics } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            if settle > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(settle));
            }
            let run = read_state(&inst).run.map(|r| format!("-r{}", r.id)).unwrap_or_default();
            let out = file.unwrap_or_else(|| inst.shots().join(format!("shot-{}{run}.png", chrono::Local::now().format("%H%M%S%.3f"))));
            ops::shot(&inst, &out)?;
            if let Some(c) = &crop {
                let c = quiver_core::lab::image::Crop::parse(c)?;
                let img = image::open(&out)?;
                let r = c.rect(img.width(), img.height());
                img.crop_imm(r.x, r.y, r.w, r.h).save(&out)?;
            }
            println!("{}", out.display());
            if metrics {
                let g = quiver_core::lab::image::load_gray(&out)?;
                let (mean, sd) = quiver_core::lab::image::mean_std(&g);
                println!("{}x{} mean {mean:.1} sd {sd:.1} sharpness {:.2} blank {}", g.width(), g.height(), quiver_core::lab::image::sharpness(&g), quiver_core::lab::image::is_blank(&g));
            }
        }
        LabAction::Key { instance, keys, hold } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let hold = format!("{:.3}", hold as f64 / 1000.0);
            let mut a = Vec::new();
            for k in &keys {
                a.extend(["keydown".into(), k.clone(), "sleep".into(), hold.clone(), "keyup".into(), k.clone(), "sleep".into(), "0.05".into()]);
            }
            ops::xdotool(&inst, &a)?;
        }
        LabAction::Type { instance, text } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            ops::xdotool(&inst, &["type".into(), "--delay".into(), "40".into(), "--".into(), text])?;
        }
        LabAction::Mouse { instance, dx, dy, steps, repeat, gap, log } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            mouse(&inst, dx, dy, steps, repeat, &gap, log.as_deref())?;
        }
        LabAction::Click { instance, fx, fy, button, expect_change } => {
            let (inst, m) = inst_of(&lab, &instance)?;
            let (x, y) = m.display.at(fx, fy);
            let tmp = inst.dir.join(".click.png");
            let before = if expect_change { Some(ops::shot_gray(&inst, &tmp)?) } else { None };
            let b = button.to_string();
            ops::xdotool(&inst, &["mousemove".into(), x.to_string(), y.to_string(), "sleep".into(), "0.2".into(), "mousedown".into(), b.clone(), "sleep".into(), "0.12".into(), "mouseup".into(), b])?;
            println!("clicked ({x}, {y}) on {}", m.display);
            if let Some(before) = before {
                std::thread::sleep(Duration::from_millis(700));
                let after = ops::shot_gray(&inst, &tmp)?;
                let changed = quiver_core::lab::image::changed_fraction(&before, &after, 12)?;
                if changed < 0.002 {
                    bail!("the screen didn't change after the click ({:.3}% of pixels); it may have missed", changed * 100.0);
                }
                println!("screen changed ({:.1}% of pixels)", changed * 100.0);
            }
        }
        LabAction::Exec { instance, cmd } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let st = ops::podman_cmd(&podman::exec_args(&inst.container(), false, None, &[], &cmd)).status()?;
            std::process::exit(st.code().unwrap_or(1));
        }
        LabAction::Set { instance, assigns, wait, timeout } => {
            let (inst, m) = inst_of(&lab, &instance)?;
            let a: Vec<configset::Assign> = assigns.iter().map(|s| configset::Assign::parse(s)).collect::<Result<_>>()?;
            for d in set_and_wait(&inst, &m, &a, wait, Duration::from_secs(timeout))? {
                println!("{} [{}]{}: {} -> {}", d.file, d.target, d.key, if d.old.is_empty() { "(unset)".into() } else { d.old.join(",") }, d.new);
            }
        }
        LabAction::Profile { instance, name, default } => {
            let (inst, mut m) = inst_of(&lab, &instance)?;
            let dir = lab.profiles(&m.game.dir_name());
            let Some(name) = name else {
                println!("profiles in {}:", dir.display());
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    if let Some(n) = e.file_name().to_string_lossy().strip_suffix(".set") {
                        println!("  {n}{}", if m.graphics.as_deref() == Some(n) { " (default)" } else { "" });
                    }
                }
                return Ok(());
            };
            let text = std::fs::read_to_string(dir.join(format!("{name}.set"))).with_context(|| format!("no profile {name} in {}", dir.display()))?;
            let done = configset::apply(&inst, &m, &configset::parse_lines(&text)?)?;
            println!("applied {name}: {} key(s)", done.len());
            if default {
                m.graphics = Some(name);
                m.save(&inst.manifest_path())?;
            }
        }
        LabAction::Fixture { action } => fixture(&lab, action)?,
        LabAction::Call { instance, verb, args, timeout } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            println!("{}", serde_json::to_string_pretty(&call(&inst, &verb, &args, Duration::from_secs(timeout))?)?);
        }
        LabAction::Where { instance } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            println!("{}", call(&inst, "where", &[], Duration::from_secs(15))?);
        }
        LabAction::Teleport { instance, to, timeout } => {
            let (inst, m) = inst_of(&lab, &instance)?;
            let table = std::fs::read_to_string(lab.locations(&m.game.dir_name())).unwrap_or_default();
            if to == ["list"] {
                print!("{table}");
                return Ok(());
            }
            let args = if to.len() == 1 {
                table
                    .lines()
                    .find_map(|l| {
                        let mut w = l.split_whitespace();
                        (w.next() == Some(to[0].as_str())).then(|| w.map(str::to_owned).collect::<Vec<_>>())
                    })
                    .with_context(|| format!("no location '{}' in {}", to[0], lab.locations(&m.game.dir_name()).display()))?
            } else {
                to
            };
            println!("{}", call(&inst, "teleport", &args, Duration::from_secs(timeout))?);
        }
        LabAction::Gpu { for_instance, cmd } => gpu(&lab, for_instance.as_deref(), &cmd)?,
        LabAction::Bench(a) => bench::run(&lab, source, a)?,
        LabAction::Host { json } => {
            let all = procs::scan();
            let hs = host::snapshot(&all, &ops::containers());
            let holder = std::fs::read_to_string(lab.gpu_lock().with_extension("holder")).ok();
            if json {
                return print_json(&serde_json::json!({ "host": hs, "gpu_lock": holder }));
            }
            println!("{}", hs.summary());
            println!("gpu lock: {}", holder.as_deref().map(str::trim).unwrap_or("free"));
        }
        LabAction::Logs { instance, run, tail } => {
            let (inst, _) = inst_of(&lab, &instance)?;
            let run = run.or(read_state(&inst).run.map(|r| r.id)).context("no runs yet")?;
            let dir = inst.run_dir(run);
            println!("{}", dir.display());
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                println!("  {} ({} bytes)", e.file_name().to_string_lossy(), e.metadata().map(|m| m.len()).unwrap_or(0));
            }
            // What the EOS filter let through and what it blocked (design/LAB.md section 3.1).
            if let Ok(text) = std::fs::read_to_string(dir.join(eosfilter::LOG_FILE)) {
                let s = eosfilter::summarize(&text);
                println!("EOS filter: loaded in {} process(es), {} request(s) allowed, {} blocked ({})", s.loaded, s.allowed, s.blocked, eosfilter::LOG_FILE);
                for (what, n) in &s.blocked_requests {
                    println!("  blocked {what}{}", if *n > 1 { format!(" (x{n})") } else { String::new() });
                }
            }
            if let Some(n) = tail {
                let text = std::fs::read_to_string(dir.join("game.log")).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                for l in &lines[lines.len().saturating_sub(n)..] {
                    println!("{l}");
                }
            }
        }
        LabAction::Brief { game, instance, display, out, exe } => {
            let client = Client::connect_running(source).await.context("quiverd isn't running (quiver status starts it)")?;
            let text = brief(&lab, &client, &game, instance, &display, exe).await?;
            match out {
                Some(p) => {
                    std::fs::write(&p, &text)?;
                    println!("wrote {}", p.display());
                }
                None => print!("{text}"),
            }
        }
    }
    Ok(())
}

fn default_name(title: &str) -> String {
    let s = quiver_core::launcher::slug(title);
    let s: String = s.chars().take(20).collect();
    let s = s.trim_end_matches('-').to_owned();
    if s.is_empty() { "game".into() } else { s }
}

fn edit(m: &mut Manifest, change: &str) -> Result<()> {
    if let Some(line) = change.strip_prefix("throttle+=") {
        configset::Assign::parse(line)?;
        m.throttle.push(line.to_owned());
        return Ok(());
    }
    let (k, v) = change.split_once('=').with_context(|| format!("'{change}': expected key=value"))?;
    let opt = |v: &str| (!v.is_empty()).then(|| v.to_owned());
    match k {
        "display" => m.display = Display::parse(v)?,
        "proton" => {
            let p = quiver_core::launcher::resolve_proton(v)?;
            (m.runtime.dxvk, m.runtime.vkd3d, m.runtime.nvapi) = layer_versions(&p);
            m.runtime.proton = v.to_owned();
            m.runtime.proton_path = p;
        }
        "mod_dir" => m.mod_dir = opt(v).map(PathBuf::from).map(|p| p.canonicalize().unwrap_or(p)),
        "graphics" => m.graphics = opt(v),
        "throttle" if v.is_empty() => m.throttle.clear(),
        "reload_pattern" => {
            if !v.is_empty() {
                regex::Regex::new(v).context("reload_pattern")?;
            }
            m.reload_pattern = opt(v);
        }
        "log_file" => m.log_file = opt(v),
        "notes" => m.notes = v.to_owned(),
        "args" => m.paths.args = v.split_whitespace().map(str::to_owned).collect(),
        "exe" if !v.is_empty() => m.paths.exe = v.to_owned(),
        _ => {
            if let Some(name) = k.strip_prefix("env.") {
                if v.is_empty() { m.env.remove(name) } else { m.env.insert(name.into(), v.into()) };
            } else if let Some(name) = k.strip_prefix("dll.") {
                if v.is_empty() { m.dll_overrides.remove(name) } else { m.dll_overrides.insert(name.into(), v.into()) };
            } else if let Some(name) = k.strip_prefix("file.") {
                if v.is_empty() { m.files.remove(name) } else { m.files.insert(name.into(), v.into()) };
            } else {
                bail!("unknown manifest key '{k}' (see quiver lab edit --help)");
            }
        }
    }
    Ok(())
}

fn up(lab: &Lab, name: &str) -> Result<()> {
    let (inst, m) = inst_of(lab, name)?;
    if ops::running_id(&inst.container()).is_some() {
        let _ = sup::request(lab, &inst, &SupRequest::Ping)?;
        println!("{name} is already up");
        return Ok(());
    }
    if !ops::podman_cmd(&["image".into(), "exists".into(), m.runtime.image.clone()]).status()?.success() {
        bail!("the image {} is missing (quiver lab image build)", m.runtime.image);
    }
    let broker_port = (m.licence == quiver_core::lab::manifest::Licence::UbisoftBridge).then(|| {
        std::env::var("QUIVER_LAB_BROKER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(47810u16)
    });
    let h = podman::Host {
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        hostname: std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_owned(),
        broker_port,
        ro_mounts: podman::host_ro_mounts(),
    };
    std::fs::create_dir_all(inst.lab())?;
    std::fs::write(inst.dir.join("broker-port"), broker_port.map(|p| p.to_string()).unwrap_or_default())?;
    ops::podman_ok(&podman::run_args(&inst, &m, &h))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let display = loop {
        if let Some(d) = ops::display(&inst) {
            break d;
        }
        if Instant::now() > deadline {
            bail!("the instance's display didn't come up (see {})", inst.lab().join("gamescope.log").display());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // A fresh prefix (a Steam game's) is set up once, inside the container.
    if !inst.prefix().join("system.reg").exists() {
        println!("setting up a fresh prefix (wineboot)…");
        let proton = m.runtime.proton_path.join("proton").display().to_string();
        ops::podman_ok(&podman::exec_args(&inst.container(), false, None, &[], &["python3".into(), proton, "run".into(), "wineboot".into(), "-u".into()]))?;
        let wineserver = m.runtime.proton_path.join("files/bin/wineserver").display().to_string();
        let _ = ops::podman_ok(&podman::exec_args(&inst.container(), false, None, &[], &[wineserver, "-w".into()]));
    }
    if !sup::alive(&inst) {
        sup::spawn(lab, &inst)?;
    }
    let _ = sup::request(lab, &inst, &SupRequest::Ping)?;
    println!("{name} is up: display {display} at {} (gamescope headless); next: quiver lab launch {name} --wait frame", m.display);
    Ok(())
}

fn mouse(inst: &Inst, dx: i32, dy: i32, steps: u32, repeat: Option<u32>, gap: &str, log: Option<&str>) -> Result<()> {
    let Some(n) = repeat else {
        let steps = steps.max(1) as i32;
        let mut a = Vec::new();
        for i in 0..steps {
            let (sx, sy) = (dx * (i + 1) / steps - dx * i / steps, dy * (i + 1) / steps - dy * i / steps);
            a.extend(["mousemove_relative".into(), "--".into(), sx.to_string(), sy.to_string(), "sleep".into(), "0.01".into()]);
        }
        return ops::xdotool(inst, &a);
    };
    let (lo, hi) = gap.split_once('-').context("--gap MIN-MAX seconds")?;
    let log = log.unwrap_or("inject.csv");
    std::fs::create_dir_all(inst.lab().join("tools"))?;
    std::fs::write(inst.lab().join("tools/inject_mouse.py"), INJECT_MOUSE)?;
    let out = ops::exec(
        inst,
        &[],
        &["python3".into(), format!("{}/tools/inject_mouse.py", lab::LAB_MOUNT), format!("{}/{log}", lab::LAB_MOUNT), n.to_string(), dx.to_string(), dy.to_string(), lo.into(), hi.into()],
    )?;
    print!("{}", String::from_utf8_lossy(&out.stdout));
    println!("{}", inst.lab().join(log).display());
    Ok(())
}

/// Relative mouse motion through XTEST, each send logged in CLOCK_MONOTONIC_RAW ns (Wine's
/// QueryPerformanceCounter base): the Pharos harness's `inject_mouse.py`, generalised.
const INJECT_MOUSE: &str = r#"import ctypes, random, sys, time
out, count, dx, dy, lo, hi = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), float(sys.argv[5]), float(sys.argv[6])
x11 = ctypes.CDLL("libX11.so.6"); xtst = ctypes.CDLL("libXtst.so.6")
x11.XOpenDisplay.restype = ctypes.c_void_p; x11.XFlush.argtypes = [ctypes.c_void_p]
xtst.XTestFakeRelativeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
dpy = x11.XOpenDisplay(None)
if not dpy: sys.exit("cannot open display")
raw = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
with open(out, "w") as f:
    f.write("t_send_ns,t_flushed_ns,dx,dy\n")
    for i in range(count):
        time.sleep(random.uniform(lo, hi))
        s = 1 if i % 2 == 0 else -1
        t0 = raw(); xtst.XTestFakeRelativeMotionEvent(dpy, s * dx, s * dy, 0); x11.XFlush(dpy); t1 = raw()
        f.write(f"{t0},{t1},{s*dx},{s*dy}\n"); f.flush()
print(f"sent {count} motions")
"#;

fn call(inst: &Inst, verb: &str, args: &[String], timeout: Duration) -> Result<serde_json::Value> {
    if !verb.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("verbs are letters, digits, - and _");
    }
    let id = format!("{}-{}", chrono::Utc::now().format("%H%M%S%3f"), std::process::id());
    let (req, rsp) = (inst.lab().join("req"), inst.lab().join("rsp"));
    std::fs::create_dir_all(&req)?;
    std::fs::create_dir_all(&rsp)?;
    let body = serde_json::json!({ "id": id, "verb": verb, "args": args });
    lab::write_atomic(&req.join(format!("{id}.json")), serde_json::to_string(&body)?.as_bytes())?;
    let answer = rsp.join(format!("{id}.json"));
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&answer)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let _ = std::fs::remove_file(&answer);
            let _ = std::fs::remove_file(req.join(format!("{id}.json")));
            if v["ok"].as_bool() == Some(false) {
                bail!("{verb}: {}", v["error"].as_str().unwrap_or("failed"));
            }
            return Ok(v.get("result").cloned().unwrap_or(v));
        }
        if Instant::now() > deadline {
            let _ = std::fs::remove_file(req.join(format!("{id}.json")));
            bail!("no answer to '{verb}' from the mod in {}s (does it serve the lab channel? is the game rendering?)", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn fixture(lab: &Lab, action: FixtureAction) -> Result<()> {
    let copy_tree = |from: &Path, to: &Path| -> Result<usize> {
        let mut n = 0;
        for (rel, is_dir) in facts::walk(from, 16, 100_000) {
            if is_dir {
                continue;
            }
            let dst = to.join(&rel);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            if dst.exists() {
                let mut p = std::fs::metadata(&dst)?.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                std::fs::set_permissions(&dst, p)?;
            }
            std::fs::copy(from.join(&rel), &dst)?;
            n += 1;
        }
        Ok(n)
    };
    match action {
        FixtureAction::List { instance } => {
            let (_, m) = inst_of(lab, &instance)?;
            let dir = lab.fixtures(&m.game.dir_name());
            println!("fixtures in {}:", dir.display());
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
                let note = std::fs::read_to_string(e.path().join("README.md")).unwrap_or_default();
                println!("  {:<24} {}", e.file_name().to_string_lossy(), note.lines().find(|l| l.starts_with("Note:")).unwrap_or(""));
            }
        }
        FixtureAction::Save { instance, name, note } => {
            let (inst, m) = inst_of(lab, &instance)?;
            if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') || name.is_empty() {
                bail!("fixture names: lowercase letters, digits, - and _");
            }
            let dir = lab.fixtures(&m.game.dir_name()).join(&name);
            if dir.exists() {
                bail!("fixture {name} exists ({})", dir.display());
            }
            let folders: Vec<String> = m.saves.iter().flat_map(|p| create::expand_saves(&inst.prefix(), p)).collect();
            if folders.is_empty() {
                bail!("the manifest names no save folder that exists (quiver lab edit {instance} … or add `saves` to {})", inst.manifest_path().display());
            }
            let mut n = 0;
            for f in &folders {
                n += copy_tree(&inst.prefix().join(f), &dir.join("prefix").join(f))?;
            }
            let st = read_state(&inst);
            let running = st.run.as_ref().is_some_and(|r| r.phase != quiver_core::lab::ready::Phase::Exited);
            if running {
                let _ = ops::shot(&inst, &dir.join("fixture.png"));
            }
            let readme = format!(
                "# Save fixture: {name}\n\nNote: {note}\n\nSaved {} from instance {instance} ({}), {n} file(s): {}.\n{}\nInstall: `quiver lab fixture install <instance> {name}` (game stopped).\n",
                chrono::Utc::now().to_rfc3339(),
                m.game,
                folders.join(", "),
                if running { "The game was running: files are as of its last save. fixture.png is the screen then.\n" } else { "" }
            );
            std::fs::write(dir.join("README.md"), readme)?;
            for (rel, _) in facts::walk(&dir, 16, 100_000) {
                let p = dir.join(rel);
                if p.is_file() {
                    let mut perm = std::fs::metadata(&p)?.permissions();
                    perm.set_readonly(true);
                    std::fs::set_permissions(&p, perm)?;
                }
            }
            println!("saved fixture {name} ({n} files) in {}", dir.display());
        }
        FixtureAction::Install { instance, name } => {
            let (inst, m) = inst_of(lab, &instance)?;
            let st = read_state(&inst);
            if st.run.as_ref().is_some_and(|r| r.phase != quiver_core::lab::ready::Phase::Exited) {
                bail!("stop the game first (quiver lab kill {instance}): it rewrites its saves on exit");
            }
            let dir = lab.fixtures(&m.game.dir_name()).join(&name).join("prefix");
            if !dir.is_dir() {
                bail!("no fixture {name} ({})", dir.display());
            }
            // The instance's own saves, once, before any fixture replaces them.
            let backup = inst.dir.join("saves-orig");
            if !backup.exists() {
                for f in m.saves.iter().flat_map(|p| create::expand_saves(&inst.prefix(), p)) {
                    copy_tree(&inst.prefix().join(&f), &backup.join(&f))?;
                }
            }
            let n = copy_tree(&dir, &inst.prefix())?;
            println!("installed fixture {name} ({n} files) in {instance}");
        }
    }
    Ok(())
}

fn gpu(lab: &Lab, me: Option<&str>, cmd: &[String]) -> Result<()> {
    std::fs::create_dir_all(&lab.root)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lab.gpu_lock())?;
    let holder_file = lab.gpu_lock().with_extension("holder");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&lock);
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("waiting for the GPU ({})…", std::fs::read_to_string(&holder_file).unwrap_or_default().trim());
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            bail!("lock {}", lab.gpu_lock().display());
        }
    }
    std::fs::write(&holder_file, format!("{} pid {} since {}", me.unwrap_or("-"), std::process::id(), chrono::Local::now().format("%T")))?;
    // Throttle every other running instance.
    let mut undo: Vec<(Inst, Manifest, Vec<configset::Assign>)> = Vec::new();
    for n in lab.list() {
        if Some(n.as_str()) == me {
            continue;
        }
        let i = lab.instance(&n);
        let Ok(m) = i.load() else { continue };
        if m.throttle.is_empty() || ops::running_id(&i.container()).is_none() {
            continue;
        }
        let a: Vec<configset::Assign> = m.throttle.iter().filter_map(|l| configset::Assign::parse(l).ok()).collect();
        match configset::apply(&i, &m, &a) {
            Ok(done) => {
                eprintln!("gpu: throttled {n}");
                undo.push((i, m, done.iter().filter_map(configset::Done::undo).collect()));
            }
            Err(e) => eprintln!("gpu: couldn't throttle {n}: {e:#}"),
        }
    }
    if !undo.is_empty() {
        std::thread::sleep(Duration::from_secs(3));
    }
    let log = |what: &str| {
        let hs = host::snapshot(&procs::scan(), &ops::containers());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(lab.root.join("gpu.log")) {
            use std::io::Write;
            let _ = writeln!(f, "{} {what} {:?}: {}", chrono::Local::now().format("%F %T"), cmd, hs.summary());
        }
        hs
    };
    let before = log("start");
    if !before.contenders(&format!("lab:{}", me.unwrap_or(""))).is_empty() {
        eprintln!("gpu: still on the GPU: {}", before.contenders(&format!("lab:{}", me.unwrap_or(""))).iter().map(|u| format!("{} ({})", u.program, u.owner)).collect::<Vec<_>>().join(", "));
    }
    let status = std::process::Command::new(&cmd[0]).args(&cmd[1..]).env("QUIVER_LAB_GPU_HELD", "1").status();
    log("end");
    for (i, m, a) in undo {
        if let Err(e) = configset::apply(&i, &m, &a) {
            eprintln!("gpu: couldn't restore {}: {e:#}", i.name);
        }
    }
    let _ = std::fs::remove_file(&holder_file);
    drop(lock);
    let status = status.with_context(|| format!("run {}", cmd[0]))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

async fn doctor(lab: &Lab, json: bool) -> Result<()> {
    let mut checks: Vec<(String, bool, String)> = Vec::new();
    let mut check = |name: &str, ok: bool, detail: String| checks.push((name.into(), ok, detail));
    let podman = ops::podman_cmd(&["--version".into()]).output();
    check("podman", podman.as_ref().is_ok_and(|o| o.status.success()), podman.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_else(|e| e.to_string()));
    let cdi = ["/etc/cdi/nvidia.yaml", "/var/run/cdi/nvidia.yaml", "/run/cdi/nvidia.yaml"].iter().find(|p| Path::new(p).is_file());
    check("NVIDIA CDI spec", cdi.is_some(), cdi.map(|p| p.to_string()).unwrap_or_else(|| "none: sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml".into()));
    let image = ops::podman_cmd(&["image".into(), "exists".into(), lab::IMAGE.into()]).status().is_ok_and(|s| s.success());
    check("image", image, if image { lab::IMAGE.into() } else { "missing: quiver lab image build".into() });
    let lib32 = podman::host_ro_mounts().iter().filter(|p| p.starts_with("/usr/lib32")).count();
    check("32-bit NVIDIA libraries", lib32 > 0, format!("{lib32} in /usr/lib32"));
    std::fs::create_dir_all(&lab.root).ok();
    let probe = lab.root.join(".reflink-probe");
    let _ = std::fs::remove_dir_all(&probe);
    std::fs::create_dir_all(probe.join("a")).ok();
    std::fs::write(probe.join("a/f"), b"x").ok();
    let reflinks = lab::clone_tree(&probe.join("a"), &probe.join("b"));
    let _ = std::fs::remove_dir_all(&probe);
    check("reflinks in the lab root", reflinks.is_ok(), match &reflinks { Ok(()) => lab.root.display().to_string(), Err(e) => format!("{e:#}") });
    let quiverd = Client::connect_running("lab").await.is_ok();
    check("quiverd", quiverd, if quiverd { "running".into() } else { "not running (quiver status)".into() });
    let bridge = ops::lab_bridge();
    check("lab-aware bridge", bridge.is_ok(), match bridge { Ok(p) => p.display().to_string(), Err(e) => format!("{e:#}") });
    // Games with the EOS SDK don't launch without it (design/LAB.md section 3.1).
    let eos = eosfilter::find_build();
    check(
        "EOS filter (Epic/EOS games: cloud saves, achievements, stats blocked)",
        eos.is_ok(),
        match eos {
            Ok(p) => format!("{} ({} allowlist entries, for {})", p.display(), eosfilter::allowlist().len(), eosfilter::filtered_hosts().join(", ")),
            Err(e) => format!("{e:#}"),
        },
    );
    if json {
        return print_json(&checks.iter().map(|(n, ok, d)| serde_json::json!({ "check": n, "ok": ok, "detail": d })).collect::<Vec<_>>());
    }
    for (n, ok, d) in &checks {
        println!("{} {n}: {d}", if *ok { "ok  " } else { "FAIL" });
    }
    Ok(())
}

async fn brief(lab: &Lab, client: &Client, game: &str, instance: Option<String>, display: &str, exe: Option<String>) -> Result<String> {
    // A Steam game without --exe still gets a brief (with fewer facts).
    let src = create::source(client, game, exe.as_deref(), false).await?;
    let existing = lab.list().into_iter().filter_map(|n| lab.instance(&n).load().ok()).find(|m| m.game == src.game && instance.as_ref().is_none_or(|i| *i == m.name));
    let name = instance.or(existing.as_ref().map(|m| m.name.clone())).unwrap_or_else(|| default_name(&src.title));
    // Facts from the instance's clones when there is one; else the person's files, read only.
    let (install, prefix) = match &existing {
        Some(m) => {
            let i = lab.instance(&m.name);
            (m.install_in(&i), Some(i.prefix()))
        }
        None => (src.install.clone(), src.prefix.clone()),
    };
    let f = facts::gather(&install, &src.exe, prefix.as_deref());
    let licence = create::brief_licence(src.game.store, &install, &src.exe);
    let docs = option_env!("CARGO_MANIFEST_DIR").map(|d| Path::new(d).join("../..")).and_then(|p| p.canonicalize().ok()).filter(|p| p.join("design/LAB.md").is_file());
    let list_dir = |d: PathBuf, suffix: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).into_iter().flatten().flatten().filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if suffix.is_empty() { e.path().is_dir().then_some(n) } else { n.strip_suffix(suffix).map(str::to_owned) }
        }).collect();
        v.sort();
        v
    };
    let b = quiver_core::lab::brief::BriefInput {
        game: src.game.to_string(),
        title: src.title.clone(),
        store: src.game.store.label().into(),
        licence: licence.as_ref().ok().copied(),
        refused: licence.err(),
        install: src.install.display().to_string(),
        prefix: src.prefix.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(a fresh prefix)".into()),
        proton: existing.as_ref().map(|m| m.runtime.proton.clone()).or(src.proton.clone()).or_else(|| quiver_core::launcher::Config::load().ok().and_then(|c| c.proton.file_name().map(|n| n.to_string_lossy().into_owned()))).unwrap_or_else(|| "Quiver's default".into()),
        facts: f,
        instance_exists: existing.is_some(),
        display: existing.as_ref().map(|m| m.display.to_string()).unwrap_or_else(|| display.to_owned()),
        instance: name,
        lab_root: lab.root.display().to_string(),
        fixtures: list_dir(lab.fixtures(&src.game.dir_name()), ""),
        profiles: list_dir(lab.profiles(&src.game.dir_name()), ".set"),
        docs: docs.map(|d| d.display().to_string()),
    };
    Ok(quiver_core::lab::brief::render(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_edits() {
        let mut m: Manifest = toml::from_str(
            "name='t1'\ngame='ubisoft:3539'\ntitle='x'\nlicence='ubisoft-bridge'\ncreated='2026-11-18T10:00:00Z'\n[display]\nwidth=1280\nheight=720\nrefresh=60\n[runtime]\nproton='p'\nproton_path='/p'\nimage='i'\n[paths]\ninstall='/g'\nprefix='/pf'\nexe='g.exe'\n",
        )
        .unwrap();
        for c in ["display=2560x1440@144", "env.DXVK_HUD=fps", "dll.dxgi=n,b", "file.pharos=install:pharos.ini", "throttle+=pharos:[reflex]fps_limit=5", "reload_pattern=reloaded", "args=-a -b"] {
            edit(&mut m, c).unwrap();
        }
        assert_eq!(m.display.to_string(), "2560x1440@144");
        assert_eq!((m.env["DXVK_HUD"].as_str(), m.dll_overrides["dxgi"].as_str()), ("fps", "n,b"));
        assert_eq!(m.throttle, ["pharos:[reflex]fps_limit=5"]);
        assert_eq!(m.paths.args, ["-a", "-b"]);
        edit(&mut m, "env.DXVK_HUD=").unwrap();
        assert!(m.env.is_empty());
        assert!(edit(&mut m, "bogus=1").is_err() && edit(&mut m, "throttle+=nope").is_err() && edit(&mut m, "reload_pattern=(").is_err());
    }

    #[test]
    fn default_names() {
        assert_eq!(default_name("Assassin's Creed® Origins"), "assassins-creed-orig");
        assert!(lab::check_name(&default_name("Assassin's Creed® Origins")).is_ok());
        assert_eq!(default_name("™"), "game");
    }
}
