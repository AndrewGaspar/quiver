//! Moonlight compatibility in quiverd (design/REMOTE.md, phase 1b): a [`GameStreamServer`]
//! (`quiver-gamestream`) over this machine's library, off by default.
//!
//! - **On and off**: `remote.moonlight.enabled` in config.json, with `bind` (addresses or
//!   interface names such as `tailscale0`; empty: every address), `codecs` and `port`.
//!   `MoonlightEnable`/`MoonlightDisable` change and remember it. On a self-restart the server
//!   simply starts again in the new process from config (its listeners close with the old
//!   one; a stream in progress ends, and Moonlight can resume the app).
//! - **The library** Moonlight lists ([`Library`]) is the installed games, refreshed every
//!   [`REFRESH_EVERY`], with their portrait art from the art cache (fetched into it on first
//!   ask).
//! - **Paired clients** are pinned in `<data>/remote/moonlight-clients.json`, apart from
//!   Quiver's own peers; they may only stream.
//! - **Pairing**: a request shows up as `Event::MoonlightPairingRequested`; the PIN Moonlight
//!   shows comes back as `MoonlightPin` (not local-only, so a paired Quiver client can enter
//!   it for a headless server).
//! - **What a launch streams** goes through one seam, [`stream_host_for`]: the game, launched
//!   into the stream host's display as `StartStream` does (`streams::MoonlightGames`).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quiver_api::moonlight::{self as api, MoonlightClient, MoonlightPairing, MoonlightSettings, MoonlightStatus, MoonlightStream};
use quiver_api::{Event, Request};
use quiver_core::launcher::{self, Config};
use quiver_gamestream::{App, AppLibrary, Art, ClientStore, Codecs, FileClientStore, GameStreamServer, PairedClient, Ports, ServerConfig, ServerEvent, StreamHost};
use serde_json::{Value, json};

use crate::Daemon;

/// How often the app list follows the library.
const REFRESH_EVERY: Duration = Duration::from_secs(60);

struct Running {
    server: Arc<GameStreamServer>,
    listening: Vec<IpAddr>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// `_nvstream._tcp` on the LAN-ish bound interfaces, withdrawn when dropped.
    advert: Option<quiver_remote::discovery::Advert>,
    advertised_on: Vec<String>,
}

impl Drop for Running {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// The Moonlight server's state in the daemon.
pub(crate) struct Moonlight {
    dir: PathBuf,
    running: tokio::sync::Mutex<Option<Running>>,
    error: Mutex<Option<String>>,
    library: Arc<Library>,
    clients: Arc<FileClientStore>,
}

impl Moonlight {
    pub fn new() -> Moonlight {
        let dir = launcher::data_dir().join("remote");
        let art_dir = quiver_core::exports::Paths::from_env().art();
        Moonlight {
            clients: Arc::new(FileClientStore::new(dir.join("moonlight-clients.json"))),
            dir,
            running: tokio::sync::Mutex::new(None),
            error: Mutex::new(None),
            library: Arc::new(Library { apps: Mutex::default(), art_dir, fetching: Mutex::default() }),
        }
    }
}

// --- the seam -----------------------------------------------------------------------------

/// **The seam for what a Moonlight launch streams**: games as `StartStream` runs them, in
/// the host's headless display with the launch Play does ([`crate::streams::MoonlightGames`]).
pub(crate) fn stream_host_for(daemon: &Arc<Daemon>) -> Arc<dyn StreamHost> {
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    Arc::new(crate::streams::MoonlightGames::new(daemon, run.join("quiver").join("moonlight-streams")))
}

/// Moonlight lists a "Test pattern" app first. Off now that games stream, unless
/// `QUIVER_MOONLIGHT_TEST_PATTERN=1` (scratch daemons and the end-to-end test); the key still
/// streams the test pattern for a client that remembers it.
pub(crate) const LIST_TEST_PATTERN: bool = false;

fn list_test_pattern() -> bool {
    LIST_TEST_PATTERN || std::env::var("QUIVER_MOONLIGHT_TEST_PATTERN").is_ok_and(|v| v == "1")
}
pub(crate) const TEST_PATTERN_KEY: &str = "quiver:test-pattern";

// --- the library Moonlight lists ------------------------------------------------------------

/// The installed games, as Moonlight apps, with where their portrait art can come from.
pub(crate) struct Library {
    apps: Mutex<Vec<(App, Vec<quiver_api::art::ArtRef>)>>,
    art_dir: PathBuf,
    /// Art being fetched into the cache now.
    fetching: Mutex<std::collections::HashSet<String>>,
}

impl Library {
    async fn refresh(&self, daemon: &Arc<Daemon>) {
        // Every store's games that Quiver itself launches (Ubisoft, and Epic games it
        // installed): a stream runs them in its own display. Steam's start through Steam, on
        // the desktop, so they aren't offered.
        let entries = match daemon.library_all(false).await {
            Ok(e) => e,
            Err(e) => {
                tracing::info!("moonlight: no games to list: {e:#}");
                Vec::new()
            }
        };
        let mut apps: Vec<(App, Vec<quiver_api::art::ArtRef>)> = entries
            .iter()
            .filter(|e| e.on_disk() && e.game.store != quiver_api::StoreId::Steam && (e.installed.is_some() || crate::epic::is_installed(daemon, e.game)))
            .map(|e| {
                let art = quiver_api::art::candidates(e, quiver_api::art::ArtSlot::Tile, Default::default()).into_iter().map(|(_, r)| r).collect();
                (App { key: e.game.key(), title: e.name.clone(), hdr: false }, art)
            })
            .collect();
        // A scratch daemon's test programs (`QUIVER_TEST_GAMES`).
        apps.extend(crate::testgames::all().into_iter().filter_map(|g| Some((App { key: g.game?.key(), title: g.name, hdr: false }, Vec::new()))));
        apps.sort_by_key(|a| a.0.title.to_lowercase());
        if list_test_pattern() {
            apps.insert(0, (App { key: TEST_PATTERN_KEY.into(), title: "Test pattern".into(), hdr: false }, Vec::new()));
        }
        *self.apps.lock().unwrap() = apps;
    }

