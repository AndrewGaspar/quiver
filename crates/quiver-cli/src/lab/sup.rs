//! The per-instance supervisor (`quiver lab supervise <i>`, started by `quiver lab up`):
//! the one owner of an instance's launches. It
//!
//! - launches on request (`launch`): kills every Wine process in the instance, re-checks
//!   the Connect guard, puts the mod's files in, applies the graphics profile, gets fresh
//!   launch material from quiverd, starts the game;
//! - kills on request (`kill`);
//! - watches the run: the readiness ladder (process, window, frame), the mod's state file,
//!   the game's exit, and Connect (a Connect process kills the run);
//! - writes `state.json` (whole, atomically) and `events.jsonl` (one line per change, each
//!   with the run id).
//!
//! Requests come over `<instance>/supervisor.sock`, one JSON line each way, so agents never
//! assemble pkill/sleep sequences themselves. One supervisor per instance (a lock file).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use quiver_api::lab::LaunchMaterial;
use quiver_api::{Request, StoreId};
use quiver_core::lab::manifest::Licence;
use quiver_core::lab::ready::{Exit, Phase, Run, State};
use quiver_core::lab::{self, Inst, Lab, Manifest, configset, eos, eosproxy, guard, host, podman, procs, wincert};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ops;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SupRequest {
    Ping,
    Launch {
        source: String,
        #[serde(default)]
        mod_dir: Option<PathBuf>,
        #[serde(default)]
        no_mod: bool,
        #[serde(default)]
        args: Vec<String>,
        /// The clock for this launch (`real`, `virtual:<Hz>`), instead of the manifest's.
        #[serde(default)]
        clock: Option<String>,
    },
    Kill {
        source: String,
    },
    /// Stop supervising (the container is going down).
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Value,
}

struct Sup {
    lab: Lab,
    inst: Inst,
    state: Mutex<State>,
    /// Serialises launches and kills.
    busy: Mutex<()>,
    /// The instance's EOS proxy (lab::eosproxy), when its container forwards port 443 to it.
    eos: Option<EosProxy>,
    /// What the watch loop has already said about the current run's network.
    net_seen: Mutex<(u32, std::collections::BTreeSet<String>, eosproxy::Stats)>,
}

struct EosProxy {
    shared: Arc<eosproxy::Shared>,
    port: u16,
    ca: eosproxy::Ca,
}

/// The file `up` records the EOS proxy's port in (the container forwards 443 to it).
pub const EOS_PORT_FILE: &str = "eos-proxy-port";

/// Start the instance's EOS proxy on the port its container forwards to.
fn start_eos_proxy(inst: &Inst) -> Result<Option<EosProxy>> {
    let Some(port) = std::fs::read_to_string(inst.dir.join(EOS_PORT_FILE)).ok().and_then(|p| p.trim().parse::<u16>().ok()) else {
        return Ok(None);
    };
    let ca = eosproxy::Ca::load_or_create(&inst.dir.join("eos-proxy"), &inst.name)?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("the EOS proxy's port {port} is taken"))?;
    let shared = eosproxy::Shared::new(inst.dir.join("eos-proxy").join("proxy.log"));
    eosproxy::spawn(listener, eosproxy::Config::new(&ca, None, eosproxy::Upstream::Dns)?, shared.clone())?;
    Ok(Some(EosProxy { shared, port, ca }))
}

