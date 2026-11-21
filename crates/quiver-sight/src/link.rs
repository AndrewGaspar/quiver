//! quiver-sight's connection to quiverd, on its own tokio thread: attaches to every running
//! session whose HUD is enabled (`HudAttach`), follows their `HudModel`s, turns achievement
//! and option-outcome events of Quiver launches, notices (design/SIGHT.md §16) and finished
//! downloads into toasts, passes panel commands and Sight's settings on, does what the panel
//! and the quick menu ask ([`LinkReq`]), and says when to quit (quiverd went away, or no
//! session for a while). The GPUI side gets [`Msg`]s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::channel::mpsc::UnboundedSender;
use quiver_api::options::{Live, OptionOp, OptionsChange, OptionsEvent};
use quiver_api::perf::{HudModel, HudScope, PerfEvent, PerfRequest, PerfSessionInfo, PerfUpdate};
use quiver_api::sight::{PanelCommand, PanelReport, QuickPin, SightNotice, SightRequest, SightSettings, SightStatus};
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
    /// The compositor's toplevels changed (or there's no list): [`crate::toplevels`].
    Toplevels(crate::toplevels::Event),
    /// The panel's surface (the `map_gen`th one) gained or lost the keyboard.
    PanelKeyboard { map_gen: u64, active: bool },
    /// Ask again whether the game is in front (a hide or grace timer ran out).
    Front,
    /// Keyboard and mouse input the game's input preload forwarded to the panel drawn in the
    /// game (`panel::ingame`).
    GameInput { session: String, events: Vec<quiver_perf::panel::PanelEvent> },
    // ---- design/SIGHT.md §16
    /// A notice for the toast lane.
    Notice(Box<SightNotice>),
    /// A notice was answered (from here or elsewhere): its toast goes.
    NoticeAnswered(String),
    /// Sight's settings changed.
    Settings(Box<SightSettings>),
    PinFailed { scope: String, reason: String },
    /// Read the panel chord again (the quick menu, every `quick::POLL` while it matters).
    QuickPoll,
    Replay { session: String, action: crate::replay::Action },
    ReplayFeedback { session: String, result: Result<String, String> },
    /// quiverd's answer to the Performance section (`panel::sections::performance`).
    Perf { session: String, reply: crate::panel::sections::performance::Reply },
    /// The panel's layout changed (docked or centred, design/SIGHT.md §9): the backdrop
    /// follows (no blur or dim while docked), and the sections' choices are saved.
    PanelLayout { docked: bool, prefs: std::collections::BTreeMap<String, bool> },
}

// Sent boxed (`Msg::Loaded`): the variants' sizes don't matter.
#[allow(clippy::large_enum_variant)]
pub enum Loaded {
    All(Box<PanelData>),
    /// The option surfaces, and their readouts (`PanelData::readouts`).
    Options(Load<Vec<quiver_api::options::SurfaceState>>, std::collections::BTreeMap<String, Vec<(String, String)>>),
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
    /// A screenshot, and where it went (empty when it failed).
    Screenshot(std::path::PathBuf),
    /// A batch from Mods (`tweaks` false) or Tweaks: Undo, X, a preset, Revert this session,
    /// Session only, an A/B compare.
    Ops { tweaks: bool, why: Why },
}

/// Why the Mods or Tweaks section sent a batch ([`Change::Ops`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Undo,
    Reset,
    Preset,
    Revert,
    SessionOnly,
    Compare,
}