    /// The cached file for an art reference, if it is there.
    fn cached(&self, r: &quiver_api::art::ArtRef) -> Option<PathBuf> {
        use quiver_api::art::ArtRef;
        match r {
            ArtRef::File(p) => Some(p.clone()).filter(|p| p.is_file()),
            ArtRef::Asset(f) if !f.contains('/') => Some(self.art_dir.join(f)).filter(|p| p.is_file()),
            ArtRef::Asset(_) => None,
            ArtRef::Url(u) => {
                let stem = quiver_core::exports::art::url_stem(u);
                ["png", "jpg"].iter().map(|x| self.art_dir.join(format!("{stem}.{x}"))).find(|p| p.is_file())
            }
        }
    }
}

impl AppLibrary for Library {
    fn apps(&self) -> Vec<App> {
        self.apps.lock().unwrap().iter().map(|(a, _)| a.clone()).collect()
    }

    fn art(&self, key: &str) -> Option<Art> {
        let refs = self.apps.lock().unwrap().iter().find(|(a, _)| a.key == key).map(|(_, r)| r.clone())?;
        if let Some(path) = refs.iter().find_map(|r| self.cached(r)) {
            let bytes = std::fs::read(&path).ok()?;
            let content_type = if bytes.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" };
            return Some(Art { content_type: content_type.into(), bytes });
        }
        // Not cached yet: fetch the first source into the cache for Moonlight's next ask.
        let first = refs.into_iter().next()?;
        if self.fetching.lock().unwrap().insert(key.to_owned())
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let dir = self.art_dir.clone();
            rt.spawn(async move {
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = quiver_core::exports::art::fetch_ref(&dir, &first).await {
                    tracing::info!("moonlight: art: {e:#}");
                }
            });
        }
        None
    }
}

// --- starting and stopping ------------------------------------------------------------------

fn settings() -> MoonlightSettings {
    Config::load().map(|c| c.remote.moonlight).unwrap_or_default()
}

fn save_settings(f: impl FnOnce(&mut MoonlightSettings)) -> Result<()> {
    let mut cfg = Config::load()?;
    f(&mut cfg.remote.moonlight);
    cfg.save()
}

/// What `bind` means: each entry an address or an interface's addresses; empty: every
/// address (IPv4 and IPv6).
pub(crate) fn resolve_bind(bind: &[String]) -> Result<Vec<IpAddr>> {
    if bind.is_empty() {
        return Ok(vec!["0.0.0.0".parse().unwrap(), "::".parse().unwrap()]);
    }
    let mut out = Vec::new();
    for b in bind {
        let b = b.trim().trim_matches(['[', ']']);
        match b.parse::<IpAddr>() {
            Ok(ip) => out.push(ip),
            Err(_) => out.extend(quiver_gamestream::interface_addresses(b).with_context(|| format!("'{b}' is neither an address nor a network interface with one"))?),
        }
    }
    out.dedup();
    Ok(out)
}

