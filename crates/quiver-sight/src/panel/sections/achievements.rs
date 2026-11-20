//! The Achievements section (the `SightAchievements` mock-up): a progress card (the count, a
//! bar, "N unlocked this session · M in progress"), where the unlocks are reported (Ubisoft's
//! report journal; Epic's and Steam's own), what happens on an unlock (a setting the toast
//! lane reads), and the list: filters on LT / RT (All, Unlocked, Locked, Recent), hidden ones
//! masked or shown, per-achievement progress bars where the store keeps progress, icons where
//! quiverd has them as files, and "Unlocked 21:02 today".
//!
//! The data is `SightRequest::Achievements` (any store; Ubisoft's from what Quiver already
//! knows, nothing sent) and `SightRequest::Icons`; a quiverd from before falls back to the
//! game's store page (`GameDetail`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{AnyElement, Context, IntoElement, ObjectFit, ParentElement, SharedString, Styled, StyledImage, div, img, prelude::*, px};
use quiver_api::achievements::ReportState;
use quiver_api::sight::SightRequest;
use quiver_api::sight::session::{Reporting, SessionAchievements, reporting_words};
use quiver_api::{Client, Request, StoreId};
use quiver_ui::assets::DISPLAY_FONT;
use quiver_ui::components::{BOLD, FocusExt, Icon, MEDIUM, SEMIBOLD, glyph, icon};
use quiver_ui::input::Input;
use quiver_ui::theme::Theme;

use super::prefs::{OnUnlock, Pref};
use super::{bar, card, heading, when};
use crate::panel::state::{Ach, Achs, Load, PanelData, Row, SessionView};
use crate::panel::view::PanelView;

/// The list's filter (LT / RT).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Unlocked,
    Locked,
    /// Unlocked in the last [`RECENT_DAYS`] days, newest first.
    Recent,
}

pub const RECENT_DAYS: i64 = 7;

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Unlocked, Filter::Locked, Filter::Recent];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Unlocked => "Unlocked",
            Filter::Locked => "Locked",
            Filter::Recent => "Recent",
        }
    }

    pub fn step(self, dir: i32) -> Filter {
        crate::panel::state::cycle(&Filter::ALL, &self, dir)
    }
}

/// The progress card's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Progress {
    pub this_session: usize,
    pub in_progress: usize,
    pub fraction: f64,
}

pub fn progress(a: &Achs, started: Option<DateTime<Utc>>) -> Progress {
    Progress {
        this_session: started.map_or(0, |t0| a.list.iter().filter(|x| x.unlocked && x.unlocked_at.is_some_and(|t| t >= t0)).count()),
        in_progress: a.list.iter().filter(|x| !x.unlocked && x.progress.is_some_and(|p| p.in_progress())).count(),
        fraction: if a.total > 0 { a.unlocked as f64 / a.total as f64 } else { 0.0 },
    }
}

/// The list's achievements under `f` (indices into `a.list`, which is unlocked-newest-first,
/// then locked). Locked ones that are under way come first among the locked.
pub fn visible(a: &Achs, f: Filter, now: DateTime<Utc>) -> Vec<usize> {
    let mut locked: Vec<usize> = a.list.iter().enumerate().filter(|(_, x)| !x.unlocked).map(|(i, _)| i).collect();
    locked.sort_by(|&i, &j| {
        let p = |k: usize| a.list[k].progress.filter(|p| p.in_progress()).map_or(-1.0, |p| p.fraction());
        p(j).partial_cmp(&p(i)).unwrap_or(std::cmp::Ordering::Equal).then(i.cmp(&j))
    });
    let unlocked = a.list.iter().enumerate().filter(|(_, x)| x.unlocked).map(|(i, _)| i);
    match f {
        Filter::All => unlocked.chain(locked).collect(),
        Filter::Unlocked => unlocked.collect(),
        Filter::Locked => locked,
        Filter::Recent => unlocked.filter(|&i| a.list[i].unlocked_at.is_some_and(|t| (now - t).num_days() < RECENT_DAYS)).collect(),
    }
}

