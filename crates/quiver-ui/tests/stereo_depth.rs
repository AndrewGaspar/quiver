//! Stereo depth through real elements (design/STEREO-UI.md §4): an off-screen window packed
//! side by side, a page built from quiver-ui's components, and the depth GPUI resolved for
//! each element read back from the frame. Checks that
//!
//! - a card is a surface a step above the page, and a button pops from whatever it's on;
//! - focus (the app's ring) lifts two steps more, hover one, and only on controls; hover is
//!   noticed (the window redraws) for a control without a hover style of its own;
//! - a clickable wrapper and the pill with the ring inside it are one control;
//! - an overlay sits at its own depth whatever it covers, its controls pop from it, and the
//!   ceiling holds;
//! - a deferred element (a menu) is drawn at the depth of the place it was deferred from,
//!   also when deferred from inside another deferred element;
//! - the packed shifts are whole pixels per eye.
//!
//! GPUI runs on the main thread, hence `harness = false`. Without a GPU adapter the test says
//! so and passes.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, AppContext as _, Bounds, Context, IntoElement, ParentElement, Render, ScaledPixels, InteractiveElement as _, Styled, Window, div, px};
use quiver_ui::components::{Kind, Size, button, card, FocusExt as _};
use quiver_ui::depth::{self, DepthExt as _};
use quiver_ui::theme::Theme;

fn main() {
    let result = Rc::new(RefCell::new(Err::<(), String>("did not run".into())));
    let r = result.clone();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, Theme::builtin());
        cx.spawn(async move |cx| {
            let res = cx.update(run);
            cx.update(|cx| {
                *r.borrow_mut() = res;
                cx.quit();
            });
        })
        .detach();
    });
    match &*result.borrow() {
        Ok(()) => println!("stereo_depth: ok"),
        Err(e) if e.starts_with("skip:") => println!("stereo_depth: {e}"),
        Err(e) => {
            eprintln!("stereo_depth: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

struct Page;

/// Absolutely placed, so each element's bounds (and so its depth scope) are known.
fn at<E: Styled>(e: E, x: f32, y: f32, w: f32, h: f32) -> E {
    e.absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h))
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.global::<Theme>().clone();
        div()
            .size_full()
            .relative()
            .bg(th.ink0)
            // A button on the page.
            .child(at(button("page-btn", &th, Kind::Secondary, Size::Medium, None, "Page"), 20., 20., 100., 32.))
            // A tile-like control with no hover style: GPUI wouldn't redraw on hover by itself.
            .child(at(div().id("bare").cursor_pointer().bg(th.ink3), 140., 20., 100., 32.))
            // A clickable wrapper around a pill with the app's focus ring (the top bar's tabs).
            .child(at(div().id("wrap").cursor_pointer(), 260., 20., 120., 32.).child(at(div().bg(th.ink3).focus_ring(true, &th, 6.), 0., 0., 110., 30.)))
            // A card, with a button, a focused button and a menu deferred from inside it.
            .child(
                at(card(&th), 20., 80., 300., 200.)
                    .child(at(button("card-btn", &th, Kind::Secondary, Size::Medium, None, "On the card"), 10., 10., 120., 32.))
                    .child(at(button("card-focus", &th, Kind::Primary, Size::Medium, None, "Focused"), 10., 60., 120., 32.).focus_ring(true, &th, 8.))
                    .child(gpui::deferred(
                        at(div().bg(th.ink4).depth_surface(), 150., 10., 60., 60.).child(gpui::deferred(at(div().bg(th.ink5).depth_surface(), 5., 5., 20., 20.))),
                    )),
            )
            // An overlay over the card's corner, its controls popping from it.
            .child(
                at(div().bg(th.ink2).depth_overlay(), 250., 200., 300., 150.)
                    .child(at(button("ov-btn", &th, Kind::Secondary, Size::Medium, None, "Cancel"), 10., 10., 100., 32.))
                    .child(at(button("ov-focus", &th, Kind::Primary, Size::Medium, None, "OK"), 120., 10., 100., 32.).focus_ring(true, &th, 8.))
                    // A clickable scrim-like area that isn't a control.
                    .child(at(div().id("ov-flat").cursor_pointer().bg(th.ink3).depth_flat(), 10., 60., 100., 32.)),
            )
    }
}

