//! `quiver steam-run %command%`: a Steam launch option that runs a Steam copy of a Ubisoft
//! game against Quiver instead of Ubisoft Connect (see `ubi_core::steamrun`).
//!
//! Steam runs this with its Proton command line and compat environment. It finds the game
//! in the command, gets a launch context from quiverd (its existing login: no new Ubisoft
//! session), and execs Steam's own command with the game swapped for
//! `ubi-bridge.exe --run <context> -- <game.exe> <args>`. Files go to
//! `<data>/steam-run/<appid>/` (under the home folder, which Steam's runtime container
//! sees); the prefix is left alone unless `--sdk-dir` is given.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ubi_api::{Client, Request};
use ubi_core::launcher;
use ubi_core::steamrun::{self, ProtonCommand, SteamEnv};
use ubi_ipc::run::BridgeArgs;

#[derive(clap::Args)]
pub struct SteamRunArgs {
    /// Ubisoft product id (default: from the game's Steam install script or its prefix).
    #[arg(long)]
    product: Option<u32>,
    /// Account to launch as (username or user id); default: the game's launch account.
    #[arg(long)]
    account: Option<String>,
    /// Print what would run, without contacting quiverd or starting anything.
    #[arg(long)]
    dry_run: bool,
    /// Don't disable Connect's executables for this launch (WINEDLLOVERRIDES).
    #[arg(long)]
    no_guard: bool,
    /// Point the prefix's Ubisoft InstallDir at this folder of SDK DLLs (a backup of
    /// system.reg is kept as system.reg.quiver-bak). Only needed if Connect's own DLLs
    /// are missing from the prefix.
    #[arg(long)]
    sdk_dir: Option<PathBuf>,
    /// Steam's command (`%command%`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    command: Vec<String>,
}

struct Log(Option<std::fs::File>);

impl Log {
    fn line(&mut self, s: impl AsRef<str>) {
        let s = s.as_ref();
        eprintln!("quiver steam-run: {s}");
        if let Some(f) = &mut self.0 {
            let _ = writeln!(f, "{} {s}", chrono::Local::now().format("%F %T"));
        }
    }
}

fn shell_quote(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// (cmdline, environ) of every process we can read.
fn processes() -> Vec<(Vec<u8>, Vec<u8>)> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        .filter_map(|e| Some((std::fs::read(e.path().join("cmdline")).ok()?, std::fs::read(e.path().join("environ")).ok()?)))
        .collect()
}

