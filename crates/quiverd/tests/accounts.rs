//! Accounts across machines between two scratch quiverds (design/REMOTE.md §6): an Epic
//! session handed from one machine to the other with an exchange code, presence, and the
//! person-only gates. Epic is a stand-in on 127.0.0.1 (`QUIVER_EPIC_BASE`), and each
//! daemon's keyring is a scratch directory of files (`QUIVER_SCRATCH_SECRETS`), so nothing
//! here reaches Epic or touches the person's keyring, accounts or quiverd.
//!
//! "tower" is the server, "laptop" the client that paired with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quiver_api::accounts::{AccountsRequest, HandoverResult, LocalAccount, MachineAccounts, Playing, Presence};
use quiver_api::stores::AccountState;
use quiver_api::epic::{EpicAccount, EpicRequest};
use quiver_api::remote::{PairCode, PeerInfo};
use quiver_api::secret::Secret;
use quiver_api::{Client, GameRef, Request, StoreId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const ACCOUNT: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";
/// The laptop's sign-in code at the stand-in (Epic's are 32 letters and digits).
const AUTH_CODE: &str = "c0de0000c0de0000c0de0000c0de0001";

// ---------------------------------------------------------------------------------------
// A stand-in for Epic's account and library services

#[derive(Default)]
struct EpicState {
    next: u32,
    /// Live refresh tokens → the chain they belong to (a chain is one machine's session).
    refresh: HashMap<String, String>,
    /// Live access tokens → chain.
    access: HashMap<String, String>,
    /// Exchange codes not yet redeemed → the chain that minted them.
    exchange: HashMap<String, String>,
    /// What was asked: "grant authorization_code", "exchange by A", "grant exchange_code", …
    log: Vec<String>,
}

impl EpicState {
    fn session(&mut self, chain: &str) -> Value {
        self.next += 1;
        let (at, rt) = (format!("at-{chain}-{}", self.next), format!("rt-{chain}-{}", self.next));
        self.access.insert(at.clone(), chain.into());
        self.refresh.insert(rt.clone(), chain.into());
        let soon = |h| (chrono::Utc::now() + chrono::Duration::hours(h)).to_rfc3339();
        json!({
            "access_token": at, "expires_at": soon(8), "refresh_token": rt, "refresh_expires_at": soon(24 * 30),
            "account_id": ACCOUNT, "displayName": "Mock Player", "token_type": "bearer"
        })
    }

    fn answer(&mut self, method: &str, path: &str, auth: &str, form: &HashMap<String, String>) -> (u16, Value) {
        let bearer = auth.strip_prefix("bearer ").map(|t| self.access.get(t).cloned());
        let bad = (400, json!({"errorCode": "errors.com.epicgames.account.oauth.authorization_code_not_found", "errorMessage": "no"}));
        match (method, path) {
            ("POST", "/account/api/oauth/token") => {
                assert!(auth.starts_with("basic "), "the client's basic auth");
                let grant = form.get("grant_type").cloned().unwrap_or_default();
                self.log.push(format!("grant {grant}"));
                match grant.as_str() {
                    "authorization_code" if form.get("code").map(String::as_str) == Some(AUTH_CODE) => (200, self.session("A")),
                    "exchange_code" => match form.get("exchange_code").and_then(|c| self.exchange.remove(c)) {
                        // A new chain: the redeemer's own session.
                        Some(_) => {
                            let chain = format!("X{}", self.next);
                            (200, self.session(&chain))
                        }
                        None => bad,
                    },
                    "refresh_token" => match form.get("refresh_token").and_then(|r| self.refresh.remove(r)) {
                        Some(chain) => (200, self.session(&chain)),
                        None => bad,
                    },
                    _ => bad,
                }
            }
            ("GET", "/account/api/oauth/exchange") => match bearer.flatten() {
                Some(chain) => {
                    self.next += 1;
                    let code = format!("xc{}from{chain}", self.next);
                    self.exchange.insert(code.clone(), chain.clone());
                    self.log.push(format!("exchange by {chain}"));
                    (200, json!({"expiresInSeconds": 300, "code": code, "creatingClientId": "x"}))
                }
                None => (401, json!({"errorCode": "errors.com.epicgames.common.authentication.token_verification_failed"})),
            },
            (_, p) if p.starts_with("/account/api/oauth/sessions/kill/") => {
                self.log.push("kill".into());
                (204, Value::Null)
            }
            ("GET", p) if p.ends_with("/externalAuths") => (200, json!([])),
            ("GET", p) if p.starts_with("/account/api/public/account/") => (200, json!({"id": ACCOUNT, "displayName": "Mock Player"})),
            ("GET", p) if p.starts_with("/library/api/public/items") => (200, json!({"records": [], "responseMetadata": {}})),
            _ => (404, json!({"errorCode": "errors.com.epicgames.common.not_found", "errorMessage": path})),
        }
    }
}

/// Serve the stand-in on 127.0.0.1; returns its base URL.
async fn mock_epic(state: Arc<Mutex<EpicState>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let state = state.clone();
            tokio::spawn(async move {
                let mut r = BufReader::new(s);
                let mut line = String::new();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = line.split_whitespace();
                let (method, target) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                let (mut len, mut auth) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).await.unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap_or((h, ""));
                    match k.to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap_or(0),
                        "authorization" => auth = v.trim().to_owned(),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = String::from_utf8_lossy(&body)
                    .split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                let path = target.split('?').next().unwrap_or("").to_owned();
                let (status, v) = state.lock().unwrap().answer(&method, &path, &auth, &form);
                let body = if v.is_null() { String::new() } else { v.to_string() };
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let mut s = r.into_inner();
                let _ = s.write_all(reply.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    base
}

// ---------------------------------------------------------------------------------------
// Scratch machines (as in tests/remote.rs)

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Machine {
    root: PathBuf,
    name: &'static str,
}

impl Machine {
    fn new(tmp: &Path, name: &'static str) -> Machine {
        let m = Machine { root: tmp.join(name), name };
        let data = m.dir("data/quiver");
        write(&data.join("accounts.json"), r#"{"accounts":[]}"#);
        let cfg = json!({
            "library": m.dir("lib"), "prefixes": m.dir("pfx"), "proton": m.dir("proton"), "sdk_source": m.dir("sdk"),
            "save_backups": m.dir("backups"), "report_playtime": false, "steam_offers": false
        });
        write(&data.join("config.json"), &cfg.to_string());
        write(&data.join("remote/machine.json"), &json!({ "name": name }).to_string());
        m
    }

    fn dir(&self, n: &str) -> PathBuf {
        self.root.join(n)
    }

    fn secrets(&self) -> PathBuf {
        self.dir("secrets")
    }

    /// The Epic session this machine's quiverd stored (its scratch keyring).
    fn epic_session(&self) -> Option<Value> {
        let f = quiver_core::secret::scratch_file(&self.secrets(), "quiver", &format!("epic:{ACCOUNT}"));
        std::fs::read_to_string(f).ok().map(|s| serde_json::from_str(&s).unwrap())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir("quiverd.log")).unwrap_or_default()
    }

    fn daemon(&self, epic: &str) -> Daemon {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir("quiverd.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_quiverd"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir("home"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("QUIVER_SHM_ROOT", self.dir("shm"))
            .env("QUIVER_BROKER", "127.0.0.1:0")
            .env("QUIVER_AUTO_RESTART", "0")
            .env("QUIVER_REMOTE_MDNS", "0")
            .env("QUIVER_SCRATCH_SECRETS", self.secrets())
            .env("QUIVER_EPIC_BASE", epic)
            .env("RUST_LOG", "quiverd=info,epic_core=debug")
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let d = Daemon(child);
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(self.dir("run/quiver/quiverd.sock")).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "{} didn't start: {}", self.name, self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    async fn client(&self, source: &str) -> Client {
        Client::connect_running_at(self.dir("run/quiver/quiverd.sock"), source).await.unwrap()
    }

    fn server(&self, name: &str) -> quiver_remote::Machine {
        let peers = quiver_remote::Peers::load(&self.dir("data/quiver/remote")).unwrap();
        peers.one(name, Some(quiver_remote::Role::Server)).unwrap().machine().unwrap()
    }
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn refresh_token(s: &Value) -> String {
    s["refresh_token"].as_str().unwrap().to_owned()
}

async fn presence(c: &Client) -> Presence {
    c.call(Request::Accounts(AccountsRequest::Presence { wait_ms: Some(3000) })).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn epic_session_handed_over_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let epic = Arc::new(Mutex::new(EpicState::default()));
    let base = mock_epic(epic.clone()).await;
    let tower_m = Machine::new(tmp.path(), "tower");
    let laptop_m = Machine::new(tmp.path(), "laptop");
    let _tower_d = tower_m.daemon(&base);
    let laptop_d = laptop_m.daemon(&base);
    let tower = tower_m.client("cli").await;
    let laptop = laptop_m.client("cli").await;

    let addr = format!("127.0.0.1:{}", free_port());
    let _: Value = tower.call(Request::RemoteEnable { listen: Some(addr.clone()) }).await.unwrap();
    let code: PairCode = tower.call(Request::RemotePairBegin).await.unwrap();
    let _: PeerInfo = laptop.call(Request::RemotePairWith { addr, code: code.code }).await.unwrap();

    // The laptop signs in (at the stand-in) and has its own chain "A".
    let a: EpicAccount = laptop.call(Request::Epic(EpicRequest::LoginComplete { code: AUTH_CODE.into() })).await.unwrap();
    assert_eq!(a.account_id, ACCOUNT);
    let laptop_rt = refresh_token(&laptop_m.epic_session().expect("the laptop's session is in its scratch keyring"));
    assert!(laptop_rt.starts_with("rt-A-"), "{laptop_rt}");
    assert!(tower_m.epic_session().is_none());

    // Agents can't hand a session over, here or through the tunnel.
    let agent = laptop_m.client("agent").await;
    let e = agent.call::<HandoverResult>(Request::Accounts(AccountsRequest::Handover { store: StoreId::Epic, account: None, from: None, to: Some("tower".into()) })).await.unwrap_err();
    assert!(format!("{e:#}").contains("only you can"), "{e:#}");
    let remote_agent = Client::machine_at(laptop_m.dir("data/quiver/remote"), &laptop_m.server("tower"), "agent").await.unwrap();
    let e = remote_agent.call::<Value>(Request::Accounts(AccountsRequest::Import { store: StoreId::Epic, code: Secret::new("abc".into()) })).await.unwrap_err();
    assert!(format!("{e:#}").contains("remote:laptop/agent"), "{e:#}");
    // A hand-over code is only for a paired machine, never a local process.
    let e = laptop.call::<Value>(Request::Accounts(AccountsRequest::Export { store: StoreId::Epic, account: None })).await.unwrap_err();
    assert!(format!("{e:#}").contains("only given to a paired machine"), "{e:#}");
    // Ubisoft has no hand-over.
    let e = laptop.call::<Value>(Request::Accounts(AccountsRequest::Handover { store: StoreId::Ubisoft, account: None, from: None, to: Some("tower".into()) })).await.unwrap_err();
    assert!(format!("{e:#}").contains("sign in on that machine"), "{e:#}");
    assert!(!epic.lock().unwrap().log.iter().any(|l| l.starts_with("exchange")), "nothing minted for refused requests");

    // Add to tower: the laptop mints a code from its session; the tower redeems it.
    let r: HandoverResult = laptop.call(Request::Accounts(AccountsRequest::Handover { store: StoreId::Epic, account: None, from: None, to: Some("tower".into()) })).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str(), r.account_id.as_str()), ("laptop", "tower", ACCOUNT));
    let tower_rt = refresh_token(&tower_m.epic_session().expect("the tower has a session in its own scratch keyring"));
    assert!(tower_rt.starts_with("rt-X"), "the tower's own chain: {tower_rt}");
    assert_eq!(refresh_token(&laptop_m.epic_session().unwrap()), laptop_rt, "the laptop's session is untouched");
    {
        let st = epic.lock().unwrap();
        assert_eq!(st.log.iter().filter(|l| *l == "exchange by A").count(), 1, "{:?}", st.log);
        assert_eq!(st.log.iter().filter(|l| *l == "grant exchange_code").count(), 1, "{:?}", st.log);
        assert!(st.exchange.is_empty(), "the code was used once");
        assert_eq!(st.refresh.get(&laptop_rt).map(String::as_str), Some("A"));
        assert_ne!(st.refresh.get(&tower_rt), st.refresh.get(&laptop_rt), "two independent chains");
    }
    let tower_accounts: Vec<EpicAccount> = tower.call(Request::Epic(EpicRequest::Accounts)).await.unwrap();
    assert_eq!(tower_accounts.iter().map(|a| a.account_id.as_str()).collect::<Vec<_>>(), [ACCOUNT]);
    // Neither log has a token or code in it.
    for log in [tower_m.log(), laptop_m.log()] {
        assert!(!log.contains("rt-") && !log.contains("at-A") && !log.contains("fromA") && !log.contains(AUTH_CODE), "{log}");
    }

    // Presence on the laptop: the account is on both machines (the tower asked now).
    let p = presence(&laptop).await;
    let e = p.account(StoreId::Epic, ACCOUNT).expect("the Epic account");
    let on: Vec<(&str, bool)> = e.machines.iter().map(|m| (m.machine.as_str(), m.here)).collect();
    assert_eq!(on, [("laptop", true), ("tower", false)], "{p:?}");
    assert!(e.playing().is_none());
    // And on the tower, from what the laptop announced.
    let t = Instant::now();
    loop {
        let p = presence(&tower).await;
        if p.account(StoreId::Epic, ACCOUNT).is_some_and(|a| a.machines.len() == 2) {
            assert!(p.machines.iter().any(|m| m.machine == "laptop" && m.reached), "{p:?}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the laptop never announced: {p:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // The other way: signed out on the laptop, it takes a session back from the tower
    // (Export over the tunnel, redeemed here). The tower keeps its own.
    let _: EpicAccount = laptop.call(Request::Epic(EpicRequest::Logout { account: None })).await.unwrap();
    assert!(laptop_m.epic_session().is_none());
    let r: HandoverResult = laptop.call(Request::Accounts(AccountsRequest::Handover { store: StoreId::Epic, account: None, from: Some("tower".into()), to: None })).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("tower", "laptop"));
    let back = refresh_token(&laptop_m.epic_session().unwrap());
    assert!(back.starts_with("rt-X") && back != tower_rt, "{back}");
    assert_eq!(refresh_token(&tower_m.epic_session().unwrap()), tower_rt, "the tower's session is untouched");
    let activity: Vec<quiver_api::ActivityEntry> = tower.call(Request::Activity { limit: 50 }).await.unwrap();
    assert!(activity.iter().any(|a| a.source == "remote:laptop" && a.action.contains("one-time code")), "{activity:?}");

    // What the tower's Ubisoft launch check reads: a paired client playing on an account.
    // (The launch itself isn't run here: it would reach Ubisoft.) Only a paired machine
    // can announce, and under its own paired name whatever it claims. The laptop's quiverd
    // stops first, so its own announcements don't race this one; the test speaks for it.
    drop(laptop_d);
    let playing = MachineAccounts {
        machine: "not-the-laptop".into(),
        id: "whatever".into(),
        accounts: vec![LocalAccount {
            store: StoreId::Ubisoft,
            account_id: "u-1".into(),
            name: "ubiplayer".into(),
            state: AccountState::SignedIn,
            default: true,
            playing: Some(Playing { game: GameRef::ubisoft(3539), name: "Origins".into(), started: chrono::Utc::now() }),
        }],
        at: chrono::Utc::now(),
    };
    let e = tower.call::<Value>(Request::Accounts(AccountsRequest::Announce(playing.clone()))).await.unwrap_err();
    assert!(format!("{e:#}").contains("only a paired machine"), "{e:#}");
    let remote = Client::machine_at(laptop_m.dir("data/quiver/remote"), &laptop_m.server("tower"), "cli").await.unwrap();
    let _: Value = remote.call(Request::Accounts(AccountsRequest::Announce(playing))).await.unwrap();
    let p = presence(&tower).await;
    let u = p.account(StoreId::Ubisoft, "u-1").expect("announced");
    let on = u.playing().unwrap();
    assert_eq!((on.machine.as_str(), on.playing.as_ref().unwrap().name.as_str()), ("laptop", "Origins"));
}
