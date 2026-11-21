//! Background jobs: `quiver lab … --bg` runs the command detached and prints a job id;
//! `quiver lab job wait <id>` blocks until it ends (printing its output, exiting with its
//! code), so a long run never holds an agent's tool call. Jobs live in `<lab>/jobs/<id>/`:
//! `job.json` (the command, when, its pid; then its exit), `out.log`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quiver_core::lab::Lab;
use serde_json::json;

#[derive(Subcommand)]
pub enum JobAction {
    List,
    Show { id: String },
    Wait {
        id: String,
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
    /// (internal) Run a job's command and record how it ended.
    #[command(hide = true)]
    Run {
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

fn read(dir: &Path) -> serde_json::Value {
    std::fs::read(dir.join("job.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Start this process's command again, detached, without `--bg`.
pub fn start_background(lab: &Lab) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--bg").collect();
    let id = format!("j{}-{}", chrono::Local::now().format("%m%d-%H%M%S"), std::process::id() % 1000);
    let dir = lab.jobs().join(&id);
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::File::create(dir.join("out.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    // The job runs detached (its own session, no parents to look at): who started it goes along.
    let owner = quiver_core::lab::owner::Owner::current("cli", None);
    cmd.args(["lab", "job", "run", &id, "--"])
        .args(&args)
        .env("QUIVER_LAB_ROOT", &lab.root)
        .env(quiver_core::lab::owner::OWNER_HANDOFF, owner.handoff()).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start the job")?;
    println!("{id}");
    eprintln!("started in the background: quiver lab job wait {id}   (output: {})", dir.join("out.log").display());
    Ok(())
}

pub fn run(lab: &Lab, a: JobAction) -> Result<()> {
    match a {
        JobAction::Run { id, cmd } => {
            let dir = lab.jobs().join(&id);
            let started = chrono::Utc::now();
            let mut rec = json!({ "id": id, "cmd": cmd, "started": started, "pid": std::process::id() });
            quiver_core::lab::write_atomic(&dir.join("job.json"), &serde_json::to_vec_pretty(&rec)?)?;
            let st = std::process::Command::new(std::env::current_exe()?).args(&cmd).status();
            rec["ended"] = json!(chrono::Utc::now());
            rec["exit_code"] = json!(st.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1));
            if let Err(e) = &st {
                rec["error"] = json!(e.to_string());
            }
            quiver_core::lab::write_atomic(&dir.join("job.json"), &serde_json::to_vec_pretty(&rec)?)?;
        }
        JobAction::List => {
            let mut ids: Vec<String> = std::fs::read_dir(lab.jobs()).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            ids.sort();
            for id in ids {
                let j = read(&lab.jobs().join(&id));
                let state = match j.get("exit_code") {
                    Some(c) => format!("exit {c}"),
                    None if j.is_null() => "starting".into(),
                    None => "running".into(),
                };
                let cmd: Vec<&str> = j["cmd"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
                println!("{id:<20} {state:<10} {}", cmd.join(" "));
            }
        }
        JobAction::Show { id } => {
            let dir = lab.jobs().join(&id);
            println!("{}", serde_json::to_string_pretty(&read(&dir))?);
            print!("{}", std::fs::read_to_string(dir.join("out.log")).unwrap_or_default());
        }
        JobAction::Wait { id, timeout } => {
            let dir = lab.jobs().join(&id);
            if !dir.is_dir() {
                bail!("no job {id} (quiver lab job list)");
            }
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let j = read(&dir);
                if let Some(code) = j.get("exit_code").and_then(|c| c.as_i64()) {
                    print!("{}", std::fs::read_to_string(dir.join("out.log")).unwrap_or_default());
                    if code != 0 {
                        std::process::exit(code as i32);
                    }
                    return Ok(());
                }
                if Instant::now() > deadline {
                    bail!("job {id} still running after {timeout}s");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Ok(())
}
