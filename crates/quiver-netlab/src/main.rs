//! `quiver-netlab`: Quiver's network-conditions lab. See the crate's README.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

use quiver_netlab::link::WanderState;
use quiver_netlab::matrix::{MatrixItem, MatrixSpec, run_matrix};
use quiver_netlab::ns::{self, LabInfo, SERVER_V4, SERVER_V6};
use quiver_netlab::run::RunSpec;
use quiver_netlab::shape::{LinkPatch, Loss};
use quiver_netlab::{Dir, Lab, Profile, Scenario, Shaper, Side, fake, probe, profile, scenario};

#[derive(Parser)]
#[command(
    name = "quiver-netlab",
    about = "Quiver's network-conditions lab: rootless namespaces shaped by the kernel's netem, tbf, fq_codel and cake"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The built-in profiles and scenarios.
    List,
    /// Measure what a profile's shaping delivers (RTT, one-way delays, loss, rate, loaded
    /// RTT) and check it against a model of netem. `--profile all` checks every one.
    Verify {
        #[arg(long, default_value = "all")]
        profile: String,
        /// Write the measurements as JSON too.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Run a server and a client workload across a shaped link and write a report.
    Run {
        #[arg(long, conflicts_with = "scenario", required_unless_present = "scenario")]
        profile: Option<String>,
        #[arg(long)]
        scenario: Option<String>,
        /// The server-side command (`sh -c`).
        #[arg(long)]
        server: Option<String>,
        /// The client-side command (`sh -c`).
        #[arg(long)]
        client: Option<String>,
        /// Use the built-in stand-in stream (fake-stream serve --aimd / play) for whichever
        /// of --server and --client is not given.
        #[arg(long)]
        fake: bool,
        /// Seconds (a profile's default is 30; a scenario has its own).
        #[arg(long)]
        duration: Option<f64>,
        /// The output directory (logs, stats, report). Default: netlab-out/<name>.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Where report.json goes (report.html beside it). Default: <out>/report.json.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Packets a second for the netlab's own RTT probe; 0 turns it off.
        #[arg(long, default_value_t = 20)]
        probe_pps: u32,
    },
    /// Run every profile and/or scenario for the same workloads and write a summary table.
    Matrix {
        /// `all`, `none`, or a comma-separated list.
        #[arg(long, default_value = "all")]
        profiles: String,
        #[arg(long, default_value = "none")]
        scenarios: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        client: Option<String>,
        #[arg(long)]
        fake: bool,
        /// Seconds per profile (scenarios keep their own unless given).
        #[arg(long, default_value_t = 20.0)]
        duration: f64,
        /// Override scenarios' durations too.
        #[arg(long)]
        scenario_duration: Option<f64>,
        #[arg(long, default_value = "netlab-out/matrix")]
        out: PathBuf,
        #[arg(long, default_value_t = 20)]
        probe_pps: u32,
    },
    /// A shell (or a command) on one side of a shaped lab, for poking around. While it runs,
    /// `quiver-netlab set` changes the shaping from another terminal.
    Shell {
        #[arg(long, default_value = "open")]
        profile: String,
        #[arg(long, value_enum, default_value_t = Side::Client)]
        side: Side,
        /// The command to run instead of $SHELL.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Change the shaping of a running `shell` lab.
    Set {
        /// The lab's pid (default: the only one running).
        #[arg(long)]
        lab: Option<u32>,
        #[arg(long)]
        profile: Option<String>,
        /// Patch the current shape, as TOML: e.g. 'to_client.rate_mbit = 20' or 'both.delay_ms = 50'.
        #[arg(long)]
        patch: Option<String>,
        /// Move the client to a new address.
        #[arg(long)]
        roam: bool,
        /// Drop everything both ways for this many ms.
        #[arg(long)]
        outage: Option<u64>,
        /// Print the current shapes.
        #[arg(long)]
        show: bool,
    },
    /// Run a command in a running lab's namespace, as you.
    Exec {
        #[arg(long)]
        lab: Option<u32>,
        #[arg(long, value_enum, default_value_t = Side::Client)]
        side: Side,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// The labs running now.
    Labs,
    /// The stand-in stream: a UDP "video" sender and receiver.
    FakeStream {
        #[command(subcommand)]
        cmd: FakeCmd,
    },
}

#[derive(Subcommand)]
enum FakeCmd {
    /// The sender, for the server side.
    Serve(fake::ServeOpts),
    /// The receiver, for the client side.
    Play(fake::PlayOpts),
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn catch_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("quiver-netlab: {e:#}");
        std::process::exit(1);
    }
}

fn self_exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "quiver-netlab".into())
}

