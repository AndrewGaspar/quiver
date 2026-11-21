//! Remote play's streams in quiverd (design/REMOTE.md §2.1, §5): `StartStream`, the host
//! process per stream, and the stream sessions `Status` and the app show.
//!
//! - **A host process per stream**: `quiver-stream-host serve`, in its own scope
//!   (`systemd-run --user --scope`, so it lives outside quiverd's cgroup), its media socket
//!   bound here from [`remote::STREAM_PORT`] up (or `remote.stream_port`) and passed as
//!   fd 3, the spec with the first session's keys as the first line on its stdin
//!   ([`quiver_stream_proto::serve`]). Every `StartStream` gets fresh keys
//!   ([`StreamTicket::new`]).
//! - **One stream per target.** A target already streaming to another machine is refused.
//!   One whose client left (kept for [`KEEP_SECS`]) resumes: the host gets new keys and
//!   starts a new session, the game untouched. The same machine asking again takes its own
//!   stream over.
//! - **Games** run in the host's virtual display ([`ServeTarget::Display`]): once it is
//!   up, quiverd launches the game exactly as `Play` does (launch plan, profile, bridge,
//!   Sight, tweaks), with the host's [`LaunchEnv`] (its display, its sink, the pad
//!   namespace) in the plan ([`take_launch`]). The game exiting ends the stream; the client's
//!   Quit game ends the game.
//! - **Restarts.** The host isn't quiverd's: an exec onto a new build leaves it running.
//!   Each stream has a folder under `$XDG_RUNTIME_DIR/quiver/streams/<id>` with its record
//!   (`stream.json`) and the host's `control.sock`; on start quiverd re-adopts every live one
//!   ([`readopt`]).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use quiver_api::remote::{self, ClientCaps, LaunchEnv, StartStreamReply, StreamPrefs, StreamSession, StreamState, StreamTarget, StreamTicket};
use quiver_api::{Event, GameRef, StoreId};
use quiver_stream_proto::serve::{KEEP_SECS, ServeIn, ServeOut, ServeSpec, ServeState, ServeTarget};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::Daemon;
use crate::remote::{Coded, RemotePeer};

/// Launch environments for games about to start into a stream's display, by game; the
/// launch plan takes its game's ([`take_launch`]).
static LAUNCH: LazyLock<Mutex<HashMap<GameRef, LaunchEnv>>> = LazyLock::new(Default::default);

/// The stream launch environment waiting for `game`'s next launch, if a stream is starting it.
pub(crate) fn take_launch(game: GameRef) -> Option<LaunchEnv> {
    LAUNCH.lock().unwrap().remove(&game)
}

/// What quiverd keeps about a stream, in its folder (`stream.json`) for a restarted quiverd.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    session: StreamSession,
    key: String,
    #[serde(default)]
    profile: Option<String>,
}

struct Entry {
    rec: Record,
    tx: mpsc::UnboundedSender<ServeIn>,
    /// Set until the host has said it is listening (and, for a game, its display is up).
    ready: Option<oneshot::Sender<Result<Option<LaunchEnv>, String>>>,
    listening: bool,
}

#[derive(Default)]
pub(crate) struct Streams {
    entries: Mutex<HashMap<String, Entry>>,
}

/// The streams' folder.
fn root() -> PathBuf {
    quiver_api::socket_path().parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir).join("streams")
}

impl Streams {
    pub fn list(&self) -> Vec<StreamSession> {
        let mut v: Vec<StreamSession> = self.entries.lock().unwrap().values().map(|e| e.rec.session.clone()).collect();
        v.sort_by_key(|s| s.started_at);
        v
    }

    fn save(e: &Entry) {
        let path = root().join(&e.rec.session.id).join("stream.json");
        if let Ok(text) = serde_json::to_vec_pretty(&e.rec) {
            let _ = quiver_core::private::write_private(&path, &text);
        }
    }
}

/// What a target is, as quiverd runs it.
struct Plan {
    key: String,
    serve: ServeTarget,
    title: String,
    game: Option<GameRef>,
    profile: Option<String>,
}

fn not_implemented(error: impl Into<String>) -> anyhow::Error {
    Coded { code: remote::NOT_IMPLEMENTED, error: error.into() }.into()
}

