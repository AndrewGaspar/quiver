//! The panel drawn in the game, quiver-sight's side (design/SIGHT.md §14), without a game:
//! the test plays the Sight layer and the input preload on a scratch `panel.input` (their
//! words: the layer can draw at 1280x800, the preload hooked X11), opens the real `PanelCtl`,
//! and checks that it
//!
//! - chooses the in-game presenter and opens the panel in `panel.input`;
//! - publishes the panel's image into `panel.image` at the swapchain's extent, and only the
//!   rows that changed afterwards;
//! - takes forwarded keys (Page Up moves to the previous section; Shift+F2 is ignored, the
//!   layer toggles; Tab docks the panel as a column with the game in full view beside it, and
//!   centres it again), and mouse motion into the cursor the layer draws;
//! - writes the hyprpad state file while open, closes `panel.input` and removes it on close;
//! - opens on the desktop presenter's path instead when the preload isn't in the game.
//!
//! GPUI runs on the main thread, hence `harness = false`. Without a GPU adapter the test says
//! so and passes. `XDG_RUNTIME_DIR` is a scratch dir: the state file never reaches the user's
//! hyprpad.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext as _};
use quiver_api::perf::LayerBackdrop;
use quiver_api::sight::{PanelAction, PanelCommand, PanelSection};
use quiver_perf::clock::mono_raw_ns;
use quiver_perf::hud::HudPoll;
use quiver_perf::panel::{EV_POINTER, INPUT_X11, PANEL_INPUT_FILE, PANEL_LAYER_CAN_DRAW, PanelEvent, open_panel_image_reader, open_panel_input};
use quiver_sight::link::Msg;
use quiver_sight::panel::{PanelCtl, state_path};
use quiver_ui::theme;

const EXTENT: (u32, u32) = (1280, 800);

