//! Is the game what the user is looking at? (design/SIGHT.md §14, "Tied to the game window".)
//! The Sight panel and the desktop HUD are layer-shell surfaces, which belong to an output,
//! not a workspace: without this they'd stay up over other workspaces and other windows.
//! Here, per session, the answer from the signals there are:
//!
//! 1. **The game's toplevel** in wlr-foreign-toplevel-management ([`crate::toplevels`]): in
//!    front while `activated` and not `minimized`. Measured on Hyprland 0.56: a workspace
//!    switch, a focus change and moving the window to a special workspace each take
//!    `activated` away in the same batch of events; a layer surface taking the keyboard
//!    leaves the game `activated` (so the panel opening doesn't count as the game leaving).
//!    Other compositors may deactivate the game when the panel takes the keyboard, so "the
//!    panel has the keyboard and no other toplevel is activated" counts as in front too,
//!    unless this compositor was seen keeping the game activated under the panel
//!    ([`Front::observe_panel`]): then it means an empty workspace (Hyprland gives the
//!    keyboard back to an on-demand layer surface when there's no window to focus).
//! 2. **Which toplevel is the game's**: the protocol has no pids, so it's found (in order) from
//!    Hyprland's window for the session (its class and title, when Hyprland is there: the
//!    window whose process has `QUIVER_SESSION`), from the toplevel activated when the panel
//!    was summoned by the game's own Shift+F2, or learned: the toplevel that stays the only
//!    activated one while the game's Sight layer says the game is in front.
//! 3. **Without a toplevel protocol, or before the game's toplevel is known**: the layer's
//!    word (`HudModel::game_focus`: a Wayland window's keyboard focus, an X11 window being
//!    `_NET_ACTIVE_WINDOW`) together with the panel's own keyboard focus.
//! 4. **Nothing to go on** (no protocol and no layer, as for an OpenGL game on a compositor
//!    without the protocol): unknown, and things stay shown, as before. Hiding is only done
//!    when a signal can say when to show again.
//!
//! Pure logic, no I/O: `quiver-sight` feeds it events and asks for verdicts.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::toplevels::{Event, Toplevel};

/// Learning which toplevel is the game's needs the layer's "in front" and a single activated
/// toplevel to agree for this long (the layer's word comes through quiverd, ≤ ~150 ms late).
pub const LEARN_AFTER: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Hyprland's window for the session (class and title).
    Hyprland,
    /// Active when the game's own Shift+F2 summoned the panel.
    Keys,
    /// Active while the game's Sight layer said the game was in front.
    Learned,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::Hyprland => "Hyprland's window for the session",
            How::Keys => "active when the game's Shift+F2 summoned the panel",
            How::Learned => "active while the game's Sight layer said it was in front",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The game is in front (why).
    Front(String),
    /// It isn't (why): hide what's tied to it.
    Away(String),
    /// No signal to tell: leave things shown.
    Unknown(String),
}

impl Verdict {
    pub fn away(&self) -> bool {
        matches!(self, Verdict::Away(_))
    }

