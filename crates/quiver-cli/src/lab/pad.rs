//! `quiver lab pad N …` (design/LAB.md §15): virtual game controllers for an instance.
//!
//! An instance with `[pad] count = N` gets N uinput pads, laid out as wired Xbox 360
//! controllers, made on the host by its **pad host** (`quiver lab pad-host N`, started by `up`
//! before the container) and passed into that container only. The pad host holds the uinput
//! fds, so the pads die with it, whatever kills it; it exits by itself when the container
//! stops. The udev rule (`contrib/lab/72-quiver-lab-pad.rules`) keeps every lab pad from the
//! host; the pad host refuses to make one without it and destroys a pad udev didn't hide.
//!
//! The commands talk to the pad host over `<instance>/pad/host.sock`. Durations follow the
//! virtual clock when the run has one: the game is held, the input applied and left to settle,
//! then exactly the duration's frames are stepped, so a script is the same every run.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use quiver_core::lab::podman::{self, PadMounts};
use quiver_core::lab::vclock::control::{Cmd, Control, Snapshot};
use quiver_core::lab::{Inst, Lab, Manifest};
use quiver_pad::host::{PadInfo, Ready, ReadyPad, Reply, Request};
use quiver_pad::model::{self, Axis, PadState};
use quiver_pad::seq::{self, Action, Dur};
use quiver_pad::{probe, rule, uinput};

use super::{ops, read_state};

#[derive(Args)]
pub struct PadArgs {
    pub instance: String,
    #[command(subcommand)]
    pub what: PadCmd,
    /// Which pad, from 1 (player 1).
    #[arg(long, global = true, default_value_t = 1)]
    pub pad: usize,
    /// Wall-clock durations even when the run has the virtual clock (not repeatable).
    #[arg(long, global = true)]
    pub real: bool,
    /// Real ms input is left to settle while the game is held, before each step (virtual clock;
    /// default the manifest's pad.settle_ms, else 40).
    #[arg(long, global = true)]
    pub settle: Option<u32>,
    /// A hyprpad grip map for this command (`r4=a,r5=b,l4=up,l5=down`; default pad.grips).
    #[arg(long, global = true)]
    pub grips: Option<String>,
}