fn run(cx: &mut App) -> Result<(), String> {
    let opts = gpui::OffscreenWindowOptions { size: gpui::size(px(800.), px(400.)), scale_factor: 1.0, ..Default::default() };
    let handle = match cx.open_offscreen_window(opts, |_| {}, |_, cx| cx.new(|_| Page)) {
        Ok(h) => h,
        Err(e) => return Err(format!("skip: no off-screen window (no GPU adapter?): {e:#}")),
    };
    let any: gpui::AnyWindowHandle = handle.into();
    let pane = 1920.0;
    let scopes = |cx: &mut App, refresh: bool| {
        cx.update_window(any, |_, win, cx| {
            if refresh {
                win.refresh();
            }
            win.render_now(cx);
            (win.stereo_scopes(), win.stereo_scope_shifts(), win.stereo_stats().packed)
        })
        .map_err(|e| e.to_string())
    };
    cx.update_window(any, |_, win, _| {
        win.set_stereo_policy(depth::policy());
        win.set_stereo_packing(Some(depth::packing(pane, 100)))
    })
    .map_err(|e| e.to_string())?
    .then_some(())
    .ok_or("the off-screen window can't pack")?;
    let (s, shifts, _) = scopes(cx, true)?;

    let find = |s: &[gpui::StereoScope], x: f32, y: f32, w: f32, h: f32| -> Result<(usize, f32), String> {
        // Children of a bordered card are placed inside its 1 px border.
        let close = |b: Bounds<ScaledPixels>| {
            (b.origin.x.0 - x).abs() <= 1. && (b.origin.y.0 - y).abs() <= 1. && b.size.width.0 == w && b.size.height.0 == h
        };
        s.iter()
            .enumerate()
            .rev()
            .find(|(_, sc)| sc.bounds.is_some_and(close))
            .map(|(i, sc)| (i, sc.depth))
            .ok_or_else(|| {
                let all: Vec<String> = s.iter().filter_map(|sc| sc.bounds.map(|b| format!("{}@{},{} {}x{}", sc.depth, b.origin.x.0, b.origin.y.0, b.size.width.0, b.size.height.0))).collect();
                format!("no depth scope at {x},{y} {w}x{h}; scopes: {}", all.join("; "))
            })
    };
    let near = |what: &str, got: f32, want: f32| -> Result<(), String> {
        if (got - want).abs() < 1e-4 { Ok(()) } else { Err(format!("{what}: depth {got}, want {want}")) }
    };
    let px_of = |d: f32| {
        let [l, r] = depth::eye_shifts(d, pane, 100);
        l - r
    };
    use depth::{CONTROL, FOCUS, HOVER, MAX, OVERLAY, SURFACE};

    near("page button", find(&s, 20., 20., 100., 32.)?.1, CONTROL)?;
    near("bare control", find(&s, 140., 20., 100., 32.)?.1, CONTROL)?;
    // The wrapper is the control; the pill inside adds only its focus.
    near("wrapper", find(&s, 260., 20., 120., 32.)?.1, CONTROL)?;
    near("focused pill in the wrapper", find(&s, 260., 20., 110., 30.)?.1, CONTROL + FOCUS)?;
    let (card_ix, card_d) = find(&s, 20., 80., 300., 200.)?;
    near("card", card_d, SURFACE)?;
    near("button on the card", find(&s, 30., 90., 120., 32.)?.1, SURFACE + CONTROL)?;
    near("focused button on the card", find(&s, 30., 140., 120., 32.)?.1, SURFACE + CONTROL + FOCUS)?;
    near("menu deferred from the card", find(&s, 170., 90., 60., 60.)?.1, SURFACE + SURFACE)?;
    near("menu deferred from that menu", find(&s, 175., 95., 20., 20.)?.1, 3. * SURFACE)?;
    near("overlay", find(&s, 250., 200., 300., 150.)?.1, OVERLAY)?;
    near("button on the overlay", find(&s, 260., 210., 100., 32.)?.1, OVERLAY + CONTROL)?;
    near("focused button on the overlay (the ceiling)", find(&s, 370., 210., 100., 32.)?.1, MAX)?;
    if s.iter().any(|sc| sc.bounds.is_some_and(|b| b.origin.x.0 == 260. && b.origin.y.0 == 260.)) {
        return Err("the flat area on the overlay got a depth of its own".into());
    }
    // In pixels: page 0, card 1, its button 2, focused 4; overlay 4, its button 5, focused 7.
    let ladder = [0.0, card_d, SURFACE + CONTROL, SURFACE + CONTROL + FOCUS, OVERLAY, OVERLAY + CONTROL, MAX].map(px_of);
    if ladder != [0, 1, 2, 4, 4, 5, 7] {
        return Err(format!("ladder in px: {ladder:?}"));
    }
    // The card as packed: whole pixels, left eye +1, right eye 0.
    if shifts.get(card_ix) != Some(&[1.0, 0.0]) {
        return Err(format!("card shifts {:?}", shifts.get(card_ix)));
    }

    // Hover: one step more, only while the pointer is there, and the window notices by itself
    // even for the control without a hover style.
    for (what, x, y, rest) in [("page button", 20., 20., CONTROL), ("bare control", 140., 20., CONTROL)] {
        let (w, h) = (100., 32.);
        move_to(cx, any, x + 10., y + 10.)?;
        let (s, _, _) = scopes(cx, false)?;
        near(&format!("hovered {what}"), find(&s, x, y, w, h)?.1, rest + HOVER)?;
        move_to(cx, any, 790., 390.)?;
        let (s, _, _) = scopes(cx, false)?;
        near(&format!("{what} after the pointer left"), find(&s, x, y, w, h)?.1, rest)?;
    }
    // A hovered focused control doesn't rise twice.
    move_to(cx, any, 40., 150.)?;
    let (s, _, _) = scopes(cx, false)?;
    near("hovered focused button", find(&s, 30., 140., 120., 32.)?.1, SURFACE + CONTROL + FOCUS)?;
    near("hovered card", find(&s, 20., 80., 300., 200.)?.1, SURFACE)?;

    // Off: nothing resolved, nothing packed.
    cx.update_window(any, |_, win, _| win.set_stereo_packing(None)).map_err(|e| e.to_string())?;
    let (s, _, packed) = scopes(cx, true)?;
    let (_, _, packed_after) = scopes(cx, true)?;
    if s.iter().any(|sc| sc.bounds.is_some()) || packed != packed_after {
        return Err("mono frames still carry depths or pack".into());
    }
    Ok(())
}

fn move_to(cx: &mut App, any: gpui::AnyWindowHandle, x: f32, y: f32) -> Result<(), String> {
    cx.update_window(any, |_, win, cx| {
        let ev = gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: gpui::point(px(x), px(y)), pressed_button: None, modifiers: Default::default() });
        win.dispatch_event(ev, cx);
    })
    .map_err(|e| e.to_string())
}
