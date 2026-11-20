//! The Session and Achievements sections (design/SIGHT.md §3.3; the `SightSession` and
//! `SightAchievements` mock-ups): their data beyond the session itself, their rows, and their
//! card layouts. The panel's shell (`view.rs`) hands them the body; everything they change
//! goes through the same rows and link requests as the other sections.

pub mod achievements;
pub mod prefs;
pub mod session;

use chrono::{DateTime, Local, Utc};
use gpui::{Div, Hsla, IntoElement, ParentElement, Styled, div, px};
use quiver_ui::components::{MEDIUM, SEMIBOLD};
use quiver_ui::options::Tone;
use quiver_ui::theme::Theme;

/// "2 min ago", "1 h ago", "yesterday", "3 Nov".
pub fn ago(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = (now - t).num_seconds().max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => {
            let (d, n) = (t.with_timezone(&Local).date_naive(), now.with_timezone(&Local).date_naive());
            if n.pred_opt() == Some(d) { "yesterday".into() } else { t.with_timezone(&Local).format("%-d %b").to_string() }
        }
    }
}

/// "21:02 today", "21:02 yesterday", "3 Nov 2026".
pub fn when(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (lt, ln) = (t.with_timezone(&Local), now.with_timezone(&Local));
    let (d, n) = (lt.date_naive(), ln.date_naive());
    if d == n {
        format!("{} today", lt.format("%H:%M"))
    } else if n.pred_opt() == Some(d) {
        format!("{} yesterday", lt.format("%H:%M"))
    } else {
        lt.format("%-d %b %Y").to_string()
    }
}

/// "1 h 12 m", "12 m"; from 10 hours, whole hours ("88 h").
pub fn duration(secs: u64) -> String {
    let m = secs / 60;
    match m {
        0..=59 => format!("{m} m"),
        60..=599 => format!("{} h {} m", m / 60, m % 60),
        _ => format!("{} h", m / 60),
    }
}

pub fn tone_color(th: &Theme, t: Tone) -> Hsla {
    match t {
        Tone::Ok => th.ok,
        Tone::Warn => th.warn,
        Tone::Bad => th.bad,
        Tone::Accent => th.acc_hi,
        Tone::Muted => th.tx2,
    }
}

/// The mock-ups' card: a raised panel with a border.
pub fn card(th: &Theme) -> Div {
    div().flex().flex_col().p(px(18.)).rounded(px(14.)).bg(th.ink2).border_1().border_color(th.line1)
}

/// A card's small uppercase heading.
pub fn heading(th: &Theme, t: &str) -> Div {
    div().pb(px(10.)).text_size(px(12.)).font_weight(SEMIBOLD).text_color(th.tx3).child(t.to_uppercase())
}

/// A rounded chip (profile, Proton build, a mod's version).
pub fn chip(th: &Theme, t: impl Into<gpui::SharedString>, tone: Option<Tone>) -> Div {
    let (fg, bg) = match tone {
        Some(Tone::Ok) => (th.ok, th.ok_soft),
        Some(Tone::Warn) => (th.warn, th.warn_soft),
        Some(Tone::Bad) => (th.bad, th.bad_soft),
        Some(Tone::Accent) => (th.acc_hi, th.acc_soft),
        _ => (th.tx2, th.ink4),
    };
    div().flex_none().flex().items_center().h(px(24.)).px(px(9.)).rounded(px(12.)).bg(bg).text_color(fg).text_size(px(12.)).font_weight(SEMIBOLD).child(t.into())
}

/// A card's row: a label (or value) on the left, a value or chip on the right, a hairline
/// above.
pub fn line(th: &Theme, left: impl IntoElement, right: impl IntoElement) -> Div {
    div().flex().items_center().justify_between().gap(px(12.)).py(px(9.)).border_t_1().border_color(th.line1).text_size(px(14.)).child(left).child(right)
}

pub fn key_text(th: &Theme, t: impl Into<gpui::SharedString>) -> Div {
    div().min_w_0().truncate().text_color(th.tx2).child(t.into())
}

pub fn value_text(th: &Theme, t: impl Into<gpui::SharedString>) -> Div {
    div().min_w_0().truncate().text_color(th.tx1).font_weight(MEDIUM).child(t.into())
}

/// A progress bar, `frac` of it filled.
pub fn bar(track: Hsla, fill: Hsla, frac: f64, h: f32) -> Div {
    let w = (frac.clamp(0.0, 1.0) * 100.0) as f32;
    div().h(px(h)).w_full().rounded(px(h / 2.)).bg(track).child(div().h(px(h)).w(gpui::relative(w / 100.)).rounded(px(h / 2.)).bg(fill))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_in_words() {
        let now: DateTime<Utc> = Local::now().with_timezone(&Utc);
        assert_eq!(ago(now - chrono::Duration::seconds(20), now), "just now");
        assert_eq!(ago(now - chrono::Duration::minutes(2), now), "2 min ago");
        assert_eq!(ago(now - chrono::Duration::minutes(130), now), "2 h ago");
        assert_eq!(duration(72 * 60), "1 h 12 m");
        assert_eq!(duration(12 * 60 + 5), "12 m");
        assert_eq!(duration(88 * 3600 + 120), "88 h");
        assert!(when(now, now).ends_with("today"));
        assert!(when(now - chrono::Duration::days(1), now).ends_with("yesterday"));
        assert!(when(now - chrono::Duration::days(40), now).chars().last().unwrap().is_ascii_digit(), "a date with its year");
    }
}
