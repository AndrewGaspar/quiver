//! Drawing the Performance section (SightPerformance) and its layout editor (HudEditor) into
//! the panel's body. Everything focusable carries the panel's focus ring and depth mark.

use gpui::{AnyElement, Bounds, Context, Div, Entity, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, SharedString, Styled, canvas, div, point, prelude::*, px, size};
use quiver_api::perf::{Captures, HudModel, HudRow, Presenter, PresenterSetting, Stat};
use quiver_ui::assets::MONO_FONT;
use quiver_ui::components::{FocusExt, MEDIUM, SEMIBOLD, caption, mono};
use quiver_ui::options::Tone;
use quiver_ui::theme::Theme;

use super::{Applies, CAP_TWEAK, Ctl, SETTINGS, Setting, Zone, anchor_cell, cap_choices, cap_enforcer, cap_label, entries, groups, hud_choices, layout_name, metric, reflex_words, shows};
use crate::hud_view::{HudView, Mode, source_mark};
use crate::panel::state::{ANCHORS, PRESETS, PanelData, Zone as NavZone};
use crate::panel::view::{MARK_PAD, Mark, PanelView, mark, tone_color};

/// The body's size for the section (logical px), from the panel's layout.
#[derive(Debug, Clone, Copy)]
pub struct Room {
    pub w: f32,
    pub h: f32,
}

/// A card as in the mock-ups (`.card`): the panel's raised surface, a hairline, radius 14.
fn card(th: &Theme) -> Div {
    div().flex().flex_col().p(px(16.)).rounded(px(14.)).bg(th.ink2).border_1().border_color(th.line1)
}

/// A card's heading ("SHOWN · layer"): the caption, then where the number comes from.
fn heading(th: &Theme, title: &str, src: Option<&str>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(caption(title, th.tx3).text_size(px(12.)))
        .when_some(src, |d, s| d.child(div().text_size(px(11.)).text_color(th.tx3).font_weight(MEDIUM).child(format!("· {s}"))))
}

/// One segmented control (`.seg`): its options, the chosen one in the accent.
fn seg(th: &Theme, items: Vec<(String, bool)>, compact: bool) -> Div {
    div().flex().gap(px(3.)).p(px(3.)).rounded(px(9.)).bg(th.tx1.opacity(0.05)).children(items.into_iter().map(move |(label, on)| {
        div()
            .flex_1()
            .min_w(px(if compact { 34. } else { 30. }))
            .h(px(if compact { 26. } else { 32. }))
            .px(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .text_size(px(if compact { 12. } else { 13. }))
            .font_weight(SEMIBOLD)
            .whitespace_nowrap()
            .bg(if on { th.acc } else { th.ink2.opacity(0.0) })
            .text_color(if on { th.acc_ink } else { th.tx2 })
            .child(label)
    }))
}

/// Whether control `c` has the panel's focus.
fn has_focus(v: &PanelView, c: Ctl) -> bool {
    v.nav.zone == NavZone::Body && v.nav.confirm.is_none() && v.perf.editor.is_none() && v.perf.focused(&v.data) == Some(c)
}

/// A focus target: the ring and the depth mark.
fn target<E: ParentElement + Styled + Sized>(v: &PanelView, th: &Theme, d: E, on: bool, radius: f32) -> E {
    let d = d.focus_ring(on, th, radius);
    if on { d.child(mark(&v.marks, Mark::Focus, MARK_PAD)) } else { d }
}

fn text_of(d: &PanelData, id: &str, stat: Stat) -> Option<String> {
    metric(d, id, stat).filter(|v| !v.stale && v.text != "—").map(|v| v.text.clone())
}

/// The section: the tiles, the graph, the cap, the HUD and latency cards; or the editor.
pub fn render(v: &PanelView, th: &Theme, room: Room, cx: &mut Context<PanelView>) -> AnyElement {
    if v.perf.editor.is_some() {
        return editor(v, th, room, cx);
    }
    let d = &v.data;
    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(tiles(th, d, v.perf.data.frames.as_ref().and_then(|f| f.hitches_per_min)))
        .child(graph_card(v, th, (room.h * 0.34).clamp(150., 300.)))
        .child(div().flex().flex_wrap().gap(px(14.)).child(cap_card(v, th, cx)).child(hud_card(v, th, cx)).child(latency_card(v, th, cx)))
        .into_any_element()
}

fn tile(th: &Theme, title: &str, src: &str, value: Option<String>, unit: &str, sub: String) -> Div {
    card(th)
        .flex_1()
        .min_w(px(150.))
        .gap(px(4.))
        .child(heading(th, title, Some(src)))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(5.))
                .child(mono(30., if value.is_some() { th.tx1 } else { th.tx4 }).font_weight(SEMIBOLD).child(value.unwrap_or_else(|| "—".into())))
                .child(div().text_size(px(14.)).font_weight(MEDIUM).text_color(th.tx2).child(unit.to_owned())),
        )
        .child(div().text_size(px(13.)).text_color(th.tx2).truncate().child(sub))
}

