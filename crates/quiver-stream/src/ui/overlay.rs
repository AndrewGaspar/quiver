//! What the overlay shows, laid out: the stats chip (top-left), the stream menu (centred),
//! the capture hint and toasts (bottom centre). The present thread calls [`build`] whenever it
//! draws; the main thread changes [`OverlayState`].

use std::time::{Duration, Instant};

use super::atlas::{Atlas, Font};
use super::draw::{Painter, Rgba, Vertex, colors::*};
use super::menu::{Menu, ROWS};
use crate::stats::Snapshot;

pub const TOAST_FOR: Duration = Duration::from_secs(4);

#[derive(Debug, Default, Clone)]
pub struct OverlayState {
    pub stats: bool,
    pub snapshot: Snapshot,
    pub menu: Option<Menu>,
    /// A line under the menu: what the keys do.
    pub host_name: String,
    /// Input isn't captured: show how to capture it.
    pub uncaptured_hint: bool,
    pub toasts: Vec<(String, Instant)>,
    /// The first-run hint ("Ctrl+Alt+Shift+Q for the stream menu …"), until this time.
    pub intro_until: Option<Instant>,
    /// No video yet (connecting, or waiting for a keyframe).
    pub waiting: Option<String>,
}

impl OverlayState {
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push((text.into(), Instant::now()));
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    /// Whether anything is on screen that changes over time (so the presenter should redraw
    /// even without new frames).
    pub fn animating(&self, now: Instant) -> bool {
        self.stats || self.menu.is_some() || self.waiting.is_some() || !self.toasts.is_empty() || self.intro_until.is_some_and(|t| t > now)
    }

    pub fn expire(&mut self, now: Instant) {
        self.toasts.retain(|(_, t)| now.duration_since(*t) < TOAST_FOR);
        if self.intro_until.is_some_and(|t| t <= now) {
            self.intro_until = None;
        }
    }
}

/// The menu's rows on screen, for mouse hit-testing: (x, y, w, h) of the first row, the row
/// pitch.
pub fn menu_rows(scale: f32, screen: (f32, f32)) -> (f32, f32, f32, f32, f32) {
    let s = |v: f32| v * scale;
    let w = s(440.0);
    let row_h = s(40.0);
    let head = s(76.0);
    let h = head + row_h * ROWS.len() as f32 + s(56.0);
    let x = ((screen.0 - w) / 2.0).round();
    let y = ((screen.1 - h) / 2.0).round().max(s(16.0));
    (x + s(12.0), y + head, w - s(24.0), row_h, row_h)
}

fn fmt_ms(us: u32) -> String {
    if us == 0 { "–".into() } else { format!("{:.1}", us as f32 / 1000.0) }
}

/// Segments of (text, colour) on one line.
fn line(p: &mut Painter, x: f32, baseline: f32, segs: &[(&str, Rgba, Font)]) -> f32 {
    let mut pen = x;
    for (t, c, f) in segs {
        pen += p.text(pen, baseline, *f, t, *c);
    }
    pen - x
}

