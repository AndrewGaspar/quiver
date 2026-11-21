//! The HTTP and HTTPS endpoints (GameStream's "nvhttp"). Answers are XML with a
//! `status_code` attribute on `<root>`; Moonlight reads that, not the HTTP status.
//!
//! Over plain HTTP only `/serverinfo`, `/pair` (stages 1–4) and `/unpair` work. Everything
//! else needs HTTPS with a pinned client certificate; an unknown certificate gets
//! `status_code="401"`, which tells Moonlight to fall back to HTTP and offer pairing.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;

use crate::host::{App, PairedClient};
use crate::http::{HttpRequest, read_request, write_response, xml};
use crate::pairing::{ClientCert, Pairing, Stage};
use crate::{PendingPairing, ServerEvent, Shared, stream};

/// The version Moonlight sees. 7.1.431 is the newest GameStream generation it knows, and the
/// negative last part marks a Sunshine-compatible host, which unlocks the protocol
/// extensions this server implements (AV1, encrypted control, multi-FEC, HDR metadata, the
/// extended input events).
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";

/// The number Moonlight files an app under: a positive 31-bit hash of its key, stable
/// across restarts and library changes.
pub fn app_number(key: &str) -> u32 {
    (crc32fast::hash(key.as_bytes()) & 0x7FFF_FFFF).max(1)
}

/// The library's apps with their numbers. A clash (rare) moves the later app up by one.
pub fn numbered(apps: Vec<App>) -> Vec<(u32, App)> {
    let mut out: Vec<(u32, App)> = Vec::with_capacity(apps.len());
    for app in apps {
        let mut n = app_number(&app.key);
        while out.iter().any(|(m, _)| *m == n) {
            n = (n % 0x7FFF_FFFF) + 1;
        }
        out.push((n, app));
    }
    out
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Reply {
    fn xml(s: String) -> Self {
        Self { status: 200, content_type: "application/xml", body: s.into_bytes() }
    }
    fn error(status: u32, message: &str) -> Self {
        Self::xml(xml(status, Some(message), &[]))
    }
}

/// Serves one request. `cert` is `None` on HTTP, `Some(client certificate)` on HTTPS.
pub(crate) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    shared: &Arc<Shared>,
    io: &mut S,
    peer: SocketAddr,
    local: IpAddr,
    cert: Option<Option<Vec<u8>>>,
) -> Result<()> {
    let req = read_request(io).await?;
    tracing::debug!(%peer, path = %req.path, https = cert.is_some(), "GameStream request");
    let reply = route(shared, &req, peer.ip(), local, cert).await;
    write_response(io, reply.status, reply.content_type, &reply.body).await
}

async fn route(shared: &Arc<Shared>, req: &HttpRequest, peer: IpAddr, local: IpAddr, cert: Option<Option<Vec<u8>>>) -> Reply {
    let https = cert.is_some();
    let paired = match &cert {
        Some(Some(der)) => {
            let fp = hex::encode(Sha256::digest(der));
            shared.clients.find(&fp).ok().flatten()
        }
        _ => None,
    };
    match req.path.as_str() {
        "/serverinfo" => {
            if https && paired.is_none() {
                return not_authorised();
            }
            Reply::xml(server_info(shared, local, paired.is_some()))
        }
        "/pair" if !https => pair(shared, req, peer).await,
        "/pair" => Reply::xml(xml(200, None, &[("paired", if paired.is_some() { "1" } else { "0" }.into())])),
        "/unpair" => {
            // Moonlight calls this over HTTP, with no certificate, when it gives up on a
            // pairing. Anyone could send it, so it only cancels this address's pending
            // request; unpinning a client happens on the server side.
            let mut p = shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
            if p.as_ref().is_some_and(|p| p.address == peer) {
                fail(shared, p.take().expect("checked"), "the client cancelled");
            }
            Reply::xml(xml(200, None, &[]))
        }
        _ if !https => Reply { status: 404, content_type: "text/plain", body: b"not found".to_vec() },
        _ => {
            let Some(client) = paired else { return not_authorised() };
            match req.path.as_str() {
                "/applist" => Reply::xml(app_list(shared)),
                "/appasset" => app_asset(shared, req),
                "/launch" => launch(shared, req, client, peer, local, false),
                "/resume" => launch(shared, req, client, peer, local, true),
                "/cancel" => cancel(shared),
                _ => Reply { status: 404, content_type: "text/plain", body: b"not found".to_vec() },
            }
        }
    }
}