/// What the panel asks of quiverd.
pub enum LinkReq {
    Load { session: String, part: Part, view: Option<SessionView> },
    /// One option (`live: only` while the Mods section's "Session only" is on).
    SetOption { view: SessionView, op: OptionOp, key: (String, String), live: Live },
    SetTweak { view: SessionView, op: OptionOp, id: String },
    /// Several changes as one revision (Mods: `OptionsSet`; `tweaks`: `TweaksSet`, which has no
    /// `live`).
    OptionOps { view: SessionView, tweaks: bool, ops: Vec<OptionOp>, live: Live, why: Why },
    /// `OptionsRevertSession`: back to what the session launched with.
    RevertSession { view: SessionView },
    /// Session only: `back` saved without touching the game (`live: no`), then `live` for the
    /// running session only (`live: only`).
    SessionOnly { view: SessionView, back: Vec<OptionOp>, live: Vec<OptionOp> },
    HudSet { view: SessionView, set: HudScope },
    HudToggle { session: String },
    Stop { game: GameRef },
    /// The game's next frame as a PNG (`SightRequest::Screenshot`).
    Screenshot { session: String },
    /// A screenshot for the quick menu or an unlock (design/SIGHT.md §16): the game's frame
    /// through quiverd (the Sight layer's capture, under Sight's own drawing) after `after`;
    /// where the layer can't, `grim` of `monitor` into the same folder. `say`: a toast with
    /// where it went (errors are said either way when `say`, logged otherwise).
    Shot { session: String, after: std::time::Duration, monitor: Option<String>, game: String, say: bool },
    /// The panel's metrics, and `extra` ones (the mods' readouts).
    Subscribe { session: String, extra: Vec<String> },
    Unsubscribe,
    Report(PanelReport),
    /// The pad's long-press chord: what `quiver sight toggle` does.
    Toggle,
    // ---- design/SIGHT.md §16
    NoticeAct { id: String, button: String },
    SettingsSet { key: String, value: serde_json::Value },
    Pin { scope: String, pin: QuickPin, on: bool },
    /// The Performance section's requests (`panel::sections::performance::link`).
    Perf { session: String, req: crate::panel::sections::performance::Req },
}

/// After the last session ends, wait this long for another before quitting.
pub const IDLE_GRACE: Duration = Duration::from_secs(5);
/// Started with no session at all (it ended before we connected): quit after this.
pub const START_GRACE: Duration = Duration::from_secs(20);