/// Before an EOS game's launch: the proxy listens on the port the container forwards to, the
/// instance's cloned prefix trusts its CA, and a probe from inside the container reaches it
/// through `api.epicgames.dev:443`. The run's proxy log starts here.
fn eos_ready(sup: &Sup, run_dir: &Path, run: u32) -> Result<Vec<String>> {
    let inst = &sup.inst;
    let i = &inst.name;
    let p = sup.eos.as_ref().with_context(|| {
        format!(
            "this instance's container has no EOS proxy (it was made before the proxy existed, or the proxy didn't start: see {}); quiver lab down {i} && quiver lab up {i}",
            inst.supervisor_log().display()
        )
    })?;
    let fwd = std::fs::read_to_string(inst.dir.join(EOS_PORT_FILE)).unwrap_or_default();
    if fwd.trim() != p.port.to_string() {
        bail!("the container forwards port 443 to {:?} but the EOS proxy listens on {}; quiver lab down {i} && quiver lab up {i}", fwd.trim(), p.port);
    }
    let host = eos::FORWARDED_HOST;
    let addrs: Vec<String> = p.shared.epic_addrs().iter().map(|a| a.to_string()).collect();
    p.shared.start_run(
        run_dir.join(eos::LOG_FILE),
        &format!(
            "--- EOS proxy for run r{run}: the container's {host} and {} resolve to 127.0.0.1, whose port 443 is this proxy (host 127.0.0.1:{}); allowed requests go to the real {host} ({}); lab CA {}",
            eos::SINKHOLED_HOSTS.join(", "),
            p.port,
            if addrs.is_empty() { "resolved at first use".to_owned() } else { addrs.join(", ") },
            wincert::thumbprint(&p.ca.der)
        ),
    );
    let mut notes: Vec<String> = wincert::install(&inst.prefix(), &p.ca.der)?.into_iter().map(|d| format!("EOS proxy: {d}")).collect();
    if !wincert::installed(&inst.prefix(), &p.ca.der) {
        bail!("the lab CA isn't in the instance prefix's ROOT store after installing it");
    }
    let nonce = format!("{:x}-{:x}", std::process::id(), Utc::now().timestamp_nanos_opt().unwrap_or_default());
    let script = format!(r#"getent hosts {host} | head -n1; exec 3<>/dev/tcp/{host}/443 || exit 7; printf '{} {nonce}\n' >&3; head -n1 <&3"#, eosproxy::PROBE);
    let out = ops::podman_ok(&["exec".into(), inst.container(), "bash".into(), "-c".into(), script]).context("probe the EOS proxy from inside the container")?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let resolved = text.lines().next().unwrap_or_default().trim().to_owned();
    if !resolved.starts_with("127.0.0.1") {
        bail!("inside the container {host} resolves to {resolved:?}, not the EOS proxy (the container was made before it); quiver lab down {i} && quiver lab up {i}");
    }
    if !text.contains(eosproxy::PROBE_REPLY) || !p.shared.probed(&nonce) {
        bail!("a probe from inside the container didn't reach the EOS proxy ({:?})", text.trim());
    }
    *sup.net_seen.lock().unwrap() = (run, Default::default(), Default::default());
    notes.push(format!(
        "EOS proxy: a probe from inside the container reached it ({host}:443 -> host 127.0.0.1:{}); the game's handshakes and requests go to runs/r{run}/{}, and `quiver lab logs {i}` says whether it saw any",
        p.port,
        eos::LOG_FILE
    ));
    Ok(notes)
}

/// During a run: say when the proxy first sees the game's TLS (or its rejection), and look
/// for :443 connections that didn't go through it. One to an address of the real
/// `api.epicgames.dev` kills the run (as a Connect process does). True: killed.
fn eos_watch(sup: &Sup, p: &EosProxy, all: &[procs::Proc], cid: &str) -> bool {
    let Some(run) = sup.state.lock().unwrap().run.as_ref().filter(|r| r.phase != Phase::Exited).map(|r| r.id) else { return false };
    let Some(pid) = procs::instance_wine(all, cid).first().map(|p| p.pid) else { return false };
    let read = |f: &str| std::fs::read_to_string(format!("/proc/{pid}/net/{f}")).unwrap_or_default();
    let conns = eos::outside_443(&read("tcp"), &read("tcp6"));
    let stats = p.shared.stats();
    let epic = p.shared.epic_addrs();
    let (old, fresh) = {
        let mut seen = sup.net_seen.lock().unwrap();
        if seen.0 != run {
            *seen = (run, Default::default(), Default::default());
        }
        let old = std::mem::replace(&mut seen.2, stats);
        let fresh: Vec<eos::TcpConn> = conns.into_iter().filter(|c| seen.1.insert(format!("{}:{}", c.remote, c.port))).collect();
        (old, fresh)
    };
    let mut say = Vec::new();
    if old.tls_ok == 0 && stats.tls_ok > 0 {
        say.push("EOS proxy: the game's TLS to api.epicgames.dev was accepted (it trusts the lab CA); its requests are in eos-filter.log".to_owned());
    }
    if old.untrusted == 0 && stats.untrusted > 0 {
        say.push("EOS proxy: the game rejected the lab's certificate, so EOS is offline for this run (fail closed; nothing reached Epic)".to_owned());
    } else if old.tls_failed == 0 && stats.tls_failed > 0 && stats.untrusted == 0 {
        say.push("EOS proxy: a TLS handshake with the game failed (see eos-filter.log)".to_owned());
    }
    if old.allowed + old.blocked == 0 && stats.allowed + stats.blocked > 0 {
        say.push(format!("EOS proxy: the game's requests arrive ({} allowed, {} blocked so far)", stats.allowed, stats.blocked));
    }
    let mut killed = false;
    for c in fresh {
        let to = format!("{}:{} ({})", c.remote, c.port, c.state);
        if epic.contains(&c.remote) {
            let msg = format!("!!! BYPASS {to} is one of api.epicgames.dev's addresses and didn't go through the EOS proxy (a hard-coded address?); run killed");
            p.shared.log(&msg);
            sup.update(|st| {
                st.violations.push(format!("{} {msg}", Utc::now().to_rfc3339()));
                Some(msg.clone())
            });
            killed = true;
        } else {
            let msg = format!("!!! OUTSIDE {to}: a :443 connection that didn't go through the EOS proxy (not one of the addresses api.epicgames.dev has on this host; check what it is)");
            p.shared.log(&msg);
            say.push(msg);
        }
    }
    for n in say {
        sup.update(|st| {
            st.run.as_mut()?.notes.push(n.clone());
            Some(n)
        });
    }
    if killed {
        let _ = kill(sup, "supervisor", "violation");
    }
    killed
}

impl Sup {
    fn update(&self, f: impl FnOnce(&mut State) -> Option<String>) {
        let mut st = self.state.lock().unwrap();
        let before = st.clone();
        let event = f(&mut st);
        if *st != before {
            st.seq += 1;
            st.updated = Some(Utc::now());
            if let Ok(bytes) = serde_json::to_vec_pretty(&*st) {
                let _ = lab::write_atomic(&self.inst.state_path(), &bytes);
            }
        }
        if let Some(e) = event {
            let run = st.run.as_ref().map(|r| r.id);
            self.event(run, &e);
        }
    }

    fn event(&self, run: Option<u32>, what: &str) {
        let line = json!({ "at": Utc::now(), "run": run.map(|r| format!("r{r}")), "event": what });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.inst.events_path()) {
            let _ = writeln!(f, "{line}");
        }
        eprintln!("{} {}{what}", Utc::now().format("%F %T"), run.map(|r| format!("[r{r}] ")).unwrap_or_default());
    }
}

