//! Owned-product listing via `ownership_service`, plus product configuration parsing.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use quiver_proto::ownership;

use crate::demux::{Demux, Ownership, ServiceConnection};

/// Ownership protocol version Connect speaks; newer servers accept older versions.
const OWNERSHIP_PROTO_VERSION: u32 = 7;

/// Re-initialize an ownership session older than this before using it, in case the
/// server expires idle sessions without saying so.
const MAX_SESSION_AGE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub product_id: u32,
    pub name: Option<String>,
    pub product_type: u32,
    pub state: u32,
    pub owned: bool,
    pub download_id: Option<u32>,
    pub latest_manifest: Option<String>,
    pub associations: Vec<u32>,
    pub space_id: Option<String>,
    pub app_id: Option<String>,
    pub locked_by_subscription: bool,
    #[serde(skip)]
    pub configuration: Option<String>,
    #[serde(skip)]
    pub raw: ownership::OwnedGame,
}

impl Product {
    fn from_owned(g: ownership::OwnedGame) -> Self {
        let config = g.configuration.clone().filter(|c| !c.is_empty());
        let name = config.as_deref().and_then(|c| ProductConfig::parse(c).ok()).and_then(|c| c.name());
        Self {
            product_id: g.product_id,
            name,
            product_type: g.product_type.unwrap_or_default(),
            state: g.state.unwrap_or_default(),
            owned: g.owned.unwrap_or(false),
            download_id: g.download_id.filter(|&d| d != 0),
            // Products without a PC build report 40 spaces here.
            latest_manifest: g.latest_manifest.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned),
            associations: g.product_associations.clone(),
            space_id: g.ubiservices_space_id.clone(),
            app_id: g.ubiservices_app_id.clone(),
            locked_by_subscription: g.locked_by_subscription.unwrap_or(false),
            configuration: config,
            raw: g,
        }
    }

    /// Has a downloadable PC build.
    pub fn is_installable(&self) -> bool {
        self.latest_manifest.is_some()
    }
}

/// An initialized `ownership_service` session and the owned products it reported.
///
/// `ownership_service` only answers the most recently initialized ownership connection on
/// a demux socket, and every `Library::fetch` (library refresh, launch context, install,
/// launch, the broker) opens one. Requests on an older one go unanswered until they time
/// out, so every request first re-initializes the session if another has replaced it, the
/// server closed it, or it is older than [`MAX_SESSION_AGE`].
pub struct Library {
    demux: Demux,
    conn: ServiceConnection<Ownership>,
    opened: Instant,
    pub products: Vec<Product>,
}

impl Library {
    pub async fn fetch(demux: &Demux) -> Result<Self> {
        let (conn, products) = {
            let _one_at_a_time = demux.exclusive().await;
            initialize(demux).await?
        };
        Ok(Self { demux: demux.clone(), conn, opened: Instant::now(), products })
    }

    /// Why this session can't be used as is, if it can't.
    pub fn stale_reason(&self) -> Option<&'static str> {
        if !self.conn.is_open() {
            Some("closed by the server")
        } else if !self.conn.is_latest() {
            Some("replaced by a newer ownership session")
        } else if self.opened.elapsed() > MAX_SESSION_AGE {
            Some("idle too long")
        } else {
            None
        }
    }

    /// Re-initialize the session (and refresh `products`) if it went stale. The caller holds
    /// [`Demux::exclusive`].
    async fn ensure_fresh(&mut self) -> Result<()> {
        if let Some(reason) = self.stale_reason() {
            tracing::info!(connection = self.conn.id(), reason, "reinitializing ownership session");
            let (conn, products) = initialize(&self.demux).await?;
            self.conn = conn;
            self.products = products;
            self.opened = Instant::now();
        }
        Ok(())
    }

    async fn request(&mut self, req: ownership::Req) -> Result<ownership::Rsp> {
        let demux = self.demux.clone();
        let _one_at_a_time = demux.exclusive().await;
        self.ensure_fresh().await?;
        self.conn.request(req).await
    }

    pub fn get(&self, product_id: u32) -> Option<&Product> {
        self.products.iter().find(|p| p.product_id == product_id)
    }

    /// Fetch a product's configuration YAML (needed for products not in the owned list).
    pub async fn product_config(&mut self, product_id: u32) -> Result<String> {
        let rsp = self
            .request(ownership::Req {
                get_product_config_req: Some(ownership::GetProductConfigReq {
                    product_id,
                    deprecated_test_config: Some(false),
                }),
                ..Default::default()
            })
            .await?;
        let cfg = rsp.get_product_config_rsp.context("no get_product_config_rsp")?;
        if cfg.result != ownership::get_product_config_rsp::Result::Success as i32 {
            bail!("product config for {product_id}: result {}", cfg.result);
        }
        cfg.configuration.context("product config response had no configuration")
    }
}

