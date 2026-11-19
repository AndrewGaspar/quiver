//! `quiver lab clock N pause|step [K]|resume|status` (design/LAB.md §8.2): pause and frame
//! stepping on the virtual clock, through the control page the hook DLL maps in the game's
//! process (`runs/r<N>/vclock.ctl`).
//!
//! A pause holds the game at the end of the Present in flight (after the real Present, so that
//! frame stays on screen); a step lets exactly K more Presents through, each advancing virtual
//! time by exactly 1/rate, then holds again; resume runs free. Waiting is on by default, so
//! `quiver lab clock N step 12 && quiver lab shot N` shoots the 12th frame.
//!
//! With Sight capturing (HDR instances), a hold also asks the layer to keep a copy of every
//! frame it presents (`capture.hold` in the session dir), so `quiver lab shot --hdr` works while
//! nothing presents.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use quiver_core::lab::Inst;
use quiver_core::lab::vclock::control::{Cmd, Control, Snapshot};
use quiver_perf::layout::{HDR_CAPTURE_HELD, HDR_CAPTURE_READY};

use super::{read_state, sight};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum What {
    /// Hold the game at the end of the frame in flight.
    Pause,
    /// Let K more frames through (default 1), then hold again.
    Step,
    /// Run free again.
    Resume,
    /// Paused or running, the frame number, virtual time, steps taken.
    Status,
}

#[derive(Args)]
pub struct ClockArgs {
    pub instance: String,
    pub what: What,
    /// For step: how many frames (default 1).
    pub count: Option<u64>,
    /// Return once the game has the command, without waiting for it to hold.
    #[arg(long)]
    pub no_wait: bool,
    /// Seconds to wait for the hold (step: for all K frames) before giving up.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
    /// pause/step: the game resumes by itself after being held this many seconds (a safety net
    /// for a controller that may not come back). Default: held until resumed.
    #[arg(long)]
    pub auto_resume: Option<f64>,
    #[arg(long)]
    pub json: bool,
}

/// The current run's folder (and that it is running).
fn run_dir(inst: &Inst) -> Result<(u32, PathBuf)> {
    let st = read_state(inst);
    let Some(r) = st.run.as_ref() else { bail!("{} has no run (quiver lab launch {} --clock virtual:62.5)", inst.name, inst.name) };
    Ok((r.id, inst.run_dir(r.id)))
}

fn exited(inst: &Inst, run: u32) -> Option<String> {
    let st = read_state(inst);
    match st.run.as_ref() {
        Some(r) if r.id == run => r.exit.as_ref().map(|e| format!("{}: {}", e.how, e.detail)),
        Some(r) => Some(format!("a new run (r{}) replaced r{run}", r.id)),
        None => Some("the instance has no run".into()),
    }
}

/// The run's control page, or why there is none.
pub fn open(inst: &Inst, dir: &Path) -> Result<Control> {
    match Control::open(dir) {
        Ok(c) => Ok(c),
        Err(e) => match quiver_core::lab::vclock::Report::read(dir) {
            None => bail!("the virtual clock isn't on in this run: quiver lab launch {0} --clock virtual:62.5 (or quiver lab edit {0} clock=virtual:62.5)", inst.name),
            Some(r) if !r.control => bail!(
                "this run's clock has no control page ({e}): its DLL predates pause and step, or the game's process isn't {} (QUIVER_VCLOCK_EXE); relaunch with a current quiver",
                if r.proxy.is_empty() { "the one named" } else { "the one it was told" }
            ),
            Some(_) => bail!("{e}"),
        },
    }
}

/// The paused state of the run, if the clock is on and controllable (for `status`, `key`).
pub fn snapshot(inst: &Inst) -> Option<Snapshot> {
    let (_, dir) = run_dir(inst).ok()?;
    Control::open(&dir).ok().map(|c| c.snapshot())
}

pub fn describe(s: &Snapshot) -> String {
    let rate = s.rate();
    let state = match s.state.as_str() {
        "held" => "held (paused: virtual time stopped)",
        "holding" => "holding (asked to stop; the frame in flight hasn't presented)",
        _ => "running",
    };
    let mut out = format!("{state}: frame {}, virtual {:.6} s", s.frames, s.virtual_s);
    if rate > 0.0 {
        out.push_str(&format!(" at {rate} Hz"));
    }
    if s.state == "holding" {
        out.push_str(&format!(", stops at frame {}", s.hold_at));
    }
    out
}

fn counters(s: &Snapshot) -> String {
    format!(
        "holds started {}, step frames asked {}, presents while holding {}, blocked {} time(s), {:.1} s held, {} clock read(s) while held (frozen), leaked {} tick(s), auto-resumes {}; valves: creeping frames {}, escapes {}, overruns {}",
        s.pauses, s.steps, s.stepped, s.holds, s.held_s, s.held_reads, s.leaked, s.auto_resumes, s.creep_frames, s.escapes, s.overruns
    )
}

/// Sight's "keep the presented frame" switch, for HDR shots while held. Returns a note when it
/// matters and didn't take.
fn layer_hold(inst: &Inst, dir: &Path, on: bool, wait: bool) -> Option<String> {
    let m = inst.load().ok()?;
    if !m.sight {
        return None;
    }
    let sd = sight::session_dir(inst, Some(dir))?;
    let f = sd.join("capture.hold");
    if !on {
        let _ = std::fs::remove_file(&f);
        return None;
    }
    let st = sight::layer_status(&sd)?;
    if st.hdr_flags & HDR_CAPTURE_READY == 0 {
        return None;
    }
    if std::fs::write(&f, b"1\n").is_err() {
        return Some(format!("couldn't write {} (shot --hdr while paused won't work)", f.display()));
    }
    if !wait {
        return None;
    }
    // The layer's heartbeat (250 ms) sees it; the next present keeps a copy.
    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        if sight::layer_status(&sd).is_some_and(|s| s.hdr_flags & HDR_CAPTURE_HELD != 0) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Some("the Sight layer didn't start keeping frames (an older layer, or the game isn't presenting): quiver lab shot --hdr may fail while paused".into())
}