/// Run the supervisor in this process until the container stops.
pub fn supervise(lab: Lab, name: &str) -> Result<()> {
    let inst = lab.instance(name);
    let _m = inst.load()?;
    // One supervisor per instance.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(inst.supervisor_lock())?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("instance '{name}' already has a supervisor");
    }
    let _ = std::fs::remove_file(inst.socket());
    let listener = UnixListener::bind(inst.socket()).with_context(|| format!("bind {}", inst.socket().display()))?;
    let mut st: State = std::fs::read(inst.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    st.instance = name.to_owned();
    st.supervisor_pid = Some(std::process::id());
    st.container_up = ops::running_id(&inst.container()).is_some();
    st.display = ops::display(&inst);
    let (eos, eos_note) = match start_eos_proxy(&inst) {
        Ok(Some(p)) => {
            let note = format!("; EOS proxy on 127.0.0.1:{} (the container's api.epicgames.dev:443)", p.port);
            (Some(p), note)
        }
        Ok(None) => (None, "; no EOS proxy (a container from before it: quiver lab down/up)".into()),
        Err(e) => (None, format!("; the EOS proxy didn't start: {e:#}")),
    };
    let sup = Arc::new(Sup { lab, inst, state: Mutex::new(st), busy: Mutex::new(()), eos, net_seen: Mutex::default() });
    let _ = lab::write_atomic(&sup.inst.state_path(), &serde_json::to_vec_pretty(&*sup.state.lock().unwrap())?);
    sup.update(|_| Some(format!("supervisor {} started{eos_note}", std::process::id())));
    {
        let sup = sup.clone();
        std::thread::spawn(move || watch(&sup));
    }
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let sup = sup.clone();
        std::thread::spawn(move || {
            let quit = serve(&sup, conn);
            if quit {
                sup.update(|_| Some("supervisor stopping".into()));
                let _ = std::fs::remove_file(sup.inst.socket());
                std::process::exit(0);
            }
        });
    }
    Ok(())
}

/// One connection: a request line, a reply line. True: quit.
fn serve(sup: &Sup, conn: UnixStream) -> bool {
    let mut line = String::new();
    let mut r = BufReader::new(&conn);
    if r.read_line(&mut line).is_err() {
        return false;
    }
    let req: SupRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            reply(&conn, Err(anyhow::anyhow!("bad request: {e}")));
            return false;
        }
    };
    let quit = matches!(req, SupRequest::Quit);
    let result = match req {
        SupRequest::Ping => Ok(json!({ "pid": std::process::id() })),
        SupRequest::Quit => Ok(Value::Null),
        SupRequest::Launch { source, mod_dir, no_mod, args, clock } => launch(sup, &source, mod_dir, no_mod, &args, clock.as_deref()),
        SupRequest::Kill { source } => kill(sup, &source, "killed"),
    };
    reply(&conn, result);
    quit
}

fn reply(mut conn: &UnixStream, r: Result<Value>) -> bool {
    let rep = match r {
        Ok(v) => SupReply { ok: true, error: None, result: v },
        Err(e) => SupReply { ok: false, error: Some(format!("{e:#}")), result: Value::Null },
    };
    let mut out = serde_json::to_vec(&rep).unwrap_or_default();
    out.push(b'\n');
    let _ = conn.write_all(&out);
    true
}

fn kill(sup: &Sup, source: &str, how: &str) -> Result<Value> {
    let _busy = sup.busy.lock().unwrap();
    kill_locked(sup, source, how)
}