impl Library {
    /// Short-lived signed ownership token, the input to `denuvo_service`.
    pub async fn ownership_token(&mut self, product_id: u32) -> Result<String> {
        let rsp = self
            .request(ownership::Req {
                ownership_token_req: Some(ownership::OwnershipTokenReq { product_id: Some(product_id) }),
                ..Default::default()
            })
            .await?;
        let t = rsp.ownership_token_rsp.context("no ownership_token_rsp")?;
        if t.success != Some(true) {
            bail!("ownership token for {product_id} refused");
        }
        t.token.context("ownership token response had no token")
    }

    /// Per-game ticket handed to the SDK as `InitRsp.upcTicket`.
    pub async fn uplay_pc_ticket(&mut self, uplay_id: u32) -> Result<String> {
        let rsp = self
            .request(ownership::Req {
                get_uplay_pc_ticket_req: Some(ownership::GetUplayPcTicketReq { uplay_id, platform: None }),
                ..Default::default()
            })
            .await?;
        let t = rsp.get_uplay_pc_ticket_rsp.context("no get_uplay_pc_ticket_rsp")?;
        if !t.success {
            bail!("uplay PC ticket for {uplay_id} refused: error code {:?}", t.error_code);
        }
        t.uplay_pc_ticket.context("ticket response had no ticket")
    }
}

