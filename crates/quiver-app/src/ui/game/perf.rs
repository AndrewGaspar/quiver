//! Performance: the game's play sessions with their performance summaries (decision 13),
//! one session's report, a comparison with others (profiles, mod versions), the live
//! numbers while it runs, and its HUD settings (design/SIGHT-HUD.md §3.12).
//!
//! Everything shown comes from what quiverd records (`PerfRequest::Summaries`): a summary
//! per session, no per-second series. Graphs over a session need recordings (phase 2), and
//! the report says so instead of drawing one.

use gpui::{AnyElement, Context, Hsla, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::perf::{FpsStats, HudScopeRef, PerfSummary, Presenter, SessionSummary};
use quiver_api::{PlaySession, Profile};

use super::page::{load_note, note};
use crate::app::Quiver;
use crate::model::{GameRow, clock, day, duration};
use crate::nav::Target;
use crate::theme::Theme;
use crate::ui::perf::{HUD_FIELDS, HudField, ScopePick};
use crate::ui::{BOLD, FocusExt, Icon, MEDIUM, SEMIBOLD, card, chip, command_line, icon, mono};

/// Sessions listed (and focusable) on the tab.
pub const PERF_ROWS: usize = 12;

/// What a session ran: profile and mod versions, from its play-history record.
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    pub profile: String,
    /// "DLSS + DLSSTweaks 3.8.10"; empty for vanilla or unknown.
    pub mods: String,
    /// Same key, same configuration.
    pub key: String,
}

impl RunConfig {
    pub fn title(&self) -> String {
        if self.mods.is_empty() || self.mods == self.profile { self.profile.clone() } else { format!("{} · {}", self.profile, self.mods) }
    }
}

/// The configuration of a session: its profile's name (or Vanilla) and the versions of the
/// mods that have more than one, from the matching play-history record.
pub fn run_config(s: &SessionSummary, history: &[PlaySession], profiles: &[Profile]) -> RunConfig {
    let rec = history.iter().find(|p| p.started == s.started);
    let profile_id = s.profile.clone().or_else(|| rec.and_then(|r| r.profile.clone()));
    let profile = match &profile_id {
        None => "Vanilla".to_owned(),
        Some(id) => profiles.iter().find(|p| p.id == *id).map(|p| p.name.clone()).unwrap_or_else(|| format!("Profile {id}")),
    };
    let mods = rec.map(|r| r.mods.as_slice()).unwrap_or_default();
    let versioned: Vec<String> = mods.iter().filter(|m| m.versions > 1 || m.pinned).map(|m| format!("{} {}", m.name, m.label).trim().to_owned()).collect();
    let key = format!(
        "{}|{}",
        profile_id.as_deref().unwrap_or("vanilla"),
        mods.iter().map(|m| format!("{}@{}", m.mod_id, m.version)).collect::<Vec<_>>().join(",")
    );
    RunConfig { profile, mods: versioned.join(", "), key }
}

/// Sessions to compare `sel` with when the user hasn't picked (at most two): the latest
/// run of the same profile on other mod versions, the latest vanilla run, then the latest
/// of any other configuration; else the session before.
pub fn auto_compare(configs: &[RunConfig], usable: &[bool], sel: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let Some(me) = configs.get(sel) else { return out };
    let ok = |i: usize| i != sel && usable.get(i).copied().unwrap_or(false) && configs[i].key != me.key;
    let profile_of = |c: &RunConfig| c.key.split('|').next().unwrap_or_default().to_owned();
    let candidates = [
        (0..configs.len()).find(|&i| ok(i) && profile_of(&configs[i]) == profile_of(me)),
        (0..configs.len()).find(|&i| ok(i) && profile_of(&configs[i]) == "vanilla"),
    ];
    for i in candidates.into_iter().flatten() {
        if !out.iter().any(|&o| configs[o].key == configs[i].key) {
            out.push(i);
        }
    }
    for i in 0..configs.len() {
        if out.len() >= 2 {
            break;
        }
        if ok(i) && !out.iter().any(|&o| configs[o].key == configs[i].key) {
            out.push(i);
        }
    }
    if out.is_empty()
        && let Some(i) = (0..configs.len()).find(|i| *i != sel && usable.get(*i).copied().unwrap_or(false))
    {
        out.push(i);
    }
    out
}

/// `a` against `b`, as a signed percentage: "+4%", "−12%", "±0%".
pub fn delta_pct(a: f64, b: f64) -> Option<f64> {
    (b.abs() > 1e-9 && a.is_finite() && b.is_finite()).then(|| (a - b) / b * 100.)
}

pub fn delta_text(d: f64) -> String {
    let r = d.round();
    if r.abs() < 0.5 {
        "±0%".into()
    } else if r > 0. {
        format!("+{r:.0}%")
    } else {
        format!("−{:.0}%", -r)
    }
}

fn fps_text(f: &FpsStats) -> String {
    match f.p1low {
        Some(l) => format!("{:.0} / {:.0}", f.mean, l),
        None => format!("{:.0}", f.mean),
    }
}