fn not_authorised() -> Reply {
    Reply::error(401, "The client is not authorized. Certificate verification failed.")
}

/// The number Moonlight knows a running app by: its launch's, else its number in the
/// library.
fn number_of(shared: &Shared, key: &str) -> u32 {
    if let Some(l) = shared.current_launch().filter(|l| l.app.key == key) {
        return l.app_number;
    }
    numbered(shared.library.apps()).into_iter().find(|(_, a)| a.key == key).map_or_else(|| app_number(key), |(n, _)| n)
}

/// The app running for Moonlight (streaming, or kept for a resume) and its number.
fn running(shared: &Shared) -> Option<(u32, App)> {
    let key = shared.host.running()?;
    if let Some(l) = shared.current_launch().filter(|l| l.app.key == key) {
        return Some((l.app_number, l.app.clone()));
    }
    let n = number_of(shared, &key);
    let app = shared.library.apps().into_iter().find(|a| a.key == key).unwrap_or(App { title: key.clone(), key, hdr: false });
    Some((n, app))
}

fn server_info(shared: &Shared, local: IpAddr, paired: bool) -> String {
    let c = &shared.config;
    let local_ip = match local.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        // Moonlight keeps LocalIP as an IPv4 address and knows to ignore 127.0.0.1.
        IpAddr::V6(_) => "127.0.0.1".into(),
    };
    // Running (or kept for a resume): Moonlight shows it with Resume and Quit, and its
    // state must end in _SERVER_BUSY or it counts no current game at all.
    let current_game = running(shared).map_or(0, |(n, _)| n);
    xml(
        200,
        None,
        &[
            ("hostname", c.name.clone()),
            ("appversion", APP_VERSION.into()),
            ("GfeVersion", GFE_VERSION.into()),
            ("uniqueid", shared.identity.unique_id().into()),
            ("HttpsPort", c.ports.https.to_string()),
            ("ExternalPort", c.ports.http.to_string()),
            ("MaxLumaPixelsHEVC", if c.codecs.hevc { "1869449984" } else { "0" }.into()),
            ("mac", "00:00:00:00:00:00".into()),
            ("LocalIP", local_ip),
            ("ServerCodecModeSupport", c.codecs.mode_flags().to_string()),
            ("PairStatus", if paired { "1" } else { "0" }.into()),
            ("currentgame", current_game.to_string()),
            ("state", if current_game != 0 { "QUIVER_SERVER_BUSY" } else { "QUIVER_SERVER_FREE" }.into()),
        ],
    )
}

fn app_list(shared: &Shared) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">");
    for (n, app) in numbered(shared.library.apps()) {
        s.push_str(&format!(
            "<App><IsHdrSupported>{}</IsHdrSupported><AppTitle>{}</AppTitle><ID>{n}</ID></App>",
            u8::from(app.hdr),
            crate::http::escape(&app.title)
        ));
    }
    s.push_str("</root>\n");
    s
}

fn find_app(shared: &Shared, req: &HttpRequest) -> Option<(u32, App)> {
    let n: u32 = req.arg("appid")?.parse().ok()?;
    numbered(shared.library.apps()).into_iter().find(|(m, _)| *m == n)
}

fn app_asset(shared: &Shared, req: &HttpRequest) -> Reply {
    let Some((_, app)) = find_app(shared, req) else { return Reply::error(404, "no such app") };
    match shared.library.art(&app.key) {
        Some(art) => Reply {
            status: 200,
            content_type: if art.content_type == "image/jpeg" { "image/jpeg" } else { "image/png" },
            body: art.bytes,
        },
        None => Reply { status: 404, content_type: "text/plain", body: b"no art".to_vec() },
    }
}

