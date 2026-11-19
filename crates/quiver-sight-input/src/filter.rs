//! Who gets a key or button: the game or the panel. Pure logic over two bit sets in the evdev
//! code space (keys and buttons share it: `KEY_A` 30, `BTN_LEFT` 0x110), shared by the X11 and
//! Wayland paths and unit-tested here.
//!
//! - `game`: what the game saw pressed and not yet released.
//! - `panel`: what was pressed while the panel captured input (the game never saw it).
//!
//! While capturing, presses (and motion, scrolling, pointer crossings) go to the panel; a
//! release goes to the game when the game saw the press, so nothing it holds when the panel
//! opens stays stuck down. A release of a key pressed in the panel is kept from the game even
//! after the panel closes (the Escape that closed it never reaches the game as a key-up, nor
//! does a key still held when it closes). Everything else passes.
//!
//! The decision ([`Tracker::decide`]) only reads, because Xlib asks for it inside an event
//! predicate that may run several times for one event; [`Tracker::commit`] records the
//! outcome once the event is taken off the queue.

use std::sync::atomic::{AtomicU64, Ordering};

/// Codes tracked: `KEY_MAX` + 1 (0x300).
pub const CODES: usize = 0x300;
const WORDS: usize = CODES / 64;

/// What an event is, for the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A key or button went down (or repeats).
    Down(u32),
    /// A key or button went up.
    Up(u32),
    /// Pointer motion, scrolling, entering or leaving a window: the panel's while capturing.
    Pointer,
    /// The game's window lost the keyboard: the game releases everything itself.
    Reset,
    /// A press of XInput2 class `c` ([`COUNTED`]) whose code isn't known when deciding (it's
    /// in the cookie's data): tracked by count per class.
    CountDown(u8),
    CountUp(u8),
    /// Anything else (focus, configuration, modifiers state, keymaps): always the game's.
    Other,
}

/// Classes tracked by count: XInput2 device keys, device buttons, raw keys, raw buttons.
pub const COUNTED: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Give it to the game.
    Pass,
    /// Keep it from the game (and forward it to the panel while capturing).
    Block,
}

pub struct Tracker {
    game: [AtomicU64; WORDS],
    panel: [AtomicU64; WORDS],
    /// Per counted class: presses the game got and hasn't had released, and presses kept from
    /// it. A release goes to the game while it has presses outstanding (so nothing it holds
    /// sticks); with several held across an open the pairing is by count, not by code.
    game_n: [AtomicU64; COUNTED],
    panel_n: [AtomicU64; COUNTED],
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

fn bit(code: u32) -> Option<(usize, u64)> {
    let c = code as usize;
    (c < CODES).then(|| (c / 64, 1u64 << (c % 64)))
}

fn test(set: &[AtomicU64; WORDS], code: u32) -> bool {
    bit(code).is_some_and(|(w, b)| set[w].load(Ordering::Relaxed) & b != 0)
}

fn set(set: &[AtomicU64; WORDS], code: u32, on: bool) {
    if let Some((w, b)) = bit(code) {
        if on {
            set[w].fetch_or(b, Ordering::Relaxed);
        } else {
            set[w].fetch_and(!b, Ordering::Relaxed);
        }
    }
}

fn count(n: &[AtomicU64; COUNTED], k: u8) -> u64 {
    n.get(k as usize).map_or(0, |c| c.load(Ordering::Relaxed))
}

/// Adds `d` (±1), never below zero.
fn add(n: &[AtomicU64; COUNTED], k: u8, d: i64) {
    if let Some(c) = n.get(k as usize) {
        let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(if d < 0 { v.saturating_sub(1) } else { (v + 1).min(64) }));
    }
}

impl Tracker {
    pub const fn new() -> Tracker {
        Tracker {
            game: [const { AtomicU64::new(0) }; WORDS],
            panel: [const { AtomicU64::new(0) }; WORDS],
            game_n: [const { AtomicU64::new(0) }; COUNTED],
            panel_n: [const { AtomicU64::new(0) }; COUNTED],
        }
    }

