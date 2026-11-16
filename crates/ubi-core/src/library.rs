//! Owned-product listing via `ownership_service`, plus product configuration parsing.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use ubi_proto::ownership;

use crate::demux::{Demux, Ownership, ServiceConnection};

/// Ownership protocol version Connect speaks; newer servers accept older versions.
const OWNERSHIP_PROTO_VERSION: u32 = 7;

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

pub struct Library {
    conn: ServiceConnection<Ownership>,
    pub products: Vec<Product>,
}

impl Library {
    pub async fn fetch(demux: &Demux) -> Result<Self> {
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
        Ok(Self { conn, products })
    }

    pub fn get(&self, product_id: u32) -> Option<&Product> {
        self.products.iter().find(|p| p.product_id == product_id)
    }

    /// Fetch a product's configuration YAML (needed for products not in the owned list).
    pub async fn product_config(&mut self, product_id: u32) -> Result<String> {
        let rsp = self
            .conn
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
            .conn
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
            .conn
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