async fn plan(d: &Arc<Daemon>, target: &StreamTarget) -> Result<Plan> {
    Ok(match target {
        StreamTarget::TestPattern => Plan { key: "test-pattern".into(), serve: ServeTarget::TestPattern { motion: true }, title: "Test pattern".into(), game: None, profile: None },
        StreamTarget::Game { game, profile } => {
            let g = GameRef::parse(game).with_context(|| format!("'{game}' isn't a game ref (ubisoft:<id>, epic:<app>, …)"))?;
            if g.store == StoreId::Steam && crate::testgames::find(g).is_none() {
                bail!("{g}: Steam games start through Steam, which a stream can't put in its display yet (design/REMOTE.md §6: Steam on headless)");
            }
            let title = match crate::testgames::find(g) {
                Some(t) => t.name,
                None => d.game_name(g).await,
            };
            Plan { key: format!("game:{g}"), serve: ServeTarget::Display, title, game: Some(g), profile: profile.clone() }
        }
        StreamTarget::LabInstance { .. } => {
            return Err(not_implemented(
                "streaming a lab instance isn't built: its gamescope runs inside the instance's container, which has no PipeWire for the \
                 screencast capture reads and keeps its X display in the container's own network namespace, where the host's input \
                 can't reach it; it needs a capture and an injector inside the container (quiver lab exec), under the lab's limits",
            ));
        }
        StreamTarget::Desktop => return Err(not_implemented("streaming this machine's desktop isn't built (design/REMOTE.md phase 4)")),
    })
}

/// The address the client reaches this machine on: the one its control connection came in
/// on, else loopback (a local client).
fn reach_ip(peer: Option<&RemotePeer>) -> IpAddr {
    peer.and_then(|p| p.local).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn peer_name(peer: Option<&RemotePeer>) -> String {
    peer.map(|p| p.name.clone()).unwrap_or_else(|| "This PC".into())
}

/// A free media port from `base` up, skipping the ones streams here hold.
fn bind_port(base: u16, taken: &[u16]) -> Result<UdpSocket> {
    for port in base..base.saturating_add(64) {
        if taken.contains(&port) {
            continue;
        }
        if let Ok(s) = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)) {
            return Ok(s);
        }
    }
    bail!("no free UDP port for the stream in {base}–{}", base.saturating_add(63))
}

/// `quiver-stream-host`: `QUIVER_STREAM_HOST_BIN`, beside quiverd, else on `PATH`.
fn host_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("QUIVER_STREAM_HOST_BIN") {
        return p.into();
    }
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
        && dir.join("quiver-stream-host").is_file()
    {
        return dir.join("quiver-stream-host");
    }
    let on_path = std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("quiver-stream-host")).find(|p| p.is_file()));
    on_path.unwrap_or_else(|| "quiver-stream-host".into())
}

/// Whether to run hosts in a scope of their own: `systemd-run` exists and the user's
/// manager is reachable (`QUIVER_STREAM_SCOPE=0` turns it off).
fn use_scope() -> bool {
    if std::env::var("QUIVER_STREAM_SCOPE").is_ok_and(|v| v == "0") {
        return false;
    }
    let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| Path::new(&d).join("bus").exists());
    bus && Path::new("/usr/bin/systemd-run").exists()
}

/// The host's command line: nothing secret in it.
fn host_argv(id: &str, scope: bool) -> Vec<String> {
    let mut a = Vec::new();
    if scope {
        a.extend(["/usr/bin/systemd-run", "--user", "--scope", "--collect", "--quiet"].map(String::from));
        a.push(format!("--unit=quiver-stream-{id}"));
        a.push(format!("--slice={}", quiver_api::service::SLICE));
        // systemd-run reaches the user manager through the real runtime folder; the host
        // keeps this quiverd's (a scratch daemon's has its own).
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            a.push(format!("--setenv=XDG_RUNTIME_DIR={}", rt.to_string_lossy()));
        }
        a.push("--".into());
    }
    a.push(host_exe().display().to_string());
    a.extend(["serve", "--socket-fd", "3"].map(String::from));
    a
}

