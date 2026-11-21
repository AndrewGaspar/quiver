//! `Request::Accounts`: accounts across machines (design/REMOTE.md §6,
//! `quiver_api::accounts`).
//!
//! - **This machine's accounts** ([`local`]): Ubisoft's and Epic's, and the game each is
//!   playing (from `Sessions`: Ubisoft's play sessions, Epic launches' local sessions, lab
//!   instances included, since they hold the account's live session too).
//! - **Presence** ([`presence`]): this machine, each paired server asked now over the
//!   tunnel, and what paired clients announced ([`announcer`] runs on every machine and
//!   tells its servers whenever its accounts or games change, and every
//!   `ANNOUNCE_EVERY` while a game runs).
//! - **Epic hand-over** ([`handover`]): a one-time exchange code minted from the session on
//!   one machine (here, or a server's via `Export`) and redeemed on another (here, or a
//!   server's via `Import`). It crosses only pinned mutual TLS, is a `Secret` everywhere,
//!   and is never logged; the receiving machine ends up with a refresh-token chain of its
//!   own and the giving machine's session is untouched.
//! - **"Playing on <machine>"** ([`refuse_if_playing_elsewhere`]): before a Ubisoft game
//!   starts, its account must not be playing on a paired machine.
//!
//! Sign-in, sign-out and hand-over are the person's ([`person_only`]).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use quiver_api::accounts::{
    self as api, AccountsRequest, HandoverCode, HandoverResult, ImportedAccount, LocalAccount, MachineAccounts, MachineReach, Playing, Presence,
};
use quiver_api::remote::{is_person, is_remote};
use quiver_api::secret::Secret;
use quiver_api::stores::AccountState;
use quiver_api::{Client, Event, Request, StoreId};
use quiver_remote::{Peer, Role};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::Daemon;

/// How long a paired server gets to answer by default.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a launch waits for paired servers to say whether its account is playing.
const LAUNCH_ASK_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the announcer looks at this machine's accounts and games.
const LOOK_EVERY: Duration = Duration::from_secs(5);

/// Refuse anything but the person: the app or the terminal, here or on a paired machine
/// (STORES.md §7, item 10: "Sign in, sign out … agents: Deny").
pub(crate) fn person_only(source: &str, what: &str) -> Result<()> {
    if !is_person(source) {
        bail!("only you can {what}, from the app or a terminal (this request came from '{source}')");
    }
    Ok(())
}

/// What paired clients last announced, by the name their connection is paired under.
static ANNOUNCED: LazyLock<Mutex<HashMap<String, (Instant, MachineAccounts)>>> = LazyLock::new(Default::default);

pub(crate) async fn handle(d: &Arc<Daemon>, source: &str, r: AccountsRequest) -> Result<Value> {
    Ok(match r {
        AccountsRequest::Local => json!(local(d).await?),
        AccountsRequest::Presence { wait_ms } => {
            let wait = wait_ms.map(Duration::from_millis).unwrap_or(ASK_TIMEOUT).min(Duration::from_secs(10));
            json!(presence(d, wait).await?)
        }
        AccountsRequest::Handover { store, account, from, to } => json!(handover(d, source, store, account, from, to).await?),
        AccountsRequest::Export { store, account } => {
            if !is_remote(source) || !is_person(source) {
                bail!("a hand-over code is only given to a paired machine, for you (this request came from '{source}'); use `quiver epic add-to <machine>`");
            }
            epic_only(store)?;
            let (a, code) = d.epic.handover_code(account.as_deref()).await?;
            let to = source.trim_start_matches("remote:");
            d.log_activity(source, format!("gave {to} a one-time code for its own Epic session of {}", a.display_name), None);
            json!(HandoverCode { store, account_id: a.account_id, name: a.display_name, code })
        }
        AccountsRequest::Import { store, code } => {
            person_only(source, "add an Epic account")?;
            epic_only(store)?;
            json!(import(d, source, &code).await?)
        }
        AccountsRequest::Announce(mut m) => {
            let Some(peer) = source.strip_prefix("remote:").map(|p| p.split('/').next().unwrap_or(p).to_owned()) else {
                bail!("only a paired machine announces its accounts");
            };
            m.machine = peer.clone();
            let changed = {
                let mut map = ANNOUNCED.lock().unwrap();
                let changed = map.get(&peer).is_none_or(|(_, old)| old.accounts != m.accounts || old.id != m.id);
                map.insert(peer, (Instant::now(), m));
                changed
            };
            if changed {
                d.emit(Event::AccountsPresenceChanged);
            }
            Value::Null
        }
    })
}