fn main() {
    let scratch = tempfile::tempdir().unwrap();
    // SAFETY: before any thread starts.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", scratch.path()) };
    std::fs::create_dir_all(scratch.path().join("quiver")).unwrap();
    let session_dir = scratch.path().join("s-ingame");
    std::fs::create_dir(&session_dir).unwrap();
    let result = Rc::new(RefCell::new(Err::<(), String>("did not run".into())));
    let r = result.clone();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        // Keeps the app up while the panel reopens laid out for a stereo frame (its window
        // closes before the next one opens). Without a GPU it fails; the panel says so later.
        let _keep = cx.open_offscreen_window(gpui::OffscreenWindowOptions { size: gpui::size(gpui::px(8.), gpui::px(8.)), ..Default::default() }, |_| {}, |_, cx| cx.new(|_| Blank));
        let (tx, rx) = futures::channel::mpsc::unbounded::<Msg>();
        let ctl = Rc::new(RefCell::new(PanelCtl::new(false, None, Some(tx))));
        let rx = Rc::new(RefCell::new(rx));
        cx.spawn(async move |cx| {
            let res = run(&ctl, &rx, &session_dir, cx).await;
            cx.update(|cx| {
                *r.borrow_mut() = res;
                cx.quit();
            });
        })
        .detach();
    });
    match &*result.borrow() {
        Ok(()) => println!("ingame_panel: ok"),
        Err(e) if e.starts_with("skip:") => println!("ingame_panel: {e}"),
        Err(e) => {
            eprintln!("ingame_panel: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

struct Blank;

impl gpui::Render for Blank {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
    }
}

fn cmd(action: PanelAction) -> PanelCommand {
    PanelCommand { seq: 1, action, session: Some("s-ingame".into()), section: Some(PanelSection::Session), monitor: None, by: "test".into(), at: chrono::Utc::now() }
}

macro_rules! check {
    ($c:expr, $($t:tt)*) => {
        if !$c {
            return Err(format!($($t)*));
        }
    };
}

async fn run(ctl: &Rc<RefCell<PanelCtl>>, rx: &Rc<RefCell<futures::channel::mpsc::UnboundedReceiver<Msg>>>, dir: &std::path::Path, cx: &mut gpui::AsyncApp) -> Result<(), String> {
    let exec = cx.background_executor().clone();
    let wait = |ms: u64| exec.timer(Duration::from_millis(ms));
    // The session appears: quiver-sight makes panel.input next to hud.image.
    let backdrop = LayerBackdrop { image: dir.join("hud.image"), ready: false, blurring: false, reason: "test".into() };
    cx.update(|cx| ctl.borrow_mut().layer_backdrop("s-ingame", Some(backdrop.clone()), cx));
    let p = open_panel_input(&dir.join(PANEL_INPUT_FILE), true).map_err(|e| format!("panel.input wasn't made: {e}"))?;
    let me = std::process::id();

    // No preload in the game: not in the game (and no layer-shell here: it can't open).
    p.set_layer(me, PANEL_LAYER_CAN_DRAW, EXTENT, mono_raw_ns());
    cx.update(|cx| ctl.borrow_mut().command(cmd(PanelAction::Open), None, cx));
    check!(!cx.update(|_| ctl.borrow().in_game()), "opened in the game without the preload");
    let err = cx.update(|_| ctl.borrow().report().last_error.clone()).unwrap_or_default();
    check!(err.contains("input preload"), "the reason should name the preload: {err:?}");

    // The preload is there: in the game.
    p.set_input(me, INPUT_X11, mono_raw_ns());
    cx.update(|cx| ctl.borrow_mut().command(cmd(PanelAction::Open), None, cx));
    if !cx.update(|_| ctl.borrow().in_game()) {
        let e = cx.update(|_| ctl.borrow().report().presenter_detail.clone());
        if e.contains("adapter") || e.contains("GPU") || e.contains("wgpu") {
            return Err(format!("skip: no GPU adapter for off-screen rendering ({e})"));
        }
        return Err(format!("didn't open in the game: {e}"));
    }
    let report = cx.update(|_| ctl.borrow().report().clone());
    check!(report.presenter == "game", "{report:?}");
    let c = p.control();
    check!(c.open() && c.sight_pid == me, "panel.input isn't open: {c:?}");
    let state = std::fs::read_to_string(state_path()).map_err(|e| format!("no state file: {e}"))?;
    check!(state.starts_with(&format!("open {me} game s-ingame")), "state file: {state:?}");

    // The first image, at the extent.
    wait(300).await;
    let mut reader = open_panel_image_reader(&dir.join("panel.image")).map_err(|e| format!("no panel.image: {e}"))?;
    let mut dst = vec![0u8; reader.info().buffer_bytes as usize];
    let (first, _) = reader.poll_since(EXTENT, &mut dst, 0);
    let HudPoll::Image { published, desc } = first else { return Err(format!("no first image: {first:?}")) };
    check!((desc.width, desc.height) == EXTENT && desc.extent_w == EXTENT.0, "image {}x{}", desc.width, desc.height);
    let opaque = dst[..desc.pixel_bytes()].chunks_exact(4).filter(|px| px[3] > 0).count();
    check!(opaque > (EXTENT.0 * EXTENT.1 / 2) as usize, "the image is mostly empty ({opaque} painted pixels)");
    // The layer draws it (the test marks it, as the layer does per present).
    p.mark_drawn(c.epoch(), mono_raw_ns());

    // Keys: Shift+F2 does nothing here (the layer toggles); Down moves to the next section.
    let before = cx.update(|cx| ctl.borrow().nav(cx)).ok_or("no nav")?;
    let t = mono_raw_ns();
    for e in [PanelEvent::key(t, 42, true, 0, 0), PanelEvent::key(t, 60, true, 0, 0), PanelEvent::key(t, 60, false, 0, 0), PanelEvent::key(t, 42, false, 0, 0)] {
        p.push(&e);
    }
    let pump = |cx: &mut gpui::AsyncApp| {
        while let Ok(m) = rx.borrow_mut().try_recv() {
            match m {
                Msg::GameInput { session, events } => cx.update(|cx| ctl.borrow_mut().game_input(&session, &events, cx)),
                Msg::PanelLayout { section, docked, prefs } => cx.update(|cx| ctl.borrow_mut().layout(section, docked, prefs, cx)),
                _ => {}
            }
        }
    };
    wait(100).await;
    pump(cx);
    check!(cx.update(|_| ctl.borrow().is_open()), "Shift+F2 from the game closed the panel (the layer's chord does that)");
    // Page Up twice, through Quick to Achievements (centred like Session).
    for _ in 0..2 {
        p.push(&PanelEvent::key(t, 104, true, 0, 0));
        p.push(&PanelEvent::key(t, 104, false, 0, 0));
    }
    wait(100).await;
    pump(cx);
    let after = cx.update(|cx| ctl.borrow().nav(cx)).ok_or("no nav")?;
    check!(after.section != before.section, "Page Up didn't move: {:?} → {:?}", before.section, after.section);
    // The next image says only some rows changed since the last.
    wait(300).await;
    let (next, rows) = reader.poll_since(EXTENT, &mut dst, published);
    check!(matches!(next, HudPoll::Image { .. }), "no image after the key: {next:?}");
    let (y0, y1) = rows.ok_or("the image after a key wasn't partial")?;
    check!(y1 - y0 < EXTENT.1, "every row changed ({y0}..{y1})");
    eprintln!("after Page Up: rows {y0}..{y1} of {}", EXTENT.1);

    // Tab (R3 through hyprpad): docked. A column on the left; the rest of the image is clear,
    // so the game shows unobstructed (no dim, and no blur request).
    let alpha = |dst: &[u8], stride: u32, x: u32, y: u32| dst[(y * stride + x * 4) as usize + 3];
    p.push(&PanelEvent::key(t, 15, true, 0, 0));
    p.push(&PanelEvent::key(t, 15, false, 0, 0));
    wait(100).await;
    pump(cx);
    wait(300).await;
    pump(cx);
    let (img, _) = reader.poll_since(EXTENT, &mut dst, 0);
    let HudPoll::Image { desc, .. } = img else { return Err(format!("no image after Tab: {img:?}")) };
    let report = cx.update(|_| ctl.borrow().report().clone());
    check!(report.docked && report.backdrop_detail.starts_with("docked"), "the report should say docked: {report:?}");
    check!(alpha(&dst, desc.stride, 100, 400) > 200, "the dock should be drawn on the left");
    check!(alpha(&dst, desc.stride, 1000, 400) == 0 && alpha(&dst, desc.stride, 1270, 790) == 0, "beside the dock the game should show (no dim)");
    // Tab again: centred, the dim back over the whole frame.
    p.push(&PanelEvent::key(t, 15, true, 0, 0));
    p.push(&PanelEvent::key(t, 15, false, 0, 0));
    wait(100).await;
    pump(cx);
    wait(300).await;
    pump(cx);
    let (img, _) = reader.poll_since(EXTENT, &mut dst, 0);
    let HudPoll::Image { desc, published } = img else { return Err(format!("no image after the second Tab: {img:?}")) };
    check!(!cx.update(|_| ctl.borrow().report().docked), "still docked after the second Tab");
    check!(alpha(&dst, desc.stride, 1000, 400) > 0, "centred: the scrim covers the game again");
    let _ = published;

    // The mouse: the cursor follows the pointer, in swapchain px, shown once the mouse moves.
    check!(p.cursor().is_none(), "a cursor before the mouse moved");
    p.push(&PanelEvent { t, kind: EV_POINTER, a: 300, b: 200, ..Default::default() });
    wait(100).await;
    pump(cx);
    let cur = p.cursor().ok_or("no cursor after motion")?;
    check!(cur == ((300, 200), EXTENT), "cursor {cur:?}");

    // A full-SBS frame (design/STEREO-UI.md): laid out for one eye, with the card and the
    // focused section as lifts at their rungs, back to front; the scrim at the game's plane.
    use quiver_perf::stereo::rung;
    // (The panel reopens at once, on the swapchain it was open on: 1280x800, so 640x800 eyes.)
    cx.update(|cx| ctl.borrow_mut().set_stereo("s-ingame", Some(quiver_perf::stereo::Stereo::Sbs), cx));
    check!(cx.update(|_| ctl.borrow().in_game()), "the stereo panel didn't open in the game");
    wait(300).await;
    let mut reader = open_panel_image_reader(&dir.join("panel.image")).map_err(|e| format!("no panel.image: {e}"))?;
    let mut dst = vec![0u8; reader.info().buffer_bytes as usize];
    let (img, _) = reader.poll_since(EXTENT, &mut dst, 0);
    let HudPoll::Image { desc, published } = img else { return Err(format!("no stereo image: {img:?}")) };
    check!(desc.lifts().len() == 1 && desc.lifts()[0].depth == rung::PANEL, "the card alone (nothing focused in an empty section): {:?}", desc.lifts());
    // Page Down twice, through Quick and back to Session, its Quit button focused.
    for _ in 0..2 {
        p.push(&PanelEvent::key(t, 109, true, 0, 0));
        p.push(&PanelEvent::key(t, 109, false, 0, 0));
    }
    wait(100).await;
    pump(cx);
    wait(300).await;
    let (img, _) = reader.poll_since(EXTENT, &mut dst, published);
    let HudPoll::Image { desc, published } = img else { return Err(format!("no image after Page Down: {img:?}")) };
    check!((desc.width, desc.height, desc.stereo) == (EXTENT.0 / 2, EXTENT.1, quiver_perf::stereo::STEREO_SBS), "stereo image {}x{} layout {}", desc.width, desc.height, desc.stereo);
    let lifts = desc.lifts().to_vec();
    eprintln!("stereo panel lifts: {lifts:?}");
    check!(desc.depth == 0, "the scrim should stay at the game's plane: depth {}", desc.depth);
    check!(lifts.len() == 2 && lifts[0].depth == rung::PANEL && lifts[1].depth == rung::FOCUS, "lifts {lifts:?}");
    let (card, focus) = (lifts[0], lifts[1]);
    check!(card.w as u32 > EXTENT.0 / 4 && card.h as u32 > EXTENT.1 / 2, "the card is the panel's bulk: {card:?}");
    check!(focus.x >= card.x && focus.y >= card.y && focus.x + focus.w <= card.x + card.w && focus.h < 100, "the focused button is a small part of the card: {focus:?}");

    // Docked in stereo: each eye gets the column, and the lifts follow it (the card is the
    // column now, the focused button inside it).
    p.push(&PanelEvent::key(t, 15, true, 0, 0));
    p.push(&PanelEvent::key(t, 15, false, 0, 0));
    wait(100).await;
    pump(cx);
    wait(300).await;
    pump(cx);
    let (img, _) = reader.poll_since(EXTENT, &mut dst, published);
    let HudPoll::Image { desc, .. } = img else { return Err(format!("no image after Tab in stereo: {img:?}")) };
    let lifts = desc.lifts().to_vec();
    eprintln!("stereo docked lifts: {lifts:?}");
    if let Ok(p) = std::env::var("QUIVER_TEST_DUMP") {
        let img = image::RgbaImage::from_fn(desc.width, desc.height, |x, y| {
            let o = (y * desc.stride + x * 4) as usize;
            image::Rgba([dst[o], dst[o + 1], dst[o + 2], dst[o + 3]])
        });
        let _ = img.save(p);
    }
    check!(desc.width == EXTENT.0 / 2 && lifts.len() == 2 && lifts[0].depth == rung::PANEL && lifts[1].depth == rung::FOCUS, "lifts {lifts:?}");
    let (dock, focus) = (lifts[0], lifts[1]);
    check!(dock.x < 30 && dock.w as u32 <= 500 + 2 && dock.h as u32 > EXTENT.1 / 2, "the column, laid out in the eye: {dock:?}");
    check!(focus.x >= dock.x && focus.x + focus.w <= dock.x + dock.w, "the focused button inside the column: {focus:?}");

    // Close: panel.input closed, the state file gone.
    cx.update(|cx| ctl.borrow_mut().command(cmd(PanelAction::Close), None, cx));
    check!(!p.control().open(), "panel.input still open");
    check!(!state_path().exists(), "the state file is still there");
    // Summoning from the game always lands on Quick, even after closing from Session.
    // A live game keeps both producer heartbeats fresh between opens.
    p.set_layer(me, PANEL_LAYER_CAN_DRAW, EXTENT, mono_raw_ns());
    p.set_input(me, INPUT_X11, mono_raw_ns());
    let toggle = PanelCommand { section: None, ..cmd(PanelAction::Toggle) };
    cx.update(|cx| ctl.borrow_mut().command(toggle.clone(), None, cx));
    let report = cx.update(|_| ctl.borrow().report().clone());
    check!(report.open && report.section == PanelSection::Quick, "toggle didn't open Quick: {report:?}");
    check!(!report.docked && report.backdrop_detail.contains("full view"), "compact Quick backdrop: {report:?}");
    cx.update(|cx| ctl.borrow_mut().command(cmd(PanelAction::Open), None, cx));
    cx.update(|cx| ctl.borrow_mut().command(cmd(PanelAction::Quick), None, cx));
    check!(cx.update(|cx| ctl.borrow().nav(cx).unwrap().section) == PanelSection::Quick, "quick command didn't select the tab");
    cx.update(|cx| ctl.borrow_mut().command(toggle, None, cx));
    check!(!p.control().open(), "second toggle didn't return control to the game");
    Ok(())
}