#[derive(Subcommand)]
pub enum PadCmd {
    /// Press buttons together, hold them for --for (default 120ms, or Nf frames), let go.
    Press {
        #[arg(required = true)]
        buttons: Vec<String>,
        #[arg(long = "for")]
        dur: Option<String>,
    },
    /// Press buttons and leave them down (until `release`).
    Hold {
        #[arg(required = true)]
        buttons: Vec<String>,
    },
    /// Let buttons go (none named: every button).
    Release { buttons: Vec<String> },
    /// A stick to (X, Y), each -1..1, Y up. With --for, back where it was after.
    Stick {
        /// left or right
        which: String,
        #[arg(allow_hyphen_values = true)]
        x: f32,
        #[arg(allow_hyphen_values = true)]
        y: f32,
        #[arg(long = "for")]
        dur: Option<String>,
    },
    /// A trigger (lt or rt) to 0..1. With --for, back where it was after.
    Trigger {
        which: String,
        value: f32,
        #[arg(long = "for")]
        dur: Option<String>,
    },
    /// Every button up, sticks centred, triggers released.
    Neutral,
    /// Run a script: "a 100ms; wait 200ms; lstick=0,1 2s; +rt=1; x; neutral" (or @FILE).
    /// Words: BUTTON (down for the step), +BUTTON / -BUTTON (stays), AXIS=V or lstick=X,Y
    /// (for the step), +AXIS=V (stays), neutral, wait, and a duration: 120ms, 1.5s, 12f.
    Seq {
        script: String,
        /// Print what it would do, and how long, without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// The pads, how they're hidden from the host, their state and rumble, who has them open
    /// in the container, and what a Windows game sees (XInput, DirectInput, HID, raw input).
    Status {
        #[arg(long)]
        json: bool,
        /// Skip the Windows probe (it starts a small program in the game's Wine session).
        #[arg(long)]
        no_probe: bool,
    },
}

// ---- Files ----

pub fn dir(inst: &Inst) -> PathBuf {
    inst.dir.join("pad")
}
/// The pad host's socket: in the runtime dir, named by a stable hash of the instance's folder
/// (a lab root can be deeper than a unix socket path may be long).
fn sock(inst: &Inst) -> PathBuf {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in inst.dir.as_os_str().as_encoded_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    base.join(format!("quiver-lab-pad-{h:016x}.sock"))
}
fn ready_file(inst: &Inst) -> PathBuf {
    dir(inst).join("host.json")
}
fn lock_file(inst: &Inst) -> PathBuf {
    dir(inst).join("host.lock")
}
fn udev_dir(inst: &Inst) -> PathBuf {
    dir(inst).join("udev")
}
const SYS_INPUT: &str = "/sys/devices/virtual/input";

fn log(msg: &str) {
    println!("{} {msg}", chrono::Local::now().format("%H:%M:%S%.3f"));
    let _ = std::io::stdout().flush();
}

// ---- The pad host ----

/// `quiver lab pad-host N`: make the pads, check udev hid them, serve the socket, and exit
/// (destroying them) when the container is gone or `down` says so.
pub fn host(lab: &Lab, name: &str) -> Result<()> {
    let inst = lab.instance(name);
    let m = inst.load()?;
    std::fs::create_dir_all(dir(&inst))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_file(&inst))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("instance '{name}' already has a pad host");
    }
    let _ = std::fs::remove_file(ready_file(&inst));
    let _ = std::fs::remove_file(sock(&inst));
    let fail = |e: anyhow::Error| -> Result<()> {
        log(&format!("refused: {e:#}"));
        let r = Ready { pid: std::process::id(), instance: name.into(), error: Some(format!("{e:#}")), plan: None, pads: vec![] };
        let _ = quiver_core::lab::write_atomic(&ready_file(&inst), &serde_json::to_vec_pretty(&r)?);
        Err(e)
    };
    let count = (m.pad.count.min(quiver_core::lab::manifest::PadConfig::MAX)) as usize;
    if count == 0 {
        return fail(anyhow::anyhow!("the instance has no pads (quiver lab edit {name} pads=1)"));
    }
    // Never a pad without the rule (rule::plan).
    let plan = match rule::plan() {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    log(&format!("rule {} (group {} gid {}, container gid {})", plan.rule.display(), plan.group, plan.gid, plan.container_gid));
    // The canary first: same name pattern and phys, no keys or axes (not a joystick, so nothing
    // grants or binds it). The rule must have run on it before any real pad exists.
    if let Err(e) = (|| -> Result<()> {
        let c = uinput::Pad::canary(Path::new("/dev/uinput"), &format!("{} canary", model::pad_name(name, 0)), &format!("{}{name}/canary", model::PHYS_PREFIX))?;
        let end = Instant::now() + Duration::from_secs(3);
        let nodes = loop {
            match c.nodes() {
                Ok(n) if n.event.exists() => break n,
                _ if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
                Ok(n) => bail!("{} didn't appear", n.event.display()),
                Err(e) => return Err(e),
            }
        };
        rule::wait_hidden(std::slice::from_ref(&nodes.event), plan.gid, Duration::from_secs(5)).context("a test device (not a joystick) wasn't hidden, so no pad was made")?;
        log(&format!("canary {} {}: the rule ran on it (root:{} 0660, no ACL)", nodes.sysname, nodes.event.display(), plan.gid));
        Ok(())
    })() {
        return fail(e);
    }
    let mut pads = Vec::new();
    let mut ready = Vec::new();
    for i in 0..count {
        let made = (|| -> Result<(uinput::Pad, ReadyPad)> {
            let pad = uinput::Pad::create(Path::new("/dev/uinput"), &model::pad_name(name, i), &model::pad_phys(name, i))?;
            let end = Instant::now() + Duration::from_secs(3);
            let nodes = loop {
                match pad.nodes() {
                    Ok(n) if n.event.exists() && n.js.as_ref().is_none_or(|j| j.exists()) => break n,
                    Ok(_) | Err(_) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
                    Ok(n) => bail!("{} didn't appear", n.event.display()),
                    Err(e) => return Err(e),
                }
            };
            let paths: Vec<PathBuf> = std::iter::once(nodes.event.clone()).chain(nodes.js.clone()).collect();
            let checks = rule::wait_hidden(&paths, plan.gid, Duration::from_secs(5))?;
            Ok((pad, ReadyPad { index: i, name: model::pad_name(name, i), phys: model::pad_phys(name, i), nodes, checks }))
        })();
        match made {
            Ok((pad, r)) => {
                log(&format!("pad {} \"{}\" {}: {} {} hidden (root:{} 0660, no ACL, not openable by uid {})", i + 1, r.name, r.nodes.sysname, r.nodes.event.display(), r.nodes.js.as_ref().map(|j| j.display().to_string()).unwrap_or_default(), plan.gid, unsafe { libc::getuid() }));
                pads.push(pad);
                ready.push(r);
            }
            Err(e) => {
                // Destroy every pad made so far (closing the fds) before saying why.
                drop(pads);
                return fail(e.context(format!("pad {} destroyed", i + 1)));
            }
        }
    }
    write_udev_db(&udev_dir(&inst), &ready)?;
    let listener = UnixListener::bind(sock(&inst))?;
    listener.set_nonblocking(true)?;
    let r = Ready { pid: std::process::id(), instance: name.into(), error: None, plan: Some(plan.clone()), pads: ready.clone() };
    quiver_core::lab::write_atomic(&ready_file(&inst), &serde_json::to_vec_pretty(&r)?)?;
    log(&format!("{} pad(s) ready; serving {}", pads.len(), sock(&inst).display()));

    let mut frames = vec![0u64; pads.len()];
    // The container's lifetime is the pads'. Checked on a thread of its own: `podman inspect`
    // can take seconds on a loaded machine, and requests must never wait behind it.
    let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (gone, container) = (gone.clone(), inst.container());
        std::thread::spawn(move || {
            let started = Instant::now();
            let (mut seen_up, mut down_checks) = (false, 0);
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if ops::running_id(&container).is_some() {
                    seen_up = true;
                    down_checks = 0;
                } else {
                    down_checks += 1;
                    if (seen_up && down_checks >= 2) || (!seen_up && started.elapsed() > Duration::from_secs(120)) {
                        gone.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
    }
    let mut last_recheck = Instant::now();
    loop {
        let mut fds: Vec<libc::pollfd> = std::iter::once(listener.as_raw_fd()).chain(pads.iter().map(|p| p.fd())).map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 500) };
        for (i, f) in fds.iter().enumerate().skip(1) {
            if f.revents & libc::POLLIN != 0 {
                pads[i - 1].service();
            }
        }
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((conn, _)) = listener.accept() {
                if serve(conn, &mut pads, &ready, &mut frames) {
                    log("quit: destroying the pads");
                    let _ = std::fs::remove_file(sock(&inst));
                    let _ = std::fs::remove_file(ready_file(&inst));
                    return Ok(());
                }
            }
        }
        if gone.load(std::sync::atomic::Ordering::SeqCst) {
            log(&format!("the container {} isn't running: destroying the pads", inst.container()));
            let _ = std::fs::remove_file(sock(&inst));
            let _ = std::fs::remove_file(ready_file(&inst));
            return Ok(());
        }
        // Still hidden? (A rule changed and re-triggered would show here.)
        if last_recheck.elapsed() > Duration::from_secs(5) {
            last_recheck = Instant::now();
            for p in &ready {
                for n in std::iter::once(&p.nodes.event).chain(&p.nodes.js) {
                    let bad = match rule::check_node(n) {
                        Ok(c) => c.openable || c.acl || c.gid != plan.gid,
                        Err(_) => false,
                    };
                    if bad {
                        log(&format!("{} is no longer hidden from the host: destroying the pads", n.display()));
                        std::process::exit(3);
                    }
                }
            }
        }
    }
}