pub(crate) fn codecs_from(names: &[String]) -> Result<Codecs> {
    if names.is_empty() {
        return Ok(Codecs::default());
    }
    let mut c = Codecs { h264: false, hevc: false, hevc_main10: false, av1: false, av1_main10: false, yuv444: false };
    for n in names {
        match n.trim().to_lowercase().as_str() {
            "h264" | "h.264" | "avc" => c.h264 = true,
            "hevc" | "h265" | "h.265" => c.hevc = true,
            "hevc10" | "hevc-main10" | "main10" => c.hevc_main10 = true,
            "av1" => c.av1 = true,
            "av1-10" | "av1_10" | "av1main10" => c.av1_main10 = true,
            other => bail!("unknown codec '{other}' (one of: {})", api::CODECS.join(", ")),
        }
    }
    // Moonlight assumes H.264 always works.
    c.h264 = true;
    Ok(c)
}

fn codec_names(c: &Codecs) -> Vec<String> {
    let mut v = Vec::new();
    for (on, name) in [(c.h264, "h264"), (c.hevc, "hevc"), (c.hevc_main10, "hevc10"), (c.av1, "av1"), (c.av1_main10, "av1-10")] {
        if on {
            v.push(name.to_owned());
        }
    }
    v
}

/// Start the server if config.json says so (at startup, and in the next build after a
/// restart).
pub(crate) fn start(daemon: &Arc<Daemon>) {
    if !settings().enabled {
        return;
    }
    let d = daemon.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&d, &settings()).await {
            tracing::warn!("moonlight: not listening: {e:#}");
            *d.moonlight.error.lock().unwrap() = Some(format!("{e:#}"));
        }
    });
}

async fn run(daemon: &Arc<Daemon>, s: &MoonlightSettings) -> Result<()> {
    let m = &daemon.moonlight;
    let mut slot = m.running.lock().await;
    *slot = None;
    let mut config = ServerConfig::new(server_name(), &m.dir);
    config.bind = resolve_bind(&s.bind)?;
    config.ports = Ports::from_http(s.port());
    config.codecs = codecs_from(&s.codecs)?;
    m.library.refresh(daemon).await;
    let server = Arc::new(GameStreamServer::start(config.clone(), m.library.clone(), m.clients.clone(), stream_host_for(daemon)).await?);
    let mut events = server.events();
    let weak = Arc::downgrade(daemon);
    let forward = tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            let Some(d) = weak.upgrade() else { return };
            forward_event(&d, e);
        }
    });
    let weak = Arc::downgrade(daemon);
    let refresh = tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_EVERY).await;
            let Some(d) = weak.upgrade() else { return };
            d.moonlight.library.refresh(&d).await;
        }
    });
    tracing::info!("moonlight: listening on {:?} (HTTP port {})", config.bind, config.ports.http);
    let (advert, advertised_on) = if s.mdns() { advertise(&server, &config.bind, config.ports.http) } else { (None, Vec::new()) };
    *slot = Some(Running { server, listening: config.bind, tasks: vec![forward, refresh], advert, advertised_on });
    *m.error.lock().unwrap() = None;
    Ok(())
}

/// The mDNS service type Moonlight browses for.
pub(crate) const NVSTREAM: &str = "_nvstream._tcp.local.";

/// A tailnet address (Tailscale's CGNAT range or its IPv6 prefix): mDNS doesn't cross those.
fn is_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(v6) => quiver_api::moonlight::is_tailnet_v6(&v6.to_string()),
    }
}