fn stats_chip(p: &mut Painter, st: &OverlayState) {
    let s = &st.snapshot;
    let c = &s.client;
    let pad = p.s(10.0);
    let lh = p.s(19.0);
    let (x0, y0) = (p.s(14.0), p.s(14.0));
    let sep = (" · ", TX3, Font::Mono);
    let codec = if s.hdr { format!("{} HDR", s.codec) } else { s.codec.clone() };
    let fps = format!("{:.0}", s.fps);
    let mbps = format!("{:.1}", s.kbps as f32 / 1000.0);
    let size = format!("{}×{}", s.size.0, s.size.1);
    let rtt = fmt_ms(c.rtt_us);
    let loss = format!("{:.1}%", s.loss_percent);
    let jit = fmt_ms(c.jitter_us);
    let net = fmt_ms(c.network_us);
    let dec = fmt_ms(c.decode_us);
    let pres = fmt_ms(c.present_us);
    let total = fmt_ms(c.capture_to_present_us);
    let host = s.host.as_ref().map(|h| (fmt_ms(h.capture_us), fmt_ms(h.encode_us)));
    let audio = format!("{:.0}", s.audio_ms);
    let lines: Vec<Vec<(&str, Rgba, Font)>> = {
        let mut l = vec![
            vec![
                (fps.as_str(), TX1, Font::Mono),
                (" fps", TX3, Font::Mono),
                sep,
                (mbps.as_str(), TX1, Font::Mono),
                (" Mb/s", TX3, Font::Mono),
                sep,
                (codec.as_str(), CYAN, Font::Mono),
                (" ", TX3, Font::Mono),
                (size.as_str(), TX2, Font::Mono),
            ],
            vec![
                ("rtt ", TX3, Font::Mono),
                (rtt.as_str(), TX1, Font::Mono),
                sep,
                ("loss ", TX3, Font::Mono),
                (loss.as_str(), if s.loss_percent > 1.0 { RED } else { TX1 }, Font::Mono),
                sep,
                ("jitter ", TX3, Font::Mono),
                (jit.as_str(), TX1, Font::Mono),
            ],
            vec![
                ("net ", TX3, Font::Mono),
                (net.as_str(), TX1, Font::Mono),
                sep,
                ("decode ", TX3, Font::Mono),
                (dec.as_str(), TX1, Font::Mono),
                sep,
                ("present ", TX3, Font::Mono),
                (pres.as_str(), TX1, Font::Mono),
            ],
            vec![
                ("capture→present ", TX3, Font::Mono),
                (total.as_str(), CYAN, Font::Mono),
                (" ms", TX3, Font::Mono),
                sep,
                ("audio ", TX3, Font::Mono),
                (audio.as_str(), TX1, Font::Mono),
                (" ms", TX3, Font::Mono),
            ],
        ];
        if let Some((cap, enc)) = &host {
            l.push(vec![
                ("host capture ", TX3, Font::Mono),
                (cap.as_str(), TX1, Font::Mono),
                sep,
                ("encode ", TX3, Font::Mono),
                (enc.as_str(), TX1, Font::Mono),
            ]);
        }
        l.push(vec![(s.decoder.as_str(), TX3, Font::Mono), sep, (s.present_mode.as_str(), TX3, Font::Mono)]);
        l
    };
    let width = lines.iter().map(|segs| segs.iter().map(|(t, _, f)| p.measure(*f, t)).sum::<f32>()).fold(0.0, f32::max);
    let h = pad * 2.0 + lh * lines.len() as f32 - p.s(4.0);
    p.rect(x0, y0, width + pad * 2.0, h, p.s(8.0), INK0.alpha(0.82));
    p.rect(x0, y0 + p.s(8.0), p.s(2.0), h - p.s(16.0), p.s(1.0), CYAN);
    let asc = p.atlas.metrics(Font::Mono).ascent;
    for (i, segs) in lines.iter().enumerate() {
        line(p, x0 + pad, y0 + pad + asc + lh * i as f32, segs);
    }
}

fn menu(p: &mut Painter, st: &OverlayState, m: &Menu, screen: (f32, f32)) {
    let (rx, ry, rw, rh, pitch) = menu_rows(p.scale, screen);
    let scale = p.scale;
    let s = |v: f32| v * scale;
    let x = rx - s(12.0);
    let w = rw + s(24.0);
    let y = ry - s(76.0);
    let h = s(76.0) + pitch * ROWS.len() as f32 + s(56.0);
    // Dim the stream behind.
    p.rect(0.0, 0.0, screen.0, screen.1, 0.0, INK0.alpha(0.45));
    p.rect(x, y, w, h, s(14.0), INK2.alpha(0.97));
    p.rect(x + s(1.0), y + s(1.0), w - s(2.0), s(2.0), s(1.0), LINE2);
    // Header: the mark, the title, the host.
    let logo = p.atlas.logo;
    p.image(x + s(18.0), y + s(20.0), &logo, Rgba([255; 4]));
    let tm = p.atlas.metrics(Font::Title);
    p.text(x + s(62.0), y + s(20.0) + tm.ascent, Font::Title, "Stream", TX1);
    let sub = if st.host_name.is_empty() { "Quiver remote play".to_owned() } else { format!("from {}", st.host_name) };
    let sm = p.atlas.metrics(Font::Small);
    p.text(x + s(62.0), y + s(20.0) + tm.line + sm.ascent - s(2.0), Font::Small, &sub, TX3);
    let bm = p.atlas.metrics(Font::Body);
    for (i, row) in ROWS.iter().enumerate() {
        let yy = ry + pitch * i as f32;
        let sel = i == m.selected;
        if sel {
            p.rect(rx, yy + s(2.0), rw, rh - s(4.0), s(8.0), INK4);
            p.rect(rx, yy + s(10.0), s(3.0), rh - s(20.0), s(1.5), CYAN);
        }
        let (label, value) = m.text(*row);
        let danger = matches!(row, super::menu::Row::QuitGame);
        let base = yy + (rh + bm.ascent - bm.descent) / 2.0;
        p.text(
            rx + s(16.0),
            base,
            Font::Body,
            label,
            if danger {
                RED
            } else if sel {
                TX1
            } else {
                TX2
            },
        );
        if !value.is_empty() {
            let choice = Menu::is_choice(*row);
            let vtext = if choice && sel { format!("‹  {value}  ›") } else { value };
            let vw = p.measure(Font::Body, &vtext);
            let col = if danger {
                RED
            } else if choice {
                if sel { CYAN } else { TX2 }
            } else {
                TX3
            };
            p.text(rx + rw - s(16.0) - vw, base, Font::Body, &vtext, col);
        }
    }
    let hint = "↑↓ select · ←→ change · Enter choose · Esc close";
    let pad_hint = "Pad: hold Back+LB+RB for this menu · Guide opens Sight";
    let fy = ry + pitch * ROWS.len() as f32 + s(22.0);
    let hw = p.measure(Font::Small, hint);
    p.text(x + (w - hw) / 2.0, fy, Font::Small, hint, TX3);
    let pw = p.measure(Font::Small, pad_hint);
    p.text(x + (w - pw) / 2.0, fy + sm.line, Font::Small, pad_hint, TX3);
}