/// One request; true: quit.
fn serve(conn: UnixStream, pads: &mut [uinput::Pad], ready: &[ReadyPad], frames: &mut [u64]) -> bool {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).is_err() {
        return false;
    }
    let info = |pads: &[uinput::Pad], frames: &[u64]| -> Vec<PadInfo> {
        pads.iter()
            .zip(ready)
            .zip(frames)
            .map(|((p, r), f)| PadInfo { index: r.index, name: p.name.clone(), phys: p.phys.clone(), nodes: r.nodes.clone(), state: p.state, rumble: p.rumble.clone(), frames: *f })
            .collect()
    };
    let (reply, quit) = match serde_json::from_str::<Request>(&line) {
        Err(e) => (Reply { ok: false, error: Some(format!("bad request: {e}")), pads: vec![] }, false),
        Ok(Request::Quit) => (Reply { ok: true, error: None, pads: vec![] }, true),
        Ok(Request::Get) => (Reply { ok: true, error: None, pads: info(pads, frames) }, false),
        Ok(Request::Apply { pad, changes }) => match pads.get_mut(pad) {
            None => (Reply { ok: false, error: Some(format!("no pad {} (this instance has {})", pad + 1, ready.len())), pads: vec![] }, false),
            Some(p) => {
                let mut next = p.state;
                for c in &changes {
                    next.apply(c);
                }
                match p.set(next) {
                    Ok(n) => {
                        frames[pad] += (n > 0) as u64;
                        (Reply { ok: true, error: None, pads: info(pads, frames) }, false)
                    }
                    Err(e) => (Reply { ok: false, error: Some(format!("{e:#}")), pads: vec![] }, false),
                }
            }
        },
    };
    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
    out.push(b'\n');
    let _ = (&conn).write_all(&out);
    quit
}

/// The container has no udevd: a minimal database, so libudev users (and SDL's udev path)
/// see the pads as joysticks. Only this instance's nodes are in it.
pub fn write_udev_db(dir: &Path, pads: &[ReadyPad]) -> Result<()> {
    let data = dir.join("data");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(&data)?;
    let props = format!(
        "E:ID_INPUT=1\nE:ID_INPUT_JOYSTICK=1\nE:ID_BUS=usb\nE:ID_VENDOR_ID={:04x}\nE:ID_MODEL_ID={:04x}\nE:ID_SERIAL=Quiver_Lab_Pad\n",
        model::VENDOR,
        model::PRODUCT
    );
    for p in pads {
        std::fs::write(data.join(format!("+input:{}", p.nodes.sysname)), &props)?;
        for n in std::iter::once(&p.nodes.event).chain(&p.nodes.js) {
            use std::os::unix::fs::MetadataExt;
            let rdev = std::fs::metadata(n)?.rdev();
            std::fs::write(data.join(format!("c{}:{}", libc::major(rdev), libc::minor(rdev))), &props)?;
        }
    }
    Ok(())
}