/// `Request::StartStream`.
pub(crate) async fn start(d: &Arc<Daemon>, source: &str, peer: Option<RemotePeer>, target: StreamTarget, prefs: StreamPrefs, caps: ClientCaps) -> Result<StartStreamReply> {
    let p = plan(d, &target).await?;
    let who = peer_name(peer.as_ref());
    let ip = reach_ip(peer.as_ref());
    let host_name = d.remote.machine_name();
    // A stream of this target already: resume (or take over our own), else refuse.
    let resume = {
        let mut entries = d.streams.entries.lock().unwrap();
        match entries.values_mut().find(|e| e.rec.key == p.key) {
            Some(e) if e.rec.session.state == StreamState::Streaming && e.rec.session.peer != who => {
                bail!("{} is streaming to {} now", e.rec.session.title, e.rec.session.peer);
            }
            Some(e) if e.listening => {
                let ticket = StreamTicket::new(SocketAddr::new(ip, e.rec.session.port));
                let keys = keys_hex(&ticket);
                e.tx.send(ServeIn::Session { keys }).map_err(|_| anyhow::anyhow!("the stream's host has gone"))?;
                e.rec.session.state = StreamState::Starting;
                e.rec.session.peer = who.clone();
                e.rec.session.kept_until = None;
                Streams::save(e);
                Some((ticket, e.rec.session.title.clone()))
            }
            Some(_) => bail!("that stream is still starting"),
            None => None,
        }
    };
    if let Some((ticket, title)) = resume {
        d.log_activity_game(source, format!("resumed streaming {title} to {who}"), p.game);
        d.emit(Event::StreamsChanged);
        return Ok(StartStreamReply { ticket, host_name });
    }

    let id = format!("s{:08x}", rand_u32());
    let dir = root().join(&id);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let base = d.cfg.remote.stream_port.unwrap_or(remote::STREAM_PORT);
    let taken: Vec<u16> = d.streams.entries.lock().unwrap().values().map(|e| e.rec.session.port).collect();
    let sock = bind_port(base, &taken)?;
    let port = sock.local_addr()?.port();
    let ticket = StreamTicket::new(SocketAddr::new(ip, port));
    let spec = ServeSpec {
        target: p.serve.clone(),
        keys: keys_hex(&ticket),
        caps,
        prefs,
        host_name: host_name.clone(),
        session: id.clone(),
        dir: dir.clone(),
        listen: None,
        keep_secs: KEEP_SECS,
        connect_secs: 60,
        headless_server: false,
        transport: None,
    };
    let scope = use_scope();
    let argv = host_argv(&id, scope);
    let log = std::fs::File::create(dir.join("host.log"))?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    if scope {
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{}", unsafe { libc::getuid() }));
    }
    cmd.args(&argv[1..]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(log).kill_on_drop(false);
    let fd = sock.as_raw_fd();
    // SAFETY: dup2, fcntl and setsid are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if fd == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            } else if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if !scope {
                libc::setsid();
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("start {}", argv.join(" ")))?;
    drop(sock);
    let pid = child.id().unwrap_or(0);
    let mut stdin = child.stdin.take().context("the host's stdin")?;
    let stdout = child.stdout.take().context("the host's stdout")?;
    let mut first = serde_json::to_string(&ServeIn::Start(Box::new(spec)))?;
    first.push('\n');
    stdin.write_all(first.as_bytes()).await.context("give the host its spec")?;
    let session = StreamSession {
        id: id.clone(),
        target: target.clone(),
        title: p.title.clone(),
        game: p.game.map(|g| g.to_string()),
        peer: who.clone(),
        state: StreamState::Starting,
        started_at: Utc::now(),
        kept_until: None,
        port,
        pid,
    };
    let (tx, rx) = mpsc::unbounded_channel::<ServeIn>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let entry = Entry { rec: Record { session, key: p.key.clone(), profile: p.profile.clone() }, tx, ready: Some(ready_tx), listening: false };
    Streams::save(&entry);
    d.streams.entries.lock().unwrap().insert(id.clone(), entry);
    d.emit(Event::StreamsChanged);
    attach(d, &id, Box::new(stdin), Box::new(stdout), rx);
    // Reap it (this process is its parent until an exec onto a new build).
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let launch = match tokio::time::timeout(Duration::from_secs(45), ready_rx).await {
        Ok(Ok(Ok(l))) => l,
        Ok(Ok(Err(why))) => bail!("the stream host didn't start: {why}"),
        _ => {
            stop(d, &id, "it didn't start in time");
            bail!("the stream host didn't start in time; see {}", dir.join("host.log").display());
        }
    };
    d.log_activity_game(source, format!("streaming {} to {who} (stream {id}, port {port})", p.title), p.game);
    if let (Some(game), Some(launch)) = (p.game, launch) {
        // Into the host's display, as Play launches it.
        LAUNCH.lock().unwrap().insert(game, launch);
        let job = match d.play(source, game, p.profile.clone()).await {
            Ok(j) => j,
            Err(e) => {
                LAUNCH.lock().unwrap().remove(&game);
                stop(d, &id, "the game didn't start");
                return Err(e.context(format!("streaming {}", p.title)));
            }
        };
        LAUNCH.lock().unwrap().remove(&game);
        watch_game(d, &id, job, game);
    }
    Ok(StartStreamReply { ticket, host_name })
}