    /// Who gets `input` (reads only).
    pub fn decide(&self, input: Input, capturing: bool) -> Verdict {
        match input {
            Input::Down(_) | Input::CountDown(_) | Input::Pointer if capturing => Verdict::Block,
            Input::Up(c) if test(&self.game, c) => Verdict::Pass,
            Input::Up(c) if capturing || test(&self.panel, c) => Verdict::Block,
            Input::CountUp(k) if count(&self.game_n, k) > 0 => Verdict::Pass,
            Input::CountUp(k) if capturing || count(&self.panel_n, k) > 0 => Verdict::Block,
            _ => Verdict::Pass,
        }
    }

    /// Records what happened to `input` once it's off the queue. Returns true for a release
    /// the game gets while the panel captures (a key it held when the panel opened).
    pub fn commit(&self, input: Input, v: Verdict, capturing: bool) -> bool {
        match (input, v) {
            (Input::Down(c), Verdict::Pass) => {
                set(&self.game, c, true);
                set(&self.panel, c, false);
            }
            (Input::Down(c), Verdict::Block) => set(&self.panel, c, true),
            (Input::Up(c), Verdict::Pass) => {
                set(&self.game, c, false);
                set(&self.panel, c, false);
                return capturing;
            }
            (Input::Up(c), Verdict::Block) => set(&self.panel, c, false),
            (Input::CountDown(k), Verdict::Pass) => add(&self.game_n, k, 1),
            (Input::CountDown(k), Verdict::Block) => add(&self.panel_n, k, 1),
            (Input::CountUp(k), Verdict::Pass) => {
                add(&self.game_n, k, -1);
                return capturing;
            }
            (Input::CountUp(k), Verdict::Block) => add(&self.panel_n, k, -1),
            (Input::Reset, _) => {
                for w in self.game.iter().chain(self.panel.iter()) {
                    w.store(0, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        false
    }

    /// The game holds `code` down (as far as it was told).
    pub fn game_holds(&self, code: u32) -> bool {
        test(&self.game, code)
    }

    /// `code` was pressed in the panel and isn't released yet.
    pub fn panel_holds(&self, code: u32) -> bool {
        test(&self.panel, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Input::*;
    use Verdict::*;

    /// Runs a sequence of (event, capturing) through decide + commit; returns what the game
    /// was given.
    fn run(t: &Tracker, seq: &[(Input, bool)]) -> Vec<Input> {
        let mut game = Vec::new();
        for &(i, cap) in seq {
            let v = t.decide(i, cap);
            t.commit(i, v, cap);
            if v == Pass {
                game.push(i);
            }
        }
        game
    }

    const W: u32 = 17;
    const ESC: u32 = 1;
    const SHIFT: u32 = 42;
    const F2: u32 = 60;
    const LMB: u32 = 0x110;

    #[test]
    fn closed_everything_passes() {
        let t = Tracker::new();
        let seq = [(Down(W), false), (Pointer, false), (Up(W), false), (Down(LMB), false), (Up(LMB), false), (Other, false)];
        assert_eq!(run(&t, &seq), seq.iter().map(|s| s.0).collect::<Vec<_>>());
        assert!(!t.game_holds(W) && !t.panel_holds(W));
    }

    #[test]
    fn open_panel_gets_presses_and_motion() {
        let t = Tracker::new();
        let game = run(&t, &[(Down(W), true), (Pointer, true), (Up(W), true), (Down(LMB), true), (Up(LMB), true), (Other, true)]);
        assert_eq!(game, vec![Other]);
    }

    /// Shift+F2 opens the panel: the game saw both go down, and gets both releases, so
    /// neither is stuck; keys pressed afterwards are the panel's.
    #[test]
    fn keys_held_at_open_are_released_to_the_game() {
        let t = Tracker::new();
        let game = run(&t, &[(Down(SHIFT), false), (Down(F2), false), (Down(W), false)]);
        assert_eq!(game.len(), 3);
        // The panel opens.
        let game = run(&t, &[(Up(F2), true), (Down(ESC), true), (Up(SHIFT), true), (Pointer, true)]);
        assert_eq!(game, vec![Up(F2), Up(SHIFT)]);
        assert!(t.game_holds(W));
        // W is still held by the game: its release passes whenever it comes.
        assert_eq!(t.decide(Up(W), true), Pass);
        assert!(t.commit(Up(W), Pass, true), "a release the game gets while capturing");
        assert!(!t.game_holds(W));
    }

    /// Escape closes the panel on its press; its release comes after close and must not
    /// reach the game (no key-up without a key-down). Same for anything still held.
    #[test]
    fn keys_pressed_in_the_panel_stay_the_panels_after_close() {
        let t = Tracker::new();
        let game = run(&t, &[(Down(W), true), (Down(ESC), true)]);
        assert!(game.is_empty());
        // Closed now.
        let game = run(&t, &[(Up(ESC), false), (Pointer, false), (Up(W), false)]);
        assert_eq!(game, vec![Pointer]);
        assert!(!t.panel_holds(ESC) && !t.panel_holds(W));
        // A key held across the close that repeats is a new press for the game, and then its
        // release passes too.
        let t = Tracker::new();
        run(&t, &[(Down(W), true)]);
        assert_eq!(run(&t, &[(Down(W), false), (Up(W), false)]), vec![Down(W), Up(W)]);
    }

    #[test]
    fn a_release_of_something_unknown() {
        let t = Tracker::new();
        // Never seen pressed: kept while capturing, passed otherwise.
        assert_eq!(t.decide(Up(W), true), Block);
        assert_eq!(t.decide(Up(W), false), Pass);
        // Out of range codes are never tracked.
        assert_eq!(t.decide(Up(5000), false), Pass);
        assert_eq!(t.decide(Down(5000), true), Block);
        t.commit(Down(5000), Pass, false);
        assert!(!t.game_holds(5000));
    }

    #[test]
    fn reset_forgets_everything() {
        let t = Tracker::new();
        run(&t, &[(Down(W), false), (Down(ESC), true)]);
        assert!(t.game_holds(W) && t.panel_holds(ESC));
        assert_eq!(run(&t, &[(Reset, true)]), vec![Reset]);
        assert!(!t.game_holds(W) && !t.panel_holds(ESC));
    }

    /// XInput2 raw buttons: counted per class (the code isn't known when deciding).
    #[test]
    fn counted_classes() {
        let t = Tracker::new();
        const RAW: u8 = 3;
        // Held before the panel opens: its release passes while capturing.
        assert_eq!(run(&t, &[(CountDown(RAW), false), (CountUp(RAW), true)]), vec![CountDown(RAW), CountUp(RAW)]);
        // Pressed in the panel: neither the press nor (after close) the release reach the game.
        assert!(run(&t, &[(CountDown(RAW), true), (CountUp(RAW), false)]).is_empty());
        // Closed and nothing outstanding: pass.
        assert_eq!(run(&t, &[(CountUp(RAW), false)]), vec![CountUp(RAW)]);
        // Open, nothing held: a release is kept.
        assert!(run(&t, &[(CountUp(RAW), true)]).is_empty());
        // Classes are separate; counts never go negative.
        run(&t, &[(CountDown(0), false)]);
        assert_eq!(t.decide(CountUp(1), true), Block);
        assert_eq!(t.decide(CountUp(0), true), Pass);
        assert_eq!(t.decide(CountUp(9), true), Block, "unknown class");
    }

    /// The decision doesn't change state (Xlib may ask several times per event).
    #[test]
    fn decide_is_pure() {
        let t = Tracker::new();
        for _ in 0..3 {
            assert_eq!(t.decide(Down(W), true), Block);
        }
        assert!(!t.panel_holds(W));
    }
}
