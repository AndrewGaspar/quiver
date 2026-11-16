//! Data the native client hands to `ubi-bridge` inside the game's prefix.

use serde::{Deserialize, Serialize};

/// Everything the bridge needs to answer a game's SDK `InitReq` like Connect would.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchContext {
    pub uplay_id: u32,
    pub account_id: String,
    pub username: String,
    pub name_on_platform: String,
    /// Games read it via `UPLAY_USER_GetEmailUtf8`; Origins skips loading saves without it.
    #[serde(default)]
    pub email: Option<String>,
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
    /// Installed chunks from the game's `uplay_install.manifest` (for `UPLAY_INSTALLER_*`).
    #[serde(default)]
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: u32,
    #[serde(default)]
    pub tags: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdKey {
    pub uplay_id: u32,
    pub key: String,
}

/// Requests from `ubi-bridge` to the native broker (in `quiverd`), one JSON object per line
/// over TCP. The broker holds the Ubisoft session; the bridge never sees account credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerRequest {
    /// Denuvo activation: relay the game's request token, get a game token back.
    DenuvoGameToken {
        uplay_id: u32,
        request_token: Vec<u8>,
        /// The launch context's account (user id); older bridges omit it: default account.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
    /// The game started (SDK initialized). The broker answers `Ok` and keeps a play session
    /// open for `uplay_id` until this connection closes, so the bridge holds the connection
    /// for as long as the game's SDK connection lives.
    PlaySession {
        uplay_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerResponse {
    GameToken { game_token: Vec<u8> },
    Ok,
    Error { message: String },
}
