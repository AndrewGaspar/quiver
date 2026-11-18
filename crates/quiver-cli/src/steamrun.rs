//! `quiver steam-run %command%`: a Steam launch option that runs a Steam game with Quiver's
//! launch plan (see `quiver_core::steamrun`).
//!
//! **A Ubisoft game's Steam copy** (`--product`, or a product its files name): Steam runs
//! this with its Proton command line and compat environment. It finds the game in the
//! command, gets a launch context from quiverd (its existing login: no new Ubisoft session),
//! and execs Steam's own command with the game swapped for
//! `quiver-bridge.exe --run <context> -- <game.exe> <args>`. Files go to
//! `<data>/steam-run/<appid>/` (under the home folder, which Steam's runtime container
//! sees); the prefix is left alone unless `--sdk-dir` is given.
//!
//! **Any other Steam game** (`--app <appid>`, or no product found; design/STORE-STEAM.md §9):
//! nothing is swapped. It asks quiverd for `steam:<appid>`'s launch plan (settings, tweaks,
//! the HUD's session and Sight layer), registers itself as the game's local play session, and
//! execs Steam's command with the plan's wrappers in front, its arguments after the game's
//! and its environment set. The exec keeps the pid, so quiverd sees the launch end when
//! Steam's command does; Steam's process tree, Stop, overlay and Steam Input are as without
//! the hook. When quiverd can't be reached the game starts anyway, without the plan.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use quiver_api::{Client, Request};
use quiver_core::launcher;
use quiver_api::steamlib::SteamLaunchPlan;
use quiver_core::steamplan;
use quiver_core::steamrun::{self, ProtonCommand, SteamEnv};
use quiver_ipc::run::BridgeArgs;

#[derive(clap::Args)]
pub struct SteamRunArgs {
    /// Ubisoft product id (default: from the game's Steam install script or its prefix).
    #[arg(long, conflicts_with = "app")]
    product: Option<u32>,
    /// A Steam game that isn't a Ubisoft copy: its app id (default: `SteamAppId` from Steam).
    /// Nothing is swapped; the game's launch plan is applied to Steam's command.
    #[arg(long)]
    app: Option<u32>,
    /// Account to launch as (username or user id); default: the game's launch account.
    #[arg(long)]
    account: Option<String>,
    /// Print what would run, without contacting quiverd or starting anything.
    #[arg(long)]
    dry_run: bool,
    /// Don't disable Connect's executables for this launch (WINEDLLOVERRIDES).
    #[arg(long)]
    no_guard: bool,
    /// Don't apply the game's Quiver launch settings (env, DLL overrides, args, wrappers).
    #[arg(long)]
    no_settings: bool,
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
        format!("quiver-bridge.exe not found (looked for {})", candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
    })
}

/// Start quiverd through systemd if it isn't running. Started by the client instead, it
/// would be a descendant of Steam's reaper, and Steam would show the game running (and
/// kill quiverd on Stop) for as long as the daemon lives.
fn ensure_daemon_outside_steam(log: &mut Log, here: bool) {
    let up = || std::os::unix::net::UnixStream::connect(quiver_api::socket_path()).is_ok();
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
    if here {
        log.line("quiverd isn't running and the systemd unit didn't start it; starting it here (Steam will count it as part of the game until it exits)");
    }
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
    let route = steamrun::route(a.product, a.app, |k| std::env::var(k).ok(), |p| std::fs::read_to_string(p).ok())?;
    match route {
        steamrun::Route::Ubisoft => run_ubisoft(source, a).await,
        steamrun::Route::Steam { app_id } => run_steam_game(source, a, app_id).await,
    }
}

/// Any Steam game (not a Ubisoft copy): Steam's command with the game's launch plan.
async fn run_steam_game(source: &str, a: SteamRunArgs, app_id: u32) -> Result<()> {
    let game = quiver_api::GameRef::steam(app_id);
    let run_dir = launcher::data_dir().join("steam-run").join(app_id.to_string());
    let mut log = Log(None);
    if !a.dry_run {
        std::fs::create_dir_all(&run_dir)?;
        log.0 = std::fs::OpenOptions::new().create(true).append(true).open(run_dir.join("steam-run.log")).ok();
    }
    log.line(format!("{game} (a Steam game: nothing swapped) command: {}", a.command.join(" ")));
    for (set, flag) in [(a.account.is_some(), "--account"), (a.no_guard, "--no-guard"), (a.sdk_dir.is_some(), "--sdk-dir")] {
        if set {
            log.line(format!("{flag} is for Steam copies of Ubisoft games; ignored"));
        }
    }
    let mut argv = a.command.clone();
    let mut env: Vec<(String, String)> = Vec::new();
    if a.dry_run {
        println!("game: {game} (Steam game; Steam's command runs as it is, with the game's launch plan from quiverd)");
        println!("{}", argv.iter().map(|x| shell_quote(x)).collect::<Vec<_>>().join(" "));
        return Ok(());
    }
    // Never started from here: inside Steam's process tree, Steam would count quiverd as
    // part of the game (and kill it on Stop).
    ensure_daemon_outside_steam(&mut log, false);
    match Client::connect_running(source).await {
        Err(e) => log.line(format!("quiverd unreachable ({e:#}); starting the game without Quiver's launch plan")),
        Ok(client) => {
            let mut plan = None;
            if a.no_settings {
                log.line("--no-settings: the game's Quiver launch plan is not applied");
            } else {
                let steam_env = steamplan::plan_env(std::env::vars().collect::<Vec<_>>().iter().map(|(k, v)| (k.as_str(), v.as_str())));
                match client.call::<SteamLaunchPlan>(Request::SteamLaunchPlan { game: game.into(), perf: true, env: steam_env }).await {
                    Ok(p) => {
                        for l in steamplan::describe(&p) {
                            log.line(format!("settings r{}: {l}", p.revision));
                        }
                        if let Some(s) = &p.perf_session {
                            log.line(format!("perf session {s}"));
                        }
                        for l in &p.perf_notes {
                            log.line(l);
                        }
                        argv = steamplan::steam_command(&p, &a.command);
                        env = steamplan::steam_env(&p, |k| std::env::var(k).ok());
                        plan = Some(p);
                    }
                    Err(e) => log.line(format!("no launch plan from quiverd ({e:#}); starting without it")),
                }
            }
            // The local play session: quiverd watches this pid, which the exec keeps.
            let req = Request::SteamSession {
                app_id,
                pid: std::process::id(),
                perf_session: plan.as_ref().and_then(|p| p.perf_session.clone()),
                settings_revision: plan.as_ref().map_or(0, |p| p.revision),
            };
            match client.call::<quiver_api::ActiveSession>(req).await {
                Ok(_) => log.line(format!("play session held for {game} (pid {})", std::process::id())),
                Err(e) => log.line(format!("no play session from quiverd ({e:#}); the game starts anyway")),
            }
        }
    }
    log.line(format!("exec: {}", argv.iter().map(|x| shell_quote(x)).collect::<Vec<_>>().join(" ")));
    for (k, v) in &env {
        log.line(format!("env {k}={v}"));
    }
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.envs(env);
    let err = cmd.exec();
    bail!("exec {}: {err}", argv[0])
}

