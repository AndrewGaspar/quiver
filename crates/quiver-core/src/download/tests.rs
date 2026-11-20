//! The downloader against a fake demux (ownership_service and download_service) and a
//! fake CDN: connections the server closes and sockets that drop mid-install.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use prost::Message;
use quiver_proto::{demux as dm, ownership};
use tokio::sync::mpsc;

use super::*;
use crate::demux::fake;
use crate::install::tests::{FakeCdn, data, fixture};
use crate::install::{self, Progress, RetryPolicy, SliceSource};

/// What the fake ownership and download services do, shared with the test.
#[derive(Default)]
struct Services {
    /// Tokens handed out so far (`tok1`, `tok2`…); download_service takes those from
    /// `tok<min_token>` on.
    tokens: AtomicUsize,
    min_token: AtomicUsize,
    /// Tokens download_service was initialized with, in order.
    initialized: Mutex<Vec<String>>,
    url_requests: AtomicUsize,
    /// URL requests (1-based) answered by closing the connection instead (which also
    /// expires the old tokens).
    close_at: Mutex<HashSet<usize>>,
    /// Answer URL requests with `NotOwned`.
    not_owned: AtomicBool,
    inject: Mutex<Option<mpsc::UnboundedSender<dm::Downstream>>>,
}

fn handler(s: Arc<Services>) -> fake::Handler {
    Box::new(move |service, conn, body| match service {
        "ownership_service" => {
            let req = ownership::Upstream::decode(body).ok()?.request?;
            let mut rsp = ownership::Rsp { request_id: req.request_id, ..Default::default() };
            if req.initialize_req.is_some() {
                rsp.initialize_rsp = Some(ownership::InitializeRsp { success: true, ..Default::default() });
            }
            if req.ownership_token_req.is_some() {
                let n = s.tokens.fetch_add(1, Ordering::SeqCst) + 1;
                rsp.ownership_token_rsp = Some(ownership::OwnershipTokenRsp { success: Some(true), token: Some(format!("tok{n}")), expiration: None });
            }
            Some(ownership::Downstream { response: Some(rsp), push: None }.encode_to_vec())
        }
        "download_service" => {
            let req = ds::Upstream::decode(body).ok()?.request?;
            let mut rsp = ds::Rsp { request_id: req.request_id, ..Default::default() };
            if let Some(init) = req.initialize_req {
                let n: usize = init.ownership_token.trim_start_matches("tok").parse().unwrap_or(0);
                let ok = n >= s.min_token.load(Ordering::SeqCst);
                s.initialized.lock().unwrap().push(init.ownership_token);
                rsp.initialize_rsp = Some(ds::InitializeRsp { ok });
            }
            if let Some(u) = req.url_req {
                let n = s.url_requests.fetch_add(1, Ordering::SeqCst) + 1;
                if s.close_at.lock().unwrap().contains(&n) {
                    s.min_token.store(s.tokens.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let _ = s.inject.lock().unwrap().as_ref().unwrap().send(fake::closed_push(conn));
                    return None;
                }
                let paths = &u.url_requests[0].relative_file_path;
                let result = if s.not_owned.load(Ordering::SeqCst) { ds::url_rsp::Result::NotOwned } else { ds::url_rsp::Result::Success };
                rsp.url_rsp = Some(ds::UrlRsp {
                    url_responses: vec![ds::url_rsp::UrlResponse {
                        result: result as i32,
                        download_urls: paths.iter().map(|p| ds::url_rsp::DownloadUrls { urls: vec![format!("{}@0", p.rsplit('/').next().unwrap())] }).collect(),
                    }],
                    ttl_seconds: 0,
                });
            }
            Some(ds::Downstream { response: Some(rsp) }.encode_to_vec())
        }
        _ => None,
    })
}

async fn server(s: &Arc<Services>) -> fake::Server {
    let srv = fake::start(Duration::from_secs(5), handler(s.clone())).await;
    *s.inject.lock().unwrap() = Some(srv.inject.clone());
    srv
}

/// URLs from the real downloader (over the fake demux), slices from the fake CDN.
struct Mixed<'a>(&'a Downloader, &'a FakeCdn);

impl SliceSource for Mixed<'_> {
    async fn urls(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        self.0.urls(paths).await
    }
    async fn get(&self, mirrors: &[String]) -> Result<Vec<u8>, SliceError> {
        self.1.get(mirrors).await
    }
}

fn quick() -> Progress {
    Progress { retry: RetryPolicy { base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(4), ..RetryPolicy::default() }, ..Progress::default() }
}

/// The server closes download_service between two URL batches (and the token it was
/// opened with has expired meanwhile): the next request opens a new connection, gets a
/// fresh token when the old one is refused, and the install finishes.
#[tokio::test]
async fn closed_download_service_reconnects_mid_install() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("game");
    let mut cdn = FakeCdn::default();
    // 300 slices: two URL batches.
    let m = fixture(&[("a.bin", data(1, 300_000))], &mut cdn.slices);
    let plan = install::plan(&m, "en-US").unwrap();
    let s = Arc::new(Services::default());
    s.close_at.lock().unwrap().insert(2);
    let srv = server(&s).await;
    let mut lib = Library::fetch(&srv.demux).await.unwrap();
    let dl = Downloader::open(&srv.demux, &mut lib, 5059).await.unwrap();
    let progress = quick();
    install::install(&mut Mixed(&dl, &cdn), &plan, &dest, true, &progress).await.unwrap();
    assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(1, 300_000));
    assert_eq!(*s.initialized.lock().unwrap(), ["tok1", "tok1", "tok2"], "reopened, the old token refused, a fresh one taken");
    assert_eq!(s.url_requests.load(Ordering::SeqCst), 3);
    assert_eq!(progress.retries.load(Ordering::SeqCst), 1);
}

