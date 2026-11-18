//! quiver-sight's connection to quiverd, on its own tokio thread: attaches to every running
//! session whose HUD is enabled (`HudAttach`), follows their `HudModel`s, turns achievement
//! and option-outcome events of Quiver launches into toasts, passes panel commands on, does
//! what the panel asks ([`LinkReq`]), and says when to quit (quiverd went away, or no session
//! for a while). The GPUI side gets [`Msg`]s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::options::{OptionOp, OptionsChange, OptionsEvent};
use quiver_api::perf::{HudModel, HudScope, PerfEvent, PerfRequest, PerfSessionInfo, PerfUpdate};
use quiver_api::sight::{PanelCommand, PanelReport, SightRequest, SightStatus};
use quiver_api::{Client, Event, GameRef, Request};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::panel::data::{self, Part};
use crate::panel::state::{HudState, Load, PanelData, SessionView};
use crate::toasts::{self, ToastData};

pub enum Msg {
    Model(Box<HudModel>),
    /// A session ended: drop its HUD (and its panel).
    Ended(String),
    Toast(ToastData),
    Quit(String),
    /// Open, close or toggle the panel.
    Panel(PanelCommand),
    /// Data for the panel's session.
    Loaded { session: String, data: Box<Loaded> },
    /// A change the panel made, and what came of it.
    Done { change: Change, result: Result<Option<OptionsChange>, String> },
    Metrics(PerfUpdate),
    /// Someone (a mod's own menu, the app) changed options of this session.
    OptionsChanged { session: String },
    Pad(quiver_ui::gamepad::PadEvent),
}

// Sent boxed (`Msg::Loaded`): the variants' sizes don't matter.
#[allow(clippy::large_enum_variant)]
pub enum Loaded {
    All(Box<PanelData>),
    Options(Load<Vec<quiver_api::options::SurfaceState>>),
    Tweaks(Load<quiver_api::options::SurfaceState>),
    Hud(Load<HudState>),
}

/// What a change was about, for its outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// (surface key, option id)
    Option(String, String),
    Tweak(String),
    Hud,
    Stop,
}