/// A streamed launch that couldn't give its processes the stream's pad group: the host
/// removes the pads and tells the client, and the activity log says why.
pub(crate) fn pad_problem(d: &Daemon, source: &str, game: GameRef) -> Option<String> {
    let why = quiver_core::launcher::take_stream_pad_problem(game)?;
    d.log_activity_game(source, format!("no pads in the stream of {game}: {why}"), Some(game));
    Some(why)
}

/// The game's Play job ending ends its stream; a pad problem in its launch turns the
/// stream's pads off.
fn watch_game(d: &Arc<Daemon>, id: &str, job: u64, game: GameRef) {
    let (weak, id) = (Arc::downgrade(d), id.to_owned());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(d) = weak.upgrade() else { return };
            if !d.streams.entries.lock().unwrap().contains_key(&id) {
                return;
            }
            if let Some(why) = pad_problem(&d, "system", game)
                && let Some(e) = d.streams.entries.lock().unwrap().get(&id)
            {
                let _ = e.tx.send(ServeIn::PadsOff { reason: why });
            }
            let done = d.jobs.lock().unwrap().get(&job).is_none_or(|j| j.done);
            if done {
                stop(&d, &id, "the game exited");
                return;
            }
        }
    });
}

fn keys_hex(t: &StreamTicket) -> String {
    let mut s: String = t.session_id.to_be_bytes().iter().map(|b| format!("{b:02x}")).collect();
    s.extend(t.key.iter().map(|b| format!("{b:02x}")));
    s
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    u32::from_le_bytes(b) ^ std::process::id()
}

/// Tell stream `id`'s host to end.
pub(crate) fn stop(d: &Arc<Daemon>, id: &str, reason: &str) {
    if let Some(e) = d.streams.entries.lock().unwrap().get(id) {
        let _ = e.tx.send(ServeIn::Stop { reason: reason.to_owned() });
    }
}

/// `Request::StopStream`: by id, or by the game it streams.
pub(crate) async fn stop_request(d: &Arc<Daemon>, source: &str, id: &str) -> Result<()> {
    let found = {
        let entries = d.streams.entries.lock().unwrap();
        entries.values().find(|e| e.rec.session.id == id || e.rec.session.game.as_deref() == Some(id)).map(|e| (e.rec.session.id.clone(), e.rec.session.title.clone(), e.rec.session.game.clone()))
    };
    let Some((sid, title, game)) = found else { bail!("no stream {id}") };
    stop(d, &sid, "stopped on the server");
    if let Some(g) = game.as_deref().and_then(|s| GameRef::parse(s).ok()) {
        stop_game(d, source, g).await;
    }
    d.log_activity(source, format!("stopped streaming {title}"), None);
    Ok(())
}

async fn stop_game(d: &Arc<Daemon>, source: &str, game: GameRef) {
    if let Err(e) = Box::pin(d.handle(source, quiver_api::Request::Stop { game: game.into() })).await {
        tracing::warn!("streams: stopping {game}: {e:#}");
    }
}

/// Read the host's lines (stdout, or `control.sock` after a restart) and write it commands.
fn attach(d: &Arc<Daemon>, id: &str, mut w: Box<dyn tokio::io::AsyncWrite + Send + Unpin>, r: Box<dyn tokio::io::AsyncRead + Send + Unpin>, mut rx: mpsc::UnboundedReceiver<ServeIn>) {
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let mut line = serde_json::to_string(&m).unwrap_or_default();
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                return;
            }
        }
    });
    let (weak, id) = (Arc::downgrade(d), id.to_owned());
    tokio::spawn(async move {
        let mut lines = BufReader::new(r).lines();
        let mut reason = "the stream host exited".to_owned();
        while let Ok(Some(l)) = lines.next_line().await {
            let Ok(ev) = serde_json::from_str::<ServeOut>(&l) else { continue };
            let Some(d) = weak.upgrade() else { return };
            if let ServeOut::Ended { reason: r } = &ev {
                reason = r.clone();
            }
            event(&d, &id, ev).await;
        }
        if let Some(d) = weak.upgrade() {
            ended(&d, &id, &reason).await;
        }
    });
}