/// The stand-in commands, sized to the link: AIMD up to a little over the link's rate.
fn fake_commands(sc: &Scenario) -> (String, String) {
    let exe = self_exe();
    let top = sc.base.to_client.wander.map_or(sc.base.to_client.rate_mbit, |w| w.max_mbit).max(1.0);
    let max = (top * 1.2).min(150.0);
    let start = (top * 0.25).clamp(2.0, 20.0);
    (format!("'{exe}' fake-stream serve --aimd --mbit {start:.1} --max-mbit {max:.1}"), format!("'{exe}' fake-stream play"))
}

fn names(arg: &str, all: &[(&str, &str)]) -> Vec<String> {
    match arg {
        "all" => all.iter().map(|(n, _)| n.to_string()).collect(),
        "none" | "" => vec![],
        s => s.split(',').map(|x| x.trim().to_string()).collect(),
    }
}

fn real_main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::List => {
            println!("profiles:");
            for p in profile::builtin() {
                println!(
                    "  {:<18} rtt {:>5.1} ms  to client {:>6.0} Mbit/s  to server {:>5.0} Mbit/s  loss {:.2}%",
                    p.name,
                    p.base_rtt_ms(),
                    p.to_client.rate_mbit,
                    p.to_server.rate_mbit,
                    p.to_client.mean_loss_pct()
                );
                println!("  {:<18} {}", "", p.summary);
            }
            println!("\nscenarios:");
            for s in scenario::builtin() {
                println!("  {:<18} {:>4.0} s on {}", s.name, s.duration_s, s.base.name);
                println!("  {:<18} {}", "", s.summary);
            }
        }
        Cmd::Verify { profile: which, json } => {
            catch_signals();
            let lab = Lab::new()?;
            let mut all = Vec::new();
            let mut failed = 0;
            for name in names(&which, profile::BUILTIN) {
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
                let p = profile::load(&name)?;
                let v = probe::verify(&lab, &p, &probe::ProbeOptions::default())?;
                print!("{}", v.render());
                if !v.ok() {
                    failed += 1;
                }
                all.push(v);
            }
            if let Some(path) = json {
                std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
                println!("wrote {}", path.display());
            }
            if failed > 0 {
                bail!("{failed} profile(s) failed verification");
            }
        }
        Cmd::Run { profile: p, scenario: s, server, client, fake, duration, out, report, probe_pps } => {
            catch_signals();
            let mut sc = match (&p, &s) {
                (Some(p), _) => Scenario::steady(&profile::load(p)?, duration.unwrap_or(30.0)),
                (None, Some(s)) => scenario::load(s)?,
                _ => unreachable!(),
            };
            if let Some(d) = duration {
                sc.duration_s = d;
            }
            let (fs, fc) = fake_commands(&sc);
            let server = server.or(fake.then_some(fs));
            let client = client.or(fake.then_some(fc));
            let out = out.unwrap_or_else(|| PathBuf::from("netlab-out").join(&sc.name));
            let lab = Lab::new()?;
            eprintln!(
                "netlab: {} for {:.0} s; server {SERVER_V4} / {SERVER_V6}, client {} (logs in {})",
                sc.name,
                sc.duration_s,
                lab.client_addrs().v4,
                out.display()
            );
            let spec = RunSpec {
                label: sc.name.clone(),
                scenario: sc,
                server,
                client,
                out_dir: out.clone(),
                path_probe_pps: probe_pps,
                env: vec![],
            };
            let r = quiver_netlab::run::run(&lab, &spec, &STOP)?;
            let json = report.unwrap_or_else(|| out.join("report.json"));
            let html = r.write(&json)?;
            let s = &r.summary;
            println!(
                "latency p50 {} ms, p99 {} ms; gaps/min {}; bitrate {} of {:.0} Mbit/s; path RTT p50 {} ms",
                fmt(s.latency_p50_ms),
                fmt(s.latency_p99_ms),
                fmt(s.gaps_per_min),
                fmt(s.mean_bitrate_mbit),
                s.mean_capacity_mbit,
                fmt(s.path_rtt_p50_ms)
            );
            println!("report: {}\n        {}", json.display(), html.display());
        }
        Cmd::Matrix { profiles, scenarios, server, client, fake, duration, scenario_duration, out, probe_pps } => {
            catch_signals();
            let mut items = Vec::new();
            for n in names(&profiles, profile::BUILTIN) {
                items.push(("profile".to_string(), Scenario::steady(&profile::load(&n)?, duration)));
            }
            for n in names(&scenarios, scenario::BUILTIN) {
                let mut s = scenario::load(&n)?;
                if let Some(d) = scenario_duration {
                    s.duration_s = d;
                }
                items.push(("scenario".to_string(), s));
            }
            if items.is_empty() {
                bail!("nothing to run: give --profiles and/or --scenarios");
            }
            if server.is_none() && client.is_none() && !fake {
                bail!("give --server and --client, or --fake for the stand-in stream");
            }
            // With --fake each item gets the stand-in sized to its own link.
            let items = items
                .into_iter()
                .map(|(kind, sc)| {
                    let (fs, fc) = fake_commands(&sc);
                    MatrixItem {
                        kind,
                        server: server.clone().or(fake.then_some(fs)),
                        client: client.clone().or(fake.then_some(fc)),
                        scenario: sc,
                    }
                })
                .collect();
            let spec = MatrixSpec { items, out_dir: out.clone(), path_probe_pps: probe_pps };
            let rows = run_matrix(&spec, &STOP, |r| match &r.error {
                Some(e) => eprintln!("netlab: {} failed: {e}", r.name),
                None => eprintln!("netlab: {} done", r.name),
            })?;
            print!("{}", quiver_netlab::matrix::render_md(&rows));
            println!("summary: {}", out.join("summary.html").display());
        }
        Cmd::Shell { profile: name, side, command } => shell(&name, side, command)?,
        Cmd::Set { lab, profile: p, patch, roam, outage, show } => {
            let info = find_lab(lab)?;
            let path = info.control.ok_or_else(|| anyhow!("lab {} has no control socket", info.pid))?;
            let mut reqs = Vec::new();
            if let Some(p) = p {
                reqs.push(format!("profile {p}"));
            }
            if let Some(t) = patch {
                reqs.push(format!("patch {}", t.replace('\n', "; ")));
            }
            if roam {
                reqs.push("roam".into());
            }
            if let Some(ms) = outage {
                reqs.push(format!("outage {ms}"));
            }
            if show || reqs.is_empty() {
                reqs.push("show".into());
            }
            for r in reqs {
                println!("{}", ns::control_request(&path, &r)?);
            }
        }
        Cmd::Exec { lab, side, command } => {
            let info = find_lab(lab)?;
            let lab = Lab::attach(&info)?;
            let mut cmd = Command::new(&command[0]);
            cmd.args(&command[1..]);
            set_env(&mut cmd, &lab, side);
            lab.workload_command(side, &mut cmd, false, None);
            let st = cmd.status().with_context(|| format!("running {}", command[0]))?;
            std::process::exit(st.code().unwrap_or(1));
        }
        Cmd::Labs => {
            for l in ns::running_labs() {
                println!("{}  {}  client {}", l.pid, l.label, l.client.v4);
            }
        }
        Cmd::FakeStream { cmd } => {
            catch_signals();
            match cmd {
                FakeCmd::Serve(o) => fake::serve(&o, &STOP)?,
                FakeCmd::Play(o) => fake::play(&o, &STOP)?,
            }
        }
    }
    Ok(())
}