fn launch(shared: &Arc<Shared>, req: &HttpRequest, client: PairedClient, peer: IpAddr, local: IpAddr, resume: bool) -> Reply {
    let current = shared.current_launch();
    let running = running(shared);
    let (number, app) = match (resume, &running, find_app(shared, req)) {
        // A resume is of the running app, whatever number it names.
        (true, Some(r), _) => r.clone(),
        (true, None, _) => return Reply::error(503, "The app isn't running any more: launch it again."),
        // Launching the running app attaches to it, as a resume does.
        (false, _, Some(a)) => a,
        (false, _, None) => return Reply::error(404, "That app isn't in this server's library."),
    };
    if let Some((_, r)) = &running
        && r.key != app.key
    {
        // GameStream's answer (and Sunshine's): Moonlight asks the person to quit the running
        // app first, then cancels it and launches again.
        return Reply::error(400, &format!("An app is already running on this host ({}): quit it first.", r.title));
    }
    if let Some(l) = current.as_ref().filter(|l| l.is_streaming()) {
        // The same client launching again has left its last stream (crashed, or lost the
        // network before its control stream timed out): end that one. Another client can't
        // take over a running stream.
        if l.client.fingerprint != client.fingerprint {
            return Reply::error(400, "A stream is already running on this server.");
        }
        l.end("the client started again");
    }
    // (Moonlight wants `gamesession` back from a launch and `resume` from a resume, whether
    // or not the app was running.)
    let attach = running.is_some();
    let key: Option<[u8; 16]> = req.arg("rikey").and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok());
    let key_id = req.arg("rikeyid").and_then(|s| s.parse::<i64>().ok());
    let (Some(key), Some(key_id)) = (key, key_id) else { return Reply::error(400, "Missing a launch parameter (rikey, rikeyid).") };
    let mode: Vec<u32> = req.arg("mode").unwrap_or("0x0x0").split('x').map(|v| v.parse().unwrap_or(0)).collect();
    let pads = req.arg("gcmap").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let encrypted_rtsp = req.arg("corever").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) >= 1;
    let launch = match stream::Launch::new(stream::LaunchParams {
        app,
        app_number: number,
        client,
        client_ip: peer,
        local_ip: local,
        key,
        key_id: key_id as u32,
        mode: (mode.first().copied().unwrap_or(0), mode.get(1).copied().unwrap_or(0), mode.get(2).copied().unwrap_or(0)),
        pad_mask: pads,
        encrypted_rtsp,
    }) {
        Ok(l) => Arc::new(l),
        Err(e) => return Reply::error(500, &format!("{e:#}")),
    };
    if let Some(old) = shared.launch.lock().unwrap_or_else(|e| e.into_inner()).replace(launch.clone()) {
        old.end("replaced by a new launch");
    }
    let host = match local.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let url = format!("{}://{host}:{}", if encrypted_rtsp { "rtspenc" } else { "rtsp" }, shared.config.ports.rtsp);
    tracing::info!(app = %launch.app.title, resume, attach, %url, "Moonlight launch");
    let field = if resume { "resume" } else { "gamesession" };
    Reply::xml(xml(200, None, &[("sessionUrl0", url), (field, "1".into())]))
}

fn cancel(shared: &Shared) -> Reply {
    let launch = shared.launch.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(l) = &launch {
        l.end("the client quit the app");
    }
    // The running app, kept or streaming (its launch may be an earlier connection's).
    if let Some(key) = shared.host.running().or_else(|| launch.as_ref().map(|l| l.app.key.clone())) {
        tracing::info!(app = %key, "Moonlight quit the app");
        shared.host.quit(&key);
    }
    Reply::xml(xml(200, None, &[("cancel", "1".into())]))
}

// --- pairing ------------------------------------------------------------------------------

fn fail(shared: &Shared, p: PendingPairing, reason: &str) {
    for w in p.waiters {
        let _ = w.send(Err(reason.to_owned()));
    }
    shared.emit(ServerEvent::PairingFailed { address: p.address, reason: reason.to_owned() });
}

fn paired0(status: u32, message: &str) -> Reply {
    Reply::xml(xml(status, Some(message), &[("paired", "0".into())]))
}

