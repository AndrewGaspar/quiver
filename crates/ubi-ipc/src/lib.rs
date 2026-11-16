//! Data the native client hands to `ubi-bridge` inside the game's prefix.

use serde::{Deserialize, Serialize};

/// Everything the bridge needs to answer a game's SDK `InitReq` like Connect would.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchContext {
    pub uplay_id: u32,
    pub account_id: String,
    pub username: String,
    pub name_on_platform: String,
    /// Per-game ticket from `ownership_service.GetUplayPCTicketReq`.
    pub upc_ticket: String,
    /// The game plus owned associated products (DLC, editions).
    pub owned_uplay_ids: Vec<u32>,
    pub cd_keys: Vec<CdKey>,
    /// Windows path for SDK savegames (`UPLAY_SAVE_*`).
    pub savegame_path: Option<String>,
    pub ubiservices_app_id: Option<String>,
    pub language: Option<String>,
    /// `host:port` of the native broker for live requests (Denuvo tokens).
    #[serde(default)]
    pub broker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdKey {
    pub uplay_id: u32,
    pub key: String,
}

/// Requests from `ubi-bridge` to the native broker (`ubi broker`), one JSON object per line
/// over TCP. The broker holds the Ubisoft session; the bridge never sees account credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerRequest {
    /// Denuvo activation: relay the game's request token, get a game token back.
    DenuvoGameToken { uplay_id: u32, request_token: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerResponse {
    GameToken { game_token: Vec<u8> },
    Error { message: String },
}