fn epic_only(store: StoreId) -> Result<()> {
    match store {
        StoreId::Epic => Ok(()),
        StoreId::Ubisoft => bail!(
            "Ubisoft has no way to hand a session to another machine: sign in on that machine instead (from here: `quiver --machine <name> login`, or Settings → Accounts with the machine selected)"
        ),
        s => bail!("{} sessions can't be handed over yet", s.label()),
    }
}

/// This machine's accounts in every store, and what each is playing.
pub(crate) async fn local(d: &Arc<Daemon>) -> Result<MachineAccounts> {
    let me = d.remote.identity()?;
    let sessions: Vec<quiver_api::ActiveSession> = d.sessions.list().into_iter().chain(d.local.list()).collect();
    let playing = |store: StoreId, account: &str| -> Option<Playing> {
        let s = sessions.iter().find(|s| s.game_ref().store == store && s.account == account)?;
        let game = s.game_ref();
        let mut name = match game.as_ubisoft() {
            Some(pid) => d.product_name(pid),
            None => d.epic.installed_game(game.id.as_str()).map(|i| i.title).unwrap_or_else(|| game.to_string()),
        };
        if s.session.as_deref().is_some_and(quiver_api::lab::is_lab_session) {
            name.push_str(" (lab)");
        }
        Some(Playing { game, name, started: s.started })
    };
    let mut accounts = Vec::new();
    match quiver_core::store::accounts_and_default() {
        Ok((list, default)) => accounts.extend(list.into_iter().map(|a| LocalAccount {
            store: StoreId::Ubisoft,
            playing: playing(StoreId::Ubisoft, &a.user_id),
            default: default.as_deref() == Some(a.user_id.as_str()),
            account_id: a.user_id,
            name: a.name,
            state: AccountState::SignedIn,
        })),
        Err(e) => tracing::info!("accounts: reading Ubisoft accounts: {e:#}"),
    }
    match d.epic.accounts() {
        Ok(list) => accounts.extend(list.into_iter().map(|a| LocalAccount {
            store: StoreId::Epic,
            playing: playing(StoreId::Epic, &a.account_id),
            account_id: a.account_id,
            name: a.display_name,
            state: a.state,
            default: a.default,
        })),
        Err(e) => tracing::info!("accounts: reading Epic accounts: {e:#}"),
    }
    Ok(MachineAccounts { machine: me.name().to_owned(), id: me.id(), accounts, at: Utc::now() })
}

/// A request to paired server `peer`, as `source` (the person's own, when it acts for them),
/// waiting at most `wait`.
async fn ask<T: DeserializeOwned>(d: &Daemon, peer: &Peer, source: &str, req: Request, wait: Duration) -> Result<T> {
    let machine = peer
        .machine()
        .with_context(|| format!("{} pairs with this machine, but this machine has no address for it: do this from {} instead", peer.name, peer.name))?;
    let dir = d.remote.dir().to_owned();
    let call = async move {
        let c = Client::machine_at(dir, &machine, source).await?;
        c.call::<T>(req).await
    };
    match tokio::time::timeout(wait, call).await {
        Ok(r) => r.with_context(|| format!("asking {}", peer.name)),
        Err(_) => bail!("{} didn't answer in {} s", peer.name, wait.as_secs_f32()),
    }
}

/// The paired servers, each once (a peer paired both ways counts as a server).
fn servers(d: &Daemon) -> Vec<Peer> {
    d.remote.peers().into_iter().filter(|p| p.role == Role::Server).collect()
}