/// The four tiles of the mock-up, each saying where its number comes from.
fn tiles(th: &Theme, d: &PanelData, hitches: Option<f32>) -> Div {
    let t = |id: &str, stat| text_of(d, id, stat);
    let shown_sub = match (t("fps.base", Stat::Mean), t("fps.generated", Stat::Mean)) {
        (Some(b), Some(g)) => format!("{b} rendered · {g} generated"),
        (Some(b), None) => format!("{b} rendered"),
        _ => "presented by the game".into(),
    };
    let p1 = t("fps.p1low", Stat::Last).or_else(|| t("fps", Stat::P1low));
    let ft_sub = [p1.map(|p| format!("1% low {p} fps")), hitches.map(|h| format!("{h:.0} hitches / min"))].into_iter().flatten().collect::<Vec<_>>();
    let gpu_sub = [
        t("gpu.temp", Stat::Last).map(|x| format!("{x} °C")),
        t("gpu.power", Stat::Last).map(|x| format!("{x} W")),
        match (t("gpu.vram.used", Stat::Last), t("gpu.vram.total", Stat::Last)) {
            (Some(u), Some(tot)) => Some(format!("VRAM {u} / {tot} GB")),
            (Some(u), None) => Some(format!("VRAM {u} GB")),
            _ => None,
        },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let cpu_sub = [t("cpu.temp", Stat::Last).map(|x| format!("{x} °C")), t("proc.rss", Stat::Last).map(|x| format!("game {x} GB RAM"))].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let gpu_src = metric(d, "gpu.source", Stat::Last).map(|v| v.text.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "system".into());
    div()
        .flex()
        .flex_wrap()
        .gap(px(14.))
        .child(tile(th, "Shown", "layer", t("fps", Stat::Mean), "fps", shown_sub))
        .child(tile(th, "Frame time", "layer", t("frametime", Stat::Mean), "ms", ft_sub.join(" · ")))
        .child(tile(th, "GPU", &gpu_src, t("gpu.load", Stat::Last), "%", gpu_sub))
        .child(tile(th, "CPU", "quiverd", t("cpu.load", Stat::Last), "%", cpu_sub))
}

/// Frame-time grid lines (ms) and the series colours.
const GRID_MS: [f32; 3] = [8.3, 16.7, 25.0];

/// The 30-second graph: presented frames (blue) and rendered ones (amber) over grid lines at
/// 120, 60 and 40 fps, with a legend; the hitches per minute in the heading.
fn graph_card(v: &PanelView, th: &Theme, h: f32) -> Div {
    let f = v.perf.data.frames.clone();
    let presented: Vec<Option<f32>> = f.as_ref().map(|f| f.presented.clone()).unwrap_or_default();
    let rendered: Vec<Option<f32>> = f.as_ref().map(|f| f.rendered.clone()).unwrap_or_default();
    let hitches = f.as_ref().and_then(|f| f.hitches_per_min);
    let top = presented.iter().chain(&rendered).flatten().fold(30.0f32, |a, b| a.max(*b * 1.1)).min(100.0);
    let (blue, amber, grid, label) = (th.who_gui, th.warn, th.tx1.opacity(0.08), th.tx3);
    let legend = |c: Hsla, t: &str| div().flex().items_center().gap(px(6.)).text_size(px(13.)).text_color(th.tx2).child(div().w(px(14.)).h(px(3.)).bg(c)).child(t.to_owned());
    let any_rendered = rendered.iter().any(Option::is_some);
    let none = presented.iter().all(Option::is_none);
    let labels = GRID_MS.iter().map(move |ms| {
        let y = 1.0 - ms / top;
        div().absolute().left(px(6.)).top(gpui::relative(y)).mt(px(-19.)).text_size(px(12.)).text_color(label).child(format!("{ms} ms"))
    });
    card(th)
        .gap(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(heading(th, &format!("Frame time, last {} s", super::SPAN_MS / 1000), None))
                .when_some(hitches, |d, h| d.child(div().text_size(px(12.)).text_color(th.tx3).child(format!("{h:.0} hitches / min"))))
                .child(div().flex_1())
                .child(legend(blue, "Presented"))
                .when(any_rendered, |d| d.child(legend(amber, "Rendered"))),
        )
        .child(
            div()
                .relative()
                .h(px(h))
                .w_full()
                .child(
                    canvas(
                        |b, _, _| b,
                        move |_, b: Bounds<Pixels>, window, _| {
                            let (x0, y0, w, hh) = (f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height));
                            if w < 4.0 || hh < 4.0 {
                                return;
                            }
                            let y_of = |ms: f32| y0 + hh * (1.0 - (ms / top).clamp(0.0, 1.0));
                            for ms in GRID_MS {
                                window.paint_quad(gpui::fill(Bounds::new(point(px(x0), px(y_of(ms))), size(px(w), px(1.))), grid));
                            }
                            let line = |window: &mut gpui::Window, pts: &[Option<f32>], c: Hsla| {
                                let n = pts.len().max(2) as f32 - 1.0;
                                let mut path: Option<PathBuilder> = None;
                                let mut segs = 0;
                                for (i, p) in pts.iter().enumerate() {
                                    let Some(ms) = p else {
                                        // A gap (nothing presented): the line breaks.
                                        if let Some(pb) = path.take()
                                            && segs > 0
                                            && let Ok(pb) = pb.build()
                                        {
                                            window.paint_path(pb, c);
                                        }
                                        segs = 0;
                                        continue;
                                    };
                                    let pt = point(px(x0 + w * i as f32 / n), px(y_of(*ms)));
                                    match &mut path {
                                        Some(pb) => {
                                            pb.line_to(pt);
                                            segs += 1;
                                        }
                                        None => {
                                            let mut pb = PathBuilder::stroke(px(2.));
                                            pb.move_to(pt);
                                            path = Some(pb);
                                        }
                                    }
                                }
                                if let Some(pb) = path
                                    && segs > 0
                                    && let Ok(pb) = pb.build()
                                {
                                    window.paint_path(pb, c);
                                }
                            };
                            line(window, &rendered, amber);
                            line(window, &presented, blue);
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .children(labels)
                .when(none, |d| {
                    d.child(div().absolute().inset_0().flex().items_center().justify_center().text_size(px(13.)).text_color(th.tx3).child("No frames yet: frame times need the Sight layer in the game."))
                }),
        )
}

fn note_line(th: &Theme, t: String, tone: Tone) -> Div {
    div().text_size(px(13.)).text_color(tone_color(th, tone)).child(t)
}

/// The frame-rate cap (Quiver Tweaks' `fps.cap`) and who enforces it.
fn cap_card(v: &PanelView, th: &Theme, cx: &mut Context<PanelView>) -> Div {
    let d = &v.data;
    let st = d.tweaks.ready().and_then(|t| t.options.get(CAP_TWEAK));
    let cur = v.cap_value();
    let on = has_focus(v, Ctl::Cap);
    let body = match st {
        Some(_) => {
            let items = cap_choices(&cur).iter().map(|c| (cap_label(c), quiver_api::options::same_value(c, &cur))).collect();
            let s = target(v, th, seg(th, items, false), on, 10.).id("perf-cap").cursor_pointer().on_click(cx.listener(|this, _, _, cx| this.perf_focus(Ctl::Cap, cx)));
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(s)
                .child(div().text_size(px(13.)).text_color(th.tx2).child(cap_enforcer(st, &cur, super::metric(d, "nv.reflex.cap", Stat::Last))))
                .when_some(v.tweak_outcome(CAP_TWEAK), |d, (t, tone)| d.child(note_line(th, format!("{} · {t}", d_scope(v)), tone)))
        }
        None => div().text_size(px(13.)).text_color(th.tx3).child("Quiver Tweaks aren't loaded for this game."),
    };
    card(th).flex_1().min_w(px(220.)).gap(px(10.)).child(heading(th, "Frame rate cap", Some("Quiver Tweaks"))).child(body)
}

fn d_scope(v: &PanelView) -> String {
    format!("Saved to {}", v.data.tweak_scope_name)
}

/// The HUD: hidden or a preset, the layout editor, and where it's drawn.
fn hud_card(v: &PanelView, th: &Theme, cx: &mut Context<PanelView>) -> Div {
    let d = &v.data;
    let p = &v.perf;
    let Some(h) = d.hud.ready() else {
        return card(th).flex_1().min_w(px(220.)).child(heading(th, "HUD", None)).child(div().text_size(px(13.)).text_color(th.tx3).child("Loading the HUD…"));
    };
    let cur = p.hud_value(d);
    let base = ["hidden", "fps", "compact", "standard", "full"];
    let mut items: Vec<(String, bool)> = base.iter().map(|id| (short_name(id), *id == cur)).collect();
    if !base.contains(&cur.as_str()) {
        items.push((layout_name(&cur, &p.data.layouts), true));
    }
    let _ = hud_choices(&p.data.layouts);
    let on_seg = has_focus(v, Ctl::Hud);
    let on_edit = has_focus(v, Ctl::Edit);
    let live = h.live();
    let why = if live {
        "Drawn in the game. Shift+F3 shows or hides it, Ctrl+Shift+F3 steps the preset.".to_owned()
    } else {
        let w = h.status.as_ref().and_then(|s| s.layer_off.clone()).map(|w| format!(" ({w})")).unwrap_or_default();
        format!("No Sight layer in this launch{w}: these apply when the game next starts.")
    };
    let edit = target(
        v,
        th,
        div()
            .id("perf-edit")
            .flex()
            .items_center()
            .justify_between()
            .h(px(36.))
            .px(px(12.))
            .rounded(px(9.))
            .bg(if on_edit { th.ink4 } else { th.ink3 })
            .text_size(px(14.))
            .font_weight(MEDIUM)
            .child("Edit the layout")
            .child(div().text_size(px(12.)).text_color(th.tx3).child(layout_name(&h.settings.effective.layout, &p.data.layouts)))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.perf_focus(Ctl::Edit, cx);
                this.input(quiver_ui::input::Input::Activate, cx);
            })),
        on_edit,
        9.,
    );
    let mut c = card(th)
        .flex_1()
        .min_w(px(260.))
        .gap(px(10.))
        .child(heading(th, "HUD", Some("Sight")))
        .child(target(v, th, seg(th, items, true), on_seg, 10.).id("perf-hud").cursor_pointer().on_click(cx.listener(|this, _, _, cx| this.perf_focus(Ctl::Hud, cx))));
    if p.ctls(d).contains(&Ctl::Enable) {
        let on = has_focus(v, Ctl::Enable);
        c = c.child(target(v, th, toggle_row(th, "Use Quiver's HUD in launches", h.settings.effective.enabled), on, 9.).id("perf-enable").on_click(cx.listener(|this, _, _, cx| this.perf_focus(Ctl::Enable, cx))));
    }
    c.child(edit).child(div().text_size(px(13.)).text_color(th.tx2).child(why))
}