/// A hidden achievement still locked shows as "Hidden achievement" unless hidden ones are
/// shown or this one was revealed.
pub fn masked(a: &Ach, show_hidden: bool, revealed: bool) -> bool {
    a.hidden && !a.unlocked && !show_hidden && !revealed
}

/// The section's rows, in focus order: the On unlock setting, the filter, the hidden ones'
/// setting, then the list.
pub fn rows(d: &PanelData, filter: Filter, now: DateTime<Utc>) -> Vec<Row> {
    let mut out = Vec::new();
    match &d.achievements {
        Load::Loading => out.push(Row::Note("Loading achievements…".into())),
        Load::Failed(e) => out.push(Row::Note(format!("Couldn't load achievements: {e}"))),
        Load::Unavailable(w) => out.push(Row::Note(w.clone())),
        Load::Ready(_) => {}
    }
    out.push(Row::Pref(Pref::OnUnlock));
    if let Some(a) = d.achievements.ready() {
        out.push(Row::AchFilter);
        out.push(Row::Pref(Pref::Hidden));
        out.extend(visible(a, filter, now).into_iter().map(Row::Achievement));
    }
    out
}

/// The position of row `r` among the list's rows (for scrolling the list to it).
pub fn list_pos(rows: &[Row], r: usize) -> Option<usize> {
    matches!(rows.get(r), Some(Row::Achievement(_))).then(|| rows[..r].iter().filter(|x| matches!(x, Row::Achievement(_))).count())
}

/// The icon to show for an achievement: its unlocked one, or the greyed one while locked.
fn icon_url(a: &Ach) -> Option<&String> {
    if a.unlocked { a.icon.as_ref().or(a.icon_gray.as_ref()) } else { a.icon_gray.as_ref().or(a.icon.as_ref()) }
}

/// [`SessionAchievements`] as the panel's [`Achs`].
pub fn from_report(r: SessionAchievements) -> Load<Achs> {
    let Some(a) = r.achievements.filter(|a| !a.list.is_empty()) else {
        let why = if r.errors.is_empty() { "No achievements known for this game (Quiver hasn't read them from the store yet).".to_owned() } else { r.errors.join("; ") };
        return Load::Unavailable(why);
    };
    let list = a
        .list
        .into_iter()
        .map(|x| Ach { id: x.id, name: x.name, description: x.description, hidden: x.hidden, unlocked: x.unlocked, unlocked_at: x.unlocked_at, icon: x.icon, icon_gray: x.icon_gray, progress: x.progress })
        .collect();
    let mut achs = Achs::sorted(list);
    achs.total = achs.total.max(a.total);
    achs.reporting = Some(r.reporting);
    achs.reports = r.reports;
    achs.note = (!r.errors.is_empty()).then(|| r.errors.join("; "));
    Load::Ready(achs)
}

/// Loads the game's achievements (with icons, where quiverd has them within a few seconds).
pub async fn load(client: &Client, s: &SessionView) -> Load<Achs> {
    let mut l = match client.call::<SessionAchievements>(Request::Sight(SightRequest::Achievements { game: s.game })).await {
        Ok(r) => from_report(r),
        // A quiverd from before: the store page's list.
        Err(e) => {
            tracing::debug!("sight achievements: {e:#}");
            crate::panel::data::achievements(client, s.game).await
        }
    };
    if let Load::Ready(a) = &mut l {
        let mut urls: Vec<String> = a.list.iter().filter_map(icon_url).cloned().collect();
        urls.sort();
        urls.dedup();
        if !urls.is_empty() {
            let call = client.call::<Vec<Option<PathBuf>>>(Request::Sight(SightRequest::Icons { urls: urls.clone() }));
            if let Ok(Ok(files)) = tokio::time::timeout(Duration::from_secs(5), call).await {
                a.icons = urls.into_iter().zip(files).filter_map(|(u, f)| Some((u, f?))).collect();
            }
        }
    }
    l
}