fn pill(p: &mut Painter, text: &str, cy: f32, screen: (f32, f32), accent: Rgba) {
    let m = p.atlas.metrics(Font::Body);
    let tw = p.measure(Font::Body, text);
    let (padx, h) = (p.s(18.0), p.s(38.0));
    let w = tw + padx * 2.0;
    let x = ((screen.0 - w) / 2.0).round();
    let y = (cy - h / 2.0).round();
    p.rect(x, y, w, h, h / 2.0, INK0.alpha(0.86));
    p.rect(x + p.s(12.0), y + h / 2.0 - p.s(3.0), p.s(6.0), p.s(6.0), p.s(3.0), accent);
    p.text(x + padx + p.s(6.0), y + (h + m.ascent - m.descent) / 2.0, Font::Body, text, TX1);
}

/// The overlay's triangles for a `screen`-sized window.
pub fn build(st: &OverlayState, atlas: &Atlas, scale: f32, screen: (f32, f32), now: Instant) -> Vec<Vertex> {
    let mut p = Painter::new(atlas, scale);
    if let Some(w) = &st.waiting {
        pill(&mut p, w, screen.1 / 2.0, screen, WARN);
    }
    if st.stats {
        stats_chip(&mut p, st);
    }
    let mut cy = screen.1 - p.s(48.0);
    for (t, _) in st.toasts.iter().rev() {
        pill(&mut p, t, cy, screen, CYAN);
        cy -= p.s(46.0);
    }
    if st.intro_until.is_some_and(|t| t > now) {
        pill(&mut p, "Ctrl+Alt+Shift+Q for the stream menu · Guide opens Sight as usual", cy, screen, CYAN);
        cy -= p.s(46.0);
    }
    if st.uncaptured_hint && st.menu.is_none() {
        pill(&mut p, "Click to capture input · Ctrl+Alt+Shift+Z releases it", cy, screen, OK);
    }
    if let Some(m) = &st.menu {
        menu(&mut p, st, m, screen);
    }
    p.verts
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_stream_proto::StreamPrefs;

    #[test]
    fn layouts_stay_on_screen() {
        let atlas = Atlas::new(1.0);
        let mut st = OverlayState { stats: true, uncaptured_hint: true, ..Default::default() };
        st.toast("Controller connected");
        st.menu = Some(Menu::new(StreamPrefs::default(), true, true));
        st.snapshot.host = Some(Default::default());
        for (w, h, scale) in [(1280.0, 720.0, 1.0), (3840.0, 2160.0, 2.0), (3840.0, 1080.0, 1.0)] {
            let atlas = if scale == 1.0 { &atlas } else { &Atlas::new(scale) };
            let v = build(&st, atlas, scale, (w, h), Instant::now());
            assert!(v.len() > 100);
            assert_eq!(v.len() % 3, 0);
            for vert in &v {
                assert!(
                    vert.pos[0] >= -2.0 && vert.pos[0] <= w + 2.0 && vert.pos[1] >= -2.0 && vert.pos[1] <= h + 2.0,
                    "{w}x{h}: {vert:?}"
                );
            }
        }
    }

    #[test]
    fn nothing_drawn_when_idle() {
        let atlas = Atlas::new(1.0);
        assert!(build(&OverlayState::default(), &atlas, 1.0, (1920.0, 1080.0), Instant::now()).is_empty());
    }

    #[test]
    fn toasts_expire() {
        let mut st = OverlayState::default();
        st.toast("a");
        assert!(st.animating(Instant::now()));
        st.expire(Instant::now() + TOAST_FOR + Duration::from_millis(1));
        assert!(st.toasts.is_empty());
    }
}
