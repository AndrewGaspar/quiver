//! Demo answers for the global screens (a child module of `demo`, so it shares its state):
//! two Ubisoft accounts with sign-in, sign-out and default switching, who owns what, Steam
//! users and shortcuts (one staged while "Steam is running"), more Activity with agents,
//! and optionally a game being played through a container bridge.
//!
//! `QUIVER_DEMO_GLOBAL` flags (comma-separated) for screenshots:
//! - `signed-out`: no account yet (the sign-in screen; any email, any password except
//!   `wrong`, then any 6 digits except `000000`);
//! - `2fa`: like `signed-out`, with a sign-in already waiting for its code;
//! - `unreachable`: every request fails as if quiverd weren't running;
//! - `playing`: AC Origins is running in a container, reported to Ubisoft;
//! - `one-account`: only the default account;
//! - `steam-closed`: Steam isn't running, so staged shortcuts can be written.
//!
//! The app side reads more flags (`settings=…`, `palette=…`, …; see `ui/global.rs`).

use std::sync::Mutex;

use ubi_api::exports::{ExportKind, ExportRecord, ExportState, ExportStatus, SteamExportResult, SteamUser, SteamUsers};
use ubi_api::{Account, LoginResult};

use super::*;

const FAMILY: &str = "f3a1c9e2-77b0-4d1f-a5e3-0c9b8e7d6a51";
/// Owned by both accounts.
const SHARED: [u32; 3] = [3539, 5059, 3600];
/// Owned only by the second account.
const FAMILY_ONLY: [u32; 2] = [5408, 5277];
const STEAM_USER: &str = "1180254417";

struct Global {
    accounts: Vec<Account>,
    pending_2fa: Option<String>,
    exports: Vec<ExportStatus>,
    steam: SteamUsers,
    seeded: bool,
}

static GLOBAL: Mutex<Option<Global>> = Mutex::new(None);

fn flag(name: &str) -> bool {
    crate::ui::global::demo_flag(name).is_some()
}

fn steam_appid(key: &str) -> u32 {
    // crc32 isn't a dependency here; any stable 32-bit value with the top bit does.
    let mut h: u32 = 2166136261;
    for b in key.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h | 0x8000_0000
}

fn export(kind: ExportKind, id: u32, name: &str, mins_ago: i64, state: ExportState) -> ExportStatus {
    let key = format!("quiver:{id}");
    let home = home();
    let (path, appid, user) = match kind {
        ExportKind::Desktop => (home.join(format!(".local/share/applications/quiver-{id}.desktop")), None, None),
        ExportKind::Steam => (
            home.join(format!(".local/share/Steam/userdata/{STEAM_USER}/config/shortcuts.vdf")),
            Some(steam_appid(&key)),
            Some(STEAM_USER.to_owned()),
        ),
    };
    ExportStatus {
        record: ExportRecord {
            key,
            kind,
            product_id: id,
            profile_id: None,
            name: name.to_owned(),
            path,
            appid,
            steam_user: user,
            written_at: Utc::now() - Span::minutes(mins_ago),
            hash: format!("{:016x}", steam_appid(name) as u64 * 2654435761),
            staged: state == ExportState::WaitingForSteam,
        },
        state,
    }
}

fn staged_line(change: &str, name: &str, id: u32) -> String {
    format!("{change} \"{name}\" (quiver:{id}) for Steam user {STEAM_USER}")
}