/// A Ubisoft game's Steam copy: the bridge swapped in for the game (as before).
async fn run_ubisoft(source: &str, a: SteamRunArgs) -> Result<()> {
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
    let bridge_dst = run_dir.join("quiver-bridge.exe");
    // The game's arguments, then the plan's (Quiver's `args` setting for the game).
    let command_for = |extra_args: &[String]| {
        let mut game_args = pc.game_args(&a.command).to_vec();
        game_args.extend(extra_args.iter().cloned());
        let bridge_args = BridgeArgs::run_command(&steamrun::z_path(&ctx_path), Some(&steamrun::z_path(&log_path)), &game_win, &game_args);
        pc.rewrite(&a.command, &bridge_dst.to_string_lossy(), &bridge_args)
    };
    let mut argv = command_for(&[]);
    let mut overrides = (!a.no_guard).then(|| steamrun::dll_overrides_without_connect(std::env::var("WINEDLLOVERRIDES").ok().as_deref()));

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
    std::fs::copy(find_bridge()?, &bridge_dst).context("copy quiver-bridge.exe")?;
    ensure_daemon_outside_steam(&mut log, true);
    let client = Client::connect(source).await?;
    let mut ctx: serde_json::Value = client
        .call(Request::LaunchContext {
            product_id,
            savegame_path: None,
            install_dir: None,
            account: a.account,
            steam_app_id: Some(env.app_id),
        })
        .await
        .with_context(|| format!("launch context for product {product_id} from quiverd"))?;
    let account = ctx["account_id"].as_str().context("launch context has no account")?.to_owned();
    ctx["savegame_path"] = steamrun::savegame_path(sdk.install_dir.as_deref(), &account, product_id).into();
    let _ = std::fs::remove_file(&log_path);
    quiver_core::private::write_private(&ctx_path, &serde_json::to_vec(&ctx)?)?;
    log.line(format!("saves: {}", ctx["savegame_path"].as_str().unwrap_or_default()));

    // The game's launch settings (env, DLL overrides, args, wrappers); an older quiverd
    // doesn't have them, and the launch goes on without. With them comes the launch's perf
    // session (its environment is in the plan's env; quiverd ends it with the play session).
    let mut plan_env = Vec::new();
    // Steam's environment goes along (as for any Steam game): tweaks yield to what the
    // person typed in Steam's launch options, and the Sight layer goes below their layers.
    let steam_env = steamplan::plan_env(std::env::vars().collect::<Vec<_>>().iter().map(|(k, v)| (k.as_str(), v.as_str())));
    match client.call::<SteamLaunchPlan>(Request::SteamLaunchPlan { game: product_id.into(), perf: !a.no_settings, env: steam_env }).await {
        Ok(plan) if !a.no_settings => {
            for l in steamplan::describe(&plan) {
                log.line(format!("settings r{}: {l}", plan.revision));
            }
            if let Some(s) = &plan.perf_session {
                log.line(format!("perf session {s}"));
            }
            for l in &plan.perf_notes {
                log.line(l);
            }
            argv = steamplan::wrap(&plan, command_for(&plan.args));
            if !plan.dll_overrides.is_empty() {
                let base = overrides.clone().or_else(|| std::env::var("WINEDLLOVERRIDES").ok()).unwrap_or_default();
                overrides = Some(steamplan::merge_dll_overrides(&base, &plan.dll_overrides));
            }
            // LD_PRELOAD kept after Steam's (its overlay); WINEDLLOVERRIDES is set below.
            plan_env = steamplan::steam_env(&plan, |k| std::env::var(k).ok());
        }
        Ok(_) => log.line("--no-settings: the game's Quiver launch settings are not applied"),
        Err(e) => log.line(format!("no launch settings from quiverd ({e:#}); starting without them")),
    }
    log.line(format!("exec: {}", argv.iter().map(|x| shell_quote(x)).collect::<Vec<_>>().join(" ")));

    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.envs(plan_env);
    if let Some(o) = overrides {
        log.line(format!("WINEDLLOVERRIDES={o}"));
        cmd.env("WINEDLLOVERRIDES", o);
    }
    drop(client);
    let err = cmd.exec();
    bail!("exec {}: {err}", argv[0])
}
