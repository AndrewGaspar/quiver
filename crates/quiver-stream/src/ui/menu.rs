//! The stream menu: its rows, navigation and what choosing does. Drawing is in
//! [`super::overlay`].
//!
//! Keys: Up/Down (or W/S) move, Left/Right change a value, Enter or Space chooses, Escape
//! closes. Pad: D-pad, A chooses, B closes. The mouse picks a row by hovering and clicks it.
//! "Quit game" asks for a second press.

use quiver_stream_proto::{Pacing, StreamPrefs, VideoCodec};

/// Bitrate caps offered, in Mbit/s (None: adaptive).
pub const BITRATES: [Option<u32>; 7] = [None, Some(10), Some(20), Some(40), Some(80), Some(150), Some(300)];
pub const CODECS: [Option<VideoCodec>; 4] = [None, Some(VideoCodec::Hevc), Some(VideoCodec::Av1), Some(VideoCodec::H264)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Resume,
    Quality,
    Codec,
    Pacing,
    Hdr,
    CaptureMouse,
    Stats,
    Disconnect,
    QuitGame,
}

pub const ROWS: [Row; 9] =
    [Row::Resume, Row::Quality, Row::Codec, Row::Pacing, Row::Hdr, Row::CaptureMouse, Row::Stats, Row::Disconnect, Row::QuitGame];

/// What the menu asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Close,
    /// Send these prefs to the host (Control::Prefs) and apply the local parts (pacing).
    Prefs(StreamPrefs),
    CaptureMouse(bool),
    Stats(bool),
    /// Leave the game running on the host and close the stream.
    Disconnect,
    /// Ask the host to quit the game, then close.
    QuitGame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    Choose,
    Back,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub selected: usize,
    pub prefs: StreamPrefs,
    pub capture_mouse: bool,
    pub stats: bool,
    /// "Quit game" was pressed once.
    pub confirm_quit: bool,
}

impl Menu {
    pub fn new(prefs: StreamPrefs, capture_mouse: bool, stats: bool) -> Self {
        Self { selected: 0, prefs, capture_mouse, stats, confirm_quit: false }
    }

    pub fn row(&self) -> Row {
        ROWS[self.selected]
    }

    /// The row's label and value text.
    pub fn text(&self, row: Row) -> (&'static str, String) {
        let onoff = |b: bool| if b { "On".to_owned() } else { "Off".to_owned() };
        match row {
            Row::Resume => ("Resume", String::new()),
            Row::Quality => (
                "Quality",
                match self.prefs.max_bitrate_kbps {
                    None => "Auto".into(),
                    Some(k) => format!("{} Mb/s", k / 1000),
                },
            ),
            Row::Codec => (
                "Codec",
                match self.prefs.codec {
                    None => "Auto".into(),
                    Some(VideoCodec::Hevc) => "HEVC".into(),
                    Some(VideoCodec::Av1) => "AV1".into(),
                    Some(VideoCodec::H264) => "H.264".into(),
                },
            ),
            Row::Pacing => (
                "Pacing",
                match self.prefs.pacing {
                    Pacing::LowestLatency => "Lowest latency".into(),
                    Pacing::Smooth => "Smooth".into(),
                },
            ),
            Row::Hdr => ("HDR", if self.prefs.hdr == Some(false) { "Off".into() } else { "Auto".into() }),
            Row::CaptureMouse => ("Capture mouse", onoff(self.capture_mouse)),
            Row::Stats => ("Stats", onoff(self.stats)),
            Row::Disconnect => ("Disconnect", "game keeps running".into()),
            Row::QuitGame => ("Quit game", if self.confirm_quit { "press again to quit".into() } else { String::new() }),
        }
    }

    /// Whether a row has values to step through (drawn with arrows).
    pub fn is_choice(row: Row) -> bool {
        matches!(row, Row::Quality | Row::Codec | Row::Pacing | Row::Hdr | Row::CaptureMouse | Row::Stats)
    }

