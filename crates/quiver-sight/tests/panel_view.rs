//! The panel view's Mods section driven as the pad drives it, without quiverd or a GPU: the
//! requests it sends (`LinkReq`) and what it does with quiverd's answers. Checks
//!
//! - Y held on a live option: an A/B compare (`live: only` to the value to compare with, then
//!   back on release, nothing saved), its chip; Y tapped: a pin, no change;
//! - X: the focused option back to its default;
//! - an outcome: the session's log and the change chip; Undo for a next-launch change;
//! - Revert this session (`OptionsRevertSession`) and Session only (`live: no`, then `live:
//!   only`);
//! - LT / RT: the pages; Tab (R3): docked ↔ centred, per section;
//! - the hint bar: compare, reset, the default and range.
//!
//! GPUI runs on the main thread (timers for Y's hold), hence `harness = false`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext as _, Entity, KeyUpEvent, Keystroke};
use quiver_api::options::{LiveChannel, Live, OpOutcome, OptionOp, OptionsChange, Outcome};
use quiver_api::sight::{Backdrop, PanelSection};
use quiver_sight::demo::sample_panel;
use quiver_sight::link::{Change, LinkReq, Msg, Why};
use quiver_sight::quick::{TileKind, tiles};
use quiver_sight::replay::State as ReplayState;
use quiver_sight::panel::hints::Glyph;
use quiver_sight::panel::state::{Act, Key, Nav, Row};
use quiver_sight::panel::view::{PanelView, Wiring};
use quiver_ui::input::Input;
use quiver_ui::theme;
use serde_json::json;

const DLSS: &str = "m1/aco-dlss";

