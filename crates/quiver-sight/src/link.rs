//! quiver-sight's connection to quiverd, on its own tokio thread: attaches to every running
//! session whose HUD is enabled (`HudAttach`), follows their `HudModel`s, turns achievement
//! and option-outcome events of Quiver launches into toasts, and says when to quit (quiverd
//! went away, or no session for a while). The GPUI side gets [`Msg`]s.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::options::OptionsEvent;
use quiver_api::perf::{HudModel, PerfEvent, PerfRequest, PerfSessionInfo};
use quiver_api::{Client, Event, Request};

use crate::toasts::{self, ToastData};

pub enum Msg {
    Model(Box<HudModel>),
    /// A session ended: drop its HUD.
    Ended(String),
    Toast(ToastData),
    Quit(String),
}

/// After the last session ends, wait this long for another before quitting.
pub const IDLE_GRACE: Duration = Duration::from_secs(5);
/// Started with no session at all (it ended before we connected): quit after this.
pub const START_GRACE: Duration = Duration::from_secs(20);

struct Known {
    product_id: u32,
    attached: bool,
}

fn perf(r: PerfRequest) -> Request {
    Request::Perf(r)
}

/// Attach to new sessions; forget ended ones. Returns whether anything changed.
async fn refresh(client: &Client, known: &mut HashMap<String, Known>, tx: &UnboundedSender<Msg>) -> Result<()> {
    let sessions: Vec<PerfSessionInfo> = client.call(perf(PerfRequest::Sessions)).await.context("listing perf sessions")?;
    known.retain(|id, _| sessions.iter().any(|s| &s.session == id));
    for s in sessions {
        let k = known.entry(s.session.clone()).or_insert(Known { product_id: s.product_id, attached: false });
        if k.attached || !(s.hud_enabled && s.dir.is_some()) {
            continue;
        }
        match client.call::<HudModel>(perf(PerfRequest::HudAttach { session: s.session.clone() })).await {
            Ok(m) => {
                tracing::info!(session = %s.session, product_id = s.product_id, presenter = ?m.presenter, "attached to the HUD");
                k.attached = true;
                let _ = tx.unbounded_send(Msg::Model(Box::new(m)));
            }
            Err(e) => tracing::warn!(session = %s.session, "HudAttach: {e:#}"),
        }
    }
    Ok(())
}

pub async fn run(tx: UnboundedSender<Msg>) {
    let why = match run_inner(&tx).await {
        Ok(why) => why,
        Err(e) => format!("{e:#}"),
    };
    let _ = tx.unbounded_send(Msg::Quit(why));
}

async fn run_inner(tx: &UnboundedSender<Msg>) -> Result<String> {
    let client = Client::connect_running("sight").await.context("connecting to quiverd")?;
    let mut events = client.subscribe().await?;
    let mut perf_events = client.subscribe_perf().await?;
    let mut options = client.subscribe_options().await?;
    let mut known: HashMap<String, Known> = HashMap::new();
    refresh(&client, &mut known, tx).await?;
    let mut idle_since = known.is_empty().then(Instant::now);
    let mut grace = START_GRACE;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            e = events.recv() => match e {
                None => return Ok("event stream closed".into()),
                Some(Event::Disconnected) => return Ok("quiverd went away".into()),
                Some(e @ Event::AchievementUnlocked { product_id, .. }) => {
                    // Quiver launches only: containers' and agents' bridges earn too.
                    if known.values().any(|k| k.product_id == product_id)
                        && let Some(t) = toasts::from_event(&e)
                    {
                        let _ = tx.unbounded_send(Msg::Toast(t));
                    }
                }
                Some(_) => {}
            },
            e = perf_events.recv() => match e {
                None => return Ok("perf stream closed".into()),
                Some(PerfEvent::HudModel(m)) => {
                    let _ = tx.unbounded_send(Msg::Model(Box::new(m)));
                }
                Some(PerfEvent::Session { session, ended: true, .. }) => {
                    if known.remove(&session).is_some() {
                        tracing::info!(%session, "session ended");
                    }
                    let _ = tx.unbounded_send(Msg::Ended(session));
                }
                Some(PerfEvent::Session { ended: false, .. }) => refresh(&client, &mut known, tx).await?,
                Some(_) => {}
            },
            e = options.recv() => match e {
                None => return Ok("options stream closed".into()),
                Some(e @ OptionsEvent::Outcome { .. }) => {
                    let OptionsEvent::Outcome { session, .. } = &e else { unreachable!() };
                    if known.contains_key(session)
                        && let Some(t) = toasts::from_options(&e)
                    {
                        let _ = tx.unbounded_send(Msg::Toast(t));
                    }
                }
                Some(_) => {}
            },
            _ = tick.tick() => {
                if known.is_empty() {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= grace {
                        return Ok(format!("no running session for {grace:?}"));
                    }
                } else {
                    idle_since = None;
                    grace = IDLE_GRACE;
                }
            }
        }
        if tx.is_closed() {
            return Ok("the UI is gone".into());
        }
    }
}