async fn event(d: &Arc<Daemon>, id: &str, ev: ServeOut) {
    let mut quit_game = None;
    {
        let mut entries = d.streams.entries.lock().unwrap();
        let Some(e) = entries.get_mut(id) else { return };
        let s = &mut e.rec.session;
        match ev {
            ServeOut::Listening { port } => {
                s.port = port;
                e.listening = true;
                if !matches!(s.target, StreamTarget::Game { .. })
                    && let Some(r) = e.ready.take()
                {
                    let _ = r.send(Ok(None));
                }
            }
            ServeOut::Ready { launch } => {
                if let Some(r) = e.ready.take() {
                    let _ = r.send(Ok(Some(launch)));
                }
            }
            ServeOut::Connected { peer } => {
                tracing::info!("stream {id}: {} connected from {peer}", s.peer);
                s.state = StreamState::Streaming;
                s.kept_until = None;
            }
            ServeOut::Disconnected { reason, keep_secs } => {
                tracing::info!("stream {id}: {} left ({reason}); kept {keep_secs} s", s.peer);
                s.state = StreamState::Kept;
                s.kept_until = Some(Utc::now() + chrono::Duration::seconds(keep_secs as i64));
            }
            ServeOut::Status { state, .. } => {
                s.state = match state {
                    ServeState::Waiting => StreamState::Starting,
                    ServeState::Streaming => StreamState::Streaming,
                    ServeState::Kept => StreamState::Kept,
                };
                e.listening = true;
            }
            ServeOut::QuitGame => quit_game = s.game.as_deref().and_then(|s| GameRef::parse(s).ok()),
            ServeOut::Notice { text } => tracing::info!("stream {id}: {text}"),
            ServeOut::Ended { .. } => {}
        }
        Streams::save(e);
    }
    d.emit(Event::StreamsChanged);
    if let Some(g) = quit_game {
        let who = d.streams.entries.lock().unwrap().get(id).map(|e| e.rec.session.peer.clone()).unwrap_or_default();
        d.log_activity_game(&remote::remote_source(&who), format!("quit {g} from the stream"), Some(g));
        stop_game(d, &remote::remote_source(&who), g).await;
    }
}

async fn ended(d: &Arc<Daemon>, id: &str, reason: &str) {
    let gone = d.streams.entries.lock().unwrap().remove(id);
    let Some(mut e) = gone else { return };
    if let Some(r) = e.ready.take() {
        let tail = std::fs::read_to_string(root().join(id).join("host.log")).unwrap_or_default();
        let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(reason).to_owned();
        let _ = r.send(Err(format!("{reason} ({last})")));
    }
    tracing::info!("stream {id} ({}) ended: {reason}", e.rec.session.title);
    let _ = std::fs::remove_dir_all(root().join(id));
    // A game the stream ran goes with it (the host's display is gone).
    if let Some(g) = e.rec.session.game.as_deref().and_then(|s| GameRef::parse(s).ok())
        && reason != "the game exited"
    {
        stop_game(d, "system", g).await;
    }
    d.log_activity_game("system", format!("stream of {} ended: {reason}", e.rec.session.title), e.rec.session.game.as_deref().and_then(|s| GameRef::parse(s).ok()));
    d.emit(Event::StreamsChanged);
}

/// After a restart: attach to every stream whose host still runs; forget the rest.
pub(crate) fn readopt(d: &Arc<Daemon>) {
    let Ok(dirs) = std::fs::read_dir(root()) else { return };
    for dir in dirs.flatten() {
        let path = dir.path();
        let rec: Option<Record> = std::fs::read(path.join("stream.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let Some(rec) = rec else {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        };
        let alive = rec.session.pid > 0 && unsafe { libc::kill(rec.session.pid as i32, 0) } == 0;
        let conn = alive
            .then(|| {
                let (addr, _dirfd) = quiver_stream_proto::serve::socket_path(&path, "control.sock").ok()?;
                std::os::unix::net::UnixStream::connect(addr).ok()
            })
            .flatten();
        let Some(conn) = conn else {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        };
        let _ = conn.set_nonblocking(true);
        let Ok(conn) = tokio::net::UnixStream::from_std(conn) else { continue };
        let id = rec.session.id.clone();
        let pid = rec.session.pid;
        tracing::info!("streams: re-adopted stream {id} ({}, pid {pid})", rec.session.title);
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(ServeIn::Status);
        d.streams.entries.lock().unwrap().insert(id.clone(), Entry { rec, tx, ready: None, listening: true });
        let (r, w) = conn.into_split();
        attach(d, &id, Box::new(w), Box::new(r), rx);
        // It is still this process's child (an exec keeps the pid): reap it when it ends.
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let mut status = 0;
                let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
                if r == pid as i32 || (r < 0 && unsafe { libc::kill(pid as i32, 0) } != 0) {
                    return;
                }
            }
        });
    }
    if !d.streams.entries.lock().unwrap().is_empty() {
        d.emit(Event::StreamsChanged);
    }
}

