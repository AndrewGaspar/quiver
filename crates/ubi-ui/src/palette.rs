//! Omarchy palette -> Quiver design tokens. A straight port of `design/mockup/src/theme.js`
//! (the mapping in PROPOSAL.md, "Omarchy theme"): read `colors.toml`, normalise the two
//! schemas Omarchy themes use, derive every token, and fix contrast where needed.
//!
//! Pure and GPUI-free so it can be unit-tested against the real theme files. Intermediate
//! colors are rounded to 8-bit after every step, exactly like the mock-up, so both produce
//! identical hex values.

use std::collections::BTreeMap;

/// An sRGB color with 8-bit channels and a separate alpha (1.0 = opaque).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Color {
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const BLACK: Color = Color::rgb(0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub const fn hex_u32(v: u32) -> Self {
        Self::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// `#rrggbb` or `rrggbb` (case-insensitive). `#rgb` is expanded.
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.trim().trim_start_matches('#');
        let h: String = match h.len() {
            3 => h.chars().flat_map(|c| [c, c]).collect(),
            6 => h.to_owned(),
            8 => h[..6].to_owned(), // #rrggbbaa: the alpha is ignored, like the mock
            _ => return None,
        };
        let v = u32::from_str_radix(&h, 16).ok()?;
        Some(Self::hex_u32(v))
    }

    #[cfg(test)]
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    /// Mix toward `other` by `t` (0 = self, 1 = other), rounded to 8-bit like the mock.
    pub fn mix(self, other: Color, t: f64) -> Color {
        let ch = |x: u8, y: u8| {
            let v = x as f64 + (y as f64 - x as f64) * t;
            v.clamp(0.0, 255.0).round() as u8
        };
        Color::rgb(ch(self.r, other.r), ch(self.g, other.g), ch(self.b, other.b))
    }

    /// WCAG relative luminance.
    pub fn luminance(&self) -> f64 {
        let lin = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio, 1..21.
    pub fn contrast(&self, other: &Color) -> f64 {
        let (x, y) = (self.luminance(), other.luminance());
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }
}

/// Move `c` toward white (dark themes) or black (light themes) in 5% steps until it
/// reaches `target` contrast against `bg`.
pub fn ensure(c: Color, bg: Color, target: f64, dark: bool) -> Color {
    let end = if dark { Color::WHITE } else { Color::BLACK };
    // Same float accumulation as the JS loop, so the step that wins is the same.
    let mut t = 0.0f64;
    while t <= 1.0001 {
        let m = c.mix(end, t);
        if m.contrast(&bg) >= target {
            return m;
        }
        t += 0.05;
    }
    end
}

/// Parse an Omarchy `colors.toml`: top-level `key = "value"` lines. Values are lowercased
/// and trimmed. Anything else (comments, tables, numbers) is ignored, like the mock-up.
pub fn parse_colors_toml(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((key, rest)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let rest = rest.trim_start();
        let quote = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => q,
            _ => continue,
        };
        let Some(end) = rest[1..].find(quote) else { continue };
        out.insert(key.to_owned(), rest[1..1 + end].trim().to_lowercase());
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

/// A palette in the current Omarchy schema, with every key the mapping needs filled in.
#[derive(Clone, Debug)]
pub struct Palette {
    pub mode: Mode,
    pub accent: Color,
    pub selection: Color,
    pub muted: Color,
    pub background: Color,
    pub dark_background: Color,
    pub darker_background: Color,
    pub lighter_background: Color,
    pub foreground: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
}

#[derive(Debug)]
pub struct PaletteError(pub String);

impl std::fmt::Display for PaletteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PaletteError {}

impl Palette {
    /// Normalise raw `colors.toml` keys. Legacy themes (`color0`..`color15`,
    /// `selection_background`, no `mode`) are mapped onto the named schema; the mode is
    /// inferred from the background's luminance; missing background steps are mixed.
    pub fn from_raw(raw: &BTreeMap<String, String>) -> Result<Self, PaletteError> {
        let mut r: BTreeMap<&str, String> = raw.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        if !r.contains_key("red") && r.contains_key("color1") {
            const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
            const BRIGHT: [&str; 8] = [
                "bright_black",
                "bright_red",
                "bright_green",
                "bright_yellow",
                "bright_blue",
                "bright_magenta",
                "bright_cyan",
                "bright_white",
            ];
            const COLORS: [&str; 16] = [
                "color0", "color1", "color2", "color3", "color4", "color5", "color6", "color7", "color8", "color9",
                "color10", "color11", "color12", "color13", "color14", "color15",
            ];
            for i in 0..8 {
                if !r.contains_key(NAMES[i])
                    && let Some(v) = raw.get(COLORS[i])
                {
                    r.insert(NAMES[i], v.clone());
                }
                if !r.contains_key(BRIGHT[i])
                    && let Some(v) = raw.get(COLORS[i + 8])
                {
                    r.insert(BRIGHT[i], v.clone());
                }
            }
            if !r.contains_key("muted")
                && let Some(v) = raw.get("color8")
            {
                r.insert("muted", v.clone());
            }
        }
        let get = |k: &str| -> Option<Color> { r.get(k).and_then(|v| Color::parse(v)) };
        let need = |k: &str| get(k).ok_or_else(|| PaletteError(format!("colors.toml has no usable '{k}'")));

        let bg = need("background")?;
        let fg = need("foreground")?;
        let accent = need("accent")?;
        let mode = match r.get("mode").map(String::as_str) {
            Some("light") => Mode::Light,
            Some("dark") => Mode::Dark,
            _ if bg.luminance() < 0.4 => Mode::Dark,
            _ => Mode::Light,
        };
        let dark = mode == Mode::Dark;
        let red = get("red").unwrap_or(Color::hex_u32(0xf06272));
        let yellow = get("yellow").unwrap_or(Color::hex_u32(0xf0b44c));
        Ok(Self {
            mode,
            accent,
            selection: get("selection").or_else(|| get("selection_background")).unwrap_or_else(|| bg.mix(accent, 0.3)),
            muted: get("muted").unwrap_or_else(|| fg.mix(bg, 0.5)),
            background: bg,
            dark_background: get("dark_background").unwrap_or_else(|| bg.mix(Color::BLACK, if dark { 0.25 } else { 0.04 })),
            darker_background: get("darker_background").unwrap_or_else(|| bg.mix(Color::BLACK, if dark { 0.45 } else { 0.08 })),
            lighter_background: get("lighter_background")
                .unwrap_or_else(|| if dark { bg.mix(fg, 0.13) } else { bg.mix(Color::BLACK, 0.15) }),
            foreground: fg,
            red,
            green: get("green").unwrap_or(Color::hex_u32(0x4fc48e)),
            yellow,
            orange: get("orange").unwrap_or_else(|| red.mix(yellow, 0.5)),
        })
    }

    pub fn parse(text: &str) -> Result<Self, PaletteError> {
        Self::from_raw(&parse_colors_toml(text))
    }
}

/// Every design token, as plain colors. Field names follow the CSS variables in the
/// mock-up (`ink-0` -> `ink0`, `acc-hi` -> `acc_hi`).
#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    pub dark: bool,
    pub ink0: Color,
    pub ink1: Color,
    pub ink2: Color,
    pub ink3: Color,
    pub ink4: Color,
    pub ink5: Color,
    pub line1: Color,
    pub line2: Color,
    pub tx1: Color,
    pub tx2: Color,
    pub tx3: Color,
    pub tx4: Color,
    pub acc: Color,
    pub acc_hi: Color,
    pub acc_ink: Color,
    pub acc_soft: Color,
    pub acc_line: Color,
    pub sel: Color,
    pub ok: Color,
    pub ok_soft: Color,
    pub warn: Color,
    pub warn_soft: Color,
    pub bad: Color,
    pub bad_soft: Color,
    pub who_gui: Color,
    pub who_cli: Color,
    pub who_agent: Color,
    pub who_sys: Color,
    pub focus: Color,
    pub focus_halo: Color,
    pub code_s: Color,
    pub scrim: Color,
}

const ACTORS_DARK: [u32; 4] = [0x7b93ff, 0x4cc9c0, 0xc38bff, 0x9aa3b5];
const ACTORS_LIGHT: [u32; 4] = [0x3d5bd9, 0x0f7f78, 0x7a35e0, 0x5f6878];

impl Tokens {
    /// "Quiver dark", the built-in fallback (the `:root` values in the mock-up's CSS).
    pub fn builtin() -> Self {
        let c = Color::hex_u32;
        Self {
            dark: true,
            ink0: c(0x0a0c10),
            ink1: c(0x0f1218),
            ink2: c(0x141821),
            ink3: c(0x1a1f2a),
            ink4: c(0x222836),
            ink5: c(0x2b3243),
            line1: c(0x1f2531),
            line2: c(0x2c3343),
            tx1: c(0xeceff5),
            tx2: c(0xa8afbf),
            tx3: c(0x727a8d),
            tx4: c(0x4f5667),
            acc: c(0x7b93ff),
            acc_hi: c(0xa6b5ff),
            acc_ink: c(0x0a0e22),
            acc_soft: c(0x7b93ff).with_alpha(0.14),
            acc_line: c(0x7b93ff).with_alpha(0.42),
            sel: c(0x222c4d),
            ok: c(0x4fc48e),
            ok_soft: c(0x4fc48e).with_alpha(0.14),
            warn: c(0xf0b44c),
            warn_soft: c(0xf0b44c).with_alpha(0.14),
            bad: c(0xf06272),
            bad_soft: c(0xf06272).with_alpha(0.14),
            who_gui: c(0x7b93ff),
            who_cli: c(0x4cc9c0),
            who_agent: c(0xc38bff),
            who_sys: c(0x9aa3b5),
            focus: c(0xa6b5ff),
            focus_halo: c(0xa6b5ff).with_alpha(0.35),
            code_s: c(0xf2c38b),
            scrim: c(0x06080c).with_alpha(0.72),
        }
    }

    /// Derive tokens from an Omarchy palette (PROPOSAL.md, "Mapping").
    pub fn derive(p: &Palette) -> Self {
        let dark = p.mode == Mode::Dark;
        let fg = p.foreground;
        let acc = p.accent;
        let (ink0, ink1, ink2, ink3, ink4, ink5) = if dark {
            (
                p.dark_background,
                p.background,
                p.background.mix(p.lighter_background, 0.35),
                p.background.mix(p.lighter_background, 0.65),
                p.lighter_background,
                p.lighter_background.mix(fg, 0.1),
            )
        } else {
            (
                p.darker_background,
                p.dark_background,
                p.background,
                p.background.mix(p.darker_background, 0.5),
                p.darker_background,
                p.lighter_background,
            )
        };
        let line1 = ink2.mix(fg, if dark { 0.08 } else { 0.1 });
        let line2 = ink2.mix(fg, if dark { 0.15 } else { 0.2 });
        let tx2 = ensure(fg.mix(ink1, 0.3), ink2, 7.0, dark);
        let tx3 = ensure(p.muted, ink2, 4.5, dark);
        let tx4 = fg.mix(ink1, 0.62);
        let acc_hi =
            if dark { ensure(acc.mix(Color::WHITE, 0.2), ink2, 4.5, true) } else { ensure(acc, ink2, 4.5, false) };
        let acc_ink =
            if acc.contrast(&Color::WHITE) >= acc.contrast(&p.darker_background) { Color::WHITE } else { p.darker_background };
        let sel = if fg.contrast(&p.selection) < 4.5 { ink1.mix(p.selection, 0.3) } else { p.selection };
        let ok = ensure(p.green, ink2, 3.0, dark);
        let warn = ensure(p.yellow, ink2, 3.0, dark);
        let bad = ensure(p.red, ink2, 3.0, dark);
        let who = if dark { ACTORS_DARK } else { ACTORS_LIGHT };
        let actor = |i: usize| ensure(Color::hex_u32(who[i]), ink2, 4.5, dark);
        Self {
            dark,
            ink0,
            ink1,
            ink2,
            ink3,
            ink4,
            ink5,
            line1,
            line2,
            tx1: fg,
            tx2,
            tx3,
            tx4,
            acc,
            acc_hi,
            acc_ink,
            acc_soft: acc.with_alpha(if dark { 0.16 } else { 0.12 }),
            acc_line: acc.with_alpha(0.45),
            sel,
            ok,
            ok_soft: ok.with_alpha(0.14),
            warn,
            warn_soft: warn.with_alpha(0.14),
            bad,
            bad_soft: bad.with_alpha(0.14),
            who_gui: actor(0),
            who_cli: actor(1),
            who_agent: actor(2),
            who_sys: actor(3),
            focus: if dark { acc.mix(Color::WHITE, 0.25) } else { acc },
            focus_halo: acc.with_alpha(0.35),
            code_s: ensure(p.orange, ink0, 4.5, dark),
            scrim: if dark { Color::hex_u32(0x06080c).with_alpha(0.72) } else { Color::hex_u32(0x0a0c10).with_alpha(0.45) },
        }
    }

    /// Name -> value for every token, in the mock-up's CSS names.
    #[cfg(test)]
    pub fn entries(&self) -> Vec<(&'static str, Color)> {
        vec![
            ("ink-0", self.ink0),
            ("ink-1", self.ink1),
            ("ink-2", self.ink2),
            ("ink-3", self.ink3),
            ("ink-4", self.ink4),
            ("ink-5", self.ink5),
            ("line-1", self.line1),
            ("line-2", self.line2),
            ("tx-1", self.tx1),
            ("tx-2", self.tx2),
            ("tx-3", self.tx3),
            ("tx-4", self.tx4),
            ("acc", self.acc),
            ("acc-hi", self.acc_hi),
            ("acc-ink", self.acc_ink),
            ("acc-soft", self.acc_soft),
            ("acc-line", self.acc_line),
            ("sel", self.sel),
            ("ok", self.ok),
            ("ok-soft", self.ok_soft),
            ("warn", self.warn),
            ("warn-soft", self.warn_soft),
            ("bad", self.bad),
            ("bad-soft", self.bad_soft),
            ("who-gui", self.who_gui),
            ("who-cli", self.who_cli),
            ("who-agent", self.who_agent),
            ("who-sys", self.who_sys),
            ("focus", self.focus),
            ("focus-halo", self.focus_halo),
            ("code-s", self.code_s),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JADE: &str = include_str!("../tests/fixtures/themes/jade-horse.toml");
    const BLACK_FLAG: &str = include_str!("../tests/fixtures/themes/black-flag-resynced.toml");
    const BRAILLE: &str = include_str!("../tests/fixtures/themes/meta-braille-light.toml");
    /// `deriveTokens` from theme.js run over the three fixtures (see tests/fixtures/README.md).
    const EXPECTED: &str = include_str!("../tests/fixtures/themes/expected-tokens.json");

    fn css(c: Color) -> String {
        if c.a >= 1.0 { c.hex() } else { format!("rgba({},{},{},{})", c.r, c.g, c.b, c.a) }
    }

    fn check(name: &str, text: &str) {
        let expected: serde_json::Value = serde_json::from_str(EXPECTED).unwrap();
        let expected = &expected[name];
        let tokens = Tokens::derive(&Palette::parse(text).unwrap());
        for (key, value) in tokens.entries() {
            let want = expected[key].as_str().unwrap_or_else(|| panic!("{name}: no expected value for {key}"));
            assert_eq!(css(value), want.replace(' ', ""), "{name}: token {key}");
        }
        assert_eq!(expected["color-scheme"] == "dark", tokens.dark, "{name}: mode");
    }

    #[test]
    fn jade_horse_matches_mockup() {
        check("jade-horse", JADE);
    }

    #[test]
    fn black_flag_legacy_schema_matches_mockup() {
        check("black-flag-resynced", BLACK_FLAG);
    }

    #[test]
    fn meta_braille_light_matches_mockup() {
        check("meta-braille-light", BRAILLE);
    }

    #[test]
    fn shipped_omarchy_themes_match_mockup() {
        check("catppuccin-latte", include_str!("../tests/fixtures/themes/catppuccin-latte.toml"));
        check("tokyo-night", include_str!("../tests/fixtures/themes/tokyo-night.toml"));
    }

    #[test]
    fn legacy_schema_is_mapped_and_mode_inferred() {
        let raw = parse_colors_toml(BLACK_FLAG);
        assert!(!raw.contains_key("mode"));
        let p = Palette::from_raw(&raw).unwrap();
        assert_eq!(p.mode, Mode::Dark);
        assert_eq!(p.red.hex(), "#b4392e"); // color1
        assert_eq!(p.muted.hex(), "#3a4a52"); // color8
        assert_eq!(p.selection.hex(), "#d4a24c"); // selection_background
        // Brass selection fails 4.5:1 against the text, so selected rows use a 30% tint.
        let t = Tokens::derive(&p);
        assert!(p.foreground.contrast(&p.selection) < 4.5);
        assert_eq!(t.sel, t.ink1.mix(p.selection, 0.3));
    }

    #[test]
    fn contrast_rules_hold() {
        for text in [JADE, BLACK_FLAG, BRAILLE] {
            let t = Tokens::derive(&Palette::parse(text).unwrap());
            assert!(t.tx2.contrast(&t.ink2) >= 7.0 || t.tx2 == Color::WHITE || t.tx2 == Color::BLACK);
            assert!(t.tx3.contrast(&t.ink2) >= 4.5);
            assert!(t.acc_hi.contrast(&t.ink2) >= 4.5);
            for c in [t.ok, t.warn, t.bad] {
                assert!(c.contrast(&t.ink2) >= 3.0);
            }
            for c in [t.who_gui, t.who_cli, t.who_agent, t.who_sys] {
                assert!(c.contrast(&t.ink2) >= 4.5);
            }
            assert!(t.sel.contrast(&t.tx1) >= 4.5 || t.sel != t.ink1);
        }
    }

    #[test]
    fn light_theme_uses_dark_background_for_app_and_background_for_cards() {
        let p = Palette::parse(BRAILLE).unwrap();
        let t = Tokens::derive(&p);
        assert!(!t.dark);
        assert_eq!(t.ink1, p.dark_background);
        assert_eq!(t.ink2, p.background);
        assert_eq!(t.focus, p.accent);
    }

    #[test]
    fn parser_ignores_comments_and_non_strings() {
        let raw = parse_colors_toml("# c\naccent = \"#ABCDEF\" # trailing\nopacity = 0.9\n[section]\nfg='#000000'\n");
        assert_eq!(raw.get("accent").map(String::as_str), Some("#abcdef"));
        assert_eq!(raw.get("fg").map(String::as_str), Some("#000000"));
        assert!(!raw.contains_key("opacity"));
    }

    #[test]
    fn missing_required_key_is_an_error() {
        assert!(Palette::parse("background = \"#000000\"").is_err());
    }

    #[test]
    fn ensure_moves_toward_white_on_dark() {
        let bg = Color::hex_u32(0x141821);
        let c = ensure(Color::hex_u32(0x333333), bg, 4.5, true);
        assert!(c.contrast(&bg) >= 4.5);
        assert!(c.r > 0x33);
    }
}