fn short_name(id: &str) -> String {
    match id {
        "hidden" => "Hidden".into(),
        "fps" => "FPS".into(),
        other => PRESETS.iter().find(|p| p.0 == other).map(|p| p.1.to_owned()).unwrap_or_else(|| other.to_owned()),
    }
}

fn toggle_row(th: &Theme, label: &str, on: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(34.))
        .px(px(10.))
        .rounded(px(9.))
        .bg(th.ink3)
        .text_size(px(14.))
        .child(label.to_owned())
        .child(
            div()
                .w(px(38.))
                .h(px(22.))
                .rounded(px(11.))
                .bg(if on { th.acc } else { th.ink5 })
                .flex()
                .items_center()
                .when(on, |d| d.justify_end())
                .px(px(3.))
                .child(div().size(px(16.)).rounded(px(8.)).bg(if on { th.acc_ink } else { th.tx2 })),
        )
}

/// Reflex's mode, PC latency, game to render, and the opt-in driver reports.
fn latency_card(v: &PanelView, th: &Theme, cx: &mut Context<PanelView>) -> Div {
    let d = &v.data;
    let row = |k: &str, val: String, first: bool| {
        div()
            .flex()
            .justify_between()
            .py(px(8.))
            .when(!first, |d| d.border_t_1().border_color(th.line1))
            .text_size(px(14.))
            .child(div().text_color(th.tx2).child(k.to_owned()))
            .child(if val.chars().next().is_some_and(|c| c.is_ascii_digit()) { div().font_family(MONO_FONT).child(val) } else { div().child(val) })
    };
    let ms = |id: &str| text_of(d, id, Stat::Mean).map(|t| format!("{t} ms")).unwrap_or_else(|| "—".into());
    let timings = v.perf.config(d).reflex_timings;
    let on = has_focus(v, Ctl::Timings);
    let live = d.hud.ready().is_some_and(|h| h.live());
    card(th)
        .flex_1()
        .min_w(px(220.))
        .gap(px(4.))
        .child(heading(th, "Latency", Some("layer · Reflex")))
        .child(row("Reflex", reflex_words(metric(d, "nv.reflex.mode", Stat::Last).filter(|v| !v.stale)), true))
        .child(row("PC latency", ms("latency.pc"), false))
        .child(row("Game to render", ms("latency.render"), false))
        .when(d.hud.ready().is_some(), |c| {
            c.child(
                target(v, th, toggle_row(th, "Driver timing reports", timings), on, 9.)
                    .id("perf-timings")
                    .mt(px(6.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.perf_focus(Ctl::Timings, cx))),
            )
            .child(div().text_size(px(12.)).text_color(th.tx3).child(if live {
                "Reports split game-to-render into its stages; from the next launch."
            } else {
                "Reports split game-to-render into its stages."
            }))
        })
}

