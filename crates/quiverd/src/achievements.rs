//! `Request::Achievements`: unlock status, the report journal and manual resends, all for
//! the game's launch account. The work is `quiver_core::achievements::Engine`'s; games reach
//! the same engine through the broker (`BrokerRequest::Achievements`, `AchievementEarned`).

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use quiver_api::achievements::{AchievementsReport, AchievementsRequest};
use quiver_core::achievements::{Engine, Framing, LiveUpstream, ReportMode};

use crate::Daemon;
use crate::info::{achievement_refs, cache_dir, needs_approval};

/// How often pending reports are retried (each try only reaches Ubisoft for games with
/// pending entries, on accounts that are signed in).
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The engine quiverd and its broker share: journals under `<data>/achievements`, each
/// game's `achievements` setting, `AchievementUnlocked` events.
pub(crate) fn engine(accounts: Arc<quiver_core::online::Accounts>, events: tokio::sync::broadcast::Sender<quiver_api::Event>) -> Arc<Engine> {
    let up = Arc::new(LiveUpstream { accounts, cache: cache_dir().join("achievements") });
    Engine::new(
        up,
        quiver_core::launcher::data_dir().join("achievements"),
        Box::new(|product_id| {
            let setting = quiver_core::settings::current(product_id).ok().and_then(|(s, _)| s.achievements);
            ReportMode::from_setting(setting.as_deref())
        }),
        Box::new(move |e| {
            let _ = events.send(e);
        }),
    )
}

/// Retry pending reports now and then (the first try a minute after start).
pub(crate) fn start(daemon: &Arc<Daemon>) {
    let engine = daemon.achievements.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            engine.retry_all().await;
            tokio::time::sleep(RETRY_EVERY).await;
        }
    });
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: AchievementsRequest) -> Result<Value> {
    let product_id = match &req {
        AchievementsRequest::List { product_id, .. }
        | AchievementsRequest::Report { product_id, .. }
        | AchievementsRequest::Probe { product_id, .. } => *product_id,
    };
    let online = daemon.account_for(product_id, None).await?;
    let Some(user) = online.user_id().map(str::to_owned) else { bail!("no account for this game") };
    if let Some(refs) = achievement_refs(&online, product_id).await {
        daemon.achievements.set_refs(product_id, refs);
    }
    let engine = daemon.achievements.clone();
    Ok(match req {
        AchievementsRequest::List { refresh, .. } => {
            let s = engine.status(Some(&user), product_id, refresh, "en-US").await?;
            json!(AchievementsReport {
                product_id,
                account: Some(s.user),
                achievements: s.achievements,
                fetched: s.journal.known_at,
                sync_id: s.journal.product.as_ref().map(|p| p.sync_id.clone()),
                platform_id: s.journal.product.as_ref().map(|p| p.platform_id.clone()),
                reporting: s.mode.as_str().into(),
                journal: s.journal.entries,
                errors: s.errors,
            })
        }
        AchievementsRequest::Report { achievement_id, dry_run, confirmed, .. } => {
            let name = daemon.product_name(product_id);
            if !dry_run && needs_approval(source) && !confirmed {
                bail!(
                    "sending an achievement to Ubisoft needs your approval: run `quiver achievements {product_id} report {achievement_id}` yourself (`--dry-run` shows the request without sending it)"
                );
            }
            let r = engine.report(Some(&user), product_id, achievement_id, dry_run).await?;
            if r.sent {
                daemon.log_activity(
                    source,
                    format!("resent {name} achievement {achievement_id} to Ubisoft{}", if confirmed { " (approved)" } else { "" }),
                    Some(product_id),
                );
            }
            json!(r)
        }
        AchievementsRequest::Probe { platform_id, framing, all_products, .. } => {
            let framing = framing.as_deref().map(Framing::parse).transpose()?;
            engine.probe(Some(&user), product_id, platform_id.as_deref(), framing, all_products).await?
        }
    })
}