/// Every paired server's accounts (asked now, concurrently), and what paired clients
/// announced within `ANNOUNCE_TTL`; with a reach line for each machine.
async fn others(d: &Arc<Daemon>, wait: Duration) -> (Vec<MachineAccounts>, Vec<MachineReach>) {
    let servers = servers(d);
    let asks = servers.iter().cloned().map(|p| {
        let d = d.clone();
        tokio::spawn(async move {
            let r = ask::<MachineAccounts>(&d, &p, "quiverd", Request::Accounts(AccountsRequest::Local), wait).await;
            (p, r)
        })
    });
    let mut got = Vec::new();
    let mut reach = Vec::new();
    for h in asks.collect::<Vec<_>>() {
        let Ok((p, r)) = h.await else { continue };
        match r {
            Ok(mut m) => {
                m.machine = p.name.clone();
                reach.push(MachineReach { machine: p.name.clone(), id: m.id.clone(), here: false, reached: true, error: None, at: Some(m.at) });
                got.push(m);
            }
            Err(e) => reach.push(MachineReach { machine: p.name.clone(), id: p.id.clone(), here: false, reached: false, error: Some(format!("{e:#}")), at: None }),
        }
    }
    let server_ids: Vec<String> = servers.iter().map(|p| p.id.clone()).collect();
    let announced: Vec<(Instant, MachineAccounts)> = {
        let mut map = ANNOUNCED.lock().unwrap();
        map.retain(|_, (t, _)| t.elapsed() < api::ANNOUNCE_TTL * 4);
        map.values().cloned().collect()
    };
    for p in d.remote.peers().into_iter().filter(|p| p.role == Role::Client && !server_ids.contains(&p.id)) {
        match announced.iter().find(|(_, m)| m.machine == p.name || m.id == p.id) {
            Some((t, m)) if t.elapsed() < api::ANNOUNCE_TTL => {
                let mut m = m.clone();
                m.machine = p.name.clone();
                reach.push(MachineReach { machine: p.name.clone(), id: m.id.clone(), here: false, reached: true, error: None, at: Some(m.at) });
                got.push(m);
            }
            other => reach.push(MachineReach {
                machine: p.name.clone(),
                id: p.id.clone(),
                here: false,
                reached: false,
                error: Some(if other.is_some() { "hasn't reported in the last minute".into() } else { "hasn't reported any accounts".into() }),
                at: other.map(|(_, m)| m.at),
            }),
        }
    }
    (got, reach)
}

/// `AccountsRequest::Presence`.
pub(crate) async fn presence(d: &Arc<Daemon>, wait: Duration) -> Result<Presence> {
    let here = local(d).await?;
    let (mut all, mut reach) = others(d, wait).await;
    reach.insert(0, MachineReach { machine: here.machine.clone(), id: here.id.clone(), here: true, reached: true, error: None, at: Some(here.at) });
    let here_id = here.id.clone();
    all.insert(0, here);
    Ok(Presence { accounts: api::merge(&all, &here_id), machines: reach })
}

/// Refuse to start `game` on `account` (Ubisoft) while a paired machine is playing on it:
/// Ubisoft keeps one live session per account, and a second would end the first. Machines
/// that can't be reached don't hold a launch up.
pub(crate) async fn refuse_if_playing_elsewhere(d: &Arc<Daemon>, store: StoreId, account_id: &str, account_name: &str) -> Result<()> {
    if d.remote.peers().is_empty() {
        return Ok(());
    }
    let here_id = d.remote.identity()?.id();
    let (all, reach) = others(d, LAUNCH_ASK_TIMEOUT).await;
    for r in reach.iter().filter(|r| !r.reached) {
        tracing::info!("accounts: couldn't ask {} whether {account_name} is playing there: {}", r.machine, r.error.as_deref().unwrap_or("?"));
    }
    if let Some((m, p)) = api::live_elsewhere(&all, &here_id, store, account_id) {
        bail!(api::playing_elsewhere_message(account_name, &m.machine, &p.name));
    }
    Ok(())
}

/// Redeem a hand-over code here.
async fn import(d: &Arc<Daemon>, source: &str, code: &Secret<String>) -> Result<ImportedAccount> {
    let a = d.epic.login_exchange(code).await?;
    d.log_activity(source, format!("added Epic account {} from another machine's session", a.display_name), None);
    d.emit(Event::StoreAccountsChanged { store: StoreId::Epic });
    d.emit(Event::LibraryChanged);
    d.emit(Event::AccountsPresenceChanged);
    let d2 = d.clone();
    tokio::spawn(async move {
        match d2.epic.library(true).await {
            Ok(lib) => tracing::info!("Epic library: {} game(s)", lib.titles.len()),
            Err(e) => tracing::info!("Epic library: {e:#}"),
        }
        d2.emit(Event::LibraryChanged);
    });
    Ok(ImportedAccount { store: StoreId::Epic, account_id: a.account_id, name: a.display_name, default: a.default })
}