async fn pair(shared: &Arc<Shared>, req: &HttpRequest, peer: IpAddr) -> Reply {
    if req.arg("phrase") == Some("getservercert") {
        return pair_start(shared, req, peer).await;
    }
    let hexarg = |name: &str| req.arg(name).and_then(|h| hex::decode(h).ok());
    let mut guard = shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = guard.as_mut().filter(|p| p.address == peer && p.state.is_some()) else {
        return paired0(400, "No pairing in progress from this address.");
    };
    let state = p.state.as_mut().expect("checked");
    let result = if let Some(c) = hexarg("clientchallenge") {
        state.answer_challenge(&shared.identity, &c).map(|r| ("challengeresponse", hex::encode(r)))
    } else if let Some(r) = hexarg("serverchallengeresp") {
        state.answer_server_challenge_response(&shared.identity, &r).map(|s| ("pairingsecret", hex::encode(s)))
    } else if let Some(s) = hexarg("clientpairingsecret") {
        state.check_client_secret(&s).map(|()| ("", String::new()))
    } else {
        return paired0(400, "Unknown pairing request.");
    };
    match result {
        Ok((field, value)) if state.stage() != Stage::Paired => Reply::xml(xml(200, None, &[("paired", "1".into()), (field, value)])),
        Ok(_) => {
            let p = guard.take().expect("checked");
            drop(guard);
            let client = PairedClient {
                name: p.device_name.clone(),
                address: p.address.to_string(),
                fingerprint: p.client.fingerprint(),
                cert_pem: p.client.pem.clone(),
                paired_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            };
            if let Err(e) = shared.clients.add(client.clone()) {
                fail(shared, p, &format!("couldn't save the client: {e:#}"));
                return paired0(500, "The server couldn't save the pairing.");
            }
            for w in p.waiters {
                let _ = w.send(Ok(client.clone()));
            }
            shared.emit(ServerEvent::Paired(client));
            Reply::xml(xml(200, None, &[("paired", "1".into())]))
        }
        Err(e) => {
            let p = guard.take().expect("checked");
            drop(guard);
            let reason = format!("{e:#}");
            fail(shared, p, &reason);
            paired0(200, &reason)
        }
    }
}

