//! Builds the [`LaunchContext`] that `ubi-bridge` serves to a game's SDK.

use anyhow::{Context, Result};
pub use ubi_ipc::{CdKey, LaunchContext};

use crate::auth::Session;
use crate::library::Library;

pub async fn build(session: &Session, lib: &mut Library, uplay_id: u32) -> Result<LaunchContext> {
    let product = lib
        .get(uplay_id)
        .with_context(|| format!("product {uplay_id} is not in your library"))?
        .clone();
    let mut owned_uplay_ids = vec![uplay_id];
    owned_uplay_ids.extend(
        product
            .associations
            .iter()
            .copied()
            .filter(|&a| lib.get(a).is_some_and(|p| p.owned)),
    );
    let cd_keys = owned_uplay_ids
        .iter()
        .filter_map(|&id| {
            let key = lib.get(id)?.raw.cd_key.clone().filter(|k| !k.is_empty())?;
            Some(CdKey { uplay_id: id, key })
        })
        .collect();
    let upc_ticket = lib.uplay_pc_ticket(uplay_id).await?;
    Ok(LaunchContext {
        uplay_id,
        account_id: session.user_id.clone(),
        username: session.name.clone(),
        name_on_platform: session.name.clone(),
        upc_ticket,
        owned_uplay_ids,
        cd_keys,
        savegame_path: None,
        ubiservices_app_id: product.app_id.clone(),
        language: Some("en-US".into()),
        broker: None,
    })
}