/// Open an ownership connection and initialize it (which lists the owned products).
async fn initialize(demux: &Demux) -> Result<(ServiceConnection<Ownership>, Vec<Product>)> {
    let mut conn = demux.open::<Ownership>().await?;
    let rsp = conn
        .request(ownership::Req {
            initialize_req: Some(ownership::InitializeReq {
                get_associations: Some(true),
                proto_version: Some(OWNERSHIP_PROTO_VERSION),
                use_staging: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await?;
    let init = rsp.initialize_rsp.context("no initialize_rsp")?;
    if !init.success {
        bail!("ownership initialize failed");
    }
    let products = init
        .owned_games
        .map(|o| o.owned_games)
        .unwrap_or_default()
        .into_iter()
        .map(Product::from_owned)
        .collect();
    Ok((conn, products))
}

pub const ART_BASE_URL: &str = "https://ubistatic3-a.akamaihd.net/orbit/uplay_launcher_3_0/assets/";

/// A product's configuration: YAML with `root:` (name, executables, registry
/// keys, ...) and `localizations:` (strings referenced by key from `root`).
pub struct ProductConfig {
    pub yaml: serde_yaml::Value,
}

impl ProductConfig {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(Self { yaml: serde_yaml::from_str(text).context("parse product configuration")? })
    }

    pub fn root(&self) -> &serde_yaml::Value {
        &self.yaml["root"]
    }

    /// Resolve a string that may be a localization key (e.g. `l1`).
    pub fn localize(&self, s: &str) -> String {
        self.yaml["localizations"]["default"][s].as_str().unwrap_or(s).to_owned()
    }

    /// An art file name from the config root (`thumb_image`, `background_image`, …),
    /// resolving localization keys. Served from Ubisoft's asset CDN ([`ART_BASE_URL`]).
    pub fn art(&self, field: &str) -> Option<String> {
        let v = self.root()[field].as_str()?;
        let resolved = self.localize(v);
        resolved.contains('.').then_some(resolved)
    }

    pub fn name(&self) -> Option<String> {
        let root = self.root();
        root["name"]
            .as_str()
            .or_else(|| root["display_name"].as_str())
            .map(|s| self.localize(s))
    }

    /// Executables under `start_game.online.executables` (falling back to offline),
    /// as `relative` paths inside the install directory.
    pub fn executables(&self) -> Vec<Executable> {
        let start = &self.root()["start_game"];
        let list = start["online"]["executables"]
            .as_sequence()
            .or_else(|| start["offline"]["executables"].as_sequence());
        list.into_iter()
            .flatten()
            .filter_map(|e| {
                Some(Executable {
                    path: e["path"]["relative"].as_str()?.to_owned(),
                    working_dir: e["working_directory"]["append"].as_str().map(str::to_owned),
                    arguments: e["arguments"].as_str().map(str::to_owned),
                    label: e["shortcut_name"].as_str().map(|s| self.localize(s)),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Executable {
    /// Relative to the working directory (install dir + `working_dir`, if any).
    pub path: String,
    /// Subdirectory of the install dir the game runs from (`working_directory.append`).
    pub working_dir: Option<String>,
    pub arguments: Option<String>,
    pub label: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use prost::Message;

    use super::*;
    use crate::demux::fake;

    /// Like the real `ownership_service`: only the most recently initialized connection
    /// gets answers; requests on older ones are silently dropped.
    fn ownership_server(inits: Arc<Mutex<u32>>) -> fake::Handler {
        let mut current = None;
        Box::new(move |service, conn, body| {
            assert_eq!(service, "ownership_service");
            let req = ownership::Upstream::decode(body).ok()?.request?;
            let mut rsp = ownership::Rsp { request_id: req.request_id, ..Default::default() };
            if req.initialize_req.is_some() {
                current = Some(conn);
                *inits.lock().unwrap() += 1;
                rsp.initialize_rsp = Some(ownership::InitializeRsp { success: true, ..Default::default() });
            } else if current != Some(conn) {
                return None;
            } else if req.ownership_token_req.is_some() {
                rsp.ownership_token_rsp =
                    Some(ownership::OwnershipTokenRsp { success: Some(true), token: Some(format!("token-{conn}")), expiration: None });
            }
            Some(ownership::Downstream { response: Some(rsp), push: None }.encode_to_vec())
        })
    }

    #[tokio::test]
    async fn superseded_session_reinitializes_before_use() {
        let inits = Arc::new(Mutex::new(0));
        let server = fake::start(Duration::from_secs(10), ownership_server(inits.clone())).await;
        // The broker's long-lived library, then a one-shot one (a launch context).
        let mut broker = Library::fetch(&server.demux).await.unwrap();
        assert_eq!(broker.stale_reason(), None);
        assert!(broker.ownership_token(1).await.unwrap().starts_with("token-"));
        let mut launch = Library::fetch(&server.demux).await.unwrap();
        assert!(launch.ownership_token(1).await.is_ok());
        assert_eq!(broker.stale_reason(), Some("replaced by a newer ownership session"));
        assert_eq!(*inits.lock().unwrap(), 2);

        // Without the refresh this would wait out the 10 s timeout.
        let t = Instant::now();
        let token = broker.ownership_token(1).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "waited {:?}", t.elapsed());
        assert_eq!(*inits.lock().unwrap(), 3);
        assert_eq!(broker.stale_reason(), None);
        // And the other one recovers the same way.
        assert_ne!(launch.ownership_token(1).await.unwrap(), token);
        assert_eq!(*inits.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn closed_session_reinitializes_before_use() {
        let inits = Arc::new(Mutex::new(0));
        let server = fake::start(Duration::from_secs(10), ownership_server(inits.clone())).await;
        let mut lib = Library::fetch(&server.demux).await.unwrap();
        server.inject.send(fake::closed_push(lib.conn.id())).unwrap();
        for _ in 0..200 {
            if lib.stale_reason().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(lib.stale_reason(), Some("closed by the server"));
        let t = Instant::now();
        assert!(lib.ownership_token(1).await.is_ok());
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(*inits.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn fresh_session_is_reused() {
        let inits = Arc::new(Mutex::new(0));
        let server = fake::start(Duration::from_secs(10), ownership_server(inits.clone())).await;
        let mut lib = Library::fetch(&server.demux).await.unwrap();
        for _ in 0..3 {
            lib.ownership_token(1).await.unwrap();
        }
        assert_eq!(*inits.lock().unwrap(), 1);
    }
}