/// What the panel asks of quiverd.
pub enum LinkReq {
    Load { session: String, part: Part, view: Option<SessionView> },
    SetOption { view: SessionView, op: OptionOp, key: (String, String) },
    SetTweak { view: SessionView, op: OptionOp, id: String },
    HudSet { view: SessionView, set: HudScope },
    HudToggle { session: String },
    Stop { game: GameRef },
    Subscribe { session: String },
    Unsubscribe,
    Report(PanelReport),
    /// The pad's long-press chord: what `quiver sight toggle` does.
    Toggle,
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

pub async fn run(tx: UnboundedSender<Msg>, reqs: UnboundedReceiver<LinkReq>) {
    let why = match run_inner(&tx, reqs).await {
        Ok(why) => why,
        Err(e) => format!("{e:#}"),
    };
    let _ = tx.unbounded_send(Msg::Quit(why));
}

async fn run_inner(tx: &UnboundedSender<Msg>, mut reqs: UnboundedReceiver<LinkReq>) -> Result<String> {
    let client = Client::connect_running("sight").await.context("connecting to quiverd")?;
    let mut events = client.subscribe().await?;
    let mut perf_events = client.subscribe_perf().await?;
    let mut options = client.subscribe_options().await?;
    let mut known: HashMap<String, Known> = HashMap::new();
    refresh(&client, &mut known, tx).await?;
    // Started for a panel command we missed (it went out before we subscribed).
    if let Ok(st) = client.call::<SightStatus>(Request::Sight(SightRequest::Status)).await
        && let Some(c) = st.last_command.filter(|c| c.fresh(chrono::Utc::now(), 0))
    {
        tracing::info!(seq = c.seq, "acting on the panel command quiverd started us for");
        let _ = tx.unbounded_send(Msg::Panel(c));
    }
    let sub: Arc<Mutex<Option<u64>>> = Arc::default();
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
                Some(PerfEvent::Panel(c)) => {
                    let _ = tx.unbounded_send(Msg::Panel(c));
                }
                Some(PerfEvent::Update(u)) => {
                    if *sub.lock().unwrap() == Some(u.sub) {
                        let _ = tx.unbounded_send(Msg::Metrics(u));
                    }
                }
                Some(_) => {}
            },
            e = options.recv() => match e {
                None => return Ok("options stream closed".into()),
                Some(e @ OptionsEvent::Outcome { .. }) => {
                    let OptionsEvent::Outcome { session, by, .. } = &e else { unreachable!() };
                    // The panel shows its own outcomes on the row.
                    if known.contains_key(session)
                        && by != "sight"
                        && let Some(t) = toasts::from_options(&e)
                    {
                        let _ = tx.unbounded_send(Msg::Toast(t));
                    }
                }
                Some(OptionsEvent::Live { session, by, .. }) if by != "sight" => {
                    let _ = tx.unbounded_send(Msg::OptionsChanged { session });
                }
                Some(_) => {}
            },
            r = reqs.recv() => match r {
                None => return Ok("the UI is gone".into()),
                Some(r) => handle(&client, tx, &sub, r),
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

/// Runs a panel request in its own task, so the event loop never waits on it.
fn handle(client: &Client, tx: &UnboundedSender<Msg>, sub: &Arc<Mutex<Option<u64>>>, r: LinkReq) {
    let (client, tx, sub) = (client.clone(), tx.clone(), sub.clone());
    tokio::spawn(async move {
        let send = |m: Msg| {
            let _ = tx.unbounded_send(m);
        };
        let done = |change: Change, r: Result<Option<OptionsChange>>| Msg::Done { change, result: r.map_err(|e| format!("{e:#}")) };
        match r {
            LinkReq::Load { session, part, view } => {
                let mut d = PanelData::default();
                if let Some(v) = view {
                    d.session = Load::Ready(v);
                }
                data::refresh(&client, &session, part, &mut d).await;
                let data = Box::new(match part {
                    Part::All => Loaded::All(Box::new(d)),
                    Part::Options => Loaded::Options(d.options),
                    Part::Tweaks => Loaded::Tweaks(d.tweaks),
                    Part::Hud => Loaded::Hud(d.hud),
                });
                send(Msg::Loaded { session, data });
            }
            LinkReq::SetOption { view, op, key } => send(done(Change::Option(key.0, key.1), data::set_option(&client, &view, op).await.map(Some))),
            LinkReq::SetTweak { view, op, id } => send(done(Change::Tweak(id), data::set_tweak(&client, &view, op).await.map(Some))),
            LinkReq::HudSet { view, set } => send(done(Change::Hud, data::hud_set(&client, view.game, set).await.map(|_| None))),
            LinkReq::HudToggle { session } => send(done(Change::Hud, data::hud_toggle(&client, &session).await.map(|_| None))),
            LinkReq::Stop { game } => send(done(Change::Stop, data::stop(&client, game).await.map(|_| None))),
            LinkReq::Subscribe { session } => {
                let old = sub.lock().unwrap().take();
                if let Some(s) = old {
                    data::unsubscribe(&client, s).await;
                }
                match data::subscribe(&client, &session).await {
                    Ok(s) => *sub.lock().unwrap() = Some(s),
                    Err(e) => tracing::debug!("metrics: {e:#}"),
                }
            }
            LinkReq::Unsubscribe => {
                let old = sub.lock().unwrap().take();
                if let Some(s) = old {
                    data::unsubscribe(&client, s).await;
                }
            }
            LinkReq::Report(report) => {
                if let Err(e) = client.call::<serde_json::Value>(Request::Sight(SightRequest::Report { report })).await {
                    tracing::debug!("panel report: {e:#}");
                }
            }
            LinkReq::Toggle => {
                if let Err(e) = client.call::<PanelCommand>(Request::Sight(SightRequest::Toggle { session: None, section: None })).await {
                    tracing::info!("controller chord: {e:#}");
                }
            }
        }
    });
}
