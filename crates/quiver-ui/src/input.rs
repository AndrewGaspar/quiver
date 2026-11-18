//! The input vocabulary shared by the app and Sight's panel: keyboard and gamepad both
//! produce [`Input`]s, which each UI's focus model applies (the app's `nav.rs`, the panel's
//! `quiver_sight::panel::state`).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    Left,
    Right,
    /// Enter / A.
    Activate,
    /// Esc / B.
    Back,
    /// LB / RB: the previous or next top-level section.
    PrevSection,
    NextSection,
    /// LT / RT: the previous or next tab.
    PrevTab,
    NextTab,
    /// Ctrl K / Start.
    Palette,
    /// Y: the menu for the focused game.
    Menu,
    /// X: play options for the focused game (the menu, for now).
    PlayOptions,
    /// P: play the focused game.
    Play,
}