fn kill_locked(sup: &Sup, source: &str, how: &str) -> Result<Value> {
    let m = sup.inst.load()?;
    let id = ops::running_id(&sup.inst.container()).context("the instance's container isn't running")?;
    let notes = ops::kill_all(&sup.inst, &m, &id)?;
    let how = how.to_owned();
    let src = source.to_owned();
    sup.update(|st| {
        let run = st.run.as_mut()?;
        if run.phase == Phase::Exited {
            return None;
        }
        run.reach(Phase::Exited, Utc::now());
        run.exit = Some(Exit { at: Utc::now(), how: how.clone(), detail: format!("by {src}") });
        Some(format!("run {how} by {src}"))
    });
    Ok(json!({ "notes": notes }))
}

/// Next run id: one more than any run folder.
fn next_run(inst: &Inst) -> u32 {
    std::fs::read_dir(inst.runs())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('r')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn block_on<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(f)
}

/// Put a mod folder's files over the install, after putting back what the previous launch
/// replaced. Returns what was copied.
pub fn apply_mod(inst: &Inst, m: &Manifest, mod_dir: Option<&Path>) -> Result<Vec<String>> {
    let install = m.install_in(inst);
    let record = inst.dir.join("mod-files.txt");
    let orig = inst.originals();
    if let Ok(text) = std::fs::read_to_string(&record) {
        for rel in text.lines().filter(|l| !l.is_empty()) {
            let (o, live) = (orig.join(rel), install.join(rel));
            if o.exists() {
                std::fs::copy(&o, &live).with_context(|| format!("put back {rel}"))?;
            } else {
                let _ = std::fs::remove_file(&live);
            }
        }
        std::fs::remove_file(&record)?;
    }
    let Some(dir) = mod_dir else { return Ok(Vec::new()) };
    let mut copied = Vec::new();
    for (rel, is_dir) in quiver_core::lab::facts::walk(dir, 16, 100_000) {
        if is_dir {
            continue;
        }
        let rel_s = rel.to_string_lossy().into_owned();
        let live = install.join(&rel);
        if live.exists() && !orig.join(&rel).exists() {
            std::fs::create_dir_all(orig.join(&rel).parent().unwrap())?;
            std::fs::copy(&live, orig.join(&rel))?;
        }
        if let Some(p) = live.parent() {
            std::fs::create_dir_all(p)?;
        }
        // Copy, not link: the mod folder may be rebuilt while the game runs.
        std::fs::copy(dir.join(&rel), &live).with_context(|| format!("copy {rel_s}"))?;
        copied.push(rel_s);
    }
    std::fs::write(&record, copied.join("\n"))?;
    Ok(copied)
}