fn find_bridge() -> Result<PathBuf> {
    if let Ok(p) = launcher::Config::load().and_then(|c| c.bridge_exe()) {
        return Ok(p);
    }
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let candidates = launcher::bridge_candidates(dir.as_deref());
    candidates.iter().find(|p| p.is_file()).cloned().with_context(|| {
        format!("ubi-bridge.exe not found (looked for {})", candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
    })
}

/// Start quiverd through systemd if it isn't running. Started by the client instead, it
/// would be a descendant of Steam's reaper, and Steam would show the game running (and
/// kill quiverd on Stop) for as long as the daemon lives.
fn ensure_daemon_outside_steam(log: &mut Log) {
    let up = || std::os::unix::net::UnixStream::connect(ubi_api::socket_path()).is_ok();
    if up() {
        return;
    }
    let started = std::process::Command::new("systemctl")
        .args(["--user", "start", "quiverd"])
        .status()
        .is_ok_and(|s| s.success());
    for _ in 0..if started { 50 } else { 0 } {
        if up() {
            log.line("started quiverd (systemd user unit)");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    log.line("quiverd isn't running and the systemd unit didn't start it; starting it here (Steam will count it as part of the game until it exits)");
}

/// `--sdk-dir`: point InstallDir at `dir`, with no Wine process running in the prefix.
fn redirect_sdk(env: &SteamEnv, dir: &Path, log: &mut Log) -> Result<()> {
    if !dir.join("uplay_r164.dll").is_file() && !dir.join("uplay_r264.dll").is_file() {
        bail!("{} has no uplay_r164.dll or uplay_r264.dll", dir.display());
    }
    let compat = env.compat_data.to_string_lossy().into_owned();
    let busy = processes().iter().any(|(_, e)| {
        e.split(|&b| b == 0).any(|kv| kv == format!("STEAM_COMPAT_DATA_PATH={compat}").as_bytes())
    });
    if busy {
        bail!("something is still running in this game's prefix; can't edit its registry now");
    }
    let reg = env.prefix().join("system.reg");
    let text = std::fs::read_to_string(&reg).with_context(|| format!("read {}", reg.display()))?;
    let value = format!("{}\\", steamrun::z_path(dir));
    if steamrun::reg_value(&text, steamrun::LAUNCHER_KEY, "InstallDir").as_deref() == Some(value.as_str()) {
        return Ok(());
    }
    let backup = env.prefix().join("system.reg.quiver-bak");
    if !backup.exists() {
        std::fs::copy(&reg, &backup)?;
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let tmp = env.prefix().join("system.reg.quiver-tmp");
    std::fs::write(&tmp, steamrun::set_reg_value(&text, steamrun::LAUNCHER_KEY, "InstallDir", &value, stamp))?;
    std::fs::rename(&tmp, &reg)?;
    log.line(format!("InstallDir -> {value} (original kept in {})", backup.display()));
    Ok(())
}

pub async fn run(source: &str, a: SteamRunArgs) -> Result<()> {
    let env = SteamEnv::from_vars(|k| std::env::var(k).ok())?;
    let run_dir = launcher::data_dir().join("steam-run").join(env.app_id.to_string());
    let mut log = Log(None);
    if !a.dry_run {
        std::fs::create_dir_all(&run_dir)?;
        log.0 = std::fs::OpenOptions::new().create(true).append(true).open(run_dir.join("steam-run.log")).ok();
    }
    log.line(format!("app {} prefix {} command: {}", env.app_id, env.prefix().display(), a.command.join(" ")));

    let pc = ProtonCommand::find(&a.command)?;
    let script = env.installscript().and_then(|p| std::fs::read_to_string(p).ok());
    let system_reg = std::fs::read_to_string(env.prefix().join("system.reg")).ok();
    let (product_id, how) = steamrun::resolve_product(env.app_id, a.product, script.as_deref(), system_reg.as_deref())?;
    log.line(format!("Ubisoft product {product_id} ({how:?})"));

    if let Some(dir) = &a.sdk_dir {
        if a.dry_run {
            log.line(format!("would point InstallDir at {}", dir.display()));
        } else {
            redirect_sdk(&env, dir, &mut log)?;
        }
    }
    let system_reg = std::fs::read_to_string(env.prefix().join("system.reg")).ok();
    let sdk = steamrun::check_sdk(&env.prefix(), system_reg.as_deref().unwrap_or_default());
    match (&sdk.install_dir, sdk.has_r1 || sdk.has_r2) {
        (Some(d), true) => log.line(format!("SDK DLLs in {d} (r1: {}, r2: {})", sdk.has_r1, sdk.has_r2)),
        (d, _) => log.line(format!(
            "warning: no Ubisoft SDK DLLs at InstallDir {d:?}; the game won't find the SDK (see --sdk-dir)"
        )),
    }
    let connect = steamrun::connect_in_prefix(processes().iter().map(|(c, e)| (c.as_slice(), e.as_slice())), &env.compat_data);
    if !connect.is_empty() {
        bail!(
            "Ubisoft Connect is running in this game's prefix ({}); quit it first (its pipes and login would clash with Quiver's)",
            connect.join("; ")
        );
    }

    let game_win = steamrun::prefix_win_path(&env.prefix(), Path::new(pc.exe(&a.command)));
    let ctx_path = run_dir.join("context.json");
    let log_path = run_dir.join("bridge.log");
    let bridge_dst = run_dir.join("ubi-bridge.exe");
    let bridge_args = BridgeArgs::run_command(
        &steamrun::z_path(&ctx_path),
        Some(&steamrun::z_path(&log_path)),
        &game_win,
        pc.game_args(&a.command),
    );
    let argv = pc.rewrite(&a.command, &bridge_dst.to_string_lossy(), &bridge_args);
    let overrides = (!a.no_guard).then(|| steamrun::dll_overrides_without_connect(std::env::var("WINEDLLOVERRIDES").ok().as_deref()));

    if a.dry_run {
        println!("product: {product_id} ({how:?})");
        if let Some(o) = &overrides {
            println!("WINEDLLOVERRIDES={}", shell_quote(o));
        }
        println!("{}", argv.iter().map(|x| shell_quote(x)).collect::<Vec<_>>().join(" "));
        return Ok(());
    }

    // The bridge goes next to the context, under the home folder: Steam's runtime container
    // sees the home folder but not the host's /usr (where a package installs the bridge).
    std::fs::copy(find_bridge()?, &bridge_dst).context("copy ubi-bridge.exe")?;
    ensure_daemon_outside_steam(&mut log);
    let client = Client::connect(source).await?;
    let mut ctx: serde_json::Value = client
        .call(Request::LaunchContext { product_id, savegame_path: None, install_dir: None, account: a.account })
        .await
        .with_context(|| format!("launch context for product {product_id} from quiverd"))?;
    let account = ctx["account_id"].as_str().context("launch context has no account")?.to_owned();
    ctx["savegame_path"] = steamrun::savegame_path(sdk.install_dir.as_deref(), &account, product_id).into();
    let _ = std::fs::remove_file(&log_path);
    {
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&ctx_path)?;
        f.write_all(&serde_json::to_vec(&ctx)?)?;
    }
    log.line(format!("saves: {}", ctx["savegame_path"].as_str().unwrap_or_default()));
    log.line(format!("exec: {}", argv.iter().map(|x| shell_quote(x)).collect::<Vec<_>>().join(" ")));

    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(o) = overrides {
        log.line(format!("WINEDLLOVERRIDES={o}"));
        cmd.env("WINEDLLOVERRIDES", o);
    }
    drop(client);
    let err = cmd.exec();
    bail!("exec {}: {err}", argv[0])
}