// ---- The editor ------------------------------------------------------------------------------

/// The layout editor: the session's metrics by producer, the live preview whose rows move,
/// and the layout's settings. Three columns when there's room, else the settings under the
/// preview.
fn editor(v: &PanelView, th: &Theme, room: Room, cx: &mut Context<PanelView>) -> AnyElement {
    let h = room.h.max(420.);
    let wide = room.w >= 1000.;
    let metrics = metrics_list(v, th, h);
    let preview = preview_pane(v, th);
    let settings = settings_pane(v, th, wide);
    let _ = cx;
    if wide {
        div()
            .flex()
            .gap(px(16.))
            .h(px(h))
            .child(metrics.w(px(320.)).flex_none())
            .child(preview.flex_1().min_w_0())
            .child(settings.w(px(330.)).flex_none())
            .into_any_element()
    } else {
        // Two columns: the metrics, and the preview or (with the settings focused) the
        // settings in its place.
        let e = v.perf.editor.as_ref().unwrap();
        let right = if e.zone == Zone::Settings { settings } else { preview };
        div()
            .flex()
            .gap(px(14.))
            .h(px(h))
            .child(metrics.w(px((room.w * 0.42).clamp(220., 320.))).flex_none())
            .child(right.flex_1().min_w_0())
            .into_any_element()
    }
}

