//! The instance's own mod SDK endpoint (design/LAB.md "Mod SDK"): a lab mod connects through
//! the mod SDK (`quiver_mod.h`) like anywhere else, and its channel ends here, in the
//! instance's supervisor, never in quiverd.
//!
//! - The bridge in the instance relays the game's `QUIVER_MODS_PIPE` to this endpoint (its
//!   context's `mods_relay` in a Ubisoft launch, its `broker` in a mods-only one), on the
//!   host's loopback, forwarded into the container at the same port (`podman::Host::mods_port`).
//! - A [`ModRegistry::lab`] serves it: lab sessions of this instance only, validated as quiverd
//!   validates every channel.
//! - **Read-only.** The welcome carries no values (a lab mod is configured through its files,
//!   `quiver lab set`); the mod's own changes are logged and kept in the instance, never saved
//!   anywhere; nothing sets options or runs actions on it. Its metrics stay in its connection's
//!   status (`runs/r<N>/sdk.json`), never in the person's perf history.
//! - **Stereo**: the mod's word (`quiver_set_stereo_layout`) goes into the run's Sight session
//!   dir ([`quiver_api::lab::SDK_STEREO_FILE`]), where the run's lab `quiver-sight` reads it,
//!   so the HUD and the panel are drawn per eye without the `sight.stereo` setting.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use quiver_api::lab::{SDK_STATUS_FILE, SDK_STEREO_FILE, SdkStereo, session_run};
use quiver_api::mods::ModSdkConnection;
use quiver_core::lab::Inst;
use quiver_core::modsdk::{ChangedValues, ModMetricsSink, ModOptionsSink, ModRegistry};

/// The file `up` records the endpoint's port in (the container forwards it).
pub const PORT_FILE: &str = "mods-port";

/// A free port on the host's loopback for the endpoint: `prev` if it still is.
pub fn pick_port(prev: Option<u16>) -> Result<u16> {
    let l = prev.and_then(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok()).map_or_else(|| std::net::TcpListener::bind(("127.0.0.1", 0)), Ok)?;
    Ok(l.local_addr()?.port())
}

/// The port the instance's container forwards to the endpoint (none: a container from before it).
pub fn port(inst: &Inst) -> Option<u16> {
    std::fs::read_to_string(inst.dir.join(PORT_FILE)).ok()?.trim().parse().ok()
}

/// What the endpoint tells the supervisor (its events log).
pub type Note = Arc<dyn Fn(Option<u32>, String) + Send + Sync>;

/// A run's Sight session dir, from the instance and the run's folder.
type SessionDirOf = Box<dyn Fn(&Inst, &Path) -> Option<PathBuf> + Send + Sync>;

struct Sink {
    inst: Inst,
    note: Note,
    /// Where a session's stereo word goes: its run's Sight session dir, if it has one.
    session_dir: SessionDirOf,
    registry: Mutex<Option<std::sync::Weak<ModRegistry>>>,
}

impl Sink {
    /// The run of `m`'s session, when it's this instance's (the registry took lab sessions only).
    fn run(&self, m: &ModSdkConnection) -> Option<u32> {
        session_run(m.session.as_deref()?, &self.inst.name)
    }

    /// Rewrite `runs/r<N>/sdk.json` with the run's live connections.
    fn write_status(&self, run: u32) {
        let Some(reg) = self.registry.lock().unwrap().as_ref().and_then(|w| w.upgrade()) else { return };
        let session = quiver_api::lab::session_id(&self.inst.name, run);
        let conns: Vec<ModSdkConnection> = reg.connections(None).into_iter().filter(|c| c.session.as_deref() == Some(&session)).collect();
        let dir = self.inst.run_dir(run);
        if dir.is_dir()
            && let Ok(b) = serde_json::to_vec_pretty(&conns)
        {
            let _ = quiver_core::lab::write_atomic(&dir.join(SDK_STATUS_FILE), &b);
        }
    }

    fn stereo_file(&self, run: u32) -> Option<PathBuf> {
        (self.session_dir)(&self.inst, &self.inst.run_dir(run)).map(|d| d.join(SDK_STEREO_FILE))
    }
}