fn main() {
    let result = Rc::new(RefCell::new(Err::<(), String>("did not run".into())));
    let r = result.clone();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        cx.spawn(async move |cx| {
            let res = run(cx).await;
            cx.update(|cx| {
                *r.borrow_mut() = res;
                cx.quit();
            });
        })
        .detach();
    });
    match &*result.borrow() {
        Ok(()) => println!("panel_view: ok"),
        Err(e) => {
            eprintln!("panel_view: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

macro_rules! check {
    ($c:expr, $($t:tt)*) => {
        if !$c {
            return Err(format!($($t)*));
        }
    };
}

/// The requests sent so far, in words.
fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<LinkReq>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(r) = rx.try_recv() {
        out.push(match r {
            LinkReq::OptionOps { tweaks, ops, live, why, .. } => format!("ops tweaks={tweaks} live={live:?} why={why:?} {}", ops_words(&ops)),
            LinkReq::SetOption { op, live, .. } => format!("set live={live:?} {}", ops_words(&[op])),
            LinkReq::RevertSession { .. } => "revert session".into(),
            LinkReq::SessionOnly { back, live, .. } => format!("session only back=[{}] live=[{}]", ops_words(&back), ops_words(&live)),
            LinkReq::Load { .. } => "load".into(),
            LinkReq::Subscribe { extra, .. } => format!("subscribe +{}", extra.len()),
            LinkReq::Pin { scope, pin, on } => format!("pin {scope} {} on={on}", pin.id),
            _ => "other".into(),
        });
    }
    out.retain(|s| s != "load");
    out
}

fn ops_words(ops: &[OptionOp]) -> String {
    ops.iter()
        .map(|o| match o {
            OptionOp::Set { id, value, .. } => format!("{id}={value}"),
            OptionOp::Reset { id, .. } => format!("reset {id}"),
            OptionOp::Preset { id, .. } => format!("preset {id}"),
            _ => "?".into(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn change(id: &str, before: serde_json::Value, after: serde_json::Value, outcome: Outcome) -> OptionsChange {
    OptionsChange { revision: 9, outcomes: vec![OpOutcome { surface: DLSS.into(), id: id.into(), outcome, before: Some(before), after: Some(after) }], ..Default::default() }
}

async fn run(cx: &mut gpui::AsyncApp) -> Result<(), String> {
    let exec = cx.background_executor().clone();
    let wait = |ms: u64| exec.timer(Duration::from_millis(ms));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LinkReq>();
    let (ui, mut actions) = futures::channel::mpsc::unbounded();
    let view: Entity<PanelView> = cx.update(|cx| {
        cx.new(|cx| {
            let mut nav = Nav::default();
            nav.open(Some(PanelSection::Mods));
            // DLSS's World page, Crowd density focused (2.0×, its default 1.0×).
            nav.pages.set_page(DLSS, 3);
            nav.set_focus_index(1);
            PanelView::new("s-demo".into(), sample_panel(), nav, Backdrop::GameBlur, Wiring { link: Some(tx), ui: Some(ui), ..Default::default() }, cx)
        })
    });
    let upd = |cx: &mut gpui::AsyncApp, f: &dyn Fn(&mut PanelView, &mut gpui::Context<PanelView>)| cx.update(|cx| view.update(cx, |v, cx| f(v, cx)));
    let y_up = KeyUpEvent { keystroke: Keystroke { key: "y".into(), ..Default::default() } };

    // The hints for the focused live option.
    let hints = cx.update(|cx| view.read(cx).hints());
    check!(cx.update(|cx| view.read(cx).docked()), "Mods opens docked");
    check!(hints.label_of(Glyph::Y) == Some("Pin · hold: compare with 1.0 ×"), "Y's hint: {:?}", hints.label_of(Glyph::Y));
    check!(hints.label_of(Glyph::X) == Some("Reset") && hints.label_of(Glyph::R3) == Some("Centre"), "{hints:?}");
    check!(hints.detail.as_deref() == Some("Default 1.0 × · range 1.0–5.0 ×"), "detail {:?}", hints.detail);

    // Y held: compare with the default, live for this session only; let go: back.
    upd(cx, &|v, cx| v.key(Key::Y, false, cx));
    wait(150).await;
    upd(cx, &|v, cx| v.key(Key::Y, true, cx)); // key repeat while held: nothing new
    wait(350).await;
    let sent = drain(&mut rx);
    check!(sent == ["ops tweaks=false live=Only why=Compare world.npc_density=1.0"], "after holding Y: {sent:?}");
    check!(cx.update(|cx| view.read(cx).compare.is_some() && view.read(cx).chip.as_ref().is_some_and(|c| c.comparing)), "comparing, with its chip");
    upd(cx, &|v, cx| v.key_up(&y_up, cx));
    wait(150).await;
    let sent = drain(&mut rx);
    check!(sent == ["ops tweaks=false live=Only why=Compare world.npc_density=2.0"], "after letting go: {sent:?}");
    check!(cx.update(|cx| view.read(cx).compare.is_none()), "still comparing");

    // Y tapped: pinned to the quick menu (design/SIGHT.md §16), no value changed; again: unpinned.
    upd(cx, &|v, cx| v.key(Key::Y, false, cx));
    wait(50).await;
    upd(cx, &|v, cx| v.key_up(&y_up, cx));
    wait(150).await;
    let sent = drain(&mut rx);
    check!(sent.len() == 1 && sent[0].starts_with("pin ") && sent[0].ends_with("world.npc_density on=true"), "a tap: {sent:?}");
    check!(cx.update(|cx| view.read(cx).hints().label_of(Glyph::Y).is_some_and(|h| h.starts_with("Unpin"))), "Y should show the saved pin");
    upd(cx, &|v, cx| v.key(Key::Y, false, cx));
    wait(50).await;
    upd(cx, &|v, cx| v.key_up(&y_up, cx));
    wait(150).await;
    let sent = drain(&mut rx);
    check!(sent.len() == 1 && sent[0].ends_with("world.npc_density on=false"), "a second tap unpins: {sent:?}");
    check!(cx.update(|cx| view.read(cx).compare.is_none()), "a tap compared");

    // X: back to the default.
    upd(cx, &|v, cx| v.input(Input::PlayOptions, cx));
    let sent = drain(&mut rx);
    check!(sent == ["ops tweaks=false live=IfRunning why=Reset reset world.npc_density"], "X: {sent:?}");

    // quiverd's answers: the log and the chip; Undo for the next-launch one.
    upd(cx, &|v, cx| v.done(Change::Option(DLSS.into(), "world.npc_density".into()), Ok(Some(change("world.npc_density", json!(1.0), json!(2.0), Outcome::AppliedLive { channel: LiveChannel::Sdk, latency: None }))), cx));
    upd(cx, &|v, cx| v.done(Change::Option(DLSS.into(), "upscale.scale".into()), Ok(Some(change("upscale.scale", json!(0.5), json!(0.58), Outcome::SavedRestart { running: true }))), cx));
    let (n, chip) = cx.update(|cx| (view.read(cx).log.count(false), view.read(cx).chip.clone()));
    check!(n == 2, "{n} changes logged");
    let chip = chip.ok_or("no chip")?;
    check!(chip.label == "Render scale" && chip.before == "0.50" && chip.after == "0.58" && chip.tail(chip.at) == "· next launch", "chip {chip:?}");
    let rows = cx.update(|cx| view.read(cx).rows());
    let undo = rows.iter().position(|r| *r == Row::Act(Act::UndoRestart { tweaks: false })).ok_or("no Undo")?;
    upd(cx, &move |v, cx| {
        let rows = v.rows();
        v.nav.focus_row(&rows, undo);
        v.input(Input::Activate, cx);
    });
    let sent = drain(&mut rx);
    check!(sent == ["ops tweaks=false live=IfRunning why=Undo upscale.scale=0.5"], "Undo: {sent:?}");

    // Session only: out of the profile (live: no), then for this session (live: only).
    let so = rows.iter().position(|r| *r == Row::Act(Act::SessionOnly)).ok_or("no Session only")?;
    upd(cx, &move |v, cx| {
        let rows = v.rows();
        v.nav.focus_row(&rows, so);
        v.input(Input::Activate, cx);
    });
    let sent = drain(&mut rx);
    check!(sent == ["session only back=[world.npc_density=1.0,upscale.scale=0.5] live=[world.npc_density=2.0,upscale.scale=0.58]"], "Session only: {sent:?}");
    upd(cx, &|v, cx| v.done(Change::Ops { tweaks: false, why: Why::SessionOnly }, Ok(Some(OptionsChange::default())), cx));
    check!(cx.update(|cx| view.read(cx).log.session_only), "Session only isn't on");
    // Now a change goes out live: only.
    upd(cx, &|v, cx| {
        let rows = v.rows();
        let i = rows.iter().position(|r| matches!(r, Row::Opt { id, .. } if id == "world.spawn")).unwrap();
        v.nav.focus_row(&rows, i);
        v.input(Input::Right, cx);
    });
    wait(500).await;
    let sent = drain(&mut rx);
    check!(sent == ["set live=Only world.spawn=1.1"], "a change in Session only: {sent:?}");

    // Revert this session.
    let rv = cx.update(|cx| view.read(cx).rows()).iter().position(|r| *r == Row::Act(Act::RevertSession)).ok_or("no Revert")?;
    upd(cx, &move |v, cx| {
        let rows = v.rows();
        v.nav.focus_row(&rows, rv);
        v.input(Input::Activate, cx);
    });
    check!(drain(&mut rx) == ["revert session"], "Revert");
    upd(cx, &|v, cx| v.done(Change::Ops { tweaks: false, why: Why::Revert }, Ok(Some(OptionsChange::default())), cx));
    check!(cx.update(|cx| view.read(cx).log.count(false) == 0 && !view.read(cx).log.session_only), "the log after Revert");

    // LT / RT: pages (wrapping); Tab: centred, remembered for Mods only.
    upd(cx, &|v, cx| v.input(Input::NextTab, cx));
    check!(cx.update(|cx| view.read(cx).nav.pages.page(DLSS, 4)) == 0, "RT from World wraps to Upscaling");
    upd(cx, &|v, cx| v.key(Key::Dock, false, cx));
    check!(!cx.update(|cx| view.read(cx).docked()), "Tab didn't centre");
    upd(cx, &|v, cx| v.input(Input::NextSection, cx));
    check!(cx.update(|cx| view.read(cx).docked()), "Tweaks docks on its own");
    upd(cx, &|v, cx| v.input(Input::PrevSection, cx));
    check!(!cx.update(|cx| view.read(cx).docked()), "Mods stays centred");
    upd(cx, &|v, cx| v.input(Input::PrevSection, cx));
    check!(cx.update(|cx| view.read(cx).nav.section == PanelSection::Session), "LB should switch sections");
    upd(cx, &|v, cx| v.input(Input::NextTab, cx));
    check!(cx.update(|cx| view.read(cx).nav.section == PanelSection::Session), "RT must stay in a section without pages");
    upd(cx, &|v, cx| {
        v.nav.open(None);
        v.quick_tiles = tiles(&v.data, &[], &[], ReplayState::Off);
        v.key(Key::Input(Input::Up), false, cx);
        v.key(Key::Input(Input::Activate), false, cx);
        v.key(Key::Input(Input::Activate), true, cx);
        v.key_up(&KeyUpEvent { keystroke: Keystroke { key: "enter".into(), ..Default::default() } }, cx);
    });
    let mut received = Vec::new();
    while let Ok(m) = actions.try_recv() {
        if let Msg::QuickAction { tile, .. } = m { received.push(tile.kind); }
    }
    check!(received == [TileKind::StartReplay { seconds: 120 }], "one explicit activation starts the selected buffer length: {received:?}");
    upd(cx, &|v, cx| {
        v.quick_tiles = tiles(&v.data, &[], &[], ReplayState::Active(120));
        v.input(Input::Right, cx);
        v.input(Input::Left, cx);
        v.input(Input::NextSection, cx);
        v.nav.open(None);
        v.input(Input::Activate, cx);
    });
    check!(matches!(actions.try_recv(), Ok(Msg::QuickAction { tile, .. }) if tile.kind == TileKind::SaveReplay), "reopening on replay and pressing A saves");
    upd(cx, &|v, cx| {
        for _ in 0..v.quick_tiles.len() { v.input(Input::Right, cx); }
        v.input(Input::Activate, cx);
    });
    check!(cx.update(|cx| view.read(cx).nav.confirm.as_ref().is_some_and(|c| !c.yes)), "Quit opens on Cancel");
    upd(cx, &|v, cx| v.input(Input::Back, cx));
    check!(cx.update(|cx| view.read(cx).nav.confirm.is_none()), "B dismisses confirmation");
    check!(actions.try_recv().is_err(), "cancel or release ran an action");
    let _ = Live::Only;
    Ok(())
}