/// Hitches per hour of frame data.
fn hitch_rate(p: &PerfSummary) -> Option<f64> {
    (p.frame_seconds > 60.).then(|| p.hitches as f64 / (p.frame_seconds / 3600.))
}

/// One row of the comparison: label, the value as text, the number compared, and whether
/// more is better.
struct Metric {
    label: &'static str,
    text: fn(&PerfSummary) -> Option<String>,
    num: fn(&PerfSummary) -> Option<f64>,
    higher_better: bool,
    missing: &'static str,
}

const METRICS: &[Metric] = &[
    Metric { label: "Shown FPS (avg)", text: |p| p.fps.map(|f| format!("{:.0}", f.mean)), num: |p| p.fps.map(|f| f.mean), higher_better: true, missing: "no frame data" },
    Metric { label: "1% low", text: |p| p.fps.and_then(|f| f.p1low).map(|l| format!("{l:.0}")), num: |p| p.fps.and_then(|f| f.p1low), higher_better: true, missing: "no frame data" },
    Metric { label: "Base FPS", text: |p| p.fps_base.map(|f| format!("{:.0}", f.mean)), num: |p| p.fps_base.map(|f| f.mean), higher_better: true, missing: "no FG mod" },
    Metric { label: "Frame generation", text: |p| p.fg_ratio.map(|r| format!("{r:.1}×")), num: |_| None, higher_better: true, missing: "off" },
    Metric {
        label: "PC latency (avg / peak)",
        text: |p| p.latency_ms.map(|l| format!("{:.0} / {:.0} ms", l.mean, l.max)),
        num: |p| p.latency_ms.map(|l| l.mean),
        higher_better: false,
        missing: "not reported",
    },
    Metric { label: "Frame time p99", text: |p| p.frametime_ms.map(|f| format!("{:.1} ms", f.p99)), num: |p| p.frametime_ms.map(|f| f.p99), higher_better: false, missing: "no frame data" },
    Metric { label: "Hitches per hour", text: |p| hitch_rate(p).map(|h| format!("{h:.0}")), num: hitch_rate, higher_better: false, missing: "—" },
    Metric {
        label: "GPU (avg / peak temp)",
        text: |p| p.gpu_load.map(|g| format!("{:.0}%{}", g.mean, p.gpu_temp.map(|t| format!(" / {:.0}°", t.max)).unwrap_or_default())),
        num: |_| None,
        higher_better: true,
        missing: "no sensor",
    },
    Metric { label: "GPU power (avg)", text: |p| p.gpu_power.map(|w| format!("{:.0} W", w.mean)), num: |p| p.gpu_power.map(|w| w.mean), higher_better: false, missing: "no sensor" },
    Metric { label: "CPU (avg)", text: |p| p.cpu_load.map(|c| format!("{:.0}%", c.mean)), num: |_| None, higher_better: true, missing: "no sensor" },
];

impl Quiver {
    /// The sessions the tab lists and the configuration each ran.
    fn perf_rows(&self, g: &GameRow) -> (Vec<SessionSummary>, Vec<RunConfig>) {
        let list: Vec<SessionSummary> = match (self.perf.summaries_for == g.id(), self.perf.summaries.ready()) {
            (true, Some(v)) => v.iter().take(PERF_ROWS).cloned().collect(),
            _ => vec![],
        };
        let history = g.playtime.as_ref().map(|p| p.sessions.as_slice()).unwrap_or_default();
        let profiles = if self.gp.id == g.id() { self.gp.profiles() } else { vec![] };
        let configs = list.iter().map(|s| run_config(s, history, &profiles)).collect();
        (list, configs)
    }

    fn perf_scope_pick(&self, id: u32) -> ScopePick {
        let pick = self.perf.scope.get(&id).cloned().unwrap_or_default();
        match &pick {
            ScopePick::Profile(p) if self.gp.id == id && !self.gp.profiles().iter().any(|x| x.id == *p) => ScopePick::Game,
            _ => pick,
        }
    }

    /// Controller rows: the sessions, the HUD's scope, then each HUD setting.
    pub(super) fn perf_body_len(&self, id: u32) -> usize {
        let n = match (self.perf.summaries_for == id, self.perf.summaries.ready()) {
            (true, Some(v)) => v.len().min(PERF_ROWS),
            _ => 0,
        };
        let scope = self.perf_scope_pick(id).scope(id);
        let hud = if self.hud_effective(&scope).is_some() { 1 + HUD_FIELDS.len() } else { 0 };
        n + hud
    }