// --- Moonlight ---------------------------------------------------------------------------------

/// What a Moonlight launch streams (`moonlight::stream_host_for`): a game the way
/// `StartStream` runs one. The host's headless display comes up in quiverd (GameStream speaks
/// its own transport, so the host runs in-process, as `quiver-gamestream`'s adapter runs it),
/// then the game is launched into it as `Play` launches it, with the display's
/// [`LaunchEnv`]. The game ending ends the stream; Moonlight's Quit app, or the stream
/// ending, stops the game (its display is gone). The test pattern's key streams the test
/// pattern.
pub(crate) struct MoonlightGames {
    daemon: std::sync::Weak<Daemon>,
    pattern: quiver_gamestream::stream_host::StreamHostAdapter,
    dir: PathBuf,
    current: Mutex<Option<(String, tokio::task::JoinHandle<()>)>>,
}

impl MoonlightGames {
    pub fn new(d: &Arc<Daemon>, dir: PathBuf) -> MoonlightGames {
        let pattern = quiver_gamestream::stream_host::StreamHostAdapter::new(dir.clone(), |_app: &quiver_gamestream::host::App| Ok(quiver_stream_host::host::Target::TestPattern { motion: true }));
        MoonlightGames { daemon: Arc::downgrade(d), pattern, dir, current: Mutex::new(None) }
    }
}

