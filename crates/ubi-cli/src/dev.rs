use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use ubi_core::auth::{Auth, LoginOutcome};
use ubi_core::library::{Library, ProductConfig};
use ubi_core::{manifest, store};

#[derive(Subcommand)]
pub enum DevCmd {
    /// Log in with email/password (+2FA) and store the session in the keyring.
    Login {
        #[arg(long)]
        email: Option<String>,
    },
    /// Forget the stored session.
    Logout,
    /// Show the logged-in account, refreshing the session if needed.
    Whoami,
    /// List owned products.
    Library {
        /// Include DLC, bundles and products without a PC build.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a product's configuration YAML.
    Config { product_id: u32 },
    /// Write the launch context ubi-bridge needs to stand in for Connect.
    LaunchContext {
        uplay_id: u32,
        /// Windows path the game should store SDK savegames under.
        #[arg(long)]
        savegame_path: Option<String>,
        /// Broker address the bridge should use for live requests.
        #[arg(long)]
        broker: Option<String>,
        /// Host path of the game's install directory (to report installed chunks).
        #[arg(long)]
        install_dir: Option<PathBuf>,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Run the native broker that ubi-bridge instances ask for live tokens.
    Broker {
        #[arg(long, default_value = "127.0.0.1:47810")]
        listen: String,
    },
    /// Sync a game's local save folder with the cloud.
    Saves {
        #[command(subcommand)]
        action: SavesAction,
    },
    /// List games installed by this launcher.
    Games,
    /// Compare installed games (dirs containing uplay_install.manifest) with the latest builds.
    Status {
        /// Game install directories, as `product_id=path`.
        installs: Vec<String>,
    },
    /// Download and install a game (resumable).
    Install {
        product_id: u32,
        dest: PathBuf,
        #[arg(long, default_value = "en-US")]
        language: String,
        /// Only print what would be installed.
        #[arg(long)]
        dry_run: bool,
        /// Also save the downloaded manifest here (for inspection).
        #[arg(long)]
        manifest_out: Option<PathBuf>,
    },
    /// Request signed CDN URLs for relative paths and report their HTTP status (debugging).
    CdnProbe { product_id: u32, paths: Vec<String> },
    /// Download raw cloud save blobs (<slot>.savegame, zlib) as a backup.
    CloudPull { uplay_id: u32, dir: PathBuf, #[arg(long, default_value_t = 10)] max_slot: u32 },
    /// Fetch Ubisoft's SDK DLLs from the public Connect installer into a directory.
    FetchSdk { dir: PathBuf },
    /// Check demux connectivity (no login needed).
    Ping,
    /// Report a play session step to Ubisoft directly: start, update or end.
    Playsession { step: String, product_id: u32 },
    /// GET a UbiServices REST path with the stored session and print the JSON (read-only;
    /// `{userId}` and `{profileId}` are replaced with the default account's ids).
    RestGet {
        /// e.g. /v1/profiles/{userId}/global/ubiconnect/economy/api/units
        path: String,
    },
    /// Fetch a game's progression over UbiServices REST only (stats, rewards, challenges,
    /// Connect level/units, achievement set), as the daemon's GameProgress does. No demux.
    Progress {
        /// The game's UbiServices space id (`space_id` in `dev library --json`).
        space_id: String,
        /// The config's `uplay.achievements` asset, to read achievement definitions.
        #[arg(long)]
        achievements: Option<String>,
        /// Where achievement sets are unpacked.
        #[arg(long, default_value = "/tmp/quiver-achievements")]
        cache: PathBuf,
    },
    /// Ubisoft Connect's record of a game's cloud save slots (UbiServices REST, no demux).
    CloudRecord { uplay_id: u32 },
    /// Summarize a .manifest file (e.g. a game's uplay_install.manifest).
    Manifest {
        path: PathBuf,
        /// List every file.
        #[arg(long)]
        files: bool,
    },
}

#[derive(Subcommand)]
pub enum SavesAction {
    /// Cloud -> local (slots with unsynced local changes are kept).
    Pull {
        uplay_id: u32,
        /// Local folder holding <slot>.save files.
        dir: PathBuf,
        /// Where overwritten local saves are copied first.
        #[arg(long)]
        backup_dir: PathBuf,
    },
    /// Local -> cloud for slots the game marked with <slot>.save.upload.
    Push { uplay_id: u32, dir: PathBuf },
}

async fn register_game(lib: &mut Library, product_id: u32, dest: &std::path::Path, language: &str) -> Result<()> {
    use ubi_core::launcher;
    let cfg = launcher::Config::load()?;
    let game = launcher::register(&cfg, lib, product_id, &dest.canonicalize()?, language).await?;
    eprintln!("registered {} (runs {} in prefix {})", game.name, game.exe, game.prefix.display());
    let mut games = launcher::Games::load()?;
    games.upsert(game);
    games.save()
}

pub fn prompt(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

pub async fn run_dev(cmd: DevCmd) -> Result<()> {
    let auth = Auth::default();
    match cmd {
        DevCmd::Login { email } => {
            let email = match email {
                Some(e) => e,
                None => prompt("Email: ")?,
            };
            let password = rpassword::prompt_password("Password: ")?;
            let session = match auth.login(&email, &password).await? {
                LoginOutcome::Done(s) => s,
                LoginOutcome::TwoFactor(challenge) => {
                    // `masked_phone` is present even for authenticator-app accounts,
                    // so describe the code source from the method list instead.
                    let methods = &challenge.methods;
                    let has = |m: &str| methods.iter().any(|x| x.eq_ignore_ascii_case(m));
                    let source = if has("googleAuthenticator") || methods.is_empty() {
                        "authenticator app".to_owned()
                    } else if has("sms") {
                        format!("SMS to {}", challenge.masked_phone.as_deref().unwrap_or("your phone"))
                    } else {
                        methods.join("/")
                    };
                    let code = prompt(&format!("2FA code ({source}): "))?;
                    auth.complete_2fa(&challenge, &code).await?
                }
            };
            store::save(&session)?;
            if session.remember_me_ticket.is_none() {
                eprintln!("warning: no remember-me ticket issued; you'll need to log in again in a few hours");
            }
            println!("Logged in as {} ({})", session.name, session.user_id);
        }
        DevCmd::Logout => {
            store::clear(None)?;
            println!("Session removed from keyring.");
        }
        DevCmd::Whoami => {
            let s = store::current(&auth, None).await?;
            println!("{} ({})", s.name, s.user_id);
            match auth.account_email(&s).await {
                Ok(e) => println!("email on file: {}", if e.contains('@') { "yes" } else { "unexpected format" }),
                Err(e) => println!("email lookup failed: {e:#}"),
            }
            println!("ticket expires {}", s.expiration.with_timezone(&chrono::Local));
        }
        DevCmd::Library { all, json } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let lib = Library::fetch(&demux).await?;
            let mut products: Vec<_> = lib
                .products
                .iter()
                .filter(|p| all || (p.owned && p.is_installable()))
                .collect();
            products.sort_by(|a, b| a.name.cmp(&b.name));
            if json {
                println!("{}", serde_json::to_string_pretty(&products)?);
            } else {
                println!("{:>6}  {:>4} {:>5}  {}", "id", "type", "state", "name");
                for p in &products {
                    let flags = match (p.owned, p.is_installable(), p.locked_by_subscription) {
                        (_, _, true) => " [subscription-locked]",
                        (false, _, _) => " [not owned]",
                        (true, false, _) => " [no download]",
                        _ => "",
                    };
                    println!(
                        "{:>6}  {:>4} {:>5}  {}{flags}",
                        p.product_id,
                        p.product_type,
                        p.state,
                        p.name.as_deref().unwrap_or("?")
                    );
                }
                eprintln!("{} of {} products shown", products.len(), lib.products.len());
            }
        }
        DevCmd::Config { product_id } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let config = match lib.get(product_id).and_then(|p| p.configuration.clone()) {
                Some(c) => c,
                None => lib.product_config(product_id).await?,
            };
            println!("{config}");
            if let Ok(cfg) = ProductConfig::parse(&config) {
                for exe in cfg.executables() {
                    eprintln!("executable: {}{}", exe.path, exe.arguments.map(|a| format!(" {a}")).unwrap_or_default());
                }
            }
        }
        DevCmd::LaunchContext { uplay_id, savegame_path, broker, install_dir, out } => {
            let (session, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let mut ctx = ubi_core::launch::build(&auth, &session, &mut lib, uplay_id).await?;
            ctx.savegame_path = savegame_path;
            ctx.broker = broker;
            if let Some(dir) = install_dir {
                ctx.chunks = ubi_core::launch::installed_chunks(&dir)?;
            }
            std::fs::write(&out, serde_json::to_vec_pretty(&ctx)?)?;
            eprintln!(
                "wrote {} (owned ids: {:?}, {} cd key(s), ticket {} chars)",
                out.display(),
                ctx.owned_uplay_ids,
                ctx.cd_keys.len(),
                ctx.upc_ticket.len()
            );
        }
        DevCmd::Broker { listen } => {
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("broker listening on {listen}");
            ubi_core::broker::Broker::new(ubi_core::online::Accounts::new(), None).serve(listener).await?;
        }
        DevCmd::Saves { action } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let mut cloud = ubi_core::cloudsave::CloudSaves::open(&demux).await?;
            let report = match action {
                SavesAction::Pull { uplay_id, dir, backup_dir } => {
                    ubi_core::savesync::pull(&mut cloud, &mut lib, uplay_id, &dir, &backup_dir).await?
                }
                SavesAction::Push { uplay_id, dir } => ubi_core::savesync::push(&mut cloud, &mut lib, uplay_id, &dir).await?,
            };
            println!(
                "downloaded {:?} unchanged {:?} kept-local {:?} uploaded {:?}",
                report.downloaded, report.unchanged, report.kept_local, report.uploaded
            );
            if let Some(b) = report.backup_dir {
                println!("previous local copies backed up to {}", b.display());
            }
        }
        DevCmd::Games => {
            for g in ubi_core::launcher::Games::load()?.games {
                println!("{:>6}  {:<40} {}", g.product_id, g.name, g.install_dir.display());
            }
        }
        DevCmd::Status { installs } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let lib = Library::fetch(&demux).await?;
            for spec in installs {
                let (id, path) = spec.split_once('=').context("expected product_id=path")?;
                let id: u32 = id.parse()?;
                let installed = manifest::manifest_hash(&std::fs::read(PathBuf::from(path).join("uplay_install.manifest"))?)?;
                let product = lib.get(id).context("not in library")?;
                let latest = product.latest_manifest.clone().unwrap_or_default();
                let state = if latest.eq_ignore_ascii_case(&installed) { "up to date" } else { "UPDATE AVAILABLE" };
                println!("{:<40} {state}  (installed {}, latest {})", product.name.as_deref().unwrap_or("?"), &installed[..8], &latest[..latest.len().min(8)]);
            }
        }
        DevCmd::Install { product_id, dest, language, dry_run, manifest_out } => {
            use std::sync::atomic::Ordering::Relaxed;
            let (_, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let product = lib.get(product_id).context("not in your library")?.clone();
            let hash = product.latest_manifest.clone().context("product has no PC build")?;
            let mut dl = ubi_core::download::Downloader::open(&demux, &mut lib, product_id).await?;
            let (manifest, raw) = dl.manifest(&hash).await?;
            if let Some(out) = &manifest_out {
                std::fs::write(out, &raw)?;
            }
            let plan = ubi_core::install::plan(&manifest, &language)?;
            println!(
                "{}: {} files, {:.2} GB on disk, {:.2} GB to download (chunks {:?})",
                product.name.as_deref().unwrap_or("?"),
                plan.files.len(),
                plan.install_bytes as f64 / 1e9,
                plan.download_bytes as f64 / 1e9,
                plan.chunks
            );
            let installed = dest.join("uplay_install.manifest");
            let mut updating = false;
            if let Ok(old_raw) = std::fs::read(&installed) {
                if manifest::manifest_hash(&old_raw)?.eq_ignore_ascii_case(&hash) {
                    let done = std::fs::read_to_string(dest.join(".ubi-install/done")).unwrap_or_default();
                    let done: std::collections::HashSet<&str> = done.lines().collect();
                    if dry_run || plan.files.iter().all(|f| done.contains(f.path.as_str())) {
                        println!("already installed and up to date");
                        if !dry_run {
                            register_game(&mut lib, product_id, &dest, &language).await?;
                        }
                        return Ok(());
                    }
                    println!("resuming incomplete install");
                } else {
                    updating = true;
                    let old_plan = ubi_core::install::plan(&manifest::parse_manifest(&old_raw)?, &language)?;
                    if dry_run {
                        println!("update from {}", manifest::manifest_hash(&old_raw)?);
                        return Ok(());
                    }
                    let (kept, removed) = ubi_core::install::prepare_update(&old_plan, &plan, &dest)?;
                    println!("updating: {kept} files unchanged, {removed} removed");
                }
            }
            if dry_run {
                return Ok(());
            }
            let progress = std::sync::Arc::new(ubi_core::install::Progress::default());
            let reporter = {
                let p = progress.clone();
                let total = plan.download_bytes;
                let files = plan.files.len();
                tokio::spawn(async move {
                    let start = std::time::Instant::now();
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let d = p.downloaded.load(Relaxed);
                        let rate = d as f64 / start.elapsed().as_secs_f64() / 1e6;
                        eprintln!(
                            "{:5.1}%  {:.2}/{:.2} GB  {:.1} MB/s  files {}/{}",
                            100.0 * d as f64 / total.max(1) as f64,
                            d as f64 / 1e9,
                            total as f64 / 1e9,
                            rate,
                            p.files_done.load(Relaxed),
                            files
                        );
                    }
                })
            };
            let compressed = manifest.is_compressed.unwrap_or(false);
            let result = ubi_core::install::install(&mut dl, &plan, &dest, compressed, &progress).await;
            reporter.abort();
            result?;
            std::fs::write(&installed, &raw)?;
            register_game(&mut lib, product_id, &dest, &language).await?;
            println!("{} {} files in {}", if updating { "updated" } else { "installed" }, plan.files.len(), dest.display());
        }
        DevCmd::CdnProbe { product_id, paths } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let dl = ubi_core::download::Downloader::open(&demux, &mut lib, product_id).await?;
            let urls = dl.urls(&paths).await?;
            for (path, mirrors) in paths.iter().zip(urls) {
                let (status, head) = match mirrors.first() {
                    Some(u) => match dl.http.get(u).send().await {
                        Ok(r) => {
                            let st = r.status().as_u16();
                            let b = r.bytes().await.unwrap_or_default();
                            (st, format!("{} bytes, starts {}", b.len(), hex::encode(&b[..b.len().min(8)])))
                        }
                        Err(_) => (0, String::new()),
                    },
                    None => (0, String::new()),
                };
                println!("{status}  {path}  ({} mirrors) {head}", mirrors.len());
            }
        }
        DevCmd::CloudPull { uplay_id, dir, max_slot } => {
            let (_, demux) = ubi_core::connect(&auth).await?;
            let mut lib = Library::fetch(&demux).await?;
            let mut cloud = ubi_core::cloudsave::CloudSaves::open(&demux).await?;
            std::fs::create_dir_all(&dir)?;
            let names: Vec<String> = (1..=max_slot).map(|s| s.to_string()).collect();
            let items: Vec<(u32, &str)> = (1..=max_slot).zip(names.iter().map(String::as_str)).collect();
            let reqs = cloud.get_requests_v2(&mut lib, uplay_id, &items).await?;
            for ((slot, _), req) in items.iter().zip(&reqs) {
                let (status, body) = cloud.execute(req, None).await?;
                if status == 200 {
                    std::fs::write(dir.join(format!("{slot}.savegame")), &body)?;
                    println!("slot {slot}: {} bytes", body.len());
                } else {
                    println!("slot {slot}: HTTP {status}");
                }
            }
        }
        DevCmd::FetchSdk { dir } => {
            let mut cfg = ubi_core::launcher::Config::load()?;
            cfg.sdk_source = dir.clone();
            ubi_core::launcher::ensure_sdk(&cfg).await?;
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                println!("{:>9}  {}", e.metadata()?.len(), e.file_name().to_string_lossy());
            }
        }
        DevCmd::Playsession { step, product_id } => {
            let online = ubi_core::online::Online::new(auth);
            match step.as_str() {
                "start" => ubi_core::playsession::start(&online, product_id).await?,
                "update" => ubi_core::playsession::heartbeat(&online, product_id).await?,
                "end" => ubi_core::playsession::end(&online, product_id).await?,
                other => anyhow::bail!("unknown step {other:?} (start, update, end)"),
            }
            println!("playsession {step} {product_id}: ok");
        }
        DevCmd::Ping => {
            let demux = ubi_core::demux::Demux::connect().await?;
            let info = demux.patch_info().await?;
            println!("demux ok: latest client {} (we send {})", info.latest_version, ubi_core::demux::CLIENT_VERSION);
            println!("patch base url: {}", info.patch_base_url);
        }
        DevCmd::RestGet { path } => {
            let session = store::current(&auth, None).await?;
            let path = ubi_core::ubiservices::expand(&path, &session);
            let (status, body) = ubi_core::ubiservices::get_raw(&auth, &session, &path).await?;
            eprintln!("HTTP {status}");
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                Err(_) => println!("{body}"),
            }
        }
        DevCmd::Progress { space_id, achievements, cache } => {
            let session = store::current(&auth, None).await?;
            let refs = ubi_core::progress::GameRefs { space_id: Some(space_id), achievements_asset: achievements };
            let p = ubi_core::progress::fetch(&auth, &session, &refs, &cache).await;
            println!("{}", serde_json::to_string_pretty(&p)?);
        }
        DevCmd::CloudRecord { uplay_id } => {
            let session = store::current(&auth, None).await?;
            let r = ubi_core::saves::connect_record(&auth, &session, uplay_id).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        DevCmd::Manifest { path, files } => {
            let m = manifest::load_manifest(&path)?;
            let s = manifest::summarize(&m);
            println!("{}", serde_json::to_string_pretty(&s)?);
            println!("compression: {:?}, slicer: {:?}", m.compression_method, m.slicer_config);
            for c in &m.chunks {
                let (n, bytes) = c.files.iter().filter(|f| !f.is_dir).fold((0, 0u64), |(n, b), f| (n + 1, b + f.size));
                println!(
                    "chunk {:>3} type {} lang {:<6} tags {:<24} uplay {:?} files {:>4} {:>8.2} GB",
                    c.id, c.r#type, c.language.as_deref().unwrap_or("-"), c.tags.as_deref().unwrap_or("-"),
                    c.uplay_id, n, bytes as f64 / 1e9
                );
            }
            if files {
                for chunk in &m.chunks {
                    for f in chunk.files.iter().filter(|f| !f.is_dir) {
                        println!("{:>14}  chunk {:<4} {}", f.size, chunk.id, f.name);
                    }
                }
            }
        }
    }
    Ok(())
}