fn init(st: &State) -> Global {
    let name = |id: u32| st.entries.iter().find(|e| e.product_id == id).map(|e| e.name.clone()).unwrap_or_default();
    let mut accounts = vec![Account { user_id: "demo".into(), name: "demo".into(), default: true }];
    if !flag("one-account") {
        accounts.push(Account { user_id: FAMILY.into(), name: "family".into(), default: false });
    }
    if flag("signed-out") || flag("2fa") {
        accounts.clear();
    }
    let steam_running = !flag("steam-closed");
    let exports = vec![
        export(ExportKind::Steam, 5059, &name(5059), 25, if steam_running { ExportState::WaitingForSteam } else { ExportState::WaitingForSteam }),
        export(ExportKind::Desktop, 3539, &name(3539), 40, ExportState::Ok),
        export(ExportKind::Steam, 3539, &name(3539), 60 * 24 * 12, ExportState::Ok),
        export(ExportKind::Desktop, 13504, &name(13504), 60 * 24 * 30, ExportState::Missing),
        export(ExportKind::Desktop, 1803, &name(1803), 60 * 24 * 61, ExportState::Changed),
    ];
    let steam = SteamUsers {
        root: Some(home().join(".local/share/Steam")),
        users: vec![
            SteamUser {
                id: STEAM_USER.into(),
                steam_id64: 76561197960265728 + 1180254417,
                persona_name: Some("Player One".into()),
                account_name: Some("playerone".into()),
                most_recent: true,
            },
            SteamUser {
                id: "40921733".into(),
                steam_id64: 76561197960265728 + 40921733,
                persona_name: Some("Couch".into()),
                account_name: Some("couch_steam".into()),
                most_recent: false,
            },
        ],
        selected: Some(STEAM_USER.into()),
        running: steam_running,
        staged: vec![staged_line("add/update", &name(5059), 5059)],
    };
    Global { accounts, pending_2fa: flag("2fa").then(|| "player.one@example.com".to_owned()), exports, steam, seeded: false }
}

/// More history than `demo.rs` has: agents (including one running a game in a container),
/// the CLI, exports and account changes, in the daemon's own wording.
fn seed_activity(st: &mut State) {
    let act = |mins: i64, source: &str, action: &str, id: Option<u32>| ActivityEntry {
        at: Utc::now() - Span::minutes(mins),
        source: source.into(),
        action: action.into(),
        product_id: id,
    };
    let name = |id: u32| st.entries.iter().find(|e| e.product_id == id).map(|e| e.name.clone()).unwrap_or_default();
    let extra = vec![
        act(2, "aco-dlss", "issued launch context for 3539 as family", Some(3539)),
        act(25, "cli", &format!("wrote Steam shortcut for {} (user {STEAM_USER}); staged until Steam restarts", name(5059)), Some(5059)),
        act(40, "gui", &format!("wrote .desktop entry for {}", name(3539)), Some(3539)),
        act(60 * 3, "claude-code", &format!("stopped {}", name(2688)), Some(2688)),
        act(60 * 3 + 20, "claude-code", &format!("play {} (settings r1)", name(2688)), Some(2688)),
        act(60 * 5, "cli", "default account is now demo", None),
        act(60 * 26, "gui", "added account family", None),
        act(60 * 24 * 2, "aco-dlss", "issued launch context for 3539 as family", Some(3539)),
        act(60 * 24 * 3, "system", "cloud saves pulled (5 slots)", Some(3539)),
        act(60 * 24 * 4, "cli", &format!("uninstalled {} (and its prefix)", name(3600)), Some(3600)),
        act(60 * 24 * 9, "gui", &format!("play {} (settings r1)", name(277)), Some(277)),
        act(60 * 24 * 9 + 3, "gui", &format!("install {}", name(277)), Some(277)),
    ];
    st.activity.extend(extra);
    st.activity.sort_by(|a, b| b.at.cmp(&a.at));
}

fn owners(g: &Global, id: u32) -> Vec<String> {
    let default = g.accounts.iter().find(|a| a.default).map(|a| a.user_id.clone());
    let mut v: Vec<String> = g
        .accounts
        .iter()
        .filter(|a| match a.user_id.as_str() {
            "demo" => !FAMILY_ONLY.contains(&id),
            FAMILY => SHARED.contains(&id) || FAMILY_ONLY.contains(&id),
            // Accounts added in the demo own a few games too.
            _ => [3539, 420, 3353].contains(&id),
        })
        .map(|a| a.user_id.clone())
        .collect();
    v.sort_by_key(|u| Some(u) != default.as_ref());
    v
}