    pub(super) fn perf_body_activate(&mut self, id: u32, i: usize, cx: &mut Context<Self>) {
        let n = match (self.perf.summaries_for == id, self.perf.summaries.ready()) {
            (true, Some(v)) => v.len().min(PERF_ROWS),
            _ => 0,
        };
        if i < n {
            if self.perf.sel != i {
                self.perf.sel = i;
                self.perf.compare = None;
            }
            return;
        }
        if i == n {
            // The scope: this game → each profile → all games → this game.
            let mut picks = vec![ScopePick::Game];
            if self.gp.id == id {
                picks.extend(self.gp.profiles().into_iter().map(|p| ScopePick::Profile(p.id)));
            }
            picks.push(ScopePick::Global);
            let cur = self.perf_scope_pick(id);
            let at = picks.iter().position(|p| *p == cur).unwrap_or(0);
            self.perf.scope.insert(id, picks[(at + 1) % picks.len()].clone());
            return;
        }
        if let Some(f) = HUD_FIELDS.get(i - n - 1).copied() {
            let scope = self.perf_scope_pick(id).scope(id);
            self.hud_cycle(scope, f, cx);
        }
    }

    pub(super) fn render_perf(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if self.perf.summaries_for != id {
            self.load_summaries(id, cx);
        }
        if self.perf.sessions.is_idle() {
            self.load_perf_sessions(cx);
        }
        self.start_perf_events(cx);
        let scope = self.perf_scope_pick(id).scope(id);
        self.ensure_hud(&scope, cx);
        if self.perf_session(id).is_some() {
            self.ensure_live(id, cx);
            if !self.perf.status.contains_key(&id) {
                self.load_hud_status(id, cx);
            }
        }

        let (list, configs) = self.perf_rows(g);
        let mut out: Vec<AnyElement> = Vec::new();
        out.extend(self.render_live_strip(th, id));
        match &self.perf.summaries {
            Load::Ready(_) if list.is_empty() => out.push(
                card(th)
                    .child(note(
                        th,
                        "No performance data yet. quiverd records a summary of every session (frame rates from the Sight layer when the HUD is set up, sensors always); it shows here after the game's next run.",
                    ))
                    .into_any_element(),
            ),
            Load::Ready(_) => {
                let sel = self.perf.sel.min(list.len() - 1);
                out.push(self.render_perf_report(th, &list[sel], &configs[sel]));
                out.push(self.render_perf_compare(th, &list, &configs, sel, cx));
                out.push(self.render_perf_sessions(th, &list, &configs, sel, cx));
            }
            l => {
                if let Some(n) = load_note(th, l, "performance summaries") {
                    out.push(card(th).child(n).into_any_element());
                }
            }
        }
        out.push(self.render_perf_hud(th, g, &scope, list.len(), cx));
        div().flex().flex_col().gap(px(16.)).children(out).into_any_element()
    }

    // -----------------------------------------------------------------------------------
    // One session