impl ModOptionsSink for Sink {
    // `snapshot`: none. A lab mod reads its own files; the person's values never reach it.
    fn changed(&self, m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        let what = if persist { "its menu" } else { "its runtime state" };
        (self.note)(self.run(m), format!("mod SDK: {} changed {} ({what}): kept in the instance, not saved", m.surface, values.keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    fn connected(&self, m: &ModSdkConnection) {
        let run = self.run(m);
        (self.note)(
            run,
            format!(
                "mod SDK: {} connected (protocol {}, {}{}; caps {}): read-only lab channel",
                m.surface,
                m.proto,
                if m.sdk.is_empty() { "SDK" } else { &m.sdk },
                m.module.as_deref().map(|x| format!(", {x}")).unwrap_or_default(),
                m.caps.join(", ")
            ),
        );
        if let Some(r) = run {
            self.write_status(r);
        }
    }
    fn disconnected(&self, m: &ModSdkConnection) {
        let run = self.run(m);
        (self.note)(run, format!("mod SDK: {} disconnected", m.surface));
    }
}

impl ModMetricsSink for Sink {
    fn declared(&self, m: &ModSdkConnection, _metrics: &[quiver_core::modsdk::DeclaredMetric]) {
        if let Some(r) = self.run(m) {
            self.write_status(r);
        }
    }
    fn stereo(&self, m: &ModSdkConnection) {
        let Some(run) = self.run(m) else { return };
        (self.note)(Some(run), format!("mod SDK: {} says its frames are {}", m.surface, m.stereo.as_deref().unwrap_or("its own business again (withdrawn)")));
        self.write_status(run);
        let Some(file) = self.stereo_file(run) else {
            (self.note)(Some(run), "mod SDK: no Sight session dir for this run (sight = off?): the stereo word goes nowhere".into());
            return;
        };
        match m.stereo.as_deref().and_then(quiver_perf::stereo::Stereo::parse) {
            Some(st) => {
                let word = SdkStereo { layout: st.as_str().to_owned(), surface: m.surface.clone(), conn: m.conn };
                if let Ok(b) = serde_json::to_vec(&word) {
                    let _ = quiver_core::lab::write_atomic(&file, &b);
                }
            }
            None => withdraw(&file, m.conn),
        }
    }
    fn disconnected(&self, m: &ModSdkConnection) {
        let Some(run) = self.run(m) else { return };
        if let Some(f) = self.stereo_file(run) {
            withdraw(&f, m.conn);
        }
        // The registry drops the connection after its sinks hear of it.
        let reg = self.registry.lock().unwrap().as_ref().and_then(|w| w.upgrade());
        let inst = self.inst.clone();
        let sink_run = run;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(reg) = reg {
                let session = quiver_api::lab::session_id(&inst.name, sink_run);
                let conns: Vec<ModSdkConnection> = reg.connections(None).into_iter().filter(|c| c.session.as_deref() == Some(&session)).collect();
                if let Ok(b) = serde_json::to_vec_pretty(&conns) {
                    let _ = quiver_core::lab::write_atomic(&inst.run_dir(sink_run).join(SDK_STATUS_FILE), &b);
                }
            }
        });
    }
}

/// The stereo word goes with its connection: remove the file if it's `conn`'s.
fn withdraw(file: &Path, conn: u64) {
    let theirs = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<SdkStereo>(&b).ok()).is_some_and(|w| w.conn == conn);
    if theirs {
        let _ = std::fs::remove_file(file);
    }
}

/// Start the endpoint on its port, on a thread of its own. `Ok(None)`: the container forwards
/// no port for it (started before the endpoint existed).
pub fn start(inst: &Inst, note: Note, session_dir: impl Fn(&Inst, &Path) -> Option<PathBuf> + Send + Sync + 'static) -> Result<Option<(u16, Arc<ModRegistry>)>> {
    let Some(port) = port(inst) else { return Ok(None) };
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("the mod SDK endpoint's port {port} is taken"))?;
    listener.set_nonblocking(true)?;
    let sink = Arc::new(Sink { inst: inst.clone(), note, session_dir: Box::new(session_dir), registry: Mutex::new(None) });
    let name = inst.name.clone();
    let reg = ModRegistry::lab(move |s| session_run(s, &name).is_some(), sink.clone(), sink.clone());
    *sink.registry.lock().unwrap() = Some(Arc::downgrade(&reg));
    let serving = reg.clone();
    std::thread::Builder::new().name("lab-mod-sdk".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("mod SDK endpoint: {e}");
                return;
            }
        };
        rt.block_on(async move {
            match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => {
                    if let Err(e) = serving.serve_listener(l).await {
                        eprintln!("mod SDK endpoint stopped: {e:#}");
                    }
                }
                Err(e) => eprintln!("mod SDK endpoint: {e}"),
            }
        });
    })?;
    Ok(Some((port, reg)))
}

/// A run's `sdk.json`, if any mod connected.
pub fn read_status(run_dir: &Path) -> Option<Vec<ModSdkConnection>> {
    serde_json::from_slice(&std::fs::read(run_dir.join(SDK_STATUS_FILE)).ok()?).ok()
}


