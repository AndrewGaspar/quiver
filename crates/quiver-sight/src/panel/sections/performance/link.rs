//! What the Performance section asks quiverd (on the link's tokio thread), and the answers.

use anyhow::Result;
use quiver_api::perf::{FrameTimes, HudLayout, HudLayoutInfo, HudPreview, HudScope, HudScopeRef, HudSettingsView, HudStatus, MetricInfo, PerfLogSaved, PerfRequest};
use quiver_api::{Client, Request};

use super::{BUCKETS, SPAN_MS};

#[derive(Debug, Clone, PartialEq)]
pub enum Req {
    /// The last 30 s of frame times (the graph).
    Frames,
    /// The session's metric catalog (the editor's list).
    Catalog,
    /// Presets and user layouts.
    Layouts,
    /// A layout's rows (to edit it).
    Layout(String),
    /// The edited layout with the session's values.
    Preview(HudLayout),
    /// HUD settings at a scope.
    Settings(HudScopeRef),
    /// Change HUD settings at a scope.
    Set { scope: HudScopeRef, set: HudScope },
    /// Save a user layout and select it at `scope`.
    Save { layout: HudLayout, scope: HudScopeRef },
    /// Save the last 30 s as a performance log.
    Log,
    /// Show or hide the HUD of the running game.
    Toggle,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Frames(Result<FrameTimes, String>),
    Catalog(Result<Vec<MetricInfo>, String>),
    Layouts(Result<Vec<HudLayoutInfo>, String>),
    Layout(Result<HudLayout, String>),
    Preview { sent: HudLayout, result: Result<HudPreview, String> },
    Settings(Result<HudSettingsView, String>),
    Set(Result<HudSettingsView, String>),
    Saved(Result<(HudLayoutInfo, HudSettingsView), String>),
    Log(Result<PerfLogSaved, String>),
    Toggled(Result<HudStatus, String>),
}

fn perf(r: PerfRequest) -> Request {
    Request::Perf(r)
}

fn s<T>(r: Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("{e:#}"))
}

const MESSAGE: &str = "in game (Sight)";

/// The answer to `req` when it can't be asked (a lab instance has no quiverd): `why` for each.
pub fn refused(req: &Req, why: String) -> Reply {
    match req {
        Req::Frames => Reply::Frames(Err(why)),
        Req::Catalog => Reply::Catalog(Err(why)),
        Req::Layouts => Reply::Layouts(Err(why)),
        Req::Layout(_) => Reply::Layout(Err(why)),
        Req::Preview(l) => Reply::Preview { sent: l.clone(), result: Err(why) },
        Req::Settings(_) => Reply::Settings(Err(why)),
        Req::Set { .. } => Reply::Set(Err(why)),
        Req::Save { .. } => Reply::Saved(Err(why)),
        Req::Log => Reply::Log(Err(why)),
        Req::Toggle => Reply::Toggled(Err(why)),
    }
}

/// Runs one request for the panel's `session`.
pub async fn run(client: &Client, session: &str, req: Req) -> Reply {
    let session = Some(session.to_owned());
    match req {
        Req::Frames => Reply::Frames(s(client.call(perf(PerfRequest::FrameTimes { session, game: None, span_ms: SPAN_MS, buckets: BUCKETS })).await)),
        Req::Catalog => Reply::Catalog(s(client.call(perf(PerfRequest::Catalog { session, game: None })).await)),
        Req::Layouts => Reply::Layouts(s(client.call(perf(PerfRequest::HudLayouts)).await)),
        Req::Layout(id) => Reply::Layout(s(client.call(perf(PerfRequest::HudLayout { id })).await)),
        Req::Preview(layout) => {
            let result = s(client.call(perf(PerfRequest::HudPreview { session: session.unwrap_or_default(), layout: layout.clone() })).await);
            Reply::Preview { sent: layout, result }
        }
        Req::Settings(scope) => Reply::Settings(s(client.call(perf(PerfRequest::HudGet { scope })).await)),
        Req::Set { scope, set } => Reply::Set(s(client.call(perf(PerfRequest::HudSet { scope, set, unset: vec![], message: Some(MESSAGE.into()) })).await)),
        Req::Save { layout, scope } => {
            let r = async {
                let info: HudLayoutInfo = client.call(perf(PerfRequest::HudLayoutSave { layout })).await?;
                let set = HudScope { layout: Some(info.id.clone()), ..Default::default() };
                let v: HudSettingsView = client.call(perf(PerfRequest::HudSet { scope, set, unset: vec![], message: Some(MESSAGE.into()) })).await?;
                Ok((info, v))
            };
            Reply::Saved(s(r.await))
        }
        Req::Log => Reply::Log(s(client.call(perf(PerfRequest::SaveLog { session, game: None, seconds: SPAN_MS / 1000 })).await)),
        Req::Toggle => Reply::Toggled(s(client.call(perf(PerfRequest::HudToggle { session, game: None })).await)),
    }
}