/// The session's metrics by producer, a checkbox and a kind each.
fn metrics_list(v: &PanelView, th: &Theme, h: f32) -> Div {
    let e = v.perf.editor.as_ref().unwrap();
    let focus_zone = v.nav.zone == NavZone::Body && e.zone == Zone::Metrics && e.moving.is_none();
    let mut c = card(th).p(px(0.)).pb(px(8.)).overflow_hidden().child(div().px(px(14.)).pt(px(14.)).pb(px(4.)).child(heading(th, "Metrics in this session", None)));
    let Some(cat) = v.perf.data.catalog.ready() else {
        let t = match &v.perf.data.catalog {
            crate::panel::state::Load::Failed(e) => format!("Couldn't load the metrics: {e}"),
            _ => "Loading the metrics…".into(),
        };
        return c.child(div().px(px(14.)).text_size(px(13.)).text_color(th.tx3).child(t));
    };
    let groups = groups(cat);
    let list = entries(&groups);
    // Only what fits, around the focused entry (≈ 31 px a line, a group's heading 34).
    let fits = (((h - 60.) / 32.) as usize).saturating_sub(groups.len() + 1).max(6);
    let start = e.metric.saturating_sub(fits / 2).min(list.len().saturating_sub(fits));
    let shown: Vec<(usize, (usize, usize))> = list.iter().copied().enumerate().skip(start).take(fits).collect();
    let mut last_group = None;
    for (k, (g, i)) in shown {
        if last_group != Some(g) {
            last_group = Some(g);
            let gr = &groups[g];
            c = c.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(14.))
                    .pt(px(10.))
                    .pb(px(4.))
                    .child(div().size(px(8.)).rounded(px(4.)).bg(source_mark(th, gr.source)))
                    .child(div().text_size(px(13.)).font_weight(SEMIBOLD).child(gr.title.clone()))
                    .child(div().flex_1())
                    .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(if gr.source == quiver_api::perf::SourceKind::Mod { th.who_cli } else { th.tx3 }).child(gr.note.clone())),
            );
        }
        let entry = &groups[g].entries[i];
        let on = shows(&e.layout, &entry.id);
        let focused = focus_zone && k == e.metric;
        c = c.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(6.))
                .text_size(px(14.))
                .text_color(th.tx1)
                .when(focused, |d| d.bg(th.acc_soft).border_l_2().border_color(th.acc))
                .when(focused, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)).relative())
                .child(div().flex_none().size(px(16.)).rounded(px(4.)).border(px(1.5)).border_color(if on { th.acc } else { th.tx4 }).when(on, |d| d.bg(th.acc)).flex().items_center().justify_center().when(on, |d| d.child(div().text_size(px(11.)).text_color(th.acc_ink).child("✓"))))
                .child(div().flex_1().min_w_0().truncate().child(entry.label.clone()))
                .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child(entry.kind)),
        );
    }
    if list.is_empty() {
        c = c.child(div().px(px(14.)).text_size(px(13.)).text_color(th.tx3).child("No metrics have values yet."));
    }
    c
}