fn fmt(v: Option<f64>) -> String {
    v.map_or("—".into(), |x| format!("{x:.1}"))
}

fn set_env(cmd: &mut Command, lab: &Lab, side: Side) {
    cmd.env("QUIVER_NETLAB_SIDE", side.name())
        .env("QUIVER_NETLAB_SERVER_ADDR", SERVER_V4.to_string())
        .env("QUIVER_NETLAB_CLIENT_ADDR", lab.client_addrs().v4.to_string())
        .env("QUIVER_NETLAB_SERVER_ADDR6", SERVER_V6.to_string())
        .env("QUIVER_NETLAB_CLIENT_ADDR6", lab.client_addrs().v6.to_string());
}

fn find_lab(pid: Option<u32>) -> Result<LabInfo> {
    let labs = ns::running_labs();
    match pid {
        Some(p) => labs.into_iter().find(|l| l.pid == p).ok_or_else(|| anyhow!("no lab with pid {p}")),
        None => match labs.len() {
            0 => bail!("no lab is running (start one with `quiver-netlab shell`)"),
            1 => Ok(labs.into_iter().next().unwrap()),
            _ => bail!("several labs are running; pick one with --lab ({})", labs.iter().map(|l| l.pid.to_string()).collect::<Vec<_>>().join(", ")),
        },
    }
}

