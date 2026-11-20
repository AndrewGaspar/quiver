//! The hint bar's API (design/SIGHT.md §9 "Panel"): each section says what its buttons do
//! right now ("Y Pin to quick menu", "X Reveal hidden", "LT RT Pages"), the shell adds its
//! own (B, LB / RB, R3), and the bar draws them.
//!
//! A section supplies its hints from one function, called with the view each frame:
//!
//! ```ignore
//! // panel/sections/achievements.rs
//! pub fn hints(v: &PanelView, h: &mut Hints) {
//!     h.add([Glyph::A], "Details").add([Glyph::X], "Reveal hidden").add([Glyph::LT, Glyph::RT], "Filter");
//! }
//! ```
//!
//! and [`super::view::PanelView::section_hints`] calls it for its section (one `match` arm per
//! section). Order: the section's hints first, as added; then the shell's (B, LB / RB, R3), which
//! a section may leave out ([`Hints::without_shell`]) or replace ([`Hints::replace`]). A
//! right-aligned line ([`Hints::detail`]) says something about the focused thing ("Default
//! 1.0× · range 1–5×").

/// A controller button (or pair) in the hint bar. Keys typed by hyprpad's `quiver-sight`
/// mode map onto these (`tools/hyprpad/quiver-sight.lua`, [`super::state::panel_key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    A,
    B,
    X,
    Y,
    LB,
    RB,
    LT,
    RT,
    /// The right stick's click.
    R3,
    /// ◀ ▶ on the D-pad.
    LeftRight,
    /// ▲ ▼ on the D-pad.
    UpDown,
    Start,
}

impl Glyph {
    /// What the round or square cap says.
    pub fn label(self) -> &'static str {
        match self {
            Glyph::A => "A",
            Glyph::B => "B",
            Glyph::X => "X",
            Glyph::Y => "Y",
            Glyph::LB => "LB",
            Glyph::RB => "RB",
            Glyph::LT => "LT",
            Glyph::RT => "RT",
            Glyph::R3 => "R3",
            Glyph::LeftRight => "◀ ▶",
            Glyph::UpDown => "▲ ▼",
            Glyph::Start => "≡",
        }
    }
}

/// One hint: its buttons and what they do.
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    pub glyphs: Vec<Glyph>,
    pub label: String,
}

/// The hint bar's contents for one frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hints {
    pub items: Vec<Hint>,
    /// Right-aligned, muted: about the focused thing ("Default 1.0× · range 1–5×").
    pub detail: Option<String>,
    /// The shell's own hints (B, LB / RB, R3) are left out.
    pub no_shell: bool,
}

impl Hints {
    /// Adds a hint after the ones before it. Chains.
    pub fn add(&mut self, glyphs: impl IntoIterator<Item = Glyph>, label: impl Into<String>) -> &mut Self {
        self.items.push(Hint { glyphs: glyphs.into_iter().collect(), label: label.into() });
        self
    }

    /// Replaces the hint for `glyph` (the first one that has it), or adds it.
    pub fn replace(&mut self, glyphs: impl IntoIterator<Item = Glyph>, label: impl Into<String>) -> &mut Self {
        let glyphs: Vec<Glyph> = glyphs.into_iter().collect();
        let label = label.into();
        match self.items.iter_mut().find(|h| h.glyphs.iter().any(|g| glyphs.contains(g))) {
            Some(h) => *h = Hint { glyphs, label },
            None => self.items.push(Hint { glyphs, label }),
        }
        self
    }

    /// The right-aligned line.
    pub fn detail(&mut self, text: impl Into<String>) -> &mut Self {
        self.detail = Some(text.into());
        self
    }

    /// The shell adds nothing (a dialog says only what its own buttons do).
    pub fn without_shell(&mut self) -> &mut Self {
        self.no_shell = true;
        self
    }

    /// Some hint uses `g`.
    pub fn has(&self, g: Glyph) -> bool {
        self.items.iter().any(|h| h.glyphs.contains(&g))
    }

    /// The label of the hint that uses `g`.
    pub fn label_of(&self, g: Glyph) -> Option<&str> {
        self.items.iter().find(|h| h.glyphs.contains(&g)).map(|h| h.label.as_str())
    }

    /// The shell's hints after the section's, unless the section has its own for that button:
    /// B (back or close), LB / RB (sections), R3 (dock or centre).
    pub fn finish(&mut self, back: &str, dock: Option<&str>) {
        if self.no_shell {
            return;
        }
        if !self.has(Glyph::B) {
            self.add([Glyph::B], back);
        }
        if !self.has(Glyph::LB) {
            self.add([Glyph::LB, Glyph::RB], "Sections");
        }
        if let Some(d) = dock
            && !self.has(Glyph::R3)
        {
            self.add([Glyph::R3], d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_add_and_the_shell_finishes() {
        let mut h = Hints::default();
        h.add([Glyph::LeftRight], "Adjust").add([Glyph::X], "Reset to default").detail("Default 1.0× · range 1–5×");
        h.finish("Back", Some("Dock"));
        let labels: Vec<&str> = h.items.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["Adjust", "Reset to default", "Back", "Sections", "Dock"]);
        assert_eq!(h.detail.as_deref(), Some("Default 1.0× · range 1–5×"));
        assert_eq!(h.label_of(Glyph::RB), Some("Sections"));
        // A section's own B wins; replace swaps a hint in place.
        let mut h = Hints::default();
        h.add([Glyph::A], "Select").add([Glyph::B], "Back to game").replace([Glyph::A], "Details");
        h.finish("Close", None);
        let labels: Vec<&str> = h.items.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["Details", "Back to game", "Sections"]);
        assert!(!h.has(Glyph::R3));
        // A dialog: only its own.
        let mut h = Hints::default();
        h.add([Glyph::A], "Select").add([Glyph::B], "Cancel").without_shell();
        h.finish("Close", Some("Dock"));
        assert_eq!(h.items.len(), 2);
        assert_eq!(Glyph::LeftRight.label(), "◀ ▶");
    }
}