/// The live preview: the edited layout as the HUD draws it, the picked row outlined.
fn preview_pane(v: &PanelView, th: &Theme) -> Div {
    let e = v.perf.editor.as_ref().unwrap();
    let on = v.nav.zone == NavZone::Body && e.zone == Zone::Preview;
    let hint = if e.moving.is_some() { "Moving · ▲▼ then A to put it down, B to put it back" } else { "Live preview · move rows with A then ▲▼" };
    card(th)
        .relative()
        .items_center()
        .justify_center()
        .gap(px(14.))
        .overflow_hidden()
        // A stand-in for the game behind the HUD.
        .bg(th.ink0)
        .when(on, |d| d.border_color(th.acc_line))
        .child(match &v.perf_preview {
            Some(p) => div().relative().when(on, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD))).child(p.clone()).into_any_element(),
            None => div().text_size(px(13.)).text_color(th.tx3).child("Loading the preview…").into_any_element(),
        })
        .child(div().text_size(px(13.)).text_color(th.tx1).px(px(10.)).py(px(6.)).rounded(px(8.)).bg(th.ink1.opacity(0.8)).child(hint))
}

/// The preview's model: the last preview's rows in the draft's order, a placeholder for rows
/// added since, and the highlighted range of the selected row.
pub fn preview_model(v: &PanelView) -> Option<(HudModel, Option<(std::ops::Range<usize>, bool)>)> {
    let e = v.perf.editor.as_ref()?;
    let (_, p) = e.preview.as_ref()?;
    let mut m = p.model.clone();
    let mut rows: Vec<HudRow> = Vec::new();
    let mut sel = None;
    for i in 0..e.layout.rows.len() {
        let start = rows.len();
        match e.model_rows(i) {
            Some(r) => rows.extend(p.model.rows[r].iter().cloned()),
            None => rows.push(HudRow { cells: vec![quiver_api::perf::HudCell::text("…", quiver_api::perf::Level::Dim)], divider: false }),
        }
        if i == e.row {
            sel = Some(start..rows.len());
        }
    }
    m.rows = rows;
    m.presenter = Presenter::Desktop;
    // As tall as the rows in it (the preview's card follows the edit).
    let slots: Vec<f32> = m.rows.iter().map(quiver_api::perf::hud_card::row_height).collect();
    m.size[1] = quiver_api::perf::hud_card::height(m.orientation, &slots, quiver_api::perf::hud_card::panel(m.background));
    m.anchor = quiver_api::perf::Anchor::TopLeft;
    let on = v.nav.zone == NavZone::Body && e.zone == Zone::Preview;
    Some((m, sel.filter(|_| on).map(|r| (r, e.moving.is_some()))))
}