fn resolve<'a>(g: &'a mut Global, q: &str) -> Result<&'a mut Account> {
    let q = q.to_lowercase();
    g.accounts
        .iter_mut()
        .find(|a| a.user_id == q || a.name.to_lowercase() == q || (q.len() >= 4 && a.user_id.starts_with(&q)))
        .ok_or_else(|| anyhow!("no signed-in account matches '{q}'"))
}

fn log(st: &mut State, action: String, id: Option<u32>) {
    st.activity.insert(0, ActivityEntry { at: Utc::now(), source: "gui".into(), action, product_id: id });
}

/// Answer `req` if it's one of the global screens' requests; `None` lets `demo.rs` answer.
pub(super) fn handle(demo: &Demo, st: &mut State, req: &Request) -> Result<Option<Value>> {
    let mut guard = GLOBAL.lock().unwrap();
    let g = guard.get_or_insert_with(|| init(st));
    if !g.seeded {
        g.seeded = true;
        seed_activity(st);
        if flag("playing") {
            st.sessions.push(ActiveSession {
                product_id: 3539,
                account: FAMILY.into(),
                started: Utc::now() - Span::seconds(72 * 60 + 8),
                holders: vec!["bridge".into()],
                reported: true,
            });
        }
    }
    if flag("unreachable") {
        bail!("could not connect to {}: connection refused (os error 111)", ubi_api::socket_path().display());
    }
    let default_name = |g: &Global| g.accounts.iter().find(|a| a.default).map(|a| a.name.clone());
    Ok(Some(match req {
        Request::Status => json!(Status {
            version: format!("{} (demo)", env!("CARGO_PKG_VERSION")),
            signed_in: default_name(g),
            awaiting_2fa: g.pending_2fa.is_some(),
            running: st.sessions.iter().map(|s| s.product_id).collect(),
            jobs: st.jobs.clone(),
            proton_default: Some("GE-Proton10-26".into()),
            broker: "127.0.0.1:47810".into(),
            accounts: g.accounts.clone(),
        }),
        Request::Library { .. } => {
            let entries: Vec<LibraryEntry> = st
                .entries
                .iter()
                .cloned()
                .map(|mut e| {
                    e.owned_by = owners(g, e.product_id);
                    e
                })
                .filter(|e| !e.owned_by.is_empty())
                .collect();
            json!(entries)
        }
        Request::ProtonList => json!(["GE-Proton9-27", "GE-Proton10-25", "GE-Proton10-26"]),
        Request::Login { email, password } => {
            if !email.contains('@') {
                bail!("Enter the email address of your Ubisoft account");
            }
            if password.is_empty() {
                bail!("Enter your password");
            }
            if password == "wrong" {
                bail!("Ubisoft didn't accept that email and password");
            }
            g.pending_2fa = Some(email.clone());
            json!(LoginResult::TwoFactorRequired)
        }
        Request::Login2fa { code } => {
            let email = g.pending_2fa.clone().ok_or_else(|| anyhow!("no sign-in waiting for a 2FA code"))?;
            if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
                bail!("Codes are 6 digits");
            }
            if code == "000000" {
                bail!("That code didn't work. Codes change every 30 seconds.");
            }
            g.pending_2fa = None;
            let name: String = email.split('@').next().unwrap_or("player").chars().filter(|c| c.is_alphanumeric()).collect();
            let user_id = if name == "family" { FAMILY.to_owned() } else { format!("{:08x}-demo-4c3e-9f10-{:012x}", steam_appid(&name), name.len()) };
            let known = g.accounts.iter().any(|a| a.user_id == user_id);
            if !known {
                let first = g.accounts.is_empty();
                g.accounts.push(Account { user_id, name: name.clone(), default: first });
            }
            log(st, format!("{} {name}", if known { "signed in again as" } else { "added account" }), None);
            let signed_in = default_name(g);
            drop(guard);
            demo.emit(Event::SessionChanged { signed_in });
            demo.emit(Event::LibraryChanged);
            json!(LoginResult::SignedIn { name })
        }
        Request::Logout { account } => {
            let id = match account {
                Some(q) => resolve(g, q)?.user_id.clone(),
                None => g.accounts.iter().find(|a| a.default).map(|a| a.user_id.clone()).ok_or_else(|| anyhow!("not signed in"))?,
            };
            if let Some(s) = st.sessions.iter().find(|s| s.account == id) {
                bail!("{} is running on this account; stop it first", clean(&name_of(st, s.product_id)?));
            }
            let i = g.accounts.iter().position(|a| a.user_id == id).unwrap();
            let gone = g.accounts.remove(i);
            if gone.default {
                if let Some(a) = g.accounts.first_mut() {
                    a.default = true;
                }
            }
            log(st, format!("signed out {}", gone.name), None);
            let signed_in = default_name(g);
            drop(guard);
            demo.emit(Event::SessionChanged { signed_in });
            demo.emit(Event::LibraryChanged);
            Value::Null
        }
        Request::AccountDefault { account } => {
            let id = resolve(g, account)?.user_id.clone();
            for a in &mut g.accounts {
                a.default = a.user_id == id;
            }
            let name = default_name(g).unwrap_or_default();
            log(st, format!("default account is now {name}"), None);
            drop(guard);
            demo.emit(Event::SessionChanged { signed_in: Some(name) });
            demo.emit(Event::LibraryChanged);
            Value::Null
        }
        Request::ExportList => json!(g.exports),
        Request::SteamUsers => json!(g.steam),
        Request::SteamApplyStaged => {
            if g.steam.running {
                bail!("Steam is running; it would overwrite shortcuts.vdf when it exits. Close Steam or run `quiver steam restart`.");
            }
            let n = g.steam.staged.len();
            g.steam.staged.clear();
            for x in &mut g.exports {
                if x.state == ExportState::WaitingForSteam {
                    x.state = ExportState::Ok;
                    x.record.staged = false;
                }
            }
            if n > 0 {
                log(st, format!("wrote {n} staged Steam shortcut change(s)"), None);
            }
            json!({ "applied": n })
        }
        Request::ExportDesktop { product_id, remove } => {
            let name = name_of(st, *product_id)?;
            let path = home().join(format!(".local/share/applications/quiver-{product_id}.desktop"));
            g.exports.retain(|x| !(x.record.kind == ExportKind::Desktop && x.record.product_id == *product_id));
            if *remove {
                log(st, format!("removed .desktop entry for {name}"), Some(*product_id));
                json!({ "removed": true, "path": path })
            } else {
                let x = export(ExportKind::Desktop, *product_id, &name, 0, ExportState::Ok);
                g.exports.insert(0, x.clone());
                log(st, format!("wrote .desktop entry for {name}"), Some(*product_id));
                json!(x.record)
            }
        }
        Request::ExportSteam { product_id, remove, .. } => {
            let name = name_of(st, *product_id)?;
            let running = g.steam.running;
            g.exports.retain(|x| !(x.record.kind == ExportKind::Steam && x.record.product_id == *product_id));
            g.steam.staged.retain(|s| !s.contains(&format!("(quiver:{product_id})")));
            if running {
                g.steam.staged.push(staged_line(if *remove { "remove" } else { "add/update" }, &name, *product_id));
            }
            if !*remove {
                let state = if running { ExportState::WaitingForSteam } else { ExportState::Ok };
                g.exports.insert(0, export(ExportKind::Steam, *product_id, &name, 0, state));
            }
            log(
                st,
                format!(
                    "{} Steam shortcut for {name} (user {STEAM_USER}){}",
                    if *remove { "removed" } else { "wrote" },
                    if running { "; staged until Steam restarts" } else { "" }
                ),
                Some(*product_id),
            );
            json!(SteamExportResult {
                key: format!("quiver:{product_id}"),
                appid: steam_appid(&format!("quiver:{product_id}")),
                steam_user: STEAM_USER.into(),
                staged: running,
                applied_staged: 0,
            })
        }
        _ => return Ok(None),
    }))
}
