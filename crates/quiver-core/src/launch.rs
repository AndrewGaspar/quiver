//! Builds the [`LaunchContext`] that `quiver-bridge` serves to a game's SDK.

use anyhow::{Context, Result};
use std::path::Path;

pub use quiver_ipc::{CdKey, Chunk, LaunchContext};

use crate::auth::{Auth, Session};
use crate::library::Library;

pub async fn build(auth: &Auth, session: &Session, lib: &mut Library, uplay_id: u32) -> Result<LaunchContext> {
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
    // Every launch context is a session of its own, with its own mod pipe.
    let session_id = crate::modsdk::new_session_id();
    Ok(LaunchContext {
        uplay_id,
        account_id: session.user_id.clone(),
        username: session.name.clone(),
        name_on_platform: session.name.clone(),
        email: match auth.account_email(session).await {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!("no account email for launch context: {e:#}");
                None
            }
        },
        upc_ticket,
        owned_uplay_ids,
        cd_keys,
        savegame_path: None,
        ubiservices_app_id: product.app_id.clone(),
        language: Some("en-US".into()),
        broker: None,
        chunks: Vec::new(),
        mods_pipe: Some(quiver_proto::modsdk::pipe_name(Some(&session_id))),
        session: Some(session_id),
        profile: None,
        ..Default::default()
    })
}

/// Chunks listed in an installed game's `uplay_install.manifest`.
pub fn installed_chunks(install_dir: &Path) -> Result<Vec<Chunk>> {
    let manifest = crate::manifest::load_manifest(&install_dir.join("uplay_install.manifest"))?;
    Ok(manifest
        .chunks
        .iter()
        .map(|c| Chunk { id: c.id, tags: c.tags.clone().filter(|t| !t.is_empty()) })
        .collect())
}
