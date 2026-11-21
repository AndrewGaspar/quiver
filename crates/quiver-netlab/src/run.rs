//! `run`: a server workload and a client workload on either side of a shaped link, with a
//! scenario's timeline applied, and their stats merged into a [`Report`].
//!
//! Each workload is `sh -c '<cmd>'`, run as the person in its side's network namespace with:
//!
//! | variable | value |
//! |---|---|
//! | `QUIVER_NETLAB_SIDE` | `server` or `client` |
//! | `QUIVER_NETLAB_SERVER_ADDR`, `QUIVER_NETLAB_CLIENT_ADDR` | 10.77.0.2 and 10.77.1.2 (the client's changes on a roam) |
//! | `QUIVER_NETLAB_SERVER_ADDR6`, `QUIVER_NETLAB_CLIENT_ADDR6` | fd77::2 and fd77:0:1::2 |
//! | `QUIVER_NETLAB_STATS`, `QUIVER_NETLAB_STATS_FD` | this side's stats file, also open on fd 3 ([`crate::stats`]) |
//! | `QUIVER_NETLAB_DURATION` | the run's length in seconds |
//! | `QUIVER_NETLAB_OUT` | the run's output directory |
//!
//! Output goes to `<out>/server.log` and `<out>/client.log`. The server starts first; the
//! client 300 ms later. The run ends at the scenario's duration, when the client exits, or
//! on SIGINT/SIGTERM; the workloads then get SIGTERM, and SIGKILL two seconds later.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use quiver_stream_proto::now_micros;
use serde::{Deserialize, Serialize};

use crate::flood::Flood;
use crate::link::{Shaper, WanderState};
use crate::ns::{ClientAddrs, Dir, Lab, SERVER_V4, SERVER_V6, Side};
use crate::probe::PathProbe;
use crate::report::Report;
use crate::scenario::{FloodProto, Op, Scenario};
use crate::shape::LinkShape;
use crate::stats;

/// What to run.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub label: String,
    pub scenario: Scenario,
    pub server: Option<String>,
    pub client: Option<String>,
    pub out_dir: PathBuf,
    /// Packets a second for the netlab's own round-trip probe (0 turns it off).
    pub path_probe_pps: u32,
    /// Extra environment for both workloads.
    pub env: Vec<(String, String)>,
}

/// The shape in force from `t_ms` on, one direction, as the report draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub t_ms: f64,
    pub dir: Dir,
    pub rate_mbit: f64,
    pub delay_ms: f64,
    pub jitter_ms: f64,
    pub loss_pct: f64,
    pub queue_ms: f64,
}

impl Applied {
    fn of(t_ms: f64, dir: Dir, s: &LinkShape) -> Applied {
        Applied {
            t_ms,
            dir,
            rate_mbit: s.rate_mbit,
            delay_ms: s.delay_ms,
            jitter_ms: s.jitter_ms,
            loss_pct: s.mean_loss_pct(),
            queue_ms: s.full_queue_ms(),
        }
    }
}

/// The router's counters for one direction at `t_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinkSample {
    pub t_ms: f64,
    pub dir: Dir,
    pub sent_bytes: u64,
    pub sent_packets: u64,
    pub drops: u64,
    pub queue_drops: u64,
    pub queue_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FloodRecord {
    pub id: usize,
    pub dir: Dir,
    pub proto: FloodProto,
    pub rate_mbit: Option<f64>,
    pub start_ms: f64,
    pub stop_ms: f64,
    pub delivered_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoamRecord {
    pub t_ms: f64,
    pub to: ClientAddrs,
}

/// How a workload ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadEnd {
    pub command: String,
    /// The exit code, or None if a signal ended it (the run's own SIGTERM included).
    pub code: Option<i32>,
    /// It exited by itself before the run ended.
    pub exited_early: bool,
    pub log: PathBuf,
}