    fn step(&mut self, row: Row, dir: i32) -> Option<Action> {
        fn cycle<T: PartialEq + Copy>(list: &[T], cur: T, dir: i32) -> T {
            let i = list.iter().position(|v| *v == cur).unwrap_or(0) as i32;
            list[(i + dir).rem_euclid(list.len() as i32) as usize]
        }
        match row {
            Row::Quality => {
                let cur = self.prefs.max_bitrate_kbps.map(|k| k / 1000);
                self.prefs.max_bitrate_kbps = cycle(&BITRATES, cur, dir).map(|m| m * 1000);
                Some(Action::Prefs(self.prefs.clone()))
            }
            Row::Codec => {
                self.prefs.codec = cycle(&CODECS, self.prefs.codec, dir);
                Some(Action::Prefs(self.prefs.clone()))
            }
            Row::Pacing => {
                self.prefs.pacing = cycle(&[Pacing::LowestLatency, Pacing::Smooth], self.prefs.pacing, dir);
                Some(Action::Prefs(self.prefs.clone()))
            }
            Row::Hdr => {
                self.prefs.hdr = if self.prefs.hdr == Some(false) { None } else { Some(false) };
                Some(Action::Prefs(self.prefs.clone()))
            }
            Row::CaptureMouse => {
                self.capture_mouse = !self.capture_mouse;
                Some(Action::CaptureMouse(self.capture_mouse))
            }
            Row::Stats => {
                self.stats = !self.stats;
                Some(Action::Stats(self.stats))
            }
            _ => None,
        }
    }

    pub fn nav(&mut self, n: Nav) -> Option<Action> {
        let row = self.row();
        if row == Row::QuitGame && n != Nav::Choose {
            self.confirm_quit = false;
        }
        match n {
            Nav::Up => {
                self.selected = (self.selected + ROWS.len() - 1) % ROWS.len();
                None
            }
            Nav::Down => {
                self.selected = (self.selected + 1) % ROWS.len();
                None
            }
            Nav::Left => self.step(row, -1),
            Nav::Right => self.step(row, 1),
            Nav::Back => Some(Action::Close),
            Nav::Choose => match row {
                Row::Resume => Some(Action::Close),
                Row::Disconnect => Some(Action::Disconnect),
                Row::QuitGame if self.confirm_quit => Some(Action::QuitGame),
                Row::QuitGame => {
                    self.confirm_quit = true;
                    None
                }
                r => self.step(r, 1),
            },
        }
    }

    /// The mouse is over row `i`.
    pub fn hover(&mut self, i: usize) {
        if i < ROWS.len() && i != self.selected {
            self.selected = i;
            self.confirm_quit = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_wraps_and_choices_cycle() {
        let mut m = Menu::new(StreamPrefs::default(), true, false);
        assert_eq!(m.row(), Row::Resume);
        m.nav(Nav::Up);
        assert_eq!(m.row(), Row::QuitGame);
        m.nav(Nav::Down);
        m.nav(Nav::Down);
        assert_eq!(m.row(), Row::Quality);
        assert_eq!(m.text(Row::Quality).1, "Auto");
        let a = m.nav(Nav::Right).unwrap();
        assert!(matches!(a, Action::Prefs(ref p) if p.max_bitrate_kbps == Some(10_000)));
        assert_eq!(m.text(Row::Quality).1, "10 Mb/s");
        m.nav(Nav::Left);
        m.nav(Nav::Left);
        assert_eq!(m.prefs.max_bitrate_kbps, Some(300_000));
        m.nav(Nav::Down);
        assert!(matches!(m.nav(Nav::Choose), Some(Action::Prefs(ref p)) if p.codec == Some(VideoCodec::Hevc)));
        m.nav(Nav::Down);
        assert!(matches!(m.nav(Nav::Right), Some(Action::Prefs(ref p)) if p.pacing == Pacing::Smooth));
        assert_eq!(m.text(Row::Pacing).1, "Smooth");
    }

    #[test]
    fn toggles_and_closing() {
        let mut m = Menu::new(StreamPrefs::default(), true, false);
        m.hover(5);
        assert_eq!(m.row(), Row::CaptureMouse);
        assert_eq!(m.nav(Nav::Choose), Some(Action::CaptureMouse(false)));
        m.nav(Nav::Down);
        assert_eq!(m.nav(Nav::Right), Some(Action::Stats(true)));
        assert_eq!(m.nav(Nav::Back), Some(Action::Close));
        m.hover(0);
        assert_eq!(m.nav(Nav::Choose), Some(Action::Close));
        m.hover(7);
        assert_eq!(m.nav(Nav::Choose), Some(Action::Disconnect));
    }

    #[test]
    fn quit_game_needs_a_second_press() {
        let mut m = Menu::new(StreamPrefs::default(), true, false);
        m.hover(8);
        assert_eq!(m.nav(Nav::Choose), None);
        assert!(m.confirm_quit);
        assert_eq!(m.text(Row::QuitGame).1, "press again to quit");
        // Moving away cancels.
        m.nav(Nav::Up);
        m.nav(Nav::Down);
        assert!(!m.confirm_quit);
        assert_eq!(m.nav(Nav::Choose), None);
        assert_eq!(m.nav(Nav::Choose), Some(Action::QuitGame));
    }
}