fn launch(sup: &Sup, source: &str, mod_dir: Option<PathBuf>, no_mod: bool, extra: &[String], clock: Option<&str>) -> Result<Value> {
    let _busy = sup.busy.lock().unwrap();
    let inst = &sup.inst;
    let mut m = inst.load()?;
    let container_id = ops::running_id(&inst.container()).with_context(|| format!("instance '{}' isn't up (quiver lab up {})", inst.name, inst.name))?;
    let client = block_on(quiver_api::Client::connect_running(&format!("lab:{}", inst.name))).ok();
    if m.licence != Licence::None && client.is_none() {
        bail!("quiverd isn't running: it mints this game's licence (start it with `quiver status`)");
    }
    // Fresh start: nothing of the last run left (a stale wineserver or bridge hangs the
    // next game before its SDK answers).
    let mut notes = kill_locked(sup, source, "killed")?["notes"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>();
    notes.extend(guard::enforce(&inst.prefix())?);
    let mod_dir = if no_mod { None } else { mod_dir.or(m.mod_dir.clone()) };
    // The virtual clock (lab::vclock): its DLL in the clone, its environment in `m` for this launch.
    notes.extend(lab::vclock::prepare(inst, &mut m, clock, mod_dir.as_deref())?);
    let copied = apply_mod(inst, &m, mod_dir.as_deref())?;
    if let Some(d) = &mod_dir {
        notes.push(format!("mod: {} file(s) from {}", copied.len(), d.display()));
    }
    if let Some(p) = &m.graphics {
        let file = sup.lab.profiles(&m.game.dir_name()).join(format!("{p}.set"));
        let text = std::fs::read_to_string(&file).with_context(|| format!("graphics profile {}", file.display()))?;
        match configset::apply(inst, &m, &configset::parse_lines(&text)?) {
            Ok(done) => notes.push(format!("graphics profile {p}: {} key(s)", done.len())),
            // A first launch may not have written the game's config yet.
            Err(e) => notes.push(format!("graphics profile {p} not applied: {e:#}")),
        }
    }
    // The EOS proxy (lab::eos): an EOS game's Epic traffic goes through it or the game doesn't
    // launch. The old secur32 filter (it never saw the SDK's TLS) is taken out of the clone.
    for p in eos::remove_old_filter(&m.install_in(inst)) {
        notes.push(format!("removed the old secur32.dll EOS filter {}", p.display()));
    }
    let eos_plan = eos::plan(&m.install_in(inst), m.licence == Licence::EpicExchangeCode);
    let run = next_run(inst);
    let run_dir = inst.run_dir(run);
    std::fs::create_dir_all(&run_dir)?;
    let session = quiver_api::lab::session_id(&inst.name, run);
    let run_win = format!(r"{}\runs\r{run}", lab::LAB_WIN);
    if let Some(plan) = &eos_plan {
        let ready = eos_ready(sup, &run_dir, run);
        if let (Err(e), Some(p)) = (&ready, &sup.eos) {
            p.shared.log(&format!("--- not ready: {e:#}"));
        }
        match ready {
            Ok(did) => notes.extend(did),
            Err(e) if plan.required => bail!(
                "refusing to launch: the game ships the EOS SDK ({}), which would sync cloud saves and report achievements and stats to the person's Epic account, and the lab's EOS proxy isn't in its path: {e:#}",
                plan.sdks.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
            Err(e) => notes.push(format!("EOS proxy not checked (no EOS SDK found in the game): {e:#}")),
        }
    }
    let run_mount = format!("{}/runs/r{run}", lab::LAB_MOUNT);
    // Sight (design/LAB.md §12): the session dir, the layer's manifests, the environment.
    let sight = super::sight::prepare(inst, &m, &session, &run_mount)?;
    if let Some(s) = &sight {
        notes.extend(s.notes.iter().cloned());
    }
    let hs = host::snapshot(&procs::scan(), &ops::containers());
    std::fs::write(run_dir.join("host.json"), serde_json::to_vec_pretty(&hs)?)?;
    notes.push(format!("host: {}", hs.summary()));
    let _ = std::fs::remove_file(inst.lab().join("state"));

    let mut bridge = None;
    let mut extra_args: Vec<String> = Vec::new();
    match m.licence {
        Licence::UbisoftBridge => {
            let bridge_exe = ops::lab_bridge()?;
            let product_id = m.game.as_ubisoft().context("a Ubisoft instance without a Ubisoft ref")?;
            let req = Request::LaunchMaterial {
                game: m.game,
                session: Some(session.clone()),
                savegame_path: None,
                install_dir: Some(m.install_in(inst)),
                ovt_dir: None,
                account: None,
            };
            let mat: LaunchMaterial = block_on(client.as_ref().unwrap().call(req))?;
            let LaunchMaterial::Ubisoft { mut context } = mat else { bail!("quiverd sent no Ubisoft launch context") };
            let account = context["account_id"].as_str().context("launch context without an account")?.to_owned();
            // The bridge reaches the broker through the port forwarded when the container booted.
            let fwd = std::fs::read_to_string(inst.dir.join("broker-port")).unwrap_or_default();
            let want = context["broker"].as_str().and_then(|b| b.rsplit(':').next()).unwrap_or_default().to_owned();
            if fwd.trim() != want {
                bail!("quiverd's broker is on port {want} but the container forwards {:?}; quiver lab down/up {} (QUIVER_LAB_BROKER_PORT={want})", fwd.trim(), inst.name);
            }
            context["savegame_path"] = quiver_core::steamrun::savegame_path(None, &account, product_id).into();
            quiver_core::private::write_private(&run_dir.join("context.json"), &serde_json::to_vec(&context)?)?;
            quiver_core::launcher::install_prefix_bridge(&inst.prefix(), &bridge_exe)?;
            bridge = Some((r"C:\quiver\quiver-bridge.exe".to_owned(), format!(r"{run_win}\context.json"), format!(r"{run_win}\bridge.log")));
            notes.push(format!("licence: Ubisoft bridge, session {session} (no play time, achievements or saves reported)"));
        }
        Licence::EpicExchangeCode => {
            let req = Request::LaunchMaterial { game: m.game, session: Some(session.clone()), savegame_path: None, install_dir: None, ovt_dir: Some(run_win.clone()), account: None };
            let mat: LaunchMaterial = block_on(client.as_ref().unwrap().call(req))?;
            let LaunchMaterial::Epic { args, shown, ownership_token } = mat else { bail!("quiverd sent no Epic launch material") };
            if let Some(t) = ownership_token {
                if !t.file_name.ends_with(".ovt") || t.file_name.contains(['/', '\\']) {
                    bail!("unexpected ownership token name {:?}", t.file_name);
                }
                let bytes = base64::engine::general_purpose::STANDARD.decode(&t.base64).context("ownership token")?;
                quiver_core::private::write_private(&run_dir.join(&t.file_name), &bytes)?;
            }
            notes.push(format!("licence: Epic exchange code ({})", shown.join(" ")));
            extra_args = args;
        }
        Licence::None => notes.push("licence: none (DRM-free)".into()),
    }
    extra_args.extend(extra.iter().cloned());

    let mut env: Vec<(String, String)> = vec![
        ("WINEDEBUG".into(), "-all".into()),
        ("DXVK_LOG_PATH".into(), run_mount.clone()),
        ("VKD3D_LOG_FILE".into(), format!("{run_mount}/vkd3d.log")),
        ("PROTON_LOG_DIR".into(), run_mount.clone()),
        ("__GL_SHADER_DISK_CACHE".into(), "1".into()),
        ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".into(), "1".into()),
        ("__GL_SHADER_DISK_CACHE_PATH".into(), format!("{}/cache/nv", lab::LAB_MOUNT)),
        ("SteamAppId".into(), "0".into()),
        ("SteamGameId".into(), "0".into()),
        ("QUIVER_LAB".into(), lab::LAB_WIN.into()),
        ("QUIVER_LAB_RUN".into(), format!("r{run}")),
        ("QUIVER_LAB_RUN_DIR".into(), run_win.clone()),
        ("QUIVER_SESSION".into(), session.clone()),
        ("QUIVER_GAME".into(), m.game.to_string()),
    ];
    // HDR (design/LAB.md §13): gamescope's WSI layer and Proton's HDR switch; the manifest's
    // env can override them.
    if m.display.hdr {
        env.extend(lab::hdr::launch_env());
        notes.push("HDR: gamescope's output is HDR10 PQ; the game gets ENABLE_GAMESCOPE_WSI=1 and PROTON_ENABLE_HDR=1 (quiver lab hdr)".into());
    }
    for (k, v) in &m.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }
    // Sight's after the manifest's (it merges the manifest's LD_PRELOAD and layer paths).
    for (k, v) in sight.as_ref().map(|s| s.env.clone()).unwrap_or_default() {
        env.retain(|(ek, _)| *ek != k);
        env.push((k, v));
    }
    let overrides: Vec<String> = m.dll_overrides.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let base = env.iter().find(|(k, _)| k == "WINEDLLOVERRIDES").map(|(_, v)| v.clone()).into_iter().chain((!overrides.is_empty()).then(|| overrides.join(";")));
    let base: Vec<String> = base.collect();
    env.retain(|(k, _)| k != "WINEDLLOVERRIDES");
    // Connect's executables are disabled for every launch (the guard's third layer).
    env.push(("WINEDLLOVERRIDES".into(), quiver_core::steamrun::dll_overrides_without_connect((!base.is_empty()).then(|| base.join(";")).as_deref())));
    std::fs::create_dir_all(inst.lab().join("cache/nv"))?;
    std::fs::create_dir_all(inst.lab().join("req"))?;
    std::fs::create_dir_all(inst.lab().join("rsp"))?;

    let br = bridge.as_ref().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()));
    let cmd = podman::launch_cmd(&m, br, &extra_args, &format!("{run_mount}/game.log"));
    let workdir = m.paths.install.join(&m.paths.exe).parent().map(Path::to_path_buf).unwrap_or(m.paths.install.clone());
    let started = Utc::now();
    let mut r = Run::new(run, session.clone(), started);
    r.notes = notes.clone();
    sup.update(|st| {
        st.container_up = true;
        st.run = Some(r);
        Some(format!("launch r{run} by {source} ({})", m.licence.describe()))
    });
    // The run's quiver-sight first: it waits for the layer's ring.
    if let Some(s) = &sight
        && let Err(e) = ops::podman_ok(&podman::exec_args(&inst.container(), true, None, &[], &s.cmd))
    {
        bail!("starting the run's quiver-sight: {e:#}");
    }
    let args = podman::exec_args(&inst.container(), true, Some(&workdir), &env, &cmd);
    if let Err(e) = ops::podman_ok(&args) {
        sup.update(|st| {
            let run = st.run.as_mut()?;
            run.reach(Phase::Exited, Utc::now());
            run.exit = Some(Exit { at: Utc::now(), how: "failed".into(), detail: format!("{e:#}") });
            Some(format!("launch failed: {e:#}"))
        });
        return Err(e);
    }
    let _ = container_id;
    Ok(json!({ "run": run, "session": session, "run_dir": run_dir, "notes": notes }))
}