// ---- Rendering -------------------------------------------------------------------------------

pub fn body(v: &PanelView, th: &Theme, rows: &[Row], focused: Option<usize>, cx: &mut Context<PanelView>) -> AnyElement {
    let now = Utc::now();
    let started = v.data.session.ready().and_then(|s| s.started);
    let a = v.data.achievements.ready();
    let pos = |r: &Row| rows.iter().position(|x| x == r);
    let mut left = div().w(px(330.)).flex_none().flex().flex_col().gap(px(16.));
    for r in rows {
        if let Row::Note(t) = r {
            left = left.child(div().text_size(px(13.)).text_color(th.tx3).child(t.clone()));
        }
    }
    if let Some(a) = a {
        let p = progress(a, started);
        let mut sub = format!("{} unlocked this session", p.this_session);
        if p.in_progress > 0 {
            sub.push_str(&format!(" · {} in progress", p.in_progress));
        }
        left = left.child(
            card(th)
                .p(px(20.))
                .child(heading(th, "Progress"))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(8.))
                        .child(div().font_family(DISPLAY_FONT).text_size(px(44.)).font_weight(BOLD).text_color(th.tx1).child(a.unlocked.to_string()))
                        .child(div().pb(px(8.)).text_size(px(16.)).text_color(th.tx2).child(format!("of {} unlocked", a.total))),
                )
                .child(div().mt(px(12.)).mb(px(16.)).child(bar(th.ink4, th.warn, p.fraction, 8.)))
                .child(div().text_size(px(14.)).text_color(th.tx2).child(sub)),
        );
        let reporting = a.reporting.clone().unwrap_or_default();
        let (title, line) = reporting_words(&reporting, &a.reports, a.unlocked);
        left = left.child(
            card(th)
                .p(px(20.))
                .child(heading(th, &title))
                .child(div().pb(px(6.)).text_size(px(14.)).text_color(th.tx1).child(line))
                .child(div().text_size(px(13.)).text_color(th.tx3).line_clamp(3).child(reporting_note(&reporting, a))),
        );
    }
    if let Some(i) = pos(&Row::Pref(Pref::OnUnlock)) {
        left = left.child(on_unlock(v, th, i, focused == Some(i), cx));
    }
    if a.is_none() {
        return div().flex().gap(px(20.)).w_full().child(left).into_any_element();
    }
    // The list's card: the filter bar, then the list in its own scroll.
    let filter_i = pos(&Row::AchFilter);
    let hidden_i = pos(&Row::Pref(Pref::Hidden));
    let mut chips = div().id("ach-filters").flex().items_center().gap(px(8.)).p(px(4.)).rounded(px(20.)).child(glyph(th, "LT", false));
    for f in Filter::ALL {
        let on = v.nav.ach_filter == f;
        chips = chips.child(
            div()
                .id(SharedString::from(format!("filter-{}", f.label())))
                .h(px(32.))
                .px(px(13.))
                .flex()
                .items_center()
                .rounded(px(16.))
                .border_1()
                .border_color(if on { th.tx1 } else { th.line2 })
                .bg(if on { th.tx1 } else { gpui::transparent_black() })
                .text_color(if on { th.ink0 } else { th.tx2 })
                .text_size(px(13.))
                .font_weight(SEMIBOLD)
                .cursor_pointer()
                .child(f.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav.ach_filter = f;
                    cx.notify();
                })),
        );
    }
    chips = chips.child(glyph(th, "RT", false));
    let filter_focused = filter_i.is_some() && focused == filter_i;
    let chips = chips.focus_ring(filter_focused, th, 20.).when(filter_focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)));
    let hidden_focused = hidden_i.is_some() && focused == hidden_i;
    let show = v.data.prefs.show_hidden;
    let hidden_chip = div()
        .id("ach-hidden")
        .h(px(32.))
        .px(px(13.))
        .flex()
        .items_center()
        .rounded(px(16.))
        .border_1()
        .border_color(th.line2)
        .text_color(th.tx1)
        .text_size(px(13.))
        .font_weight(SEMIBOLD)
        .cursor_pointer()
        .child(if show { "Shown" } else { "Masked" })
        .focus_ring(hidden_focused, th, 16.)
        .when(hidden_focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)))
        .when_some(hidden_i, |d, i| {
            d.on_click(cx.listener(move |this, _, _, cx| {
                let rows = this.rows();
                this.nav.focus_row(&rows, i);
                this.input(Input::Activate, cx);
            }))
        });
    let bar_row = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(th.line1)
        .child(chips)
        .child(div().flex_1())
        .child(div().text_size(px(13.)).text_color(th.tx2).child("Hidden achievements"))
        .child(hidden_chip);
    let items: Vec<AnyElement> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| if let Row::Achievement(k) = r { Some(item(v, th, i, *k, focused == Some(i), now, cx)) } else { None })
        .collect();
    let empty = items.is_empty();
    let list = div()
        .id("ach-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&v.ach_scroll)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .children(items)
        .when(empty, |d| d.child(div().p(px(8.)).text_size(px(13.)).text_color(th.tx3).child(format!("Nothing under {}.", v.nav.ach_filter.label()))));
    let right = div().flex_1().min_w_0().h_full().flex().flex_col().rounded(px(14.)).bg(th.ink2).border_1().border_color(th.line1).child(bar_row).child(list);
    div().flex().gap(px(20.)).w_full().h_full().child(left).child(right).into_any_element()
}