/// The whole socket goes (another login replaced it): URL requests wait for the
/// account's socket to come back, through a few failed reconnects, then carry on.
#[tokio::test]
async fn dropped_demux_waits_for_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("game");
    let mut cdn = FakeCdn::default();
    let m = fixture(&[("a.bin", data(2, 300_000))], &mut cdn.slices);
    let plan = install::plan(&m, "en-US").unwrap();
    let s = Arc::new(Services::default());
    let srv = server(&s).await;
    let mut lib = Library::fetch(&srv.demux).await.unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let next: Arc<tokio::sync::Mutex<Option<fake::Server>>> = Arc::default();
    let reconnect: Reconnect = {
        let (attempts, next, s) = (attempts.clone(), next.clone(), s.clone());
        Arc::new(move || {
            let (attempts, next, s) = (attempts.clone(), next.clone(), s.clone());
            Box::pin(async move {
                // The network is down for the first three tries.
                if attempts.fetch_add(1, Ordering::SeqCst) < 3 {
                    return Err(transient("connect to dmx.upc.ubisoft.com:443: network unreachable"));
                }
                let srv = server(&s).await;
                let d = srv.demux.clone();
                *next.lock().await = Some(srv);
                Ok(d)
            })
        })
    };
    let dl = Downloader::open_with(&srv.demux, &mut lib, 5059, Some(reconnect)).await.unwrap();
    let gone = dm::Downstream {
        response: None,
        push: Some(dm::Push {
            connection_closed: Some(dm::ConnectionClosedPush {
                connection_id: 999,
                error_code: Some(dm::connection_closed_push::ConnectionErrorCode::ConnectionMultipleLogin as i32),
            }),
            ..Default::default()
        }),
    };
    srv.inject.send(gone).unwrap();
    for _ in 0..200 {
        if !srv.demux.is_alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!srv.demux.is_alive());
    let progress = quick();
    install::install(&mut Mixed(&dl, &cdn), &plan, &dest, true, &progress).await.unwrap();
    assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), data(2, 300_000));
    assert_eq!(attempts.load(Ordering::SeqCst), 4, "three failed reconnects, then one that worked");
    assert_eq!(progress.retries.load(Ordering::SeqCst), 3);
}

/// Not owned even with a fresh token: permanent, and the installer doesn't retry it.
#[tokio::test]
async fn not_owned_is_permanent() {
    let s = Arc::new(Services::default());
    s.not_owned.store(true, Ordering::SeqCst);
    let srv = server(&s).await;
    let mut lib = Library::fetch(&srv.demux).await.unwrap();
    let dl = Downloader::open(&srv.demux, &mut lib, 5059).await.unwrap();
    let e = dl.urls(&["slices_v3/0/AA".into()]).await.unwrap_err();
    assert_eq!(crate::retry::classify(&e), crate::retry::Kind::Permanent, "{e:#}");
    assert_eq!(s.url_requests.load(Ordering::SeqCst), 2, "asked again once with a fresh token");
    assert_eq!(s.tokens.load(Ordering::SeqCst), 2);

    let dir = tempfile::tempdir().unwrap();
    let mut cdn = FakeCdn::default();
    let m = fixture(&[("a.bin", data(3, 3_000))], &mut cdn.slices);
    let plan = install::plan(&m, "en-US").unwrap();
    let progress = quick();
    let e = install::install(&mut Mixed(&dl, &cdn), &plan, &dir.path().join("g"), true, &progress).await.unwrap_err();
    assert!(!crate::retry::is_transient(&e), "{e:#}");
    assert_eq!(progress.retries.load(Ordering::SeqCst), 0);
}

/// Out of slice tries: network trouble is transient (the queue tries the game again
/// later); a slice that never matches its hash is permanent.
#[tokio::test]
async fn exhausted_slice_tries_are_classified() {
    let dir = tempfile::tempdir().unwrap();
    let mut cdn = FakeCdn::default();
    let m = fixture(&[("a.bin", data(4, 3_000))], &mut cdn.slices);
    let plan = install::plan(&m, "en-US").unwrap();
    let key = hex::encode_upper(&plan.files[0].slices[1].sha1);
    cdn.flaky.lock().unwrap().insert(key.clone(), 100);
    let e = install::install(&mut cdn, &plan, &dir.path().join("g"), true, &quick()).await.unwrap_err();
    assert!(crate::retry::is_transient(&e), "{e:#}");
    assert!(format!("{e:#}").contains("gave up after 6 tries"), "{e:#}");

    cdn.flaky.lock().unwrap().clear();
    cdn.slices.insert(key, b"garbage".to_vec());
    let e = install::install(&mut cdn, &plan, &dir.path().join("g"), true, &quick()).await.unwrap_err();
    assert!(!crate::retry::is_transient(&e), "{e:#}");
}