/// Everything a run recorded, before it is summarised.
#[derive(Debug, Clone, Default)]
pub struct Recording {
    pub start_us: u64,
    pub end_ms: f64,
    pub applied: Vec<Applied>,
    pub samples: Vec<LinkSample>,
    pub floods: Vec<FloodRecord>,
    pub roams: Vec<RoamRecord>,
    pub server: Option<WorkloadEnd>,
    pub client: Option<WorkloadEnd>,
    pub path: crate::probe::PathSamples,
}

fn spawn_side(lab: &Lab, side: Side, cmd: &str, spec: &RunSpec, addrs: ClientAddrs) -> Result<(Child, PathBuf)> {
    let log_path = spec.out_dir.join(format!("{}.log", side.name()));
    let stats_path = spec.out_dir.join(format!("{}.stats.jsonl", side.name()));
    let log = std::fs::File::create(&log_path)?;
    let stats_file = std::fs::OpenOptions::new().create(true).append(true).open(&stats_path)?;
    let mut c = Command::new("sh");
    c.args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env("QUIVER_NETLAB_SIDE", side.name())
        .env("QUIVER_NETLAB_SERVER_ADDR", SERVER_V4.to_string())
        .env("QUIVER_NETLAB_CLIENT_ADDR", addrs.v4.to_string())
        .env("QUIVER_NETLAB_SERVER_ADDR6", SERVER_V6.to_string())
        .env("QUIVER_NETLAB_CLIENT_ADDR6", addrs.v6.to_string())
        .env(stats::ENV_FILE, &stats_path)
        .env(stats::ENV_FD, stats::STATS_FD.to_string())
        .env("QUIVER_NETLAB_DURATION", spec.scenario.duration_s.to_string())
        .env("QUIVER_NETLAB_OUT", &spec.out_dir);
    for (k, v) in &spec.env {
        c.env(k, v);
    }
    // The file stays open until spawn returns.
    let child = lab.spawn_with_fd(side, c, Some((stats_file.as_raw_fd(), stats::STATS_FD)))?;
    drop(stats_file);
    Ok((child, log_path))
}

