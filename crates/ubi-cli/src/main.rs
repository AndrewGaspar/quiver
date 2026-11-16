use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ubi_core::auth::{Auth, LoginOutcome};
use ubi_core::library::{Library, ProductConfig};
use ubi_core::{manifest, store};

#[derive(Parser)]
#[command(name = "ubi", about = "Native Ubisoft Connect client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
    /// Download raw cloud save blobs (<slot>.savegame, zlib) as a backup.
    CloudPull { uplay_id: u32, dir: PathBuf, #[arg(long, default_value_t = 10)] max_slot: u32 },
    /// Check demux connectivity (no login needed).
    Ping,
    /// Summarize a .manifest file (e.g. a game's uplay_install.manifest).
    Manifest {
        path: PathBuf,
        /// List every file.
        #[arg(long)]
        files: bool,
    },
}

#[derive(Subcommand)]
enum SavesAction {
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

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let auth = Auth::default();
    match cli.cmd {
        Cmd::Login { email } => {
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
        Cmd::Logout => {
            store::clear()?;
            println!("Session removed from keyring.");
        }
        Cmd::Whoami => {
            let s = store::current(&auth).await?;
            println!("{} ({})", s.name, s.user_id);
            match auth.account_email(&s).await {
                Ok(e) => println!("email on file: {}", if e.contains('@') { "yes" } else { "unexpected format" }),
                Err(e) => println!("email lookup failed: {e:#}"),
            }
            println!("ticket expires {}", s.expiration.with_timezone(&chrono::Local));
        }
        Cmd::Library { all, json } => {
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
        Cmd::Config { product_id } => {
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
        Cmd::LaunchContext { uplay_id, savegame_path, broker, install_dir, out } => {
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
        Cmd::Broker { listen } => {
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("broker listening on {listen}");
            ubi_core::broker::Broker::new(auth).serve(listener).await?;
        }
        Cmd::Saves { action } => {
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
        Cmd::CloudPull { uplay_id, dir, max_slot } => {
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
        Cmd::Ping => {
            let demux = ubi_core::demux::Demux::connect().await?;
            let info = demux.patch_info().await?;
            println!("demux ok: latest client {} (we send {})", info.latest_version, ubi_core::demux::CLIENT_VERSION);
            println!("patch base url: {}", info.patch_base_url);
        }
        Cmd::Manifest { path, files } => {
            let m = manifest::load_manifest(&path)?;
            let s = manifest::summarize(&m);
            println!("{}", serde_json::to_string_pretty(&s)?);
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