/// The interfaces to advertise on: those holding a bound address (any non-loopback one for
/// a wildcard bind), never a tailnet's.
pub(crate) fn mdns_interfaces(bound: &[IpAddr], all: &[(String, IpAddr)]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (name, ip) in all {
        if name.starts_with("tailscale") || is_tailnet(ip) {
            continue;
        }
        let wanted = bound.contains(ip) || bound.iter().any(|b| b.is_unspecified() && b.is_ipv4() == ip.is_ipv4() && !ip.is_loopback());
        if wanted && !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// Advertise the server as `_nvstream._tcp` on its LAN-ish interfaces (a failure is logged:
/// Moonlight can still add it by address).
fn advertise(server: &GameStreamServer, bound: &[IpAddr], port: u16) -> (Option<quiver_remote::discovery::Advert>, Vec<String>) {
    let all = quiver_gamestream::interfaces().unwrap_or_default();
    let names = mdns_interfaces(bound, &all);
    if names.is_empty() {
        tracing::info!("moonlight: not advertising on mDNS (no LAN interface among {bound:?})");
        return (None, names);
    }
    let id: String = server.identity().unique_id().chars().filter(char::is_ascii_alphanumeric).take(12).collect::<String>().to_lowercase();
    let host = format!("quiver-moonlight-{id}.local.");
    match quiver_remote::discovery::Advert::service(NVSTREAM, &server_name(), &host, port, Default::default(), &names) {
        Ok(a) => {
            tracing::info!("moonlight: advertised on mDNS on {names:?}");
            (Some(a), names)
        }
        Err(e) => {
            tracing::warn!("moonlight: not advertising on mDNS: {e:#}");
            (None, Vec::new())
        }
    }
}

fn forward_event(d: &Daemon, e: ServerEvent) {
    match e {
        ServerEvent::PairingRequested { device_name, address } => {
            d.log_activity("moonlight", format!("moonlight: {device_name} at {address} wants to pair (enter the PIN it shows)"), None);
            d.emit(Event::MoonlightPairingRequested { device_name, address: address.to_string() });
        }
        ServerEvent::PairingContested { address } => {
            d.log_activity("moonlight", format!("moonlight: refused a second pairing request, from {address}, while one was waiting"), None);
        }
        ServerEvent::Paired(c) => {
            d.log_activity("moonlight", format!("moonlight: paired {} at {}", c.name, c.address), None);
            d.emit(Event::MoonlightPaired { name: c.name, fingerprint: c.fingerprint });
        }
        ServerEvent::PairingFailed { address, reason } => {
            d.log_activity("moonlight", format!("moonlight: pairing with {address} failed: {reason}"), None);
        }
        ServerEvent::StreamStarted { app, client } => d.log_activity("moonlight", format!("moonlight: streaming {app} to {client}"), None),
        ServerEvent::StreamEnded { app, reason } => d.log_activity("moonlight", format!("moonlight: stream of {app} ended: {reason}"), None),
    }
    d.emit(Event::MoonlightChanged);
}

/// What Moonlight shows for this machine: the remote name, else the hostname.
fn server_name() -> String {
    let remote = launcher::data_dir().join("remote/machine.json");
    std::fs::read_to_string(remote)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["name"].as_str().map(str::to_owned))
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|h| h.trim().to_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Quiver".into())
}

fn client_info(c: &PairedClient) -> MoonlightClient {
    MoonlightClient {
        name: c.name.clone(),
        address: c.address.clone(),
        fingerprint: c.fingerprint.clone(),
        paired_at: chrono::DateTime::from_timestamp(c.paired_at as i64, 0).unwrap_or_default(),
    }
}

async fn status(daemon: &Daemon) -> Result<MoonlightStatus> {
    let m = &daemon.moonlight;
    let s = settings();
    let running = m.running.lock().await;
    let codecs = codecs_from(&s.codecs).map(|c| codec_names(&c)).unwrap_or_default();
    let advertised_on = running.as_ref().filter(|r| r.advert.is_some()).map(|r| r.advertised_on.clone()).unwrap_or_default();
    let (listening, pending, stream) = match running.as_ref() {
        Some(r) => (
            r.listening.iter().map(ToString::to_string).collect(),
            r.server.pending_pairing().map(|p| MoonlightPairing { device_name: p.device_name, address: p.address.to_string() }),
            r.server.current().map(|(app, client, _, streaming)| MoonlightStream { app, client, streaming }),
        ),
        None => (Vec::new(), None, None),
    };
    Ok(MoonlightStatus {
        enabled: s.enabled,
        bind: s.bind.clone(),
        listening,
        error: m.error.lock().unwrap().clone(),
        ports: api::ports(s.port()),
        codecs,
        mdns: s.mdns(),
        advertised_on,
        name: server_name(),
        pending,
        clients: m.clients.list()?.iter().map(client_info).collect(),
        stream,
    })
}

/// The client `id` names: a fingerprint, a prefix of one (4+ characters), or a name.
fn find_client(clients: &[PairedClient], id: &str) -> Result<PairedClient> {
    let id = id.trim().to_lowercase();
    let hits: Vec<&PairedClient> = clients
        .iter()
        .filter(|c| c.fingerprint == id || (id.len() >= 4 && c.fingerprint.starts_with(&id)) || c.name.to_lowercase() == id)
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no paired Moonlight client {id}"),
        _ => bail!("'{id}' matches {} Moonlight clients: give more of the fingerprint", hits.len()),
    }
}