/// Creates or updates the preview's HUD view (before the frame is laid out).
pub fn sync_preview(v: &mut PanelView, cx: &mut Context<PanelView>) {
    let Some((m, hl)) = preview_model(v) else {
        v.perf_preview = None;
        return;
    };
    // A little larger than in the game at 1080p, to read it in the panel.
    let unit = if v.room.w >= 1000. { 1.3 } else { 1.1 };
    match &v.perf_preview {
        Some(p) => p.update(cx, |h, _| {
            if h.model != m || h.highlight != hl || h.unit != unit {
                h.model = m;
                h.highlight = hl;
                h.unit = unit;
            }
        }),
        None => {
            let mut h = HudView::new(m, unit, Mode::Embedded);
            h.highlight = hl;
            v.perf_preview = Some(cx.new(|_| h));
        }
    }
}

/// The layout's settings (preset, position, opacity, scale, update rate, toggle key, captures,
/// scope, where it's drawn).
fn settings_pane(v: &PanelView, th: &Theme, wide: bool) -> Div {
    let e = v.perf.editor.as_ref().unwrap();
    let cfg = v.perf.config(&v.data);
    let zone_on = v.nav.zone == NavZone::Body && e.zone == Zone::Settings;
    let session = v.data.session.ready();
    let row = |s: Option<Setting>, label: &str, value: AnyElement| {
        let focused = zone_on && s.is_some_and(|s| SETTINGS[e.setting] == s);
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .py(px(if wide { 9. } else { 6. }))
            .px(px(8.))
            .rounded(px(8.))
            .text_size(px(14.))
            .when(focused, |d| d.bg(th.ink3))
            .focus_ring(focused, th, 8.)
            .when(focused, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
            .child(div().text_color(th.tx2).child(label.to_owned()))
            .child(value)
    };
    let text = |t: String| div().child(t).into_any_element();
    let (ar, ac) = anchor_cell(cfg.anchor);
    let grid = div().grid().grid_cols(3).gap(px(3.)).w(px(66.)).children((0..9).map(|k| {
        let on = (k / 3, k % 3) == (ar, ac);
        div().h(px(14.)).rounded(px(3.)).bg(if on { th.acc } else { th.ink4 })
    }));
    let anchor_name = ANCHORS.iter().find(|a| a.0 == cfg.anchor).map(|a| a.1).unwrap_or("");
    let rate = [2u32, 4, 10].iter().map(|r| (format!("{r}/s"), *r == cfg.rate_hz)).collect();
    let applies: Vec<(String, bool)> = session
        .map(|s| super::Applies::choices(s))
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.label().to_owned(), a == e.applies))
        .collect();
    let drawn_on = |desktop: bool| (cfg.presenter == PresenterSetting::Desktop) == desktop;
    let drawn = |title: &str, help: &str, on: bool| {
        div()
            .flex()
            .flex_col()
            .px(px(10.))
            .py(px(7.))
            .rounded(px(7.))
            .bg(if on { th.acc } else { th.ink3 })
            .text_color(if on { th.acc_ink } else { th.tx1 })
            .child(div().text_size(px(12.)).font_weight(SEMIBOLD).child(title.to_owned()))
            .child(div().text_size(px(11.)).opacity(0.85).child(help.to_owned()))
    };
    let focused_drawn = zone_on && SETTINGS[e.setting] == Setting::Drawn;
    let presets = [
        row(Some(Setting::Preset), "Preset", text(e.title())),
        row(Some(Setting::Position), "Position", div().flex().items_center().gap(px(8.)).child(div().text_size(px(12.)).text_color(th.tx3).child(anchor_name)).child(grid).into_any_element()),
        row(Some(Setting::Opacity), "Opacity", text(format!("{:.0}%", cfg.background * 100.0))),
        row(Some(Setting::Scale), "Scale", text(format!("{:.0}%", cfg.scale * 100.0))),
        row(Some(Setting::Rate), "Numbers update", seg(th, rate, true).into_any_element()),
        row(None, "Toggle key", div().font_family(MONO_FONT).text_size(px(13.)).child("Shift+F3").into_any_element()),
        row(Some(Setting::Captures), "In captures", seg(th, vec![("Shown".into(), cfg.captures == Captures::Shown), ("Hidden".into(), cfg.captures == Captures::Hidden)], true).into_any_element()),
        row(Some(Setting::AppliesTo), "Applies to", seg(th, applies, true).into_any_element()),
    ];
    let drawn_box = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(4.))
        .rounded(px(9.))
        .bg(th.tx1.opacity(0.05))
        .focus_ring(focused_drawn, th, 9.)
        .when(focused_drawn, |d| d.child(mark(&v.marks, Mark::Focus, MARK_PAD)))
        .child(drawn("In the game (Sight layer)", "Keeps VRR pacing, direct scan-out and tearing", drawn_on(false)))
        .child(drawn("Over the game (compositor)", "For OpenGL and anti-cheat games; turns off direct scan-out", drawn_on(true)));
    let c = card(th).gap(px(2.)).child(div().pb(px(4.)).child(heading(th, "Layout", Some(&scope_words(e.applies)))));
    c.children(presets).child(div().pt(px(if wide { 10. } else { 6. })).pb(px(6.)).child(heading(th, "Drawn", None))).child(drawn_box)
}

fn scope_words(a: Applies) -> String {
    match a {
        Applies::Profile => "this profile".into(),
        Applies::Game => "this game".into(),
        Applies::All => "every game".into(),
    }
}

/// `SharedString` for ids.
pub fn sid(s: String) -> SharedString {
    SharedString::from(s)
}

/// The panel keeps the preview entity; this is its type.
pub type Preview = Entity<HudView>;