/// The watch loop: the readiness ladder, the mod's state, exits, Connect.
fn watch(sup: &Sup) {
    let inst = &sup.inst;
    let mut id: Option<String> = None;
    let mut last_id_check = Instant::now() - Duration::from_secs(10);
    let mut down_since: Option<Instant> = None;
    let mut misses = 0u32;
    let mut last_window = Instant::now() - Duration::from_secs(10);
    let mut last_frame = Instant::now() - Duration::from_secs(10);
    let mut state_mtime = None;
    let exe = inst.load().map(|m| m.exe_name()).unwrap_or_default();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if last_id_check.elapsed() > Duration::from_secs(2) || id.is_none() {
            id = ops::running_id(&inst.container());
            last_id_check = Instant::now();
            let up = id.is_some();
            let display = if up { ops::display(inst) } else { None };
            sup.update(|st| {
                let changed = st.container_up != up;
                st.container_up = up;
                if display.is_some() {
                    st.display = display;
                }
                if !up && let Some(r) = st.run.as_mut().filter(|r| r.phase != Phase::Exited) {
                    r.reach(Phase::Exited, Utc::now());
                    r.exit = Some(Exit { at: Utc::now(), how: "container stopped".into(), detail: String::new() });
                }
                changed.then(|| if up { "container up".into() } else { "container down".into() })
            });
            if up {
                down_since = None;
            } else if down_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                sup.update(|_| Some("container gone; supervisor exits".into()));
                let _ = std::fs::remove_file(inst.socket());
                std::process::exit(0);
            }
        }
        let Some(cid) = id.clone() else { continue };
        let (phase, started) = {
            let st = sup.state.lock().unwrap();
            match &st.run {
                Some(r) if r.phase != Phase::Exited => (r.phase, r.started),
                _ => continue,
            }
        };
        if sup.busy.try_lock().is_err() {
            continue; // a launch or kill is under way
        }
        let all = procs::scan();
        let connect = procs::connect_in(&all, &cid);
        if !connect.is_empty() {
            let what: Vec<String> = connect.iter().map(|p| format!("{} (pid {})", p.program(), p.pid)).collect();
            let msg = format!("Ubisoft Connect started in the instance ({}); run killed", what.join(", "));
            sup.update(|st| {
                st.violations.push(format!("{} {msg}", Utc::now().to_rfc3339()));
                Some(msg.clone())
            });
            let _ = kill(sup, "supervisor", "violation");
            continue;
        }
        if let Some(p) = &sup.eos
            && eos_watch(sup, p, &all, &cid)
        {
            continue;
        }
        let game = procs::instance_game(&all, &cid, &exe);
        let pids: Vec<u32> = game.iter().map(|p| p.pid).collect();
        if !pids.is_empty() {
            misses = 0;
            sup.update(|st| {
                let r = st.run.as_mut()?;
                let mut ev = None;
                if r.reach(Phase::Process, Utc::now()) {
                    ev = Some(format!("process up (pid {})", pids[0]));
                }
                if r.pids != pids {
                    r.pids = pids.clone();
                }
                ev
            });
        } else if phase >= Phase::Process {
            misses += 1;
            if misses >= 4 {
                misses = 0;
                let wine_left = procs::instance_wine(&all, &cid).len();
                sup.update(|st| {
                    let r = st.run.as_mut()?;
                    r.reach(Phase::Exited, Utc::now());
                    r.pids.clear();
                    r.exit = Some(Exit { at: Utc::now(), how: "exited".into(), detail: format!("{wine_left} Wine process(es) left; the next launch clears them") });
                    Some("game exited".into())
                });
                continue;
            }
        } else if (Utc::now() - started).num_seconds() > 45 && procs::instance_wine(&all, &cid).is_empty() {
            sup.update(|st| {
                let r = st.run.as_mut()?;
                r.reach(Phase::Exited, Utc::now());
                r.exit = Some(Exit { at: Utc::now(), how: "failed to start".into(), detail: "no Wine process in the instance; see the run's game.log".into() });
                Some("launch failed: nothing is running (see game.log)".into())
            });
            continue;
        }
        if phase >= Phase::Process && phase < Phase::Window && last_window.elapsed() > Duration::from_secs(1) {
            last_window = Instant::now();
            if ops::focused_window(inst).is_some_and(|w| w != 0) {
                sup.update(|st| st.run.as_mut()?.reach(Phase::Window, Utc::now()).then(|| "window mapped".into()));
            }
        }
        if phase == Phase::Window && last_frame.elapsed() > Duration::from_millis(1500) {
            last_frame = Instant::now();
            let probe = inst.dir.join(".probe.png");
            if let Ok(img) = ops::shot_gray(inst, &probe)
                && !quiver_core::lab::image::is_blank(&img)
            {
                sup.update(|st| st.run.as_mut()?.reach(Phase::Frame, Utc::now()).then(|| "first frame".into()));
            }
        }
        // The mod's state (written after this run started).
        let sf = inst.lab().join("state");
        let mtime = sf.metadata().and_then(|m| m.modified()).ok();
        if mtime != state_mtime {
            state_mtime = mtime;
            if let Some(t) = mtime
                && chrono::DateTime::<Utc>::from(t) >= started
                && let Some(ms) = std::fs::read_to_string(&sf).ok().and_then(|text| quiver_core::lab::ready::parse_mod_state(&text, Utc::now()))
            {
                sup.update(|st| {
                    let word = ms.state.clone();
                    st.run.as_mut()?.set_mod_state(ms).then(|| format!("mod state: {word}"))
                });
            }
        }
    }
}