/// A request's `Request::Moonlight*` handling.
pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let m = &daemon.moonlight;
    Ok(match req {
        Request::MoonlightStatus => json!(status(daemon).await?),
        Request::MoonlightEnable { bind, codecs, mdns } => {
            if let Some(b) = &bind {
                resolve_bind(b)?;
            }
            if let Some(c) = &codecs {
                codecs_from(c)?;
            }
            let mut s = settings();
            s.enabled = true;
            if let Some(b) = bind {
                s.bind = b;
            }
            if let Some(c) = codecs {
                s.codecs = c;
            }
            if let Some(on) = mdns {
                s.mdns = (!on).then_some(false);
            }
            if let Err(e) = run(daemon, &s).await {
                *m.error.lock().unwrap() = Some(format!("{e:#}"));
                return Err(e);
            }
            save_settings(|saved| *saved = s.clone())?;
            let on = if s.bind.is_empty() { "every address".to_owned() } else { s.bind.join(", ") };
            daemon.log_activity(source, format!("moonlight: on ({on})"), None);
            daemon.emit(Event::MoonlightChanged);
            json!(status(daemon).await?)
        }
        Request::MoonlightDisable => {
            m.running.lock().await.take();
            *m.error.lock().unwrap() = None;
            save_settings(|s| s.enabled = false)?;
            daemon.log_activity(source, "moonlight: off".into(), None);
            daemon.emit(Event::MoonlightChanged);
            Value::Null
        }
        Request::MoonlightPin { pin } => {
            let server = m.running.lock().await.as_ref().map(|r| r.server.clone());
            let Some(server) = server else { bail!("the Moonlight server is off (quiver remote moonlight enable)") };
            let client = server.submit_pin(&pin).await?;
            json!(client_info(&client))
        }
        Request::MoonlightClients => json!(m.clients.list()?.iter().map(client_info).collect::<Vec<_>>()),
        Request::MoonlightUnpair { fingerprint } => {
            let c = find_client(&m.clients.list()?, &fingerprint)?;
            m.clients.remove(&c.fingerprint)?;
            if let Some(r) = m.running.lock().await.as_ref() {
                r.server.disconnect(&c.fingerprint);
            }
            daemon.log_activity(source, format!("moonlight: unpaired {} ({})", c.name, &c.fingerprint[..16.min(c.fingerprint.len())]), None);
            daemon.emit(Event::MoonlightChanged);
            json!(client_info(&c))
        }
        other => bail!("not a Moonlight request: {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_and_codecs() {
        assert_eq!(resolve_bind(&[]).unwrap().len(), 2);
        assert_eq!(resolve_bind(&["100.64.1.2".into(), "[fd7a:115c:a1e0::1]".into()]).unwrap().len(), 2);
        assert!(resolve_bind(&["lo".into()]).unwrap().contains(&"127.0.0.1".parse().unwrap()));
        assert!(resolve_bind(&["no-such-if9".into()]).is_err());
        let c = codecs_from(&["hevc".into(), "AV1".into()]).unwrap();
        assert_eq!(codec_names(&c), ["h264", "hevc", "av1"]);
        assert_eq!(codec_names(&codecs_from(&[]).unwrap()), ["h264", "hevc", "hevc10", "av1", "av1-10"]);
        assert!(codecs_from(&["vp9".into()]).is_err());
    }

    #[test]
    fn mdns_only_on_lan_interfaces() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let all: Vec<(String, IpAddr)> = vec![
            ("lo".into(), ip("127.0.0.1")),
            ("wlp191s0".into(), ip("192.168.1.20")),
            ("wlp191s0".into(), ip("fd00::20")),
            ("tailscale0".into(), ip("100.101.12.7")),
            ("tailscale0".into(), ip("fd7a:115c:a1e0::7")),
        ];
        assert_eq!(mdns_interfaces(&[ip("0.0.0.0"), ip("::")], &all), ["wlp191s0"]);
        assert!(mdns_interfaces(&[ip("100.101.12.7"), ip("fd7a:115c:a1e0::7")], &all).is_empty(), "Tailscale only: nothing");
        assert_eq!(mdns_interfaces(&[ip("127.0.0.1")], &all), ["lo"], "loopback only when bound to it");
        assert_eq!(mdns_interfaces(&[ip("192.168.1.20"), ip("100.101.12.7")], &all), ["wlp191s0"]);
    }

    #[test]
    fn finding_clients() {
        let c = |name: &str, fp: &str| PairedClient { name: name.into(), address: "a".into(), fingerprint: fp.into(), cert_pem: String::new(), paired_at: 0 };
        let all = [c("roth", "abcd1234"), c("tv", "abce9999")];
        assert_eq!(find_client(&all, "abcd").unwrap().name, "roth");
        assert_eq!(find_client(&all, "TV").unwrap().fingerprint, "abce9999");
        assert!(find_client(&all, "abc").is_err(), "too short");
        assert!(find_client(&all, "zz").is_err());
    }
}