    fn render_perf_report(&self, th: &Theme, s: &SessionSummary, cfg: &RunConfig) -> AnyElement {
        let p = &s.perf;
        let tile = |label: String, big: String, unit: &str, sub: String, accent: Option<Hsla>| {
            card(th)
                .flex_1()
                .min_w(px(170.))
                .p(px(16.))
                .gap(px(4.))
                .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child(label.to_uppercase()))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().text_size(px(28.)).font_weight(BOLD).text_color(accent.unwrap_or(th.tx1)).child(big))
                        .child(div().text_size(px(13.)).font_weight(MEDIUM).text_color(th.tx2).child(unit.to_owned())),
                )
                .child(div().text_size(px(12.5)).text_color(th.tx2).child(sub))
                .into_any_element()
        };
        let mut tiles = Vec::new();
        match p.fps {
            Some(f) => tiles.push(tile(
                "Shown".into(),
                format!("{:.0}", f.mean),
                "fps avg",
                match (f.p1low, f.p01low) {
                    (Some(a), Some(b)) => format!("1% low {a:.0} · 0.1% low {b:.0}"),
                    (Some(a), None) => format!("1% low {a:.0}"),
                    _ => String::new(),
                },
                None,
            )),
            None => tiles.push(tile("Shown".into(), "—".into(), "", "No frame data: the HUD wasn't set up, so the Sight layer wasn't there".into(), Some(th.tx3))),
        }
        if let Some(b) = p.fps_base {
            let sub = match p.fg_ratio {
                Some(r) => format!("{r:.1}× frame generation on average"),
                None => "real frames, from a mod".into(),
            };
            tiles.push(tile("Base · from a mod".into(), format!("{:.0}", b.mean), "fps avg", sub, Some(th.warn)));
        }
        if let Some(l) = p.latency_ms {
            tiles.push(tile("PC latency · from a mod".into(), format!("{:.0}", l.mean), "ms avg", format!("peak {:.0} ms", l.max), None));
        }
        if p.frames > 0 {
            let rate = hitch_rate(p).map(|h| format!(" · {h:.0} an hour")).unwrap_or_default();
            tiles.push(tile("Hitches".into(), p.hitches.to_string(), "", format!("frames over 4× the median frame time{rate}"), None));
        }
        if let Some(gl) = p.gpu_load {
            let mut sub = Vec::new();
            if let Some(t) = p.gpu_temp {
                sub.push(format!("peak {:.0} °C", t.max));
            }
            if let Some(w) = p.gpu_power {
                sub.push(format!("{:.0} W", w.mean));
            }
            if let Some(c) = p.cpu_load {
                sub.push(format!("CPU {:.0}%{}", c.mean, p.cpu_temp.map(|t| format!(" {:.0} °C", t.max)).unwrap_or_default()));
            }
            tiles.push(tile("GPU".into(), format!("{:.0}", gl.mean), "% avg", sub.join(" · "), None));
        }

        let when = format!("{} {}–{}", day(s.started), clock(s.started), clock(s.started + chrono::Duration::seconds(s.seconds as i64)));
        let head = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(10.))
            .gap_y(px(4.))
            .child(div().text_size(px(15.)).font_weight(SEMIBOLD).child(format!("Session {when}")))
            .child(chip(th, Some(Icon::Layers), cfg.title(), false))
            .child(div().text_size(px(12.5)).text_color(th.tx3).child(duration(s.seconds)));

        let frame_card = self.render_frame_times(th, p);
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(head)
            .child(div().flex().flex_wrap().gap(px(12.)).children(tiles))
            .child(frame_card)
            .into_any_element()
    }

    /// The frame-time percentiles as bars, what the numbers came from, and what a graph needs.
    fn render_frame_times(&self, th: &Theme, p: &PerfSummary) -> AnyElement {
        let mut c = card(th).p(px(16.)).gap(px(10.));
        c = c.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child("FRAME TIMES"))
                .child(div().flex_1())
                .child(div().text_size(px(11.5)).text_color(th.tx3).child(match (p.frames, p.gpu.as_deref()) {
                    (0, Some(gpu)) => format!("sensors only · {gpu}"),
                    (0, None) => "sensors only".into(),
                    (n, gpu) => format!(
                        "{} frames over {}{}",
                        group_digits(n),
                        duration(p.frame_seconds as u64),
                        gpu.map(|g| format!(" · {g}")).unwrap_or_default()
                    ),
                })),
        );
        if let Some(ft) = p.frametime_ms {
            let max = (ft.p99 * 1.15).max(1.);
            let bar = |label: &str, v: f64, color: Hsla| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().w(px(36.)).text_size(px(11.5)).text_color(th.tx3).child(label.to_owned()))
                    .child(
                        div()
                            .flex_1()
                            .h(px(8.))
                            .rounded(px(4.))
                            .bg(th.track)
                            .child(div().h_full().rounded(px(4.)).bg(color).w(gpui::relative((v / max).clamp(0.02, 1.) as f32))),
                    )
                    .child(mono(12., th.tx1).w(px(62.)).text_right().child(format!("{v:.1} ms")))
            };
            c = c.child(bar("p50", ft.p50, th.acc)).child(bar("p95", ft.p95, th.acc_hi)).child(bar("p99", ft.p99, th.warn));
        }
        let mut notes: Vec<(Icon, Hsla, String)> = Vec::new();
        match p.presenter {
            Some(Presenter::Desktop) => notes.push((
                Icon::TriangleAlert,
                th.warn,
                "The HUD was drawn over the game for most of this session, which costs direct scan-out and VRR pacing: compare with care.".into(),
            )),
            Some(Presenter::Game) => notes.push((Icon::CircleCheck, th.ok, "The HUD was drawn in the game, so the numbers carry no compositor cost.".into())),
            None if p.frames > 0 => notes.push((Icon::Info, th.tx3, "The HUD was hidden for most of this session.".into())),
            None => {}
        }
        if p.fps_base.is_some() {
            notes.push((Icon::Info, th.tx3, "Base FPS is a mean from the mod's real-frame counter; its lows come with mod rings.".into()));
        }
        notes.push((
            Icon::Activity,
            th.tx3,
            "A graph over the session, with area markers, comes with recordings: summaries keep totals and percentiles, not a timeline.".into(),
        ));
        c.children(notes.into_iter().map(|(ic, color, t)| {
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(div().pt(px(1.)).child(icon(ic, 13., color)))
                .child(div().text_size(px(12.)).text_color(th.tx2).child(t))
        }))
        .into_any_element()
    }

    // -----------------------------------------------------------------------------------
    // Comparison

    fn render_perf_compare(&mut self, th: &Theme, list: &[SessionSummary], configs: &[RunConfig], sel: usize, cx: &mut Context<Self>) -> AnyElement {
        let usable: Vec<bool> = list.iter().map(|s| s.perf.fps.is_some() || s.perf.gpu_load.is_some()).collect();
        let picked: Vec<usize> = self.perf.compare.clone().unwrap_or_else(|| auto_compare(configs, &usable, sel)).into_iter().filter(|i| *i != sel && *i < list.len()).collect();
        let label = |i: usize| format!("{} · {}", configs[i].title(), day(list[i].started));
        let candidates: Vec<usize> = (0..list.len()).filter(|i| *i != sel && usable[*i]).take(6).collect();
        let chips = candidates.into_iter().map(|i| {
            let on = picked.contains(&i);
            let cur = picked.clone();
            div()
                .id(("perf-cmp", i))
                .cursor_pointer()
                .child(chip(th, on.then_some(Icon::Check), label(i), on))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut v = cur.clone();
                    if let Some(at) = v.iter().position(|x| *x == i) {
                        v.remove(at);
                    } else {
                        v.push(i);
                        if v.len() > 2 {
                            v.remove(0);
                        }
                    }
                    this.perf.compare = Some(v);
                    cx.notify();
                }))
        });
        let cols: Vec<usize> = std::iter::once(sel).chain(picked.iter().copied()).collect();
        let n = cols.len();
        let cell = |content: AnyElement, first: bool| div().flex_1().min_w(px(if first { 170. } else { 120. })).py(px(8.)).px(px(8.)).border_t_1().border_color(th.line1).child(content);
        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(
            div()
                .flex()
                .children(std::iter::once(div().text_size(px(11.5)).font_weight(SEMIBOLD).text_color(th.tx3).child("Metric").into_any_element()).chain(cols.iter().enumerate().map(|(k, &i)| {
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(11.5)).font_weight(SEMIBOLD).text_color(if k == 0 { th.tx1 } else { th.tx3 }).truncate().child(configs[i].title()))
                        .child(div().text_size(px(11.)).text_color(th.tx3).child(if k == 0 { format!("{} · this session", day(list[i].started)) } else { day(list[i].started) }))
                        .into_any_element()
                })).enumerate().map(|(k, e)| div().flex_1().min_w(px(if k == 0 { 170. } else { 120. })).px(px(8.)).pb(px(6.)).child(e)))
                .into_any_element(),
        );
        for m in METRICS {
            let base = &list[sel].perf;
            if cols.iter().all(|&i| (m.text)(&list[i].perf).is_none()) {
                continue;
            }
            let mut cells = vec![cell(div().text_size(px(12.5)).text_color(th.tx2).child(m.label).into_any_element(), true).into_any_element()];
            for (k, &i) in cols.iter().enumerate() {
                let p = &list[i].perf;
                let content = match (m.text)(p) {
                    Some(t) => {
                        // This session against the other one.
                        let d = if k > 0 { (m.num)(base).zip((m.num)(p)).and_then(|(a, b)| delta_pct(a, b)) } else { None };
                        let color = |d: f64| {
                            if d.abs() < 2. {
                                th.tx3
                            } else if (d > 0.) == m.higher_better {
                                th.ok
                            } else {
                                th.bad
                            }
                        };
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(6.))
                            .child(mono(12.5, th.tx1).child(t))
                            .when_some(d, |e, d| e.child(div().text_size(px(11.5)).font_weight(SEMIBOLD).text_color(color(d)).child(delta_text(d))))
                            .into_any_element()
                    }
                    None => div().text_size(px(12.)).text_color(th.tx4).child(m.missing).into_any_element(),
                };
                cells.push(cell(content, false).into_any_element());
            }
            rows.push(div().flex().children(cells).into_any_element());
        }
        card(th)
            .p(px(16.))
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child("COMPARE"))
                    .child(div().flex_1())
                    .children(chips),
            )
            .when(n == 1, |d| d.child(note(th, "Nothing to compare with yet: another session with frame or sensor data shows up here.")))
            .when(n > 1, |d| d.child(div().flex().flex_col().children(rows)))
            .child(div().text_size(px(11.5)).text_color(th.tx3).child(if n > 1 {
                "Percentages are this session against each. Single sessions differ by a few percent run to run; repeat a route before reading much into small changes."
            } else {
                ""
            }))
            .child(div().flex().child(command_line(th, format!("quiver perf history {}", self.perf.summaries_for))))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------
    // The list

    fn render_perf_sessions(&mut self, th: &Theme, list: &[SessionSummary], configs: &[RunConfig], sel: usize, cx: &mut Context<Self>) -> AnyElement {
        let picked: Vec<usize> = self.perf.compare.clone().unwrap_or_else(|| {
            let usable: Vec<bool> = list.iter().map(|s| s.perf.fps.is_some() || s.perf.gpu_load.is_some()).collect();
            auto_compare(configs, &usable, sel)
        });
        let rows = list.iter().enumerate().map(|(i, s)| {
            let on = i == sel;
            let focused = self.focused(Target::Body(i));
            let p = &s.perf;
            let fps = p.fps.as_ref().map(fps_text).unwrap_or_else(|| "—".into());
            let base = p.fps_base.map(|b| format!("{:.0}", b.mean));
            let hover = th.ink3;
            div()
                .id(("perf-row", i))
                .relative()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x(px(14.))
                .gap_y(px(4.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(on, |d| d.bg(th.sel))
                .when(!on, |d| d.hover(move |s| s.bg(hover)))
                .focus_ring(focused, th, 8.)
                .when(on, |d| d.child(div().absolute().left(px(0.)).top(px(8.)).bottom(px(8.)).w(px(3.)).rounded(px(2.)).bg(th.acc)))
                .child(
                    div()
                        .w(px(150.))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(12.5)).font_weight(MEDIUM).child(format!("{} {}", day(s.started), clock(s.started))))
                        .child(div().text_size(px(11.5)).text_color(th.tx3).child(duration(s.seconds))),
                )
                .child(div().flex_1().min_w(px(160.)).text_size(px(12.5)).truncate().child(configs[i].title()))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(5.))
                        .w(px(120.))
                        .child(mono(12.5, if p.fps.is_some() { th.tx1 } else { th.tx4 }).child(fps))
                        .child(div().text_size(px(10.5)).text_color(th.tx3).child("FPS")),
                )
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(5.))
                        .w(px(80.))
                        .when_some(base, |d, b| d.child(mono(12.5, th.warn).child(b)).child(div().text_size(px(10.5)).text_color(th.tx3).child("BASE"))),
                )
                .child(div().w(px(84.)).flex().justify_end().when(picked.contains(&i) && !on, |d| d.child(crate::ui::badge(th, "compared", false))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav.focus(&this.layout, Target::Body(i));
                    if this.perf.sel != i {
                        this.perf.sel = i;
                        this.perf.compare = None;
                    }
                    cx.notify();
                }))
        });
        let total = self.perf.summaries.ready().map(|v| v.len()).unwrap_or(0);
        card(th)
            .p(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(6.))
                    .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child("SESSIONS"))
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child(if total > list.len() { format!("latest {} of {total}", list.len()) } else { format!("{total}") }))
                    .child(div().flex_1())
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child("A summary of every session, ~1 KB each")),
            )
            .children(rows)
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------
    // HUD

    /// The preset where it will sit, over the game's art: placement, opacity and scale as
    /// set. Values are the running game's (else `—`); rows a mod provides show only when
    /// this game's numbers have them, as in game. The layer draws the graph itself.
    fn render_hud_preview(&mut self, th: &Theme, g: &GameRow, c: &quiver_api::perf::HudConfig, cx: &mut Context<Self>) -> AnyElement {
        use quiver_api::perf::Anchor;
        let (w, h) = (300., 188.);
        let art = self.art_for(g, crate::art::ArtSlot::Hero, crate::art::Variant::Original, cx);
        let live = self.perf.live.as_ref().filter(|l| l.product == g.id()).and_then(|l| l.last.clone());
        let latest = self.perf.summaries.ready().and_then(|v| v.first()).filter(|_| self.perf.summaries_for == g.id()).map(|s| s.perf.clone());
        let provided = |metric: &str| match metric {
            "fps.base" => live.as_ref().map(|u| crate::ui::perf::live_has(u, "fps.base")).unwrap_or(latest.as_ref().is_some_and(|p| p.fps_base.is_some())),
            "latency.render" => live.as_ref().map(|u| crate::ui::perf::live_has(u, "latency.render")).unwrap_or(latest.as_ref().is_some_and(|p| p.latency_ms.is_some())),
            _ => true,
        };
        let k = c.scale.clamp(0.75, 2.5);
        let fs = 9.5 * k;
        let label_c = th.tx3;
        let value = |metric: &str| live.as_ref().and_then(|u| crate::ui::perf::live_value(u, metric)).map(|v| if v.unit == "fps" { v.text.clone() } else { v.display() }).unwrap_or_else(|| "—".into());
        let rows: Vec<AnyElement> = preset_rows(&c.layout)
            .into_iter()
            .filter_map(|row| {
                let cells: Vec<AnyElement> = row
                    .into_iter()
                    .filter(|cell| cell.is_none_or(|(_, m)| provided(m)))
                    .map(|cell| match cell {
                        None => div().w(px(110. * k)).h(px(14. * k)).flex().items_center().child(div().w_full().h(px(1.)).bg(th.tx3.opacity(0.5))).into_any_element(),
                        Some((label, metric)) => {
                            let color = if metric == "fps.base" { th.warn } else { th.tx1 };
                            div()
                                .flex()
                                .items_baseline()
                                .gap(px(3. * k))
                                .child(mono(fs * if metric == "fps" { 1.35 } else { 1. }, color).font_weight(BOLD).child(value(metric)))
                                .child(div().text_size(px(fs * 0.75)).font_weight(BOLD).text_color(label_c).child(label))
                                .into_any_element()
                        }
                    })
                    .collect();
                (!cells.is_empty()).then(|| div().flex().items_baseline().gap(px(7. * k)).children(cells).into_any_element())
            })
            .collect();
        let hud = div()
            .absolute()
            .flex()
            .flex_col()
            .gap(px(3. * k))
            .px(px(6. * k))
            .py(px(4. * k))
            .rounded(px(5. * k))
            .bg(gpui::black().opacity(c.background.clamp(0., 1.)))
            .border_1()
            .border_color(gpui::white().opacity(0.08))
            .children(rows);
        let m = 8.;
        let hud = match c.anchor {
            Anchor::TopLeft => hud.top(px(m)).left(px(m)),
            Anchor::Top => hud.top(px(m)).left(px(w / 2. - 40.)),
            Anchor::TopRight => hud.top(px(m)).right(px(m)),
            Anchor::Left => hud.top(px(h / 2. - 12.)).left(px(m)),
            Anchor::Right => hud.top(px(h / 2. - 12.)).right(px(m)),
            Anchor::BottomLeft => hud.bottom(px(m)).left(px(m)),
            Anchor::Bottom => hud.bottom(px(m)).left(px(w / 2. - 40.)),
            Anchor::BottomRight => hud.bottom(px(m)).right(px(m)),
        };
        let bg: AnyElement = match &art {
            crate::ui::ArtState::Ready(a) => crate::ui::cover(a, w, h, (0.5, 0.4)),
            _ => div().absolute().inset_0().bg(th.ink3).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().relative().w(px(w)).h(px(h)).rounded(px(8.)).overflow_hidden().border_1().border_color(th.line2).child(bg).child(hud))
            .child(div().text_size(px(11.5)).text_color(th.tx3).child(if live.is_some() {
                "This game's numbers right now."
            } else {
                "Numbers appear while the game runs; the line is the frame-time graph."
            }))
            .into_any_element()
    }

    fn render_perf_hud(&mut self, th: &Theme, g: &GameRow, scope: &HudScopeRef, sessions: usize, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let base = sessions.min(PERF_ROWS);
        let pick = self.perf_scope_pick(id);
        let profiles = if self.gp.id == id { self.gp.profiles() } else { vec![] };
        let scope_focused = self.focused(Target::Body(base));
        let mut picks: Vec<(ScopePick, String)> = profiles.iter().map(|p| (ScopePick::Profile(p.id.clone()), format!("Profile: {}", p.name))).collect();
        picks.push((ScopePick::Game, "This game".into()));
        picks.push((ScopePick::Global, "All games".into()));
        let seg = div()
            .flex()
            .flex_wrap()
            .gap(px(3.))
            .p(px(3.))
            .rounded(px(9.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line2)
            .children(picks.into_iter().enumerate().map(|(k, (p, label))| {
                let on = p == pick;
                let hover = th.ink3;
                div()
                    .id(("hud-scope", k))
                    .px(px(11.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .text_size(px(12.))
                    .font_weight(SEMIBOLD)
                    .cursor_pointer()
                    .text_color(if on { th.acc_ink } else { th.tx2 })
                    .when(on, |d| d.bg(th.acc))
                    .when(!on, |d| d.hover(move |s| s.bg(hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.perf.scope.insert(id, p.clone());
                        cx.notify();
                    }))
            }));
        let scope_text = match &pick {
            ScopePick::Game => "Settings for this game; a profile's own settings win over them.".to_owned(),
            ScopePick::Profile(_) => "Only while this profile runs; unset ones come from the game.".to_owned(),
            ScopePick::Global => "Every game's defaults; games and profiles override them.".to_owned(),
        };
        let head = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(16.))
            .gap_y(px(8.))
            .py(px(10.))
            .px(px(6.))
            .mx(px(-6.))
            .rounded(px(8.))
            .focus_ring(scope_focused, th, 8.)
            .child(div().w(px(190.)).flex_none().text_size(px(12.5)).font_weight(MEDIUM).child("Applies to"))
            .child(div().flex().flex_col().gap(px(4.)).child(seg).child(div().text_size(px(11.5)).text_color(th.tx3).child(scope_text)))
            .into_any_element();
        let focus_of = |f: HudField| HUD_FIELDS.iter().position(|x| *x == f).map(|k| base + 1 + k);
        let focused: Vec<bool> = HUD_FIELDS.iter().map(|f| focus_of(*f).is_some_and(|i| self.focused(Target::Body(i)))).collect();
        let is_focused = move |f: HudField| HUD_FIELDS.iter().position(|x| *x == f).is_some_and(|k| focused[k]);
        let form = self.render_hud_form(th, scope, Some(head), &is_focused, cx);
        let effective = self.hud_effective(scope).cloned();
        let status = self.render_hud_status(th, id, effective.as_ref());
        let preview = effective.as_ref().map(|c| self.render_hud_preview(th, g, c, cx));
        let keys = self.render_hud_keys(th);
        let enabled = effective.as_ref().is_some_and(|c| c.enabled);
        let cli = match &pick {
            ScopePick::Global => format!("quiver hud {}", if enabled { "disable" } else { "enable" }),
            ScopePick::Game => format!("quiver hud {} {id}", if enabled { "disable" } else { "enable" }),
            ScopePick::Profile(p) => format!("quiver hud {} {id} --profile {p}", if enabled { "disable" } else { "enable" }),
        };
        let sub_card = |title: &str, body: AnyElement| {
            div()
                .flex_1()
                .min_w(px(280.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(14.))
                .rounded(px(10.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line1)
                .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(th.tx3).child(title.to_uppercase()))
                .child(body)
        };
        card(th)
            .px(px(16.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(8.))
                    .gap_y(px(4.))
                    .pt(px(12.))
                    .pb(px(6.))
                    .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child("Performance HUD"))
                    .child(div().text_color(th.tx3).text_size(px(12.)).child("Pinned on the game while you play: Quiver's own, instead of MangoHud."))
                    .child(div().flex_1())
                    .when(self.perf.saving, |d| d.child(div().text_size(px(11.5)).text_color(th.tx3).child("Saving…"))),
            )
            .child(form)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.))
                    .pt(px(12.))
                    .when_some(preview, |d, p| d.child(sub_card("Preview", p)))
                    .child(sub_card("Presenter", status))
                    .child(sub_card("Keys", keys)),
            )
            .child(div().pt(px(12.)).flex().child(command_line(th, SharedString::from(cli))))
            .into_any_element()
    }
}