/// Holds a lab while a shell runs in it, serving `set` requests on a unix socket.
fn shell(name: &str, side: Side, command: Vec<String>) -> Result<()> {
    let p = profile::load(name)?;
    let lab = Lab::new()?;
    let dir = ns::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    let sock_path = dir.join(format!("{pid}.sock"));
    let info_path = dir.join(format!("{pid}.json"));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;
    let info = lab.info(&p.name, Some(sock_path.display().to_string()));
    std::fs::write(&info_path, serde_json::to_string_pretty(&info)?)?;
    let done = AtomicBool::new(false);
    // Shaped before the shell starts, so its first packet already sees the profile.
    let mut shaper = Shaper::new(&lab);
    shaper.apply_profile(&p)?;
    let result = std::thread::scope(|s| -> Result<i32> {
        let control = s.spawn(|| control_loop(&lab, shaper, &p, &listener, &done, &info_path));
        let mut cmd = match command.split_first() {
            Some((c, rest)) => {
                let mut cmd = Command::new(c);
                cmd.args(rest);
                cmd
            }
            None => Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())),
        };
        set_env(&mut cmd, &lab, side);
        cmd.env("QUIVER_NETLAB_LAB", pid.to_string());
        lab.workload_command(side, &mut cmd, false, None);
        eprintln!(
            "netlab: {} shell on the {} side ({}), lab {pid}; server {SERVER_V4}, client {}.\n\
             netlab: from another terminal: quiver-netlab set --profile <p> | --patch 'to_client.rate_mbit = 20' | --roam | --outage 500",
            p.name,
            side.name(),
            if side == Side::Client { "10.77.1.x" } else { "10.77.0.2" },
            lab.client_addrs().v4
        );
        // The shell has the terminal; Ctrl-C is for it, not for us.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        let status = cmd.status();
        done.store(true, Ordering::Relaxed);
        control.join().unwrap()?;
        Ok(status?.code().unwrap_or(1))
    });
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&info_path);
    std::process::exit(result?);
}

fn control_loop(
    lab: &Lab,
    mut shaper: Shaper,
    start: &Profile,
    listener: &UnixListener,
    done: &AtomicBool,
    info_path: &std::path::Path,
) -> Result<()> {
    let mut wander = WanderState::new(1);
    while !done.load(Ordering::Relaxed) {
        wander.tick(&mut shaper)?;
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line)?;
                let reply = match handle(lab, &mut shaper, line.trim()) {
                    Ok(r) => r,
                    Err(e) => format!("error: {e:#}"),
                };
                let mut w = &stream;
                let _ = w.write_all(reply.as_bytes());
                if line.starts_with("roam") {
                    let info = lab.info(&start.name, std::fs::read_to_string(info_path).ok().and_then(|t| {
                        serde_json::from_str::<LabInfo>(&t).ok().and_then(|i| i.control)
                    }));
                    let _ = std::fs::write(info_path, serde_json::to_string_pretty(&info)?);
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    Ok(())
}

fn describe(shaper: &Shaper) -> String {
    Dir::BOTH
        .iter()
        .filter_map(|&d| {
            shaper.current(d).map(|s| {
                format!(
                    "{}: delay {:.1} ms ± {:.1}, loss {:.2}%, rate {:.0} Mbit/s, queue {:.0} ms {:?}",
                    d.name(),
                    s.delay_ms,
                    s.jitter_ms,
                    s.mean_loss_pct(),
                    s.rate_mbit,
                    s.full_queue_ms(),
                    s.aqm
                )
            })
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn handle(lab: &Lab, shaper: &mut Shaper, req: &str) -> Result<String> {
    let (verb, arg) = req.split_once(' ').unwrap_or((req, ""));
    match verb {
        "show" => Ok(describe(shaper)),
        "profile" => {
            let p = profile::load(arg.trim())?;
            shaper.apply_profile(&p)?;
            Ok(format!("now {}: {}", p.name, describe(shaper)))
        }
        "patch" => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                #[serde(default)]
                both: LinkPatch,
                #[serde(default)]
                to_client: LinkPatch,
                #[serde(default)]
                to_server: LinkPatch,
            }
            let p: P = toml::from_str(&arg.replace("; ", "\n"))?;
            for (d, own) in [(Dir::ToClient, &p.to_client), (Dir::ToServer, &p.to_server)] {
                let cur = shaper.current(d).cloned().unwrap_or_default();
                shaper.apply(d, &cur.patched(&p.both).patched(own))?;
            }
            Ok(describe(shaper))
        }
        "roam" => {
            let a = lab.roam()?;
            Ok(format!("client is now {} / {}", a.v4, a.v6))
        }
        "outage" => {
            let ms: u64 = arg.trim().parse().context("outage <ms>")?;
            let before: Vec<_> = Dir::BOTH.iter().map(|&d| shaper.current(d).cloned().unwrap_or_default()).collect();
            for (i, &d) in Dir::BOTH.iter().enumerate() {
                let out = before[i].patched(&LinkPatch { loss: Some(Loss { pct: 100.0, burst: None }), ..Default::default() });
                shaper.apply(d, &out)?;
            }
            std::thread::sleep(Duration::from_millis(ms));
            for (i, &d) in Dir::BOTH.iter().enumerate() {
                shaper.apply(d, &before[i])?;
            }
            Ok(format!("{ms} ms outage done"))
        }
        _ => bail!("unknown request `{req}` (show, profile <p>, patch <toml>, roam, outage <ms>)"),
    }
}