#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A lab mod's channel through the endpoint: welcome without values, its stereo word in
    /// the run's session dir (and gone with its connection), its own change kept in the
    /// instance; another instance's session and a person's session are refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn lab_channel_is_read_only_and_carries_stereo() {
        let t = tempfile::tempdir().unwrap();
        let inst = Inst { name: "sdk-t".into(), dir: t.path().join("sdk-t") };
        std::fs::create_dir_all(inst.run_dir(3)).unwrap();
        let shm = t.path().join("shm");
        std::fs::create_dir_all(&shm).unwrap();
        let port = pick_port(None).unwrap();
        std::fs::write(inst.dir.join(PORT_FILE), port.to_string()).unwrap();
        let notes: Arc<Mutex<Vec<String>>> = Arc::default();
        let n = notes.clone();
        let note: Note = Arc::new(move |run, s| n.lock().unwrap().push(format!("{run:?} {s}")));
        let sd = shm.clone();
        let (p, reg) = start(&inst, note, move |_, run_dir| (run_dir.file_name()? == "r3").then(|| sd.clone())).unwrap().unwrap();
        assert_eq!(p, port);

        async fn open(port: u16, session: &str) -> (BufReader<tokio::net::tcp::OwnedReadHalf>, tokio::net::tcp::OwnedWriteHalf) {
            let s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let (r, mut w) = s.into_split();
            let req = quiver_ipc::BrokerRequest::ModChannel {
                uplay_id: 0,
                account_id: None,
                session: Some(session.into()),
                profile: None,
                conn: 1,
                client_pid: None,
                game: Some("epic:7315f614b0504081bcdceef138075c9c".into()),
            };
            let mut l = serde_json::to_vec(&req).unwrap();
            l.push(b'\n');
            w.write_all(&l).await.unwrap();
            (BufReader::new(r), w)
        }
        async fn line(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
            let mut s = String::new();
            tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut s)).await.unwrap().unwrap();
            s
        }

        let (mut r, mut w) = open(port, "lab-sdk-t-r3").await;
        assert_eq!(line(&mut r).await.trim(), "\"ok\"");
        w.write_all(b"{\"t\":\"hello\",\"proto\":3,\"mod\":\"torchbearer\",\"caps\":[\"options\",\"metrics\"]}\n{\"t\":\"stereo\",\"layout\":\"hsbs\"}\n").await.unwrap();
        let welcome: serde_json::Value = serde_json::from_str(&line(&mut r).await).unwrap();
        assert_eq!(welcome["t"], "welcome");
        assert_eq!(welcome["values"], serde_json::json!({}), "a lab mod gets no values: it reads its files");
        let file = shm.join(SDK_STEREO_FILE);
        for _ in 0..100 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let word = SdkStereo::read(&shm).expect("the stereo word in the session dir");
        assert_eq!((word.layout.as_str(), word.surface.as_str()), ("hsbs", "torchbearer"));
        let status = read_status(&inst.run_dir(3)).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].game.map(|g| g.to_string()).as_deref(), Some("epic:7315f614b0504081bcdceef138075c9c"));
        w.write_all(b"{\"t\":\"changed\",\"values\":{\"stereo.mode\":\"off\"},\"persist\":true}\n").await.unwrap();
        // Another instance's session and a person's session: refused here.
        let (mut r2, mut w2) = open(port, "lab-other-r1").await;
        assert_eq!(line(&mut r2).await.trim(), "\"ok\"");
        w2.write_all(b"{\"t\":\"hello\",\"proto\":3,\"mod\":\"x\"}\n").await.unwrap();
        let (mut r3, mut w3) = open(port, "s-0123456789").await;
        assert_eq!(line(&mut r3).await.trim(), "\"ok\"");
        w3.write_all(b"{\"t\":\"hello\",\"proto\":3,\"mod\":\"x\"}\n").await.unwrap();
        for _ in 0..100 {
            if reg.connections(None).len() == 1 && notes.lock().unwrap().iter().any(|l| l.contains("kept in the instance")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reg.connections(None).len(), 1, "only this instance's session: {:?}", reg.connections(None));
        assert!(notes.lock().unwrap().iter().any(|l| l == "Some(3) mod SDK: torchbearer changed stereo.mode (its menu): kept in the instance, not saved"), "{:?}", notes.lock().unwrap());
        // Gone with its connection.
        w.write_all(b"{\"t\":\"bye\"}\n").await.unwrap();
        for _ in 0..100 {
            if !file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!file.exists(), "the word went with its connection");
        let _ = (w2, w3, r2, r3);
    }
}