use super::page::Load;

/// Rows of a preset in the preview: (label, metric) cells; `None` is the frame-time graph.
fn preset_rows(layout: &str) -> Vec<Vec<Option<(&'static str, &'static str)>>> {
    let fps = Some(("FPS", "fps"));
    let base = Some(("BASE", "fps.base"));
    match layout {
        "fps" => vec![vec![fps]],
        "compact" => vec![vec![fps, base, Some(("MS", "frametime")), Some(("LAT", "latency.render")), Some(("GPU", "gpu.load")), Some(("CPU", "cpu.load"))]],
        "standard" => vec![vec![fps, base], vec![None], vec![Some(("LAT", "latency.render")), Some(("GPU", "gpu.load"))], vec![Some(("CPU", "cpu.load"))]],
        "full" => vec![
            vec![fps, Some(("1%", "fps.p1low")), base],
            vec![None],
            vec![Some(("LAT", "latency.render")), Some(("MS", "frametime"))],
            vec![Some(("GPU", "gpu.load")), Some(("°", "gpu.temp")), Some(("W", "gpu.power"))],
            vec![Some(("CPU", "cpu.load")), Some(("°", "cpu.temp"))],
        ],
        "clock" => vec![vec![Some(("TIME", "clock.time")), Some(("SESSION", "session.time"))]],
        _ => vec![vec![fps], vec![None]],
    }
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::mods::ResolvedMod;

    fn summary(days: i64, profile: Option<&str>) -> SessionSummary {
        SessionSummary {
            started: chrono::Utc::now() - chrono::Duration::days(days),
            seconds: 3600,
            profile: profile.map(str::to_owned),
            perf: PerfSummary { fps: Some(FpsStats { mean: 100., p1low: Some(80.), p01low: None }), ..Default::default() },
        }
    }

    #[test]
    fn configs_and_automatic_comparison() {
        let list = [summary(1, Some("p1")), summary(2, Some("p1")), summary(3, None), summary(4, Some("p1"))];
        let m = |v: &str| ResolvedMod { mod_id: "m".into(), name: "DLSS".into(), version: v.into(), label: v.into(), versions: 2, ..Default::default() };
        let history: Vec<PlaySession> = list
            .iter()
            .enumerate()
            .map(|(i, s)| PlaySession {
                started: s.started,
                ended: s.started,
                seconds: s.seconds,
                settings_revision: 1,
                profile: s.profile.clone(),
                profile_revision: None,
                mods: if s.profile.is_some() { vec![m(if i < 2 { "v3" } else { "v2" })] } else { vec![] },
            })
            .collect();
        let profiles = vec![Profile { id: "p1".into(), name: "DLSS".into(), ..Default::default() }];
        let c: Vec<RunConfig> = list.iter().map(|s| run_config(s, &history, &profiles)).collect();
        assert_eq!(c[0].title(), "DLSS · DLSS v3");
        assert_eq!(c[2].title(), "Vanilla");
        assert_eq!(c[0].key, c[1].key);
        assert_ne!(c[0].key, c[3].key, "another mod version is another configuration");
        let usable = vec![true; 4];
        assert_eq!(auto_compare(&c, &usable, 0), [3, 2], "the other mod version, then vanilla");
        assert_eq!(auto_compare(&c, &usable, 2), [0, 3], "from vanilla: the latest of each other");
        let same: Vec<RunConfig> = vec![c[0].clone(), c[1].clone()];
        assert_eq!(auto_compare(&same, &[true, true], 0), [1], "else the session before");
        assert!(auto_compare(&same, &[true, false], 0).is_empty());
    }

    #[test]
    fn deltas() {
        assert_eq!(delta_text(delta_pct(116., 112.).unwrap()), "+4%");
        assert_eq!(delta_text(delta_pct(29., 31.).unwrap()), "−6%");
        assert_eq!(delta_text(0.2), "±0%");
        assert!(delta_pct(1., 0.).is_none());
        assert_eq!(group_digits(1234567), "1,234,567");
        assert_eq!(group_digits(999), "999");
    }
}