/// `AccountsRequest::Handover`: mint on `from`, redeem on `to` (`None`: this machine).
async fn handover(d: &Arc<Daemon>, source: &str, store: StoreId, account: Option<String>, from: Option<String>, to: Option<String>) -> Result<HandoverResult> {
    person_only(source, "hand an account's session to another machine")?;
    epic_only(store)?;
    let me = d.remote.identity()?;
    let from = from.map(|q| d.remote.peer(&q, None)).transpose()?;
    let to = to.map(|q| d.remote.peer(&q, None)).transpose()?;
    if from.as_ref().map(|p| &p.id) == to.as_ref().map(|p| &p.id) {
        bail!("the session would go to the machine it came from: name another machine (`--to`)");
    }
    let server = |p: Peer| -> Result<Peer> {
        match d.remote.peer(&p.id, Some(Role::Server)) {
            Ok(s) => Ok(s),
            Err(_) => bail!(
                "{} pairs with this machine, but this machine can't reach it: run the hand-over on {} instead (`quiver epic add-from {}`)",
                p.name,
                p.name,
                me.name()
            ),
        }
    };
    let from = from.map(server).transpose()?;
    let to = to.map(server).transpose()?;
    let from_name = from.as_ref().map_or(me.name().to_owned(), |p| p.name.clone());
    let to_name = to.as_ref().map_or(me.name().to_owned(), |p| p.name.clone());
    // The person's own source goes along, so the other machines' gates see the person.
    let code: HandoverCode = match &from {
        None => {
            let (a, code) = d.epic.handover_code(account.as_deref()).await?;
            HandoverCode { store, account_id: a.account_id, name: a.display_name, code }
        }
        Some(p) => ask(d, p, source, Request::Accounts(AccountsRequest::Export { store, account: account.clone() }), Duration::from_secs(20)).await?,
    };
    let got: ImportedAccount = match &to {
        None => import(d, source, &code.code).await?,
        Some(p) => ask(d, p, source, Request::Accounts(AccountsRequest::Import { store, code: code.code.clone() }), Duration::from_secs(30)).await?,
    };
    drop(code);
    d.log_activity(source, format!("handed Epic account {} from {from_name} to {to_name} (its own session there)", got.name), None);
    d.emit(Event::AccountsPresenceChanged);
    Ok(HandoverResult { store, account_id: got.account_id, name: got.name, from: from_name, to: to_name })
}

/// Tell every paired server what this machine is signed in to and playing: once it has an
/// account, whenever that changes, and every `ANNOUNCE_EVERY` while a game runs (so a
/// server's view expires when this machine goes away).
pub(crate) fn announcer(daemon: &Arc<Daemon>) {
    let weak = Arc::downgrade(daemon);
    let mut events = daemon.events.subscribe();
    tokio::spawn(async move {
        let mut last: Option<(Vec<LocalAccount>, Instant)> = None;
        loop {
            {
                let Some(d) = weak.upgrade() else { return };
                if !servers(&d).is_empty()
                    && let Ok(m) = local(&d).await
                {
                    let playing = m.accounts.iter().any(|a| a.playing.is_some());
                    let due = match &last {
                        // Nothing to say yet: a machine with no accounts doesn't connect.
                        None => !m.accounts.is_empty(),
                        Some((accounts, at)) => *accounts != m.accounts || (playing && at.elapsed() >= api::ANNOUNCE_EVERY),
                    };
                    if due {
                        last = Some((m.accounts.clone(), Instant::now()));
                        for p in servers(&d) {
                            let (d, m) = (d.clone(), m.clone());
                            tokio::spawn(async move {
                                if let Err(e) = ask::<Value>(&d, &p, "quiverd", Request::Accounts(AccountsRequest::Announce(m)), ASK_TIMEOUT).await {
                                    tracing::debug!("accounts: announcing to {}: {e:#}", p.name);
                                }
                            });
                        }
                    }
                }
            }
            // Sooner when sessions or accounts change.
            let _ = tokio::time::timeout(LOOK_EVERY, async {
                loop {
                    match events.recv().await {
                        Ok(Event::SessionsChanged { .. } | Event::StoreAccountsChanged { .. } | Event::SessionChanged { .. } | Event::LibraryChanged | Event::RemoteChanged) => break,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            })
            .await;
        }
    });
}