/// Ask a supervisor to stop, and wait (a few seconds at most) until it has.
pub fn quit(inst: &Inst) -> Result<()> {
    if let Ok(conn) = UnixStream::connect(inst.socket()) {
        let mut line = serde_json::to_vec(&SupRequest::Quit)?;
        line.push(b'\n');
        let _ = (&conn).write_all(&line);
        let mut rsp = String::new();
        let _ = BufReader::new(&conn).read_line(&mut rsp);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while alive(inst) {
        if Instant::now() > deadline {
            bail!("the instance's old supervisor didn't stop (see {})", inst.supervisor_log().display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Talk to an instance's supervisor, starting it if the container is up and it isn't.
pub fn request(lab: &Lab, inst: &Inst, req: &SupRequest) -> Result<Value> {
    let conn = match UnixStream::connect(inst.socket()) {
        Ok(c) => c,
        Err(_) => {
            if ops::running_id(&inst.container()).is_none() {
                bail!("instance '{}' isn't up (quiver lab up {})", inst.name, inst.name);
            }
            if !alive(inst) {
                spawn(lab, inst)?;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(c) = UnixStream::connect(inst.socket()) {
                    break c;
                }
                if Instant::now() > deadline {
                    bail!("the supervisor didn't start (see {})", inst.supervisor_log().display());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    (&conn).write_all(&line)?;
    let mut rsp = String::new();
    BufReader::new(&conn).read_line(&mut rsp)?;
    let rep: SupReply = serde_json::from_str(&rsp).context("the supervisor's reply")?;
    match rep.error {
        Some(e) if !rep.ok => bail!(e),
        _ => Ok(rep.result),
    }
}

/// Whether a supervisor holds the instance's lock.
pub fn alive(inst: &Inst) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(inst.supervisor_lock()) else { return false };
    let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        return false;
    }
    true
}

/// Start the supervisor detached (its own session; output to `supervisor.log`).
pub fn spawn(lab: &Lab, inst: &Inst) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(inst.supervisor_log())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["lab", "supervise", &inst.name])
        .env("QUIVER_LAB_ROOT", &lab.root)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start the supervisor")?;
    Ok(())
}

/// Is `store`'s licence something the lab can get? (`Err`: why not.)
pub fn licence_for(store: StoreId, drm_free: bool) -> std::result::Result<Licence, String> {
    match store {
        StoreId::Ubisoft => Ok(Licence::UbisoftBridge),
        StoreId::Epic => Ok(Licence::EpicExchangeCode),
        _ if drm_free => Ok(Licence::None),
        StoreId::Steam => Err("it uses Steam's DRM (Steamworks or SteamStub), which needs the user's running Steam client; a lab never \
                               talks to it (a game would show as running in their Steam, count play time and sync its saves to \
                               Steam Cloud at exit). Only DRM-free Steam games run in the lab (design/LAB.md \"Steam\")"
            .into()),
        s => Err(format!("{} games aren't supported in the lab yet", s.label())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licences() {
        assert_eq!(licence_for(StoreId::Ubisoft, false), Ok(Licence::UbisoftBridge));
        assert_eq!(licence_for(StoreId::Epic, true), Ok(Licence::EpicExchangeCode));
        assert_eq!(licence_for(StoreId::Steam, true), Ok(Licence::None));
        assert!(licence_for(StoreId::Steam, false).unwrap_err().contains("Steam Cloud"));
    }

    #[test]
    fn mods_go_in_and_come_back_out() {
        let t = tempfile::tempdir().unwrap();
        let lab = Lab::new(t.path().into());
        let inst = lab.instance("t1");
        let m: Manifest = toml::from_str(
            "name='t1'\ngame='ubisoft:3539'\ntitle='x'\nlicence='ubisoft-bridge'\ncreated='2026-11-18T10:00:00Z'\n[display]\nwidth=1280\nheight=720\nrefresh=60\n[runtime]\nproton='p'\nproton_path='/p'\nimage='i'\n[paths]\ninstall='/g'\nprefix='/pf'\nexe='g.exe'\n"
        )
        .unwrap();
        std::fs::create_dir_all(inst.install()).unwrap();
        std::fs::write(inst.install().join("dxgi.dll"), b"orig").unwrap();
        let md = t.path().join("mod");
        std::fs::create_dir_all(md.join("sub")).unwrap();
        std::fs::write(md.join("dxgi.dll"), b"mod").unwrap();
        std::fs::write(md.join("sub/mod.ini"), b"[a]").unwrap();
        let c = apply_mod(&inst, &m, Some(&md)).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(std::fs::read(inst.install().join("dxgi.dll")).unwrap(), b"mod");
        // The next launch without the mod: originals back, added files gone.
        assert!(apply_mod(&inst, &m, None).unwrap().is_empty());
        assert_eq!(std::fs::read(inst.install().join("dxgi.dll")).unwrap(), b"orig");
        assert!(!inst.install().join("sub/mod.ini").exists());
    }

    #[test]
    fn requests_round_trip() {
        let r: SupRequest = serde_json::from_str(r#"{"op":"launch","source":"agent"}"#).unwrap();
        assert!(matches!(r, SupRequest::Launch { no_mod: false, .. }));
        assert_eq!(next_run(&Lab::new("/nonexistent".into()).instance("x")), 1);
    }
}