impl quiver_gamestream::StreamHost for MoonlightGames {
    fn start(&self, req: quiver_gamestream::host::StreamRequest, ends: quiver_gamestream::host::HostEnds) -> Result<()> {
        if req.app.key == crate::moonlight::TEST_PATTERN_KEY {
            return self.pattern.start(req, ends);
        }
        let game = GameRef::parse(&req.app.key).map_err(|e| anyhow::anyhow!("{}: {e}", req.app.key))?;
        if game.store == StoreId::Steam && crate::testgames::find(game).is_none() {
            bail!("{game}: Steam games start through Steam, on the desktop, not in a stream's display");
        }
        let d = self.daemon.upgrade().context("quiverd is stopping")?;
        // It runs the display's placeholder and the pad namespace: never quiverd's own binary.
        let exe = host_exe();
        if !exe.is_absolute() {
            bail!("quiver-stream-host isn't installed beside quiverd or on PATH (tools/install-user.sh installs it)");
        }
        let session = format!("moonlight-{:08x}", rand_u32());
        let opts = quiver_stream_host::host::HostOptions {
            target: quiver_stream_host::host::Target::Display,
            config: quiver_gamestream::stream_host::StreamHostAdapter::config_for(&req),
            dir: self.dir.join(&session),
            session,
            preset: 1,
            recovery: if req.video.reference_frame_invalidation { quiver_stream_host::recovery::Recovery::RefInvalidation } else { quiver_stream_host::recovery::Recovery::Keyframe },
            headless_server: false,
            max_frames: None,
            force_sandbox_gid: None,
            exe: Some(exe),
            constant_bitrate_audio: req.audio.constant_bitrate,
        };
        let source = format!("moonlight:{}", req.client_name);
        let title = req.app.title.clone();
        let task = tokio::spawn(async move {
            let quiver_gamestream::host::HostEnds { video, audio, mut input, mut control_in, control_out, trigger_rumble: _, bitrate_kbps } = ends;
            let mut host = match quiver_stream_host::Host::start(opts, bitrate_kbps).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("moonlight: {title}'s display didn't start: {e:#}");
                    let _ = control_out.send(quiver_stream_proto::Control::Bye { reason: format!("the host didn't start: {e:#}") }).await;
                    return;
                }
            };
            let Some(launch) = host.launch.clone() else { return };
            // A resume right after the last stream ended finds the game still stopping (its
            // display went with that stream): wait for it, up to 20 s.
            let mut job = Err(anyhow::anyhow!("not started"));
            for _ in 0..40 {
                LAUNCH.lock().unwrap().insert(game, launch.clone());
                job = d.play(&source, game, None).await;
                LAUNCH.lock().unwrap().remove(&game);
                match &job {
                    Err(e) if format!("{e:#}").contains("already running") => tokio::time::sleep(Duration::from_millis(500)).await,
                    _ => break,
                }
            }
            let job = match job {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("moonlight: {title} didn't start: {e:#}");
                    let _ = control_out.send(quiver_stream_proto::Control::Bye { reason: format!("{title} didn't start: {e:#}") }).await;
                    let _ = host.stop().await;
                    return;
                }
            };
            d.log_activity_game(&source, format!("streaming {title} to Moonlight ({})", req.client_name), Some(game));
            let mut seq = 0u64;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    f = host.frames.recv() => match f {
                        Some(f) => if video.send(f).await.is_err() { break },
                        None => break,
                    },
                    Some(a) = host.audio.recv() => { let _ = audio.try_send(a); }
                    Some(c) = host.control_out.recv() => { let _ = control_out.try_send(c); }
                    e = input.recv() => match e {
                        Some(event) => {
                            seq += 1;
                            let _ = host.input.send(quiver_stream_proto::input::Stamped { seq, at: quiver_stream_proto::now_micros(), event }).await;
                        }
                        None => break,
                    },
                    c = control_in.recv() => match c {
                        Some(quiver_stream_proto::Control::Bye { .. }) | None => break,
                        Some(c) => { let _ = host.control_in.send(c).await; }
                    },
                    _ = tick.tick() => {
                        if let Some(why) = pad_problem(&d, &source, game) {
                            tracing::warn!("moonlight: no pads in this stream: {why}");
                            host.disable_pads(&why);
                        }
                        if d.jobs.lock().unwrap().get(&job).is_none_or(|j| j.done) {
                            let _ = control_out.send(quiver_stream_proto::Control::Bye { reason: "the game exited".into() }).await;
                            break;
                        }
                    }
                }
            }
            // The display goes with the stream, so the game does too.
            stop_game(&d, &source, game).await;
            let _ = host.stop().await;
        });
        if let Some((_, old)) = self.current.lock().unwrap().replace((req.app.key.clone(), task)) {
            old.abort();
        }
        Ok(())
    }

    fn quit(&self, app_key: &str) {
        if app_key == crate::moonlight::TEST_PATTERN_KEY {
            return self.pattern.quit(app_key);
        }
        if let Some((_, t)) = self.current.lock().unwrap().take() {
            t.abort();
        }
        if let (Some(d), Ok(g)) = (self.daemon.upgrade(), GameRef::parse(app_key)) {
            tokio::spawn(async move { stop_game(&d, "moonlight", g).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_the_transport_form() {
        let t = StreamTicket::new("10.0.0.2:47998".parse().unwrap());
        let h = keys_hex(&t);
        assert_eq!(h.len(), 80);
        let k = quiver_stream_net_keys(&h);
        assert_eq!(k.0, t.session_id);
        assert_eq!(k.1, t.key);
    }

    /// `SessionKeys::from_hex`'s layout: the id big-endian, then the key.
    fn quiver_stream_net_keys(h: &str) -> (u64, [u8; 32]) {
        let b: Vec<u8> = (0..40).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap()).collect();
        (u64::from_be_bytes(b[..8].try_into().unwrap()), b[8..].try_into().unwrap())
    }

    #[test]
    fn host_runs_in_a_scope_with_nothing_secret_in_argv() {
        let a = host_argv("s1", true).join(" ");
        assert!(a.starts_with("/usr/bin/systemd-run --user --scope --collect --quiet --unit=quiver-stream-s1 "), "{a}");
        assert!(a.contains(" -- ") && !a.contains("keys"), "{a}");
        assert!(a.ends_with("serve --socket-fd 3"));
        assert!(!host_argv("s1", false).join(" ").contains("systemd-run"));
    }

    #[test]
    fn ports_count_up_past_taken_ones() {
        let a = bind_port(48_600, &[]).unwrap();
        let pa = a.local_addr().unwrap().port();
        let b = bind_port(48_600, &[pa]).unwrap();
        assert!(b.local_addr().unwrap().port() > pa);
    }
}
