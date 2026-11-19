//! `Request::LaunchMaterial`: the licence for one launch of an installed game that a
//! `quiver lab` instance starts (design/LAB.md "Licensing"). Minted from quiverd's own
//! session for that launch only, so an instance never holds a refresh token, a password or
//! a login of its own:
//!
//! - **Ubisoft**: the bridge's launch context (the ownership ticket, owned ids, CD keys,
//!   the broker's address). The bridge in the instance relays Denuvo to the broker; with a
//!   lab session id its play session is a lab one (`playsession::LAB_HOLDER`).
//! - **Epic**: EGL's arguments with a fresh single-use exchange code, and the ownership
//!   token when the game wants one.
//! - **Steam, others**: nothing quiverd can mint (`LaunchMaterial::None`, with why).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use quiver_api::lab::{LaunchMaterial, OwnershipToken, is_lab_session};
use quiver_api::{GameRef, StoreId};
use quiver_core::library::Library;
use serde_json::{Value, json};

use crate::{Daemon, run_local};

pub(crate) struct MaterialRequest {
    pub game: GameRef,
    pub session: Option<String>,
    pub savegame_path: Option<String>,
    pub install_dir: Option<std::path::PathBuf>,
    pub ovt_dir: Option<String>,
    pub account: Option<String>,
}

/// Why quiverd has nothing to mint for `store` (a lab launch of such a game is DRM-free
/// or refused; design/LAB.md "Steam").
pub(crate) fn no_material_reason(store: StoreId) -> String {
    match store {
        StoreId::Steam => "Steam's DRM and Steamworks answer only to the running Steam client, which quiverd doesn't hand out; \
                           a lab runs a Steam game only when it is DRM-free (no Steamworks, no SteamStub)"
            .into(),
        s => format!("quiverd has no launch material for {} games; a lab runs them only when they are DRM-free", s.label()),
    }
}

pub(crate) async fn launch_material(d: &Arc<Daemon>, source: &str, r: MaterialRequest) -> Result<Value> {
    if let Some(s) = &r.session
        && !is_lab_session(s)
    {
        bail!("'{s}' is not a lab session id (lab-<instance>-r<run>)");
    }
    let game = r.game;
    let material = match game.store {
        StoreId::Ubisoft => {
            let product_id = game.as_ubisoft().context("bad Ubisoft ref")?;
            let online = d.account_for(product_id, r.account.as_deref()).await?;
            let label = d.as_account(&online);
            let broker = d.broker.clone();
            let (savegame_path, install_dir, session) = (r.savegame_path, r.install_dir, r.session.clone());
            let ctx = run_local(move || async move {
                let s = online.session().await?;
                let demux = online.demux().await?;
                let mut lib = Library::fetch(&demux).await?;
                let mut ctx = quiver_core::launch::build(&online.auth, &s, &mut lib, product_id).await?;
                ctx.savegame_path = savegame_path;
                ctx.broker = Some(broker);
                if let Some(dir) = install_dir {
                    ctx.chunks = quiver_core::launch::installed_chunks(&dir).unwrap_or_default();
                }
                if let Some(s) = session {
                    ctx.mods_pipe = Some(quiver_proto::modsdk::pipe_name(Some(&s)));
                    ctx.session = Some(s);
                }
                Ok(ctx)
            })
            .await?;
            d.log_activity(source, format!("issued lab launch material for {}{label}{}", d.product_name(product_id), session_note(&r.session)), Some(product_id));
            LaunchMaterial::Ubisoft { context: serde_json::to_value(&ctx)? }
        }
        StoreId::Epic => {
            let app = game.id.as_str().to_owned();
            let dir = r.ovt_dir.clone().unwrap_or_else(|| r"C:\quiver-lab".into());
            let m = d.epic.launch_material(&app, |name| format!(r"{}\{name}", dir.trim_end_matches('\\')), false).await?;
            let title = d.epic.installed_game(&app).map(|i| i.title).unwrap_or_else(|| game.to_string());
            d.log_activity_game(source, format!("issued lab launch material for {title} (Epic){}", session_note(&r.session)), Some(game));
            LaunchMaterial::Epic {
                shown: m.shown,
                args: m.args,
                ownership_token: m.ownership_token.map(|(file_name, t)| OwnershipToken {
                    file_name,
                    base64: base64::engine::general_purpose::STANDARD.encode(t.expose()),
                }),
            }
        }
        s => LaunchMaterial::None { reason: no_material_reason(s) },
    };
    Ok(json!(material))
}

fn session_note(s: &Option<String>) -> String {
    s.as_ref().map(|s| format!(", session {s}")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_has_no_material() {
        assert!(no_material_reason(StoreId::Steam).contains("Steam client"));
        assert!(no_material_reason(StoreId::Gog).contains("GOG"));
    }
}
