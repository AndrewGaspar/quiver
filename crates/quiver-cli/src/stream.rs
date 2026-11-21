//! `quiver stream <game> [--machine M] [--target test-pattern]`: stream from a machine, as
//! the app's **Stream from <machine>** does. It probes this machine's caps
//! (`quiver-stream caps --json`), asks the machine's quiverd for a stream (`StartStream`:
//! fresh keys every time, or a resume of a kept session), and runs `quiver-stream connect`
//! full screen with the ticket on its stdin. `quiver stream --list` shows a machine's streams,
//! and `quiver stream --stop ID` ends one.
//!
//! The stream client is `QUIVER_STREAM_CLIENT` (a command line, for tests that run it in a
//! nested display), else `quiver-stream` beside this binary, else on `PATH`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use quiver_api::remote::{ClientCaps, StartStreamReply, StreamSession, StreamTarget};
use quiver_api::{Client, Request};

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum TargetArg {
    Game,
    TestPattern,
}

#[derive(Args, Debug)]
pub struct StreamArgs {
    /// The game (a ref such as `ubisoft:3539`, or an installed game's name). Not needed with
    /// `--target test-pattern`, `--list` or `--stop`.
    pub game: Option<String>,
    #[arg(long, value_enum, default_value = "game")]
    pub target: TargetArg,
    /// The mod profile to launch the game with.
    #[arg(long)]
    pub profile: Option<String>,
    /// A window rather than full screen.
    #[arg(long)]
    pub windowed: bool,
    /// Show the stats chip from the start.
    #[arg(long)]
    pub stats: bool,
    /// Stop after this many seconds.
    #[arg(long)]
    pub seconds: Option<f64>,
    /// Read the stamp from each frame and print latencies after N frames (test pattern).
    #[arg(long)]
    pub measure: Option<u64>,
    /// Use these caps (JSON file) instead of probing this machine.
    #[arg(long)]
    pub caps: Option<PathBuf>,
    /// The machine's streams.
    #[arg(long)]
    pub list: bool,
    /// End a stream (its id or game) on the machine.
    #[arg(long)]
    pub stop: Option<String>,
}

/// The stream client's command line.
pub fn client_command() -> Vec<String> {
    if let Ok(c) = std::env::var("QUIVER_STREAM_CLIENT")
        && !c.trim().is_empty()
    {
        return c.split_whitespace().map(str::to_owned).collect();
    }
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("quiver-stream"))).filter(|p| p.is_file());
    vec![beside.map(|p| p.display().to_string()).unwrap_or_else(|| "quiver-stream".into())]
}

/// `quiver-stream caps --json`.
pub fn probe_caps() -> Result<ClientCaps> {
    let c = client_command();
    let out = Command::new(&c[0]).args(&c[1..]).args(["caps", "--json"]).stderr(Stdio::inherit()).output().with_context(|| format!("run {} caps", c[0]))?;
    if !out.status.success() {
        bail!("{} caps failed ({})", c[0], out.status);
    }
    serde_json::from_slice(&out.stdout).context("quiver-stream caps printed something that isn't ClientCaps")
}

pub async fn run(client: &Client, a: StreamArgs) -> Result<()> {
    let machine = client.remote_machine().map(|m| m.name.clone()).unwrap_or_else(|| "this machine".into());
    if a.list {
        let v: Vec<StreamSession> = client.call(Request::Streams).await?;
        if v.is_empty() {
            println!("no streams on {machine}");
        }
        for s in v {
            println!("{}  {}  {} (port {}, pid {})", s.id, s.title, s.describe(), s.port, s.pid);
        }
        return Ok(());
    }
    if let Some(id) = a.stop {
        let () = client.call(Request::StopStream { id: id.clone() }).await?;
        println!("stopped stream {id} on {machine}");
        return Ok(());
    }
    let target = match a.target {
        TargetArg::TestPattern => StreamTarget::TestPattern,
        TargetArg::Game => {
            let g = a.game.as_deref().context("which game? (or --target test-pattern)")?;
            let r = crate::perf::game_ref(client, g).await?;
            StreamTarget::Game { game: r.to_string(), profile: a.profile.clone() }
        }
    };
    let caps = match &a.caps {
        Some(p) => serde_json::from_slice(&std::fs::read(p)?).with_context(|| format!("{} isn't ClientCaps", p.display()))?,
        None => probe_caps()?,
    };
    eprintln!("asking {machine} to stream ({}x{} @ {:.0} Hz)…", caps.display.width, caps.display.height, caps.display.refresh_mhz as f64 / 1000.0);
    let reply: StartStreamReply = client.call(Request::StartStream { target: target.clone(), prefs: Default::default(), caps }).await?;
    eprintln!("{} is streaming to {} ", reply.host_name, reply.ticket.addr);
    let status = connect(&reply, &target, &a)?;
    if !status.success() {
        bail!("the stream client exited with {status}");
    }
    Ok(())
}

/// Run the stream client with the ticket on its stdin.
pub fn connect(reply: &StartStreamReply, target: &StreamTarget, a: &StreamArgs) -> Result<std::process::ExitStatus> {
    let c = client_command();
    let mut cmd = Command::new(&c[0]);
    cmd.args(&c[1..]).arg("connect");
    if a.windowed {
        cmd.arg("--windowed");
    }
    if a.stats {
        cmd.arg("--stats");
    }
    if let Some(s) = a.seconds {
        cmd.arg("--seconds").arg(s.to_string());
    }
    if let Some(n) = a.measure {
        cmd.arg("--measure").arg(n.to_string());
    }
    let mut child = cmd.stdin(Stdio::piped()).spawn().with_context(|| format!("run {} connect", c[0]))?;
    let line = serde_json::json!({"ticket": reply.ticket, "host_name": reply.host_name, "target": target});
    let mut stdin = child.stdin.take().context("the client's stdin")?;
    writeln!(stdin, "{line}")?;
    drop(stdin);
    Ok(child.wait()?)
}