fn reporting_note(r: &Reporting, a: &Achs) -> String {
    if let Some(n) = &a.note {
        return n.clone();
    }
    match r.store {
        Some(StoreId::Ubisoft) => match a.reports.values().filter(|s| matches!(s, ReportState::Pending | ReportState::Failed)).count() {
            0 => "Unlocks the game sent before are already on Ubisoft's side.".into(),
            _ => "Quiver sends what the game earned when Ubisoft answers; nothing is lost while it waits.".into(),
        },
        Some(StoreId::Epic) => "Quiver reads them from Epic after the game reports them.".into(),
        Some(StoreId::Steam) => "Quiver reads them from Steam's own files.".into(),
        _ => String::new(),
    }
}

fn on_unlock(v: &PanelView, th: &Theme, i: usize, focused: bool, cx: &mut Context<PanelView>) -> AnyElement {
    let o = v.data.prefs.on_unlock;
    let game = v.data.session.ready().map(|s| s.name.clone()).unwrap_or_else(|| "the game".into());
    let sub = match o {
        OnUnlock::ToastScreenshot => format!("Screenshots go to Pictures › Quiver › {game}."),
        OnUnlock::Toast => "A toast over the game; no screenshot.".into(),
        OnUnlock::Off => "No toast; unlocks are still listed here.".into(),
    };
    card(th)
        .id(SharedString::from(format!("row-{i}")))
        .p(px(20.))
        .bg(if focused { th.ink3 } else { th.ink2 })
        .cursor_pointer()
        .focus_ring(focused, th, 14.)
        .when(focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)))
        .child(heading(th, "On unlock"))
        .child(
            div()
                .pb(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(14.))
                .text_color(th.tx1)
                .when(focused, |d| d.child(div().text_color(th.tx3).child("◀")))
                .child(o.label())
                .when(focused, |d| d.child(div().text_color(th.tx3).child("▶"))),
        )
        .child(div().text_size(px(13.)).text_color(th.tx3).child(sub))
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            this.input(Input::Activate, cx);
        }))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn item(v: &PanelView, th: &Theme, i: usize, k: usize, focused: bool, now: DateTime<Utc>, cx: &mut Context<PanelView>) -> AnyElement {
    let Some(a) = v.data.achievements.ready().and_then(|x| x.list.get(k)) else { return div().into_any_element() };
    let icons: &BTreeMap<String, PathBuf> = &v.data.achievements.ready().map(|x| &x.icons).cloned().unwrap_or_default();
    let is_masked = masked(a, v.data.prefs.show_hidden, v.nav.revealed.contains(&k));
    let (name, desc) = if is_masked { ("Hidden achievement".to_owned(), "Keep playing to reveal it · X reveals it".to_owned()) } else { (a.name.clone(), a.description.clone()) };
    let file = if is_masked { None } else { icon_url(a).and_then(|u| icons.get(u)).cloned() };
    let gold = th.warn;
    let badge: AnyElement = match file {
        Some(p) => div().flex_none().size(px(52.)).rounded(px(10.)).overflow_hidden().bg(th.ink4).child(img(p).size(px(52.)).rounded(px(10.)).object_fit(ObjectFit::Cover).when(!a.unlocked, |i| i.grayscale(true))).into_any_element(),
        None => div()
            .flex_none()
            .size(px(52.))
            .rounded(px(10.))
            .bg(if a.unlocked { gold.opacity(0.16) } else { th.ink3 })
            .flex()
            .items_center()
            .justify_center()
            .child(icon(
                if is_masked {
                    Icon::Info
                } else if a.unlocked {
                    Icon::Trophy
                } else {
                    Icon::Lock
                },
                26.,
                if a.unlocked { gold } else { th.tx3 },
            ))
            .into_any_element(),
    };
    let right = if a.unlocked {
        a.unlocked_at.map(|t| format!("Unlocked {}", when(t, now))).unwrap_or_else(|| "Unlocked".into())
    } else {
        "Locked".into()
    };
    let prog = a.progress.filter(|p| !a.unlocked && p.current > 0.0 && !is_masked);
    div()
        .id(SharedString::from(format!("row-{i}")))
        .flex()
        .items_center()
        .gap(px(14.))
        .px(px(14.))
        .py(px(12.))
        .rounded(px(12.))
        .bg(if focused { th.ink3 } else { th.ink1 })
        .border_1()
        .border_color(if focused { th.line2 } else { gpui::transparent_black() })
        .cursor_pointer()
        .focus_ring(focused, th, 12.)
        .when(focused, |d| d.child(crate::panel::view::focus_mark(&v.marks)))
        .child(badge)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(15.)).font_weight(SEMIBOLD).text_color(if a.unlocked { th.tx1 } else { th.tx2 }).truncate().child(name))
                .child(div().mt(px(3.)).text_size(px(13.)).text_color(th.tx3).line_clamp(2).child(desc))
                .when_some(prog, |d, p| {
                    d.child(
                        div()
                            .mt(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().max_w(px(280.)).child(bar(th.ink4, th.acc, p.fraction(), 5.)))
                            .child(div().text_size(px(12.)).font_weight(MEDIUM).text_color(th.tx3).child(p.text())),
                    )
                }),
        )
        .child(div().flex_none().text_size(px(12.)).text_color(th.tx3).child(right))
        .on_click(cx.listener(move |this, _, _, cx| {
            let rows = this.rows();
            this.nav.focus_row(&rows, i);
            cx.notify();
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::stores::AchievementProgress;

    fn achs(now: DateTime<Utc>) -> Achs {
        let a = |id: &str, at: Option<i64>, hidden: bool, progress: Option<(f64, f64)>| Ach {
            id: id.into(),
            name: id.into(),
            hidden,
            unlocked: at.is_some(),
            unlocked_at: at.map(|m| now - chrono::Duration::minutes(m)),
            progress: progress.map(|(current, max)| AchievementProgress { current, max, percent: false }),
            ..Default::default()
        };
        let mut a = Achs::sorted(vec![
            a("old", Some(60 * 24 * 30), false, None),
            a("today", Some(12), false, None),
            a("earlier", Some(200), false, None),
            a("seer", None, false, Some((22.0, 50.0))),
            a("almost", None, false, Some((45.0, 50.0))),
            a("secret", None, true, None),
            a("plain", None, false, Some((0.0, 10.0))),
        ]);
        a.total = 10;
        a
    }

    #[test]
    fn filters_and_progress() {
        let now = Utc::now();
        let a = achs(now);
        let names = |f: Filter| visible(&a, f, now).into_iter().map(|i| a.list[i].id.clone()).collect::<Vec<_>>();
        assert_eq!(names(Filter::All), ["today", "earlier", "old", "almost", "seer", "secret", "plain"]);
        assert_eq!(names(Filter::Unlocked), ["today", "earlier", "old"]);
        assert_eq!(names(Filter::Locked), ["almost", "seer", "secret", "plain"], "under way first, furthest along first");
        assert_eq!(names(Filter::Recent), ["today", "earlier"], "the last week");
        let p = progress(&a, Some(now - chrono::Duration::hours(1)));
        assert_eq!((p.this_session, p.in_progress), (1, 2));
        assert!((p.fraction - 0.3).abs() < 1e-9, "3 of 10");
        assert_eq!(progress(&a, None).this_session, 0);
        assert_eq!(Filter::All.step(-1), Filter::Recent);
        assert_eq!(Filter::Recent.step(1), Filter::All);
        assert_eq!(Filter::All.step(0), Filter::Unlocked);
    }

    #[test]
    fn masking() {
        let now = Utc::now();
        let a = achs(now);
        let secret = a.list.iter().find(|x| x.id == "secret").unwrap();
        assert!(masked(secret, false, false));
        assert!(!masked(secret, true, false), "hidden ones shown");
        assert!(!masked(secret, false, true), "revealed");
        let plain = a.list.iter().find(|x| x.id == "plain").unwrap();
        assert!(!masked(plain, false, false));
        let unlocked_hidden = Ach { hidden: true, unlocked: true, ..Default::default() };
        assert!(!masked(&unlocked_hidden, false, false), "an unlocked one is never masked");
        assert_eq!(AchievementProgress { current: 22.0, max: 50.0, percent: false }.text(), "22 / 50");
        assert_eq!(AchievementProgress { current: 44.0, max: 100.0, percent: true }.text(), "44%");
    }

    #[test]
    fn rows_in_focus_order() {
        let now = Utc::now();
        let d = PanelData { achievements: Load::Ready(achs(now)), ..Default::default() };
        let r = rows(&d, Filter::Locked, now);
        assert_eq!(&r[..3], &[Row::Pref(Pref::OnUnlock), Row::AchFilter, Row::Pref(Pref::Hidden)]);
        assert_eq!(r.len(), 3 + 4);
        assert_eq!(list_pos(&r, 3), Some(0));
        assert_eq!(list_pos(&r, 6), Some(3));
        assert_eq!(list_pos(&r, 1), None);
        let loading = rows(&PanelData::default(), Filter::All, now);
        assert!(matches!(&loading[0], Row::Note(_)));
    }

    #[test]
    fn reports_become_the_list() {
        use quiver_api::stores::{Achievement, Achievements};
        let r = SessionAchievements {
            game: None,
            achievements: Some(Achievements {
                unlocked: 1,
                total: 70,
                list: vec![
                    Achievement { id: "1".into(), name: "Hidden Ones".into(), unlocked: true, icon: Some("https://x/1.png".into()), ..Default::default() },
                    Achievement { id: "2".into(), name: "Seer".into(), progress: Some(AchievementProgress { current: 1.0, max: 2.0, percent: false }), ..Default::default() },
                ],
            }),
            reporting: Reporting { store: Some(StoreId::Ubisoft), mode: "on".into() },
            reports: [("1".to_owned(), ReportState::Pending)].into(),
            errors: vec![],
        };
        let Load::Ready(a) = from_report(r) else { panic!() };
        assert_eq!((a.unlocked, a.total), (1, 70), "the store's total, not the list's length");
        assert_eq!(icon_url(&a.list[0]).map(String::as_str), Some("https://x/1.png"));
        assert_eq!(a.reports["1"], ReportState::Pending);
        assert!(matches!(from_report(SessionAchievements::default()), Load::Unavailable(_)));
    }
}