    pub fn why(&self) -> &str {
        match self {
            Verdict::Front(w) | Verdict::Away(w) | Verdict::Unknown(w) => w,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Game {
    /// The layer's word and since when.
    focus: Option<bool>,
    focus_since: Option<Instant>,
    bound: Option<(u64, How)>,
}

#[derive(Debug, Default)]
pub struct Front {
    /// `None` until the first snapshot.
    toplevels: Option<Vec<Toplevel>>,
    unavailable: Option<String>,
    /// When the set of activated toplevels last changed.
    active_since: Option<Instant>,
    games: HashMap<String, Game>,
    /// [`Front::bind_active`] before the first snapshot: the session and when.
    pending_active: Option<(String, Instant)>,
    /// Whether this compositor keeps the game's toplevel activated while the panel (a layer
    /// surface) has the keyboard ([`Front::observe_panel`]). Hyprland does; then "the panel
    /// has the keyboard but nothing is activated" means the user went to an empty workspace
    /// (Hyprland leaves the keyboard on an on-demand layer surface when there's no window to
    /// give it to), not that the panel deactivated the game.
    keeps_activated: Option<bool>,
}

/// How long a [`Front::bind_active`] waits for the first snapshot.
const PENDING_FOR: Duration = Duration::from_secs(2);
/// [`Front::observe_panel`] decides once the panel's keyboard and the activated toplevels have
/// both been settled this long.
pub const SETTLE: Duration = Duration::from_millis(250);

impl Front {
    pub fn new() -> Front {
        Front::default()
    }

    /// The toplevel protocol is there (a snapshot came).
    pub fn has_toplevels(&self) -> bool {
        self.toplevels.is_some()
    }

    pub fn toplevels(&self) -> &[Toplevel] {
        self.toplevels.as_deref().unwrap_or_default()
    }

    /// A toplevel thread event. Bindings to toplevels that closed are dropped.
    pub fn toplevels_event(&mut self, e: Event, now: Instant) {
        match e {
            Event::Snapshot(v) => {
                let active = |v: &[Toplevel]| v.iter().filter(|t| t.activated && !t.minimized).map(|t| t.id).collect::<Vec<_>>();
                if self.toplevels.as_deref().is_none_or(|old| active(old) != active(&v)) {
                    self.active_since = Some(now);
                }
                for g in self.games.values_mut() {
                    if g.bound.is_some_and(|(id, _)| !v.iter().any(|t| t.id == id)) {
                        g.bound = None;
                    }
                }
                self.toplevels = Some(v);
                self.unavailable = None;
                if let Some((s, at)) = self.pending_active.take()
                    && now.saturating_duration_since(at) < PENDING_FOR
                {
                    self.bind_active(&s, now);
                }
            }
            Event::Unavailable(why) => {
                self.toplevels = None;
                self.unavailable = Some(why);
                for g in self.games.values_mut() {
                    g.bound = None;
                }
            }
        }
    }

    /// The game's Sight layer says whether the game is in front (from its HUD model).
    pub fn game_focus(&mut self, session: &str, focus: Option<bool>, now: Instant) {
        let g = self.games.entry(session.to_owned()).or_default();
        if g.focus != focus || g.focus_since.is_none() {
            g.focus = focus;
            g.focus_since = Some(now);
        }
    }

    pub fn ended(&mut self, session: &str) {
        self.games.remove(session);
    }

    /// The game's toplevel, and how it was found.
    pub fn bound(&self, session: &str) -> Option<(&Toplevel, How)> {
        let (id, how) = self.games.get(session)?.bound?;
        Some((self.toplevels().iter().find(|t| t.id == id)?, how))
    }

    /// Not bound, but there are toplevels to bind to: worth a look for Hyprland's window.
    pub fn wants_hint(&self, session: &str) -> bool {
        self.has_toplevels() && self.games.get(session).is_none_or(|g| g.bound.is_none_or(|(_, h)| h != How::Hyprland))
    }

    /// Hyprland's window for the session (`class`, `title`): the toplevel with that app id and
    /// title (the activated one if several). True when bound.
    pub fn bind_hint(&mut self, session: &str, class: &str, title: &str) -> bool {
        let same: Vec<&Toplevel> = self.toplevels().iter().filter(|t| t.app_id == class && t.title == title).collect();
        let pick = match same.as_slice() {
            [one] => Some(one.id),
            many => many.iter().find(|t| t.activated).map(|t| t.id),
        };
        let Some(id) = pick else { return false };
        self.games.entry(session.to_owned()).or_default().bound = Some((id, How::Hyprland));
        true
    }

    /// The only activated toplevel, if exactly one is.
    fn the_active(&self) -> Option<u64> {
        let mut it = self.toplevels().iter().filter(|t| t.activated && !t.minimized);
        let one = it.next()?;
        it.next().is_none().then_some(one.id)
    }

    /// The panel was summoned by the game's own keys: the toplevel active now is the game's
    /// (unless Hyprland already said which). True when bound.
    pub fn bind_active(&mut self, session: &str, now: Instant) -> bool {
        if self.bound(session).is_some_and(|(_, h)| h == How::Hyprland) {
            return true;
        }
        if !self.has_toplevels() && self.unavailable.is_none() {
            // The list hasn't come yet (quiver-sight was started for this very command): the
            // first snapshot settles it, if it comes soon.
            self.pending_active = Some((session.to_owned(), now));
            return false;
        }
        let Some(id) = self.the_active() else { return false };
        self.games.entry(session.to_owned()).or_default().bound = Some((id, How::Keys));
        true
    }

    /// Learns (or corrects a learned or key-found) binding: the game's layer has said "in
    /// front" for [`LEARN_AFTER`] while one toplevel has been the only activated one as long.
    /// Returns the sessions whose binding changed.
    pub fn learn(&mut self, now: Instant) -> Vec<String> {
        let stable = |since: Option<Instant>| since.is_some_and(|s| now.saturating_duration_since(s) >= LEARN_AFTER);
        if !stable(self.active_since) {
            return Vec::new();
        }
        let Some(id) = self.the_active() else { return Vec::new() };
        // One toplevel can only be one game's: a layer that's wrong about being in front (two
        // sessions both saying so) teaches nothing.
        let claim: Vec<String> = self.games.iter().filter(|(_, g)| g.focus == Some(true) && stable(g.focus_since)).map(|(s, _)| s.clone()).collect();
        let [session] = claim.as_slice() else { return Vec::new() };
        let g = self.games.get_mut(session).expect("listed");
        match g.bound {
            Some((_, How::Hyprland)) => Vec::new(),
            Some((b, _)) if b == id => Vec::new(),
            _ => {
                g.bound = Some((id, How::Learned));
                vec![session.clone()]
            }
        }
    }

    /// The panel for `session` has had the keyboard since `since`: once that and the activated
    /// toplevels have settled ([`SETTLE`]), whether the game's toplevel stayed activated tells
    /// how this compositor treats a layer surface with the keyboard (kept for the run).
    pub fn observe_panel(&mut self, session: &str, since: Option<Instant>, now: Instant) {
        if self.keeps_activated.is_some() {
            return;
        }
        let settled = |t: Option<Instant>| t.is_some_and(|t| now.saturating_duration_since(t) >= SETTLE);
        if !(settled(since) && settled(self.active_since)) {
            return;
        }
        let Some((t, _)) = self.bound(session) else { return };
        let other = self.toplevels().iter().any(|x| x.activated && x.id != t.id);
        self.keeps_activated = match (t.activated, other) {
            (true, _) => Some(true),
            (false, false) => Some(false),
            (false, true) => None,
        };
        if let Some(k) = self.keeps_activated {
            tracing::info!(keeps_activated = k, "the compositor {} the game's window activated while the panel has the keyboard", if k { "keeps" } else { "doesn't keep" });
        }
    }

    /// Whether `session`'s game is in front. `panel_keyboard`: Sight's panel for this game is
    /// shown and has the keyboard (or has just been mapped and is about to get it).
    pub fn verdict(&self, session: &str, panel_keyboard: bool) -> Verdict {
        let g = self.games.get(session);
        if let Some((t, _)) = self.bound(session) {
            if t.activated && !t.minimized {
                return Verdict::Front("the game's window is active".into());
            }
            let other = self.toplevels().iter().find(|x| x.activated && !x.minimized && x.id != t.id);
            // Where the panel taking the keyboard deactivates the game (or until that's
            // known), the panel having it with nothing else active is the game in front.
            if panel_keyboard && other.is_none() && self.keeps_activated != Some(true) {
                return Verdict::Front("the panel has the keyboard".into());
            }
            return Verdict::Away(match (other, t.minimized) {
                (Some(o), _) => format!("another window is active ({})", if o.app_id.is_empty() { "no app id" } else { &o.app_id }),
                (None, true) => "the game's window is minimised".into(),
                (None, false) => "the game's window isn't active (another workspace?)".into(),
            });
        }
        // Not bound (no protocol, or the game's toplevel isn't known yet): the layer's word and
        // the panel's keyboard.
        let why_unbound = match &self.unavailable {
            Some(u) => u.clone(),
            None if self.has_toplevels() => "the game's window isn't known yet".into(),
            None => "no toplevel list yet".into(),
        };
        match g.and_then(|g| g.focus) {
            Some(true) => Verdict::Front("the game has focus (its Sight layer)".into()),
            Some(false) if panel_keyboard => Verdict::Front("the panel has the keyboard".into()),
            Some(false) => Verdict::Away(format!("neither the game nor the panel has focus ({why_unbound})")),
            None => Verdict::Unknown(format!("no way to tell ({why_unbound}; the game's Sight layer doesn't say)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tl(id: u64, app: &str, title: &str, activated: bool) -> Toplevel {
        Toplevel { id, app_id: app.into(), title: title.into(), activated, ..Default::default() }
    }

    fn snap(f: &mut Front, v: Vec<Toplevel>, now: Instant) {
        f.toplevels_event(Event::Snapshot(v), now);
    }

    const S: &str = "s-1";

    /// The Hyprland measurements (nested 0.56.2, on-demand panel): the game stays activated
    /// while the panel has the keyboard; a workspace switch activates the other window (or
    /// none); coming back activates the game again.
    #[test]
    fn hyprland_workspace_switch_and_back() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "acorigins.exe", "Assassin's Creed Origins", true), tl(2, "kitty", "~", false)], t0);
        assert!(f.bind_hint(S, "acorigins.exe", "Assassin's Creed Origins"));
        assert_eq!(f.bound(S).map(|(t, h)| (t.id, h)), Some((1, How::Hyprland)));
        assert!(matches!(f.verdict(S, true), Verdict::Front(_)));
        // Workspace 2 has the terminal: it's activated, the panel lost the keyboard.
        snap(&mut f, vec![tl(1, "acorigins.exe", "Assassin's Creed Origins", false), tl(2, "kitty", "~", true)], t0);
        let v = f.verdict(S, false);
        assert!(v.away(), "{v:?}");
        assert!(v.why().contains("kitty"));
        // Even if the panel still thought it had the keyboard: another window is active.
        assert!(f.verdict(S, true).away());
        // An empty workspace: nothing activated, and the panel (hidden) has no keyboard.
        snap(&mut f, vec![tl(1, "acorigins.exe", "Assassin's Creed Origins", false), tl(2, "kitty", "~", false)], t0);
        assert!(f.verdict(S, false).away());
        // Back.
        snap(&mut f, vec![tl(1, "acorigins.exe", "Assassin's Creed Origins", true), tl(2, "kitty", "~", false)], t0);
        assert!(!f.verdict(S, false).away());
    }

    /// Hyprland keeps the game activated while the panel has the keyboard, and on a switch to
    /// an empty workspace leaves the keyboard on the (on-demand) panel: once the first is
    /// seen, "the panel has the keyboard, nothing is activated" is the game away.
    #[test]
    fn hyprland_empty_workspace_with_the_panel_keeping_the_keyboard() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true), tl(2, "kitty", "~", false)], t0);
        f.bind_active(S, t0);
        // Not settled yet: nothing decided.
        f.observe_panel(S, Some(t0), t0 + Duration::from_millis(100));
        assert_eq!(f.keeps_activated, None);
        f.observe_panel(S, Some(t0), t0 + SETTLE);
        assert_eq!(f.keeps_activated, Some(true));
        let t1 = t0 + Duration::from_secs(1);
        snap(&mut f, vec![tl(1, "game", "Game", false), tl(2, "kitty", "~", false)], t1);
        let v = f.verdict(S, true);
        assert!(v.away(), "{v:?}");
        // Decided once: later observations change nothing.
        f.observe_panel(S, Some(t0), t1 + SETTLE);
        assert_eq!(f.keeps_activated, Some(true));
    }

    #[test]
    fn a_compositor_that_deactivates_the_game_for_the_panel() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0);
        f.bind_active(S, t0);
        snap(&mut f, vec![tl(1, "game", "Game", false)], t0 + Duration::from_millis(20));
        f.observe_panel(S, Some(t0), t0 + Duration::from_millis(20) + SETTLE);
        assert_eq!(f.keeps_activated, Some(false));
        assert!(matches!(f.verdict(S, true), Verdict::Front(_)));
    }

    /// Compositors that deactivate the game when a layer surface takes the keyboard: the
    /// panel having it, with no other window activated, still counts as the game in front.
    #[test]
    fn panel_keyboard_with_nothing_activated() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0);
        assert!(f.bind_active(S, t0));
        snap(&mut f, vec![tl(1, "game", "Game", false)], t0);
        assert!(matches!(f.verdict(S, true), Verdict::Front(_)));
        // Then a workspace switch: the panel loses the keyboard too.
        assert!(f.verdict(S, false).away());
    }

    #[test]
    fn minimised_and_closed() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0);
        f.bind_active(S, t0);
        let min = Toplevel { minimized: true, ..tl(1, "game", "Game", true) };
        snap(&mut f, vec![min], t0);
        assert!(f.verdict(S, false).why().contains("minimised"));
        // The window closes: unbound, and with no layer word, unknown (shown).
        snap(&mut f, vec![], t0);
        assert!(f.bound(S).is_none());
        assert!(matches!(f.verdict(S, false), Verdict::Unknown(_)));
    }

    #[test]
    fn hint_picks_the_active_one_of_twins_or_none() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", false), tl(2, "game", "Game", false)], t0);
        assert!(!f.bind_hint(S, "game", "Game"), "two alike, neither active: no guess");
        assert!(!f.bind_hint(S, "game", "Other"));
        snap(&mut f, vec![tl(1, "game", "Game", false), tl(2, "game", "Game", true)], t0);
        assert!(f.bind_hint(S, "game", "Game"));
        assert_eq!(f.bound(S).unwrap().0.id, 2);
        // A key-found binding doesn't replace Hyprland's.
        snap(&mut f, vec![tl(1, "game", "Game", true), tl(2, "game", "Game", false)], t0);
        assert!(f.bind_active(S, t0));
        assert_eq!(f.bound(S).unwrap().0.id, 2);
        assert!(!f.wants_hint(S));
    }

    #[test]
    fn learning_needs_both_signals_to_settle() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true), tl(2, "kitty", "~", false)], t0);
        f.game_focus(S, Some(true), t0);
        assert!(f.learn(t0 + Duration::from_millis(100)).is_empty(), "too soon");
        assert_eq!(f.learn(t0 + LEARN_AFTER), [S.to_owned()]);
        assert_eq!(f.bound(S).map(|(t, h)| (t.id, h)), Some((1, How::Learned)));
        // Alt-tab to the terminal: the toplevel list knows at once, the layer's word is late.
        // Nothing is learned from the stale "in front" while the activation is fresh.
        let t1 = t0 + Duration::from_secs(1);
        snap(&mut f, vec![tl(1, "game", "Game", false), tl(2, "kitty", "~", true)], t1);
        assert!(f.learn(t1 + Duration::from_millis(150)).is_empty());
        f.game_focus(S, Some(false), t1 + Duration::from_millis(150));
        assert!(f.learn(t1 + Duration::from_secs(2)).is_empty());
        assert_eq!(f.bound(S).unwrap().0.id, 1);
        assert!(f.verdict(S, false).away());
    }

    #[test]
    fn learning_corrects_a_key_binding_but_never_hyprlands() {
        let t0 = Instant::now();
        let mut f = Front::new();
        // Summoned from a terminal by a key chord that wasn't the game's: bound to the wrong one.
        snap(&mut f, vec![tl(1, "game", "Game", false), tl(2, "kitty", "~", true)], t0);
        f.bind_active(S, t0);
        assert_eq!(f.bound(S).unwrap().0.id, 2);
        let t1 = t0 + Duration::from_secs(1);
        snap(&mut f, vec![tl(1, "game", "Game", true), tl(2, "kitty", "~", false)], t1);
        f.game_focus(S, Some(true), t1);
        assert_eq!(f.learn(t1 + LEARN_AFTER), [S.to_owned()]);
        assert_eq!(f.bound(S).unwrap().0.id, 1);
        // Hyprland's word stands.
        f.bind_hint(S, "kitty", "~");
        assert!(f.learn(t1 + LEARN_AFTER * 3).is_empty());
        assert_eq!(f.bound(S).unwrap().1, How::Hyprland);
    }

    #[test]
    fn two_sessions_claiming_front_teach_nothing() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0);
        f.game_focus("a", Some(true), t0);
        f.game_focus("b", Some(true), t0);
        assert!(f.learn(t0 + LEARN_AFTER).is_empty());
    }

    /// No toplevel protocol: the layer's word and the panel's keyboard.
    #[test]
    fn without_the_protocol() {
        let t0 = Instant::now();
        let mut f = Front::new();
        f.toplevels_event(Event::Unavailable("the compositor has no wlr-foreign-toplevel-management".into()), t0);
        assert!(matches!(f.verdict(S, true), Verdict::Unknown(_)), "nothing from the layer: unknown, even with the panel up");
        f.game_focus(S, Some(true), t0);
        assert!(matches!(f.verdict(S, false), Verdict::Front(_)));
        // The panel took the keyboard from the game.
        f.game_focus(S, Some(false), t0);
        assert!(matches!(f.verdict(S, true), Verdict::Front(_)));
        // Then neither has it (a workspace switch).
        let v = f.verdict(S, false);
        assert!(v.away() && v.why().contains("no wlr-foreign-toplevel"), "{v:?}");
        f.ended(S);
        assert!(matches!(f.verdict(S, false), Verdict::Unknown(_)));
    }

    /// quiver-sight started for the panel command itself: the list comes just after.
    #[test]
    fn summoned_before_the_first_snapshot() {
        let t0 = Instant::now();
        let mut f = Front::new();
        assert!(!f.bind_active(S, t0));
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0 + Duration::from_millis(50));
        assert_eq!(f.bound(S).map(|(t, h)| (t.id, h)), Some((1, How::Keys)));
        // Too late, it would guess from a later state: not bound.
        let mut f = Front::new();
        f.bind_active(S, t0);
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0 + PENDING_FOR);
        assert!(f.bound(S).is_none());
    }

    #[test]
    fn the_protocol_going_away_unbinds() {
        let t0 = Instant::now();
        let mut f = Front::new();
        snap(&mut f, vec![tl(1, "game", "Game", true)], t0);
        f.bind_active(S, t0);
        f.toplevels_event(Event::Unavailable("the toplevel list's connection failed".into()), t0);
        assert!(f.bound(S).is_none());
        assert!(!f.has_toplevels());
    }
}