// ---- Lifecycle (up, down, stale pads) ----

/// Pids of pad hosts for `name` (ours, by command line).
fn host_pids(name: &str) -> Vec<u32> {
    let me = unsafe { libc::getuid() };
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        use std::os::unix::fs::MetadataExt;
        if e.metadata().map(|m| m.uid()).ok() != Some(me) {
            continue;
        }
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        if args.windows(3).any(|w| w[0] == b"lab" && w[1] == b"pad-host" && w[2] == name.as_bytes()) {
            out.push(pid);
        }
    }
    out
}

/// Stop the instance's pad host (its pads go with it). Quietly does nothing without one.
pub fn stop(inst: &Inst) {
    if let Ok(mut c) = UnixStream::connect(sock(inst)) {
        let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = c.write_all(b"{\"op\":\"quit\"}\n");
        let mut s = String::new();
        let _ = BufReader::new(&c).read_line(&mut s);
    }
    let end = Instant::now() + Duration::from_secs(3);
    while !host_pids(&inst.name).is_empty() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        let pids = host_pids(&inst.name);
        if pids.is_empty() {
            break;
        }
        for p in pids {
            unsafe { libc::kill(p as i32, sig) };
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = std::fs::remove_file(sock(inst));
    let _ = std::fs::remove_file(ready_file(inst));
}

/// Lab pads whose instance isn't up (a host that outlived its container): stopped. Returns
/// what was cleaned.
pub fn sweep(lab: &Lab, except: Option<&str>) -> Vec<String> {
    let mut done = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (sys, phys, name) in uinput::present(Path::new(SYS_INPUT)) {
        let Some((inst, _)) = model::parse_phys(&phys) else { continue };
        if Some(inst.as_str()) == except || !seen.insert(inst.clone()) {
            continue;
        }
        let i = lab.instance(&inst);
        if ops::running_id(&i.container()).is_some() {
            continue;
        }
        stop(&i);
        done.push(format!("stale lab pad {sys} \"{name}\" ({phys}): its instance isn't up, its pad host stopped"));
    }
    done
}

/// At `up`, before the container: the pad host, its pads made and hidden. The mounts to give
/// the container.
pub fn start(lab: &Lab, inst: &Inst, m: &Manifest) -> Result<Option<(PadMounts, Vec<String>)>> {
    if m.pad.count == 0 {
        return Ok(None);
    }
    let mut notes = sweep(lab, None);
    stop(inst);
    std::fs::create_dir_all(dir(inst))?;
    {
        use std::os::unix::process::CommandExt;
        let logf = std::fs::OpenOptions::new().create(true).append(true).open(dir(inst).join("host.log"))?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(["lab", "pad-host", &inst.name]).env("QUIVER_LAB_ROOT", &lab.root).stdin(std::process::Stdio::null()).stdout(logf.try_clone()?).stderr(logf);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().context("start the pad host")?;
    }
    let end = Instant::now() + Duration::from_secs(15);
    let ready: Ready = loop {
        if let Some(r) = std::fs::read(ready_file(inst)).ok().and_then(|b| serde_json::from_slice::<Ready>(&b).ok()) {
            break r;
        }
        if Instant::now() > end {
            stop(inst);
            bail!("the pad host didn't make the pads in 15 s (see {})", dir(inst).join("host.log").display());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Some(e) = ready.error {
        bail!("no pads for {}: {e}", inst.name);
    }
    let plan = ready.plan.clone().context("the pad host's report has no plan")?;
    for p in &ready.pads {
        notes.push(format!(
            "pad {}: \"{}\" ({}) at {}{}, hidden from the host (root:{} 0660, no ACL); container gid {}",
            p.index + 1,
            p.name,
            model::IDENTITY,
            p.nodes.event.display(),
            p.nodes.js.as_ref().map(|j| format!(" and {}", j.display())).unwrap_or_default(),
            plan.group,
            plan.container_gid
        ));
    }
    Ok(Some((PadMounts { devices: ready.devices(), container_gid: plan.container_gid, udev: udev_dir(inst) }, notes)))
}

// ---- Talking to the pad host ----

fn request(inst: &Inst, req: &Request) -> Result<Reply> {
    let mut c = UnixStream::connect(sock(inst)).map_err(|_| {
        let m = inst.load().ok();
        match m {
            Some(m) if m.pad.count == 0 => anyhow::anyhow!("{0} has no pads: quiver lab edit {0} pads=1, then quiver lab down {0} && quiver lab up {0}", inst.name),
            _ => anyhow::anyhow!("{0}'s pads aren't up (quiver lab up {0}; its pad host's log is {1})", inst.name, dir(inst).join("host.log").display()),
        }
    })?;
    c.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    c.write_all(&line)?;
    let mut rsp = String::new();
    BufReader::new(&c).read_line(&mut rsp)?;
    let r: Reply = serde_json::from_str(&rsp).context("the pad host's reply")?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(r)
}

fn state_of(inst: &Inst, pad: usize) -> Result<PadInfo> {
    let r = request(inst, &Request::Get)?;
    let n = r.pads.len();
    r.pads.into_iter().nth(pad).with_context(|| format!("no pad {} ({} has {n})", pad + 1, inst.name))
}

// ---- Timing: real, or the virtual clock ----

enum Clock {
    Real,
    Virtual { ctl: Control, run: u32, rate: f64, was_running: bool },
}

fn exited(inst: &Inst, run: u32) -> Option<String> {
    match read_state(inst).run {
        Some(r) if r.id == run => r.exit.map(|e| format!("{}: {}", e.how, e.detail)),
        Some(r) => Some(format!("a new run (r{}) replaced r{run}", r.id)),
        None => Some("no run".into()),
    }
}

/// Send a clock command and wait for the hold it asks for.
fn hold(inst: &Inst, ctl: &Control, run: u32, cmd: Cmd, arg: u64) -> Result<Snapshot> {
    let seq = ctl.send(cmd, arg, 0);
    let end = Instant::now() + Duration::from_secs(10);
    while !ctl.acked(seq) {
        if let Some(why) = exited(inst, run) {
            bail!("the game exited ({why})");
        }
        if Instant::now() > end {
            bail!("the game's process didn't take the clock command in 10 s");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let target = ctl.snapshot().ack_hold_at;
    let end = Instant::now() + Duration::from_secs(300);
    let mut checked = Instant::now();
    loop {
        let s = ctl.snapshot();
        if s.held() && s.frames >= target {
            return Ok(s);
        }
        if s.running() && s.ack_seq == seq {
            bail!("the hold ended before frame {target} (another controller resumed it)");
        }
        if checked.elapsed() > Duration::from_millis(250) {
            checked = Instant::now();
            if let Some(why) = exited(inst, run) {
                bail!("the game exited ({why})");
            }
        }
        if Instant::now() > end {
            bail!("not held at frame {target} after 300 s: is the game presenting?");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn clock_for(inst: &Inst, real: bool) -> Result<Clock> {
    if real {
        return Ok(Clock::Real);
    }
    let Some(r) = read_state(inst).run.filter(|r| r.exit.is_none()) else { return Ok(Clock::Real) };
    let Ok(ctl) = Control::open(&inst.run_dir(r.id)) else { return Ok(Clock::Real) };
    let s = ctl.snapshot();
    if s.rate() <= 0.0 {
        return Ok(Clock::Real);
    }
    Ok(Clock::Virtual { rate: s.rate(), was_running: !s.held() && s.state != "holding", ctl, run: r.id })
}

/// Run compiled actions on pad `pad`, timed by `clock`.
fn execute(inst: &Inst, pad: usize, actions: &[Action], clock: &Clock, settle: Duration) -> Result<()> {
    let mut held_note = None;
    if let Clock::Virtual { ctl, run, rate, was_running } = clock {
        let s = if *was_running { hold(inst, ctl, *run, Cmd::Pause, 0)? } else { ctl.snapshot() };
        println!("virtual clock at {rate} Hz: {} at frame {} (virtual {:.6} s); durations are frames", if *was_running { "paused" } else { "held" }, s.frames, s.virtual_s);
    }
    let result = (|| -> Result<()> {
        for a in actions {
            match a {
                Action::Apply(changes) => {
                    let r = request(inst, &Request::Apply { pad, changes: changes.clone() })?;
                    let st = r.pads.get(pad).map(|p| p.state.describe()).unwrap_or_default();
                    match clock {
                        Clock::Virtual { ctl, .. } => {
                            // Held: let evdev, SDL, winebus and XInput's thread catch up in real time.
                            std::thread::sleep(settle);
                            println!("  frame {:>7}: {st}", ctl.snapshot().frames);
                            held_note = Some(());
                        }
                        Clock::Real => println!("  {st}"),
                    }
                }
                Action::Wait(d) => match clock {
                    Clock::Real => match d {
                        Dur::Ms(ms) => std::thread::sleep(Duration::from_secs_f64(ms / 1000.0)),
                        Dur::Frames(_) => bail!("{} needs the virtual clock (quiver lab launch {} --clock virtual:62.5)", d.describe(), inst.name),
                    },
                    Clock::Virtual { ctl, run, rate, .. } => {
                        let k = d.frames(*rate);
                        if k > 0 {
                            hold(inst, ctl, *run, Cmd::Step, k)?;
                            held_note = None;
                        }
                    }
                },
            }
        }
        Ok(())
    })();
    if let Clock::Virtual { ctl, run, was_running, .. } = clock {
        let s = ctl.snapshot();
        if *was_running {
            let seq = ctl.send(Cmd::Resume, 0, 0);
            let end = Instant::now() + Duration::from_secs(5);
            while !ctl.acked(seq) && Instant::now() < end && exited(inst, *run).is_none() {
                std::thread::sleep(Duration::from_millis(1));
            }
            println!("resumed from frame {} (virtual {:.6} s)", s.frames, s.virtual_s);
        } else {
            println!("held at frame {} (virtual {:.6} s){}", s.frames, s.virtual_s, if held_note.is_some() { ": the game sees the last change when it next steps" } else { "" });
        }
    }
    result
}

fn dur_arg(s: Option<&str>) -> Result<Option<Dur>> {
    s.map(|s| Dur::parse(s).with_context(|| format!("'{s}': a duration is 120ms, 1.5s or 12f"))).transpose()
}

fn script_for(cmd: &PadCmd, state: &PadState) -> Result<String> {
    let d = |s: &Option<String>| -> Result<String> { Ok(dur_arg(s.as_deref())?.map(|d| format!(" {}", dur_word(&d))).unwrap_or_default()) };
    Ok(match cmd {
        PadCmd::Press { buttons, dur } => format!("{}{}", buttons.join(" "), d(dur)?),
        PadCmd::Hold { buttons } => buttons.iter().map(|b| format!("+{b}")).collect::<Vec<_>>().join(" "),
        PadCmd::Release { buttons } if buttons.is_empty() => {
            let down: Vec<String> = state.pressed_list().iter().map(|b| format!("-{}", b.name())).collect();
            if down.is_empty() { String::new() } else { down.join(" ") }
        }
        PadCmd::Release { buttons } => buttons.iter().map(|b| format!("-{b}")).collect::<Vec<_>>().join(" "),
        PadCmd::Stick { which, x, y, dur } => {
            let s = match which.to_ascii_lowercase().as_str() {
                "left" | "l" | "ls" => "lstick",
                "right" | "r" | "rs" => "rstick",
                w => bail!("'{w}': left or right"),
            };
            if dur.is_some() { format!("{s}={x},{y}{}", d(dur)?) } else { format!("+{s}={x},{y}") }
        }
        PadCmd::Trigger { which, value, dur } => {
            let a = Axis::parse(which).filter(|a| a.is_trigger()).with_context(|| format!("'{which}': lt or rt"))?;
            if dur.is_some() { format!("{}={value}{}", a.name(), d(dur)?) } else { format!("+{}={value}", a.name()) }
        }
        PadCmd::Neutral => "neutral".into(),
        PadCmd::Seq { script, .. } => match script.strip_prefix('@') {
            Some(f) => std::fs::read_to_string(f).with_context(|| format!("read {f}"))?,
            None => script.clone(),
        },
        PadCmd::Status { .. } => String::new(),
    })
}

fn dur_word(d: &Dur) -> String {
    match d {
        Dur::Ms(ms) => format!("{ms}ms"),
        Dur::Frames(f) => format!("{f}f"),
    }
}

pub fn run(lab: &Lab, a: PadArgs) -> Result<()> {
    let (inst, m) = super::inst_of(lab, &a.instance)?;
    if a.pad == 0 {
        bail!("--pad counts from 1");
    }
    let pad = a.pad - 1;
    if let PadCmd::Status { json, no_probe } = a.what {
        return status(&inst, &m, json, no_probe);
    }
    let grips = model::parse_grip_map(a.grips.as_deref().or(m.pad.grips.as_deref()).unwrap_or(""))?;
    let info = state_of(&inst, pad)?;
    let script = script_for(&a.what, &info.state)?;
    let actions = seq::compile(&script, &info.state, Dur::Ms(120.0), &grips)?;
    let clock = clock_for(&inst, a.real)?;
    let rate = match &clock {
        Clock::Virtual { rate, .. } => Some(*rate),
        Clock::Real => None,
    };
    if let PadCmd::Seq { dry_run: true, .. } = a.what {
        for act in &actions {
            match act {
                Action::Apply(c) => println!("  {}", serde_json::to_string(c)?),
                Action::Wait(d) => println!("  wait {}{}", d.describe(), rate.map(|r| format!(" = {} frame(s) at {r} Hz", d.frames(r))).unwrap_or_default()),
            }
        }
        let (ms, frames) = seq::total(&actions, rate);
        println!("{} action(s), {ms:.0} ms{}; ends at: {}", actions.len(), rate.map(|_| format!(" = {frames} frame(s)")).unwrap_or_default(), seq::end_state(&info.state, &actions).describe());
        return Ok(());
    }
    if actions.is_empty() {
        println!("pad {}: nothing to do ({})", a.pad, info.state.describe());
        return Ok(());
    }
    let settle = Duration::from_millis(a.settle.or(m.pad.settle_ms).unwrap_or(40) as u64);
    println!("pad {} \"{}\":", a.pad, info.name);
    execute(&inst, pad, &actions, &clock, settle)?;
    let after = state_of(&inst, pad)?;
    println!("pad {} now: {}", a.pad, after.state.describe());
    Ok(())
}

// ---- Status ----

/// Container processes that have one of `nodes` open: (pid, program, node).
fn openers(inst: &Inst, nodes: &[PathBuf]) -> Vec<(u32, String, PathBuf)> {
    let Some(id) = ops::running_id(&inst.container()) else { return vec![] };
    let all = quiver_core::lab::procs::scan();
    let mut out = Vec::new();
    for p in quiver_core::lab::procs::in_container(&all, &id) {
        for fd in std::fs::read_dir(format!("/proc/{}/fd", p.pid)).into_iter().flatten().flatten() {
            if let Ok(t) = std::fs::read_link(fd.path())
                && nodes.contains(&t)
            {
                out.push((p.pid, p.program().to_owned(), t));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn prefix_arch(inst: &Inst) -> &'static str {
    let head = std::fs::read(inst.prefix().join("system.reg")).ok().map(|b| String::from_utf8_lossy(&b[..b.len().min(4096)]).into_owned()).unwrap_or_default();
    if head.contains("#arch=win32") { "x86" } else { "x64" }
}

/// Run the probe in the game's Wine session: only while the game runs (the probe must not
/// start a wineserver of its own, whose winedevice wouldn't be the game's).
fn run_probe(inst: &Inst, m: &Manifest) -> Result<probe::Report> {
    let st = read_state(inst);
    let r = st.run.as_ref().filter(|r| r.exit.is_none() && r.phase != quiver_core::lab::ready::Phase::Starting).context("the game isn't running (quiver lab launch first): the probe runs in its Wine session")?;
    let id = ops::running_id(&inst.container()).context("the container isn't up")?;
    let procs = quiver_core::lab::procs::scan();
    if !quiver_core::lab::procs::instance_wine(&procs, &id).iter().any(|p| p.program() == "wineserver") {
        bail!("no wineserver in r{} (the game's Wine session is gone)", r.id);
    }
    let arch = prefix_arch(inst);
    let exe = probe::exe(arch).with_context(|| format!("this quiver was built without the {arch} probe (no mingw)"))?;
    let rel = format!("pad/quiver-pad-probe-{arch}.exe");
    let host_path = inst.lab().join(&rel);
    std::fs::create_dir_all(host_path.parent().unwrap())?;
    std::fs::write(&host_path, exe)?;
    let wine = m.runtime.proton_path.join("files/bin/wine").display().to_string();
    let env = vec![("WINEDEBUG".to_owned(), "-all".to_owned())];
    let args = podman::exec_args(&inst.container(), false, None, &env, &[wine, format!("{}/{rel}", quiver_core::lab::LAB_MOUNT)]);
    let mut child = ops::podman_cmd(&args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > end {
            let _ = child.kill();
            bail!("the probe didn't answer in 30 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    probe::Report::parse(&String::from_utf8_lossy(&out.stdout))
}

fn status(inst: &Inst, m: &Manifest, json: bool, no_probe: bool) -> Result<()> {
    let ready: Option<Ready> = std::fs::read(ready_file(inst)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let live = request(inst, &Request::Get).ok();
    let nodes: Vec<PathBuf> = ready.as_ref().map(|r| r.devices()).unwrap_or_default();
    let checks: Vec<rule::NodeCheck> = nodes.iter().filter_map(|n| rule::check_node(n).ok()).collect();
    let open = openers(inst, &nodes);
    let report = if no_probe || live.is_none() { None } else { Some(run_probe(inst, m).map_err(|e| format!("{e:#}"))) };
    if json {
        return super::print_json(&serde_json::json!({
            "instance": inst.name, "count": m.pad.count, "host": ready, "pads": live.as_ref().map(|l| &l.pads),
            "checks": checks, "open_in_container": open.iter().map(|(pid, prog, n)| serde_json::json!({"pid": pid, "program": prog, "node": n})).collect::<Vec<_>>(),
            "probe": report.as_ref().map(|r| r.as_ref().ok()), "probe_error": report.as_ref().and_then(|r| r.as_ref().err()),
        }));
    }
    if m.pad.count == 0 {
        println!("{0}: no pads (quiver lab edit {0} pads=1, then down/up)", inst.name);
        return Ok(());
    }
    let (Some(ready), Some(live)) = (ready.filter(|r| r.error.is_none()), live) else {
        println!("{}: {} pad(s) configured, none up (quiver lab up {}; {})", inst.name, m.pad.count, inst.name, dir(inst).join("host.log").display());
        return Ok(());
    };
    let plan = ready.plan.as_ref();
    println!(
        "{}: {} pad(s), pad host pid {}; rule {} (group {} = gid {}, gid {} in the container)",
        inst.name,
        live.pads.len(),
        ready.pid,
        plan.map(|p| p.rule.display().to_string()).unwrap_or_default(),
        plan.map(|p| p.group.as_str()).unwrap_or(""),
        plan.map(|p| p.gid).unwrap_or(0),
        plan.map(|p| p.container_gid).unwrap_or(0)
    );
    let slot = report.as_ref().and_then(|r| r.as_ref().ok()).and_then(|r| r.our_slot().map(|s| s.slot));
    for p in &live.pads {
        let mine: Vec<&PathBuf> = std::iter::once(&p.nodes.event).chain(&p.nodes.js).collect();
        println!(
            "pad {}: \"{}\" as {}, {} {} (the same paths in the container){}",
            p.index + 1,
            p.name,
            model::IDENTITY,
            p.nodes.sysname,
            mine.iter().map(|n| n.display().to_string()).collect::<Vec<_>>().join(" "),
            match (p.index, slot) {
                (0, Some(s)) => format!("; the game sees it as XInput slot {s}"),
                _ => String::new(),
            }
        );
        for c in checks.iter().filter(|c| mine.contains(&&c.path)) {
            let gid = plan.map(|p| p.gid).unwrap_or(rule::GID);
            match c.problem(gid) {
                None => println!("  {}: hidden from the host: root:{} {:o}, no ACL, not openable by you (checked now)", c.path.display(), c.gid, c.mode & 0o777),
                Some(why) => println!("  NOT HIDDEN: {why}"),
            }
        }
        println!("  state: {} ({} event frame(s) sent)", p.state.describe(), p.frames);
        let r = &p.rumble;
        if r.uploads > 0 || r.plays > 0 {
            println!("  rumble from the game: strong {:.2} weak {:.2}{}, {} upload(s), {} play(s)", r.strong, r.weak, if r.playing { " (playing)" } else { " (stopped)" }, r.uploads, r.plays);
        } else {
            println!("  rumble from the game: none yet");
        }
        let users: Vec<String> = open.iter().filter(|(_, _, n)| mine.contains(&n)).map(|(pid, prog, n)| format!("{prog} (pid {pid}, {})", n.display())).collect();
        println!("  open in the container by: {}", if users.is_empty() { "nothing yet (no game, or it hasn't looked for controllers)".into() } else { users.join(", ") });
    }
    match report {
        None if no_probe => {}
        None => {}
        Some(Ok(r)) => {
            println!("what a Windows game sees (quiver-pad-probe in the run's Wine session):");
            for l in r.lines() {
                println!("  {l}");
            }
        }
        Some(Err(e)) => println!("what a Windows game sees: not probed ({e})"),
    }
    Ok(())
}

/// For `quiver lab doctor`: (ok, detail).
pub fn doctor() -> (bool, String) {
    match rule::plan() {
        Ok(p) => (true, format!("hidden from the host by {} ({}, group {} gid {} = gid {} in containers)", p.rule.display(), rule::MARKER, p.group, p.gid, p.container_gid)),
        Err(e) => (false, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_pad::model::{Button, Change};

    #[test]
    fn commands_become_scripts() {
        let mut s = PadState::default();
        let f = |c: PadCmd, s: &PadState| script_for(&c, s).unwrap();
        assert_eq!(f(PadCmd::Press { buttons: vec!["a".into(), "b".into()], dur: None }, &s), "a b");
        assert_eq!(f(PadCmd::Press { buttons: vec!["a".into()], dur: Some("3f".into()) }, &s), "a 3f");
        assert_eq!(f(PadCmd::Hold { buttons: vec!["lb".into()] }, &s), "+lb");
        assert_eq!(f(PadCmd::Stick { which: "left".into(), x: 0.0, y: 1.0, dur: None }, &s), "+lstick=0,1");
        assert_eq!(f(PadCmd::Stick { which: "r".into(), x: -1.0, y: 0.5, dur: Some("500ms".into()) }, &s), "rstick=-1,0.5 500ms");
        assert_eq!(f(PadCmd::Trigger { which: "RT".into(), value: 1.0, dur: None }, &s), "+rt=1");
        assert!(script_for(&PadCmd::Trigger { which: "lx".into(), value: 1.0, dur: None }, &s).is_err());
        assert!(script_for(&PadCmd::Press { buttons: vec!["a".into()], dur: Some("soon".into()) }, &s).is_err());
        s.apply(&Change::Press { button: Button::A });
        s.apply(&Change::Press { button: Button::Up });
        assert_eq!(f(PadCmd::Release { buttons: vec![] }, &s), "-a -up");
        assert_eq!(f(PadCmd::Release { buttons: vec![] }, &PadState::default()), "");
        // Every command's script compiles.
        for c in ["a b", "a 3f", "+lb", "+lstick=0,1", "rstick=-1,0.5 500ms", "+rt=1", "-a -up", "neutral"] {
            seq::compile(c, &s, Dur::Ms(120.0), &[]).unwrap();
        }
    }

    #[test]
    fn udev_db_names_only_the_given_nodes() {
        let t = tempfile::tempdir().unwrap();
        // /dev/null stands in for a node (it is c1:3).
        let pads = vec![ReadyPad {
            index: 0,
            name: "Quiver Lab Pad t".into(),
            phys: "quiver-lab/t/0".into(),
            nodes: uinput::Nodes { sysname: "input99".into(), event: "/dev/null".into(), js: None },
            checks: vec![],
        }];
        write_udev_db(&t.path().join("udev"), &pads).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(t.path().join("udev/data")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["+input:input99", "c1:3"]);
        let text = std::fs::read_to_string(t.path().join("udev/data/c1:3")).unwrap();
        assert!(text.contains("E:ID_INPUT_JOYSTICK=1") && text.contains("E:ID_VENDOR_ID=045e"));
    }
}