pub fn run(lab: &quiver_core::lab::Lab, a: ClockArgs) -> Result<()> {
    let (inst, _) = super::inst_of(lab, &a.instance)?;
    let (run, dir) = run_dir(&inst)?;
    // Right after `quiver lab launch`, the game's process may not have mapped the page yet:
    // pause and step wait for it (so `launch` then `clock pause` holds at the first frame).
    let ctl = {
        let end = Instant::now() + Duration::from_secs(a.timeout);
        loop {
            match open(&inst, &dir) {
                Ok(c) => break c,
                Err(e) => {
                    // The launch placed the clock's DLL, and it hasn't reported without a page.
                    let clock_on = read_state(&inst).run.is_some_and(|r| r.id == run && r.notes.iter().any(|n| n.starts_with("virtual clock: ") && n.contains(" Hz, every frame")));
                    let starting = matches!(a.what, What::Pause | What::Step)
                        && clock_on
                        && exited(&inst, run).is_none()
                        && quiver_core::lab::vclock::Report::read(&dir).is_none_or(|r| r.control);
                    if !starting || Instant::now() > end {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    };
    if a.what == What::Status {
        let s = ctl.snapshot();
        if a.json {
            return super::print_json(&serde_json::json!({ "run": run, "clock": s }));
        }
        println!("{} r{run}: {}", inst.name, describe(&s));
        println!("  {}", counters(&s));
        return Ok(());
    }
    if let Some(why) = exited(&inst, run) {
        bail!("the game isn't running ({why})");
    }
    let mut notes = Vec::new();
    let (cmd, arg) = match a.what {
        What::Pause => (Cmd::Pause, 0),
        What::Step => (Cmd::Step, a.count.unwrap_or(1).max(1)),
        _ => (Cmd::Resume, 0),
    };
    if a.count.is_some() && cmd != Cmd::Step {
        bail!("only step takes a count");
    }
    // HDR captures while held: the layer keeps frames from before the hold starts.
    if cmd != Cmd::Resume {
        let was_held = ctl.snapshot().held();
        notes.extend(layer_hold(&inst, &dir, true, !was_held));
    }
    let timeout_ms = a.auto_resume.map(|s| (s * 1000.0).round().max(1.0) as u64).unwrap_or(0);
    let before = ctl.snapshot();
    let seq = ctl.send(cmd, arg, timeout_ms);
    // The DLL applies it within 50 ms (its status thread), or at once while held.
    let end = Instant::now() + Duration::from_secs(10);
    while !ctl.acked(seq) {
        if let Some(why) = exited(&inst, run) {
            bail!("the game exited ({why})");
        }
        if Instant::now() > end {
            bail!("the game's process didn't take the command within 10 s (hung, or not the process with the control page); quiver lab status {}", inst.name);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let acked = ctl.snapshot();
    if cmd == Cmd::Resume {
        layer_hold(&inst, &dir, false, false);
    }
    let mut s = acked.clone();
    if cmd != Cmd::Resume && !a.no_wait {
        let target = acked.ack_hold_at;
        let end = Instant::now() + Duration::from_secs(a.timeout);
        let mut last_check = Instant::now();
        loop {
            s = ctl.snapshot();
            if s.held() && s.frames >= target {
                break;
            }
            if s.running() && s.ack_seq == seq {
                // Released by its own timeout (or a resume from someone else).
                bail!("the hold ended before frame {target} (auto-resume or another controller): {}", describe(&s));
            }
            if last_check.elapsed() > Duration::from_millis(250) {
                last_check = Instant::now();
                if let Some(why) = exited(&inst, run) {
                    bail!("the game exited while {} ({why}); last: {}", if cmd == Cmd::Step { "stepping" } else { "pausing" }, describe(&s));
                }
            }
            if Instant::now() > end {
                bail!(
                    "not held at frame {target} after {} s ({}): is the game presenting? (a loading screen without presents holds at its next Present)",
                    a.timeout,
                    describe(&s)
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    if a.json {
        return super::print_json(&serde_json::json!({ "run": run, "command": cmd, "count": arg, "clock": s, "before": before, "notes": notes }));
    }
    let rate = s.rate();
    match cmd {
        Cmd::Pause if a.no_wait => println!("pause sent: holds at frame {} ({})", acked.ack_hold_at, describe(&s)),
        Cmd::Pause => println!("paused at frame {}, virtual {:.6} s (that frame is on screen; nothing moves until step or resume)", s.frames, s.virtual_s),
        Cmd::Step if a.no_wait => println!("step {arg} sent: holds at frame {}", acked.ack_hold_at),
        Cmd::Step => {
            let from = acked.ack_frames;
            let dv = s.virtual_s - before.virtual_s;
            println!(
                "stepped to frame {}, virtual {:.6} s (+{} frame(s), +{:.6} s{})",
                s.frames,
                s.virtual_s,
                s.frames.saturating_sub(from.max(before.frames)),
                dv,
                if rate > 0.0 && before.held() { format!("; {arg}/{rate} = {:.6} s", arg as f64 / rate) } else { String::new() }
            );
        }
        _ => println!("running free from frame {}, virtual {:.6} s", s.frames, s.virtual_s),
    }
    for n in notes {
        println!("  note: {n}");
    }
    if s.leaked != 0 {
        println!("  WARNING: virtual time moved {} tick(s) while held (quiver lab logs {})", s.leaked, inst.name);
    }
    Ok(())
}
