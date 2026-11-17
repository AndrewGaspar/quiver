//! Data the native client hands to `quiver-bridge` inside the game's prefix.

use serde::{Deserialize, Serialize};

pub mod modrelay;
pub mod run;

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
    // -- Mod SDK (appended; older bridges ignore these) -----------------------------------
    /// The launch's session id (`QUIVER_SESSION`), which also names the mod pipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The pipe the bridge creates for mods (`QUIVER_MODS_PIPE`, see
    /// `quiver_proto::modsdk::pipe_name`). The launcher puts the same name in the game's
    /// environment when it starts the game itself (the bridge is a sibling, not a parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods_pipe: Option<String>,
    /// The mod profile the launch runs (`QUIVER_PROFILE`), passed on with mod channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
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

/// Requests from `quiver-bridge` to the native broker (in `quiverd`), one JSON object per line
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
    /// A mod inside the game connected to the bridge's mod pipe. The broker answers `Ok`,
    /// then the connection carries the mod SDK's JSON messages, one per line, both ways
    /// (`quiver_proto::modsdk`), until either side closes it. The mod's `hello` is the first.
    ModChannel {
        uplay_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// The bridge's connection number, for matching its log.
        #[serde(default)]
        conn: u32,
        /// Windows pid of the process that opened the pipe, as Wine reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_pid: Option<u32>,
    },
    // -- Achievements (appended; older brokers answer these with an error) ---------------
    /// The game's achievements with this account's unlock status, for the SDK's
    /// `GetAchievements`. Answered with `BrokerResponse::Achievements`.
    Achievements {
        uplay_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        /// The game's `languageCountryCode` (`en-US`), for names and descriptions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// The game earned an achievement (`EarnAchievement`). The broker answers `Ok` at once
    /// and reports it upstream in the background (deduplicated; see quiverd's journal).
    AchievementEarned {
        uplay_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        achievement_id: u32,
    },
}

/// One achievement as the bridge hands it to the game (`uplaydll.AchievementData`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BridgeAchievement {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub earned: bool,
    /// Unix seconds; 0 when unknown or not earned.
    #[serde(default)]
    pub earned_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerResponse {
    GameToken { game_token: Vec<u8> },
    Ok,
    Error { message: String },
    /// `BrokerRequest::Achievements`. `unlocked_known`: false when Ubisoft's unlock status
    /// couldn't be read (the list then shows only what was earned through Quiver).
    Achievements {
        items: Vec<BridgeAchievement>,
        #[serde(default)]
        unlocked_known: bool,
    },
}

/// Top-level field numbers in a protobuf message, in order, for logging requests the
/// bridge's protocol doesn't know (prost drops unknown fields silently). `None` if the
/// bytes aren't a well-formed message.
pub fn field_numbers(mut b: &[u8]) -> Option<Vec<u32>> {
    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&x, rest) = b.split_first()?;
            *b = rest;
            v |= u64::from(x & 0x7f) << shift;
            if x < 0x80 {
                return Some(v);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let tag = varint(&mut b)?;
        let skip = match tag & 7 {
            0 => {
                varint(&mut b)?;
                0
            }
            1 => 8,
            2 => usize::try_from(varint(&mut b)?).ok()?,
            5 => 4,
            _ => return None,
        };
        b = b.get(skip..)?;
        out.push(u32::try_from(tag >> 3).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn field_numbers() {
        // Field 4 (the old partyReq) holding an empty InitPartyReq (field 1).
        assert_eq!(super::field_numbers(&[0x22, 0x02, 0x0a, 0x00]), Some(vec![4]));
        // varint field 1, fixed32 field 2, fixed64 field 3, then a length-delimited 300.
        let mut b = vec![0x08, 0x96, 0x01, 0x15, 1, 2, 3, 4, 0x19, 0, 0, 0, 0, 0, 0, 0, 0, 0xe2, 0x12, 0x01, 0x00];
        assert_eq!(super::field_numbers(&b), Some(vec![1, 2, 3, 300]));
        b.pop();
        assert_eq!(super::field_numbers(&b), None);
        assert_eq!(super::field_numbers(&[]), Some(vec![]));
    }
}