/// Runs `spec` in `lab` until its duration, the client's exit, or `stop`.
pub fn run(lab: &Lab, spec: &RunSpec, stop: &AtomicBool) -> Result<Report> {
    std::fs::create_dir_all(&spec.out_dir).with_context(|| format!("creating {}", spec.out_dir.display()))?;
    for side in ["server", "client"] {
        let _ = std::fs::remove_file(spec.out_dir.join(format!("{side}.stats.jsonl")));
    }
    let (steps, markers) = spec.scenario.timeline()?;
    let duration = Duration::from_millis(spec.scenario.duration_ms());
    let mut shaper = Shaper::new(lab);
    let mut rec = Recording::default();

    let t0 = Instant::now();
    rec.start_us = now_micros();
    let ms = |t: Instant| t.duration_since(t0).as_secs_f64() * 1000.0;
    let apply = |shaper: &mut Shaper, rec: &mut Recording, c: &LinkShape, s: &LinkShape| -> Result<()> {
        shaper.apply(Dir::ToClient, c)?;
        shaper.apply(Dir::ToServer, s)?;
        let t = ms(Instant::now());
        rec.applied.push(Applied::of(t, Dir::ToClient, c));
        rec.applied.push(Applied::of(t, Dir::ToServer, s));
        Ok(())
    };
    if let Op::Shape { to_client, to_server } = &steps[0].op {
        apply(&mut shaper, &mut rec, to_client, to_server)?;
    }
    let probe = if spec.path_probe_pps > 0 { Some(PathProbe::start(lab, 7999, spec.path_probe_pps)?) } else { None };

    let mut server = match &spec.server {
        Some(cmd) => Some(spawn_side(lab, Side::Server, cmd, spec, lab.client_addrs())?),
        None => None,
    };
    if server.is_some() {
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut client = match &spec.client {
        Some(cmd) => Some(spawn_side(lab, Side::Client, cmd, spec, lab.client_addrs())?),
        None => None,
    };

    let mut wander = WanderState::new(&spec.scenario.base, 0x5eed);
    let mut floods: HashMap<usize, (Flood, FloodRecord)> = HashMap::new();
    let mut next_step = 1;
    let mut next_sample = t0;
    let mut server_end: Option<(i32, bool)> = None;
    let mut client_end: Option<(i32, bool)> = None;
    loop {
        let now = Instant::now();
        if now.duration_since(t0) >= duration || stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some((c, _)) = &mut server
            && server_end.is_none()
            && let Some(st) = c.try_wait()?
        {
            server_end = Some((st.code().unwrap_or(-1), true));
        }
        if let Some((c, _)) = &mut client
            && let Some(st) = c.try_wait()?
        {
            client_end = Some((st.code().unwrap_or(-1), true));
            break;
        }
        let el_ms = now.duration_since(t0).as_millis() as u64;
        while next_step < steps.len() && steps[next_step].t_ms <= el_ms {
            match &steps[next_step].op {
                Op::Shape { to_client, to_server } => apply(&mut shaper, &mut rec, to_client, to_server)?,
                Op::Roam => {
                    let to = lab.roam()?;
                    rec.roams.push(RoamRecord { t_ms: ms(Instant::now()), to });
                }
                Op::FloodStart { id, dir, proto, rate_mbit } => {
                    let f = Flood::start(lab, *dir, *proto, *rate_mbit, 7100 + *id as u16)?;
                    let r = FloodRecord {
                        id: *id,
                        dir: *dir,
                        proto: *proto,
                        rate_mbit: *rate_mbit,
                        start_ms: ms(Instant::now()),
                        stop_ms: 0.0,
                        delivered_bytes: 0,
                    };
                    floods.insert(*id, (f, r));
                }
                Op::FloodStop { id } => {
                    if let Some((f, mut r)) = floods.remove(id) {
                        r.delivered_bytes = f.stop();
                        r.stop_ms = ms(Instant::now());
                        rec.floods.push(r);
                    }
                }
            }
            next_step += 1;
        }
        for (dir, _) in wander.tick(&mut shaper)? {
            if let Some(s) = shaper.current(dir) {
                rec.applied.push(Applied::of(ms(Instant::now()), dir, s));
            }
        }
        if now >= next_sample {
            next_sample += Duration::from_millis(250);
            for dir in Dir::BOTH {
                if let Ok(c) = shaper.counters(dir) {
                    rec.samples.push(LinkSample {
                        t_ms: ms(Instant::now()),
                        dir,
                        sent_bytes: c.sent_bytes,
                        sent_packets: c.sent_packets,
                        drops: c.drops,
                        queue_drops: c.queue_drops,
                        queue_bytes: c.queue_bytes,
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    rec.end_ms = ms(Instant::now());
    for (_, (f, mut r)) in floods.drain() {
        r.delivered_bytes = f.stop();
        r.stop_ms = rec.end_ms;
        rec.floods.push(r);
    }
    rec.floods.sort_by_key(|f| f.id);
    lab.stop_workloads(Duration::from_secs(2));
    let finish = |w: Option<(Child, PathBuf)>, end: Option<(i32, bool)>, cmd: &Option<String>| {
        w.map(|(mut c, log)| {
            let st = c.wait().ok();
            let (code, early) = match end {
                Some((code, early)) => (Some(code), early),
                None => (st.and_then(|s| s.code()), false),
            };
            WorkloadEnd { command: cmd.clone().unwrap_or_default(), code, exited_early: early, log }
        })
    };
    rec.server = finish(server, server_end, &spec.server);
    rec.client = finish(client, client_end, &spec.client);
    if let Some(p) = probe {
        rec.path = p.stop();
    }
    let (server_stats, bad_s) = stats::read(&spec.out_dir.join("server.stats.jsonl"));
    let (client_stats, bad_c) = stats::read(&spec.out_dir.join("client.stats.jsonl"));
    let report = Report::build(spec, &markers, rec, server_stats, client_stats, bad_s + bad_c);
    Ok(report)
}