/// Stage 1: hold the request until the PIN arrives (Moonlight waits as long as it takes).
async fn pair_start(shared: &Arc<Shared>, req: &HttpRequest, peer: IpAddr) -> Reply {
    let salt: Option<[u8; 16]> = req.arg("salt").and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok());
    let cert = req
        .arg("clientcert")
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|pem| ClientCert::from_pem(&pem).ok());
    let (Some(salt), Some(cert)) = (salt, cert) else { return paired0(400, "Missing or bad salt or client certificate.") };
    let device_name = req.arg("devicename").unwrap_or("Moonlight").to_owned();
    let id = shared.pairing_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (pin_tx, pin_rx) = oneshot::channel::<String>();
    {
        let mut guard = shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.as_ref() {
            if existing.address != peer && existing.since.elapsed() < shared.config.pin_timeout {
                drop(guard);
                shared.emit(ServerEvent::PairingContested { address: peer });
                return paired0(503, "Another device is pairing with this server. Try again in a few minutes.");
            }
            fail(shared, guard.take().expect("checked"), "replaced by a new request from the same device");
        }
        *guard = Some(PendingPairing { id, device_name: device_name.clone(), address: peer, salt, client: cert, pin_tx: Some(pin_tx), state: None, waiters: Vec::new(), since: Instant::now() });
    }
    shared.emit(ServerEvent::PairingRequested { device_name, address: peer });

    let pin = tokio::time::timeout(shared.config.pin_timeout, pin_rx).await;
    let mut guard = shared.pairing.lock().unwrap_or_else(|e| e.into_inner());
    let ours = guard.as_ref().is_some_and(|p| p.id == id);
    match pin {
        Ok(Ok(pin)) if ours => {
            let p = guard.as_mut().expect("checked");
            match Pairing::with_random(&p.salt, &pin, p.client.clone()) {
                Ok(state) => p.state = Some(state),
                Err(e) => return paired0(500, &format!("{e:#}")),
            }
            Reply::xml(xml(200, None, &[("paired", "1".into()), ("plaincert", hex::encode(shared.identity.cert_pem()))]))
        }
        _ => {
            if ours {
                fail(shared, guard.take().expect("checked"), "no PIN was entered in time");
            }
            paired0(408, "The pairing request timed out waiting for the PIN.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::host::{AppLibrary, Art, ClientStore, HostEnds, StreamHost, StreamRequest};
    use crate::keep::tests::{FakeHost, ends, frame, hooks, host, request};
    use crate::keep::{KEEP, KeptState, Slot};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use quiver_stream_proto::Control;

    struct Lib;
    impl AppLibrary for Lib {
        fn apps(&self) -> Vec<App> {
            ["game-a", "game-b"].map(|k| App { key: k.into(), title: k.into(), hdr: false }).to_vec()
        }
        fn art(&self, _: &str) -> Option<Art> {
            None
        }
    }

    /// A host the way quiverd's is built ([`Slot`] over a kept host), with a fake host behind.
    #[derive(Default)]
    struct Games {
        slot: Slot,
        launches: AtomicU32,
        quits: Mutex<Vec<String>>,
        hosts: Arc<Mutex<Vec<FakeHost>>>,
    }
    impl StreamHost for Games {
        fn start(&self, req: StreamRequest, ends: HostEnds) -> anyhow::Result<()> {
            let (hosts, launches) = (self.hosts.clone(), &self.launches);
            self.slot.start(req, ends, KEEP, |_req| {
                launches.fetch_add(1, Ordering::SeqCst);
                async move {
                    let (chans, fake, stopped) = host();
                    hosts.lock().unwrap().push(fake);
                    Ok((chans, hooks(stopped)))
                }
            })
        }
        fn quit(&self, key: &str) {
            self.quits.lock().unwrap().push(key.to_owned());
            self.slot.quit(key, "the client quit the app");
        }
        fn running(&self) -> Option<String> {
            self.slot.running()
        }
    }

    fn query(path: &str, args: &[(&str, String)]) -> HttpRequest {
        HttpRequest { path: path.into(), query: args.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
    }

    async fn get(shared: &Arc<Shared>, path: &str, args: &[(&str, String)]) -> String {
        let der = b"the client's certificate".to_vec();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        String::from_utf8(route(shared, &query(path, args), ip, ip, Some(Some(der))).await.body).unwrap()
    }

    fn tag(xml: &str, t: &str) -> String {
        xml.split(&format!("<{t}>")).nth(1).and_then(|r| r.split('<').next()).unwrap_or_default().to_owned()
    }

    fn launch_args(n: u32) -> Vec<(&'static str, String)> {
        vec![("appid", n.to_string()), ("rikey", "00".repeat(16)), ("rikeyid", "7".into()), ("mode", "1280x720x60".into())]
    }

    /// Moonlight's whole round: launch, disconnect (the app swiped away), the other app
    /// refused, resume onto the same host, Quit app.
    #[tokio::test]
    async fn a_disconnect_keeps_the_app_a_resume_reattaches_and_cancel_quits() {
        let dir = tempfile::tempdir().unwrap();
        let clients = Arc::new(crate::FileClientStore::new(dir.path().join("clients.json")));
        let der = b"the client's certificate";
        let fingerprint = hex::encode(Sha256::digest(der));
        clients.add(PairedClient { name: "roth".into(), address: "127.0.0.1".into(), fingerprint, cert_pem: String::new(), paired_at: 0 }).unwrap();
        let games = Arc::new(Games::default());
        let (events, _) = tokio::sync::broadcast::channel(16);
        let shared = Arc::new(Shared {
            config: crate::ServerConfig::new("tower", dir.path()),
            identity: crate::Identity::load_or_create(dir.path()).unwrap(),
            library: Arc::new(Lib),
            clients,
            host: games.clone(),
            pairing: Mutex::new(None),
            pairing_ids: Default::default(),
            launch: Mutex::new(None),
            events,
        });
        let (a, b) = (app_number("game-a"), app_number("game-b"));
        let info = get(&shared, "/serverinfo", &[]).await;
        assert_eq!((tag(&info, "currentgame"), tag(&info, "state")), ("0".into(), "QUIVER_SERVER_FREE".into()));
        assert!(get(&shared, "/resume", &launch_args(a)).await.contains("status_code=\"503\""), "nothing to resume");

        // Launch A; its RTSP PLAY starts the stream (as stream::start_stream does).
        let r = get(&shared, "/launch", &launch_args(a)).await;
        assert_eq!(tag(&r, "gamesession"), "1", "{r}");
        let (e1, mut c1) = ends();
        games.start(request("game-a", 1280), e1).unwrap();
        while games.hosts.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let info = get(&shared, "/serverinfo", &[]).await;
        assert_eq!((tag(&info, "currentgame"), tag(&info, "state")), (a.to_string(), "QUIVER_SERVER_BUSY".into()));
        { let tx = games.hosts.lock().unwrap()[0].frames.clone(); tx.send(frame(1, true)).await.unwrap(); }
        assert_eq!(c1.video.recv().await.unwrap().seq, 1);

        // The app swiped away: the session's goodbye, then its channels close. Not a quit.
        c1.control_in.send(Control::Bye { reason: "the client disconnected".into() }).await.unwrap();
        drop(c1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(games.slot.kept().unwrap().state(), KeptState::Kept { .. }));
        assert!(games.quits.lock().unwrap().is_empty());
        let info = get(&shared, "/serverinfo", &[]).await;
        assert_eq!((tag(&info, "currentgame"), tag(&info, "state")), (a.to_string(), "QUIVER_SERVER_BUSY".into()), "Moonlight shows it running");

        // Another app: GameStream's "quit it first", and A keeps running.
        let r = get(&shared, "/launch", &launch_args(b)).await;
        assert!(r.contains("status_code=\"400\"") && r.contains("already running"), "{r}");
        assert!(games.quits.lock().unwrap().is_empty());

        // Resume: the same host (one launch), the new session gets its frames.
        let r = get(&shared, "/resume", &launch_args(a)).await;
        assert_eq!(tag(&r, "resume"), "1", "{r}");
        let (e2, mut c2) = ends();
        games.start(request("game-a", 1280), e2).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(games.slot.kept().unwrap().state(), KeptState::Streaming);
        { let tx = games.hosts.lock().unwrap()[0].frames.clone(); tx.send(frame(9, true)).await.unwrap(); }
        assert_eq!(c2.video.recv().await.unwrap().seq, 9);
        // A /launch of the running app attaches the same way (Moonlight sends that too).
        let r = get(&shared, "/launch", &launch_args(a)).await;
        assert_eq!(tag(&r, "gamesession"), "1", "{r}");
        let (e3, mut c3) = ends();
        games.start(request("game-a", 1280), e3).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        { let tx = games.hosts.lock().unwrap()[0].frames.clone(); tx.send(frame(10, true)).await.unwrap(); }
        assert_eq!(c3.video.recv().await.unwrap().seq, 10);
        assert_eq!(games.launches.load(Ordering::SeqCst), 1, "never relaunched");

        // Quit app.
        get(&shared, "/cancel", &[]).await;
        assert_eq!(games.quits.lock().unwrap().as_slice(), ["game-a"]);
        let k = games.hosts.lock().unwrap().len();
        assert_eq!(k, 1);
        for _ in 0..100 {
            if games.slot.running().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let info = get(&shared, "/serverinfo", &[]).await;
        assert_eq!((tag(&info, "currentgame"), tag(&info, "state")), ("0".into(), "QUIVER_SERVER_FREE".into()));
    }

    #[test]
    fn app_numbers_are_stable_and_distinct() {
        let a = |k: &str| App { key: k.into(), title: k.into(), hdr: false };
        let n = numbered(vec![a("epic:fortnite"), a("ubisoft:5059"), a("steam:620")]);
        assert_eq!(n[0].0, app_number("epic:fortnite"));
        assert!(n.iter().all(|(m, _)| *m > 0 && *m <= 0x7FFF_FFFF));
        // A clash moves the later app up.
        let n = numbered(vec![a("x"), a("x")]);
        assert_eq!(n[1].0, n[0].0 + 1);
        assert_eq!(app_number("ubisoft:5059"), crc32fast::hash(b"ubisoft:5059") & 0x7FFF_FFFF);
    }
}