struct Known {
    /// The session's game (another store's too).
    game: GameRef,
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
        let k = known.entry(s.session.clone()).or_insert(Known { game: s.game_ref(), attached: false });
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
    // Install jobs already done when we started, or toasted: never again.
    let mut toasted: std::collections::HashSet<u64> = client.call::<quiver_api::Status>(Request::Status).await.map(|st| st.jobs).unwrap_or_default().into_iter().filter(|j| j.done).map(|j| j.id).collect();
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
                Some(e @ Event::AchievementUnlocked { product_id, game, .. }) => {
                    // Quiver launches only: containers' and agents' bridges earn too. An
                    // Epic game's unlocks (`game`) are its session's.
                    let of = game.unwrap_or(GameRef::ubisoft(product_id));
                    if let Some((sid, _)) = known.iter().find(|(_, k)| k.game == of)
                        && let Some(mut t) = toasts::from_event(&e)
                    {
                        t.session = Some(sid.clone());
                        let _ = tx.unbounded_send(Msg::Toast(t));
                    }
                }
                // Notices for a running game (or any: the focused one shows it).
                Some(Event::SightNotice(n)) => {
                    if n.spec.session.as_ref().is_none_or(|s| known.contains_key(s)) && !known.is_empty() {
                        let _ = tx.unbounded_send(Msg::Notice(Box::new(n)));
                    }
                }
                Some(Event::SightNoticeAction { id, .. }) => {
                    let _ = tx.unbounded_send(Msg::NoticeAnswered(id));
                }
                Some(Event::SightSettingsChanged(st)) => {
                    let _ = tx.unbounded_send(Msg::Settings(Box::new(st)));
                }
                // A download or update finished while a game runs (each job once).
                Some(Event::Job(j)) => {
                    if j.done && !toasted.contains(&j.id) && let Some(t) = toasts::from_job(&j) {
                        toasted.insert(j.id);
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

async fn screenshot(client: &Client, session: &str) -> Result<quiver_api::sight::session::Screenshot> {
    client.call(Request::Sight(SightRequest::Screenshot { session: Some(session.to_owned()) })).await
}

/// The layer's capture, else `grim`: the file, or both reasons.
async fn shot(client: &Client, session: &str, monitor: Option<String>, game: String) -> Result<std::path::PathBuf, String> {
    match screenshot(client, session).await {
        Ok(s) => Ok(s.path),
        Err(e) => {
            tracing::info!(session, "the game's own frame: {e:#}; grim instead");
            match tokio::task::spawn_blocking(move || crate::screenshot::take(monitor.as_deref(), &game)).await {
                Ok(Ok(p)) => Ok(p),
                Ok(Err(g)) => Err(format!("{e:#}; grim: {g:#}")),
                Err(j) => Err(format!("{e:#}; {j}")),
            }
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
                    Part::Options => Loaded::Options(d.options, d.readouts),
                    Part::Tweaks => Loaded::Tweaks(d.tweaks),
                    Part::Hud => Loaded::Hud(d.hud),
                });
                send(Msg::Loaded { session, data });
            }
            LinkReq::SetOption { view, op, key, live } => send(done(Change::Option(key.0, key.1), data::set_options(&client, &view, vec![op], live).await.map(Some))),
            LinkReq::SetTweak { view, op, id } => send(done(Change::Tweak(id), data::set_tweaks(&client, &view, vec![op]).await.map(Some))),
            LinkReq::OptionOps { view, tweaks, ops, live, why } => {
                let r = if tweaks { data::set_tweaks(&client, &view, ops).await } else { data::set_options(&client, &view, ops, live).await };
                send(done(Change::Ops { tweaks, why }, r.map(Some)));
            }
            LinkReq::RevertSession { view } => send(done(Change::Ops { tweaks: false, why: Why::Revert }, data::revert_session(&client, &view).await.map(Some))),
            LinkReq::SessionOnly { view, back, live } => {
                send(done(Change::Ops { tweaks: false, why: Why::SessionOnly }, data::session_only(&client, &view, back, live).await.map(Some)));
            }
            LinkReq::HudSet { view, set } => send(done(Change::Hud, data::hud_set(&client, view.game, set).await.map(|_| None))),
            LinkReq::HudToggle { session } => send(done(Change::Hud, data::hud_toggle(&client, &session).await.map(|_| None))),
            LinkReq::Stop { game } => send(done(Change::Stop, data::stop(&client, game).await.map(|_| None))),
            LinkReq::Shot { session, after, monitor, game, say } => {
                tokio::time::sleep(after).await;
                let r = shot(&client, &session, monitor, game).await;
                match &r {
                    Ok(p) => tracing::info!(path = %p.display(), "screenshot"),
                    Err(e) => tracing::info!("screenshot: {e}"),
                }
                if say {
                    send(Msg::Toast(crate::toasts::shot_toast(r)));
                }
            }
            LinkReq::Screenshot { session } => match screenshot(&client, &session).await {
                Ok(s) => send(Msg::Done { change: Change::Screenshot(s.path), result: Ok(None) }),
                Err(e) => send(Msg::Done { change: Change::Screenshot(Default::default()), result: Err(format!("{e:#}")) }),
            },
            LinkReq::Subscribe { session, extra } => {
                let old = sub.lock().unwrap().take();
                if let Some(s) = old {
                    data::unsubscribe(&client, s).await;
                }
                match data::subscribe(&client, &session, &extra).await {
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
            LinkReq::Perf { session, req } => {
                let reply = crate::panel::sections::performance::link::run(&client, &session, req).await;
                send(Msg::Perf { session, reply });
            }
            LinkReq::Toggle => {
                if let Err(e) = client.call::<PanelCommand>(Request::Sight(SightRequest::Toggle { session: None, section: None })).await {
                    tracing::info!("controller chord: {e:#}");
                }
            }
            LinkReq::NoticeAct { id, button } => {
                if let Err(e) = client.call::<serde_json::Value>(Request::Sight(SightRequest::NoticeAct { id: id.clone(), button })).await {
                    tracing::info!(%id, "answering the notice: {e:#}");
                }
            }
            LinkReq::SettingsSet { key, value } => match client.call::<SightSettings>(Request::Sight(SightRequest::SettingsSet { key: key.clone(), value })).await {
                Ok(st) => send(Msg::Settings(Box::new(st))),
                Err(e) => tracing::info!(%key, "Sight setting: {e:#}"),
            },
            LinkReq::Pin { scope, pin, on } => match client.call::<SightSettings>(Request::Sight(SightRequest::Pin { scope: scope.clone(), pin, on })).await {
                Ok(st) => send(Msg::Settings(Box::new(st))),
                Err(e) => send(Msg::PinFailed { scope, reason: format!("{e:#}") }),
            },
        }
    });
}
