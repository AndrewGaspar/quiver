//! Key bindings in play (design/MOD-CONFIG.md §7): every binding normalized to W3C
//! `KeyboardEvent.code`-style names with modifiers (`F9`, `Shift+Home`), so they compare.
//! Conflicts are shown, never resolved.

use quiver_api::options::{FormatSpec, KeyBinding, KeyConflict, OptionDef};

/// Windows virtual-key codes to names (the ones mods bind).
fn vk_name(code: u32) -> Option<String> {
    Some(match code {
        0x70..=0x87 => format!("F{}", code - 0x6f),
        0x30..=0x39 => format!("Digit{}", code - 0x30),
        0x41..=0x5a => format!("Key{}", (code as u8) as char),
        0x60..=0x69 => format!("Numpad{}", code - 0x60),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0d => "Enter".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x1b => "Escape".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "ArrowLeft".into(),
        0x26 => "ArrowUp".into(),
        0x27 => "ArrowRight".into(),
        0x28 => "ArrowDown".into(),
        0x2c => "PrintScreen".into(),
        0x2d => "Insert".into(),
        0x2e => "Delete".into(),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xc0 => "Backquote".into(),
        _ => return None,
    })
}

/// One key name (`f9`, `VK_F9`, `home`, `Shift_R`) in the normalized spelling.
pub fn normalize_name(name: &str) -> String {
    let n = name.trim();
    let u = n.to_ascii_uppercase();
    let u = u.strip_prefix("VK_").unwrap_or(&u);
    if let Some(num) = u.strip_prefix('F').and_then(|r| r.parse::<u32>().ok()) {
        return format!("F{num}");
    }
    let known = [
        ("HOME", "Home"),
        ("END", "End"),
        ("INSERT", "Insert"),
        ("DELETE", "Delete"),
        ("PRIOR", "PageUp"),
        ("PAGEUP", "PageUp"),
        ("NEXT", "PageDown"),
        ("PAGEDOWN", "PageDown"),
        ("TAB", "Tab"),
        ("ESCAPE", "Escape"),
        ("ESC", "Escape"),
        ("SPACE", "Space"),
        ("RETURN", "Enter"),
        ("ENTER", "Enter"),
        ("PAUSE", "Pause"),
        ("SNAPSHOT", "PrintScreen"),
        ("PRINT", "PrintScreen"),
        ("SCROLL", "ScrollLock"),
        ("SHIFT", "Shift"),
        ("SHIFT_L", "Shift"),
        ("SHIFT_R", "Shift"),
        ("CONTROL", "Ctrl"),
        ("CTRL", "Ctrl"),
        ("CONTROL_L", "Ctrl"),
        ("CONTROL_R", "Ctrl"),
        ("ALT", "Alt"),
        ("MENU", "Alt"),
        ("OEM_3", "Backquote"),
    ];
    if let Some((_, v)) = known.iter().find(|(k, _)| *k == u) {
        return (*v).to_owned();
    }
    if u.len() == 1 && u.chars().all(|c| c.is_ascii_uppercase()) {
        return format!("Key{u}");
    }
    if u.len() == 1 && u.chars().all(|c| c.is_ascii_digit()) {
        return format!("Digit{u}");
    }
    n.to_owned()
}

/// A chord (`Shift+F12`, `shift_r+f12`) normalized, modifiers first in a fixed order.
pub fn normalize_chord(chord: &str) -> String {
    let parts: Vec<String> = chord.split('+').map(normalize_name).filter(|p| !p.is_empty()).collect();
    let rank = |p: &str| match p {
        "Ctrl" => 0,
        "Alt" => 1,
        "Shift" => 2,
        "Super" => 3,
        _ => 4,
    };
    let mut parts = parts;
    parts.sort_by_key(|p| rank(p));
    parts.join("+")
}

/// A key option's value as a normalized chord (`None`: unbound).
pub fn normalize_option(def: &OptionDef, text: &str) -> Option<String> {
    let t = text.trim();
    let none = def.none.as_deref().unwrap_or("none");
    if t.is_empty() || t.eq_ignore_ascii_case(none) {
        return None;
    }
    let format = match &def.format {
        Some(FormatSpec::Name(f)) => f.as_str(),
        _ => "vk-name",
    };
    match format {
        "vk-hex" => u32::from_str_radix(t.trim_start_matches("0x").trim_start_matches("0X"), 16).ok().and_then(vk_name).or_else(|| Some(t.to_owned())),
        // ReShade: "vk,ctrl,shift,alt".
        "reshade" => {
            let n: Vec<u32> = t.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            let key = vk_name(*n.first()?)?;
            let mut chord = Vec::new();
            for (i, m) in ["Ctrl", "Shift", "Alt"].iter().enumerate() {
                if n.get(i + 1).is_some_and(|&v| v != 0) {
                    chord.push((*m).to_owned());
                }
            }
            chord.push(key);
            Some(normalize_chord(&chord.join("+")))
        }
        _ => Some(normalize_chord(t)),
    }
}

/// Bindings Quiver itself and well-known tools use.
pub fn known_bindings() -> Vec<KeyBinding> {
    let k = |key: &str, owner: &str, label: &str, kind: &str| KeyBinding {
        key: normalize_chord(key),
        raw: key.to_owned(),
        owner: owner.to_owned(),
        label: label.to_owned(),
        kind: kind.to_owned(),
        ..Default::default()
    };
    vec![
        k("Shift+F3", "Quiver HUD", "Show or hide the performance HUD", "runtime"),
        k("Ctrl+Shift+F3", "Quiver HUD", "Next HUD preset", "runtime"),
        k("Shift+Tab", "Steam", "Steam overlay (Steam launches)", "known"),
        k("F12", "Steam", "Steam screenshot (Steam launches)", "known"),
    ]
}

/// Pairs of bindings on the same chord.
pub fn conflicts(bindings: &[KeyBinding]) -> Vec<KeyConflict> {
    let mut out: Vec<KeyConflict> = Vec::new();
    for (i, b) in bindings.iter().enumerate() {
        match out.iter_mut().find(|c| c.key == b.key) {
            Some(c) => c.bindings.push(i),
            None => out.push(KeyConflict { key: b.key.clone(), bindings: vec![i] }),
        }
    }
    out.retain(|c| c.bindings.len() > 1 && c.bindings.iter().any(|&i| bindings[i].kind == "option" || bindings[i].kind == "runtime"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::options::OptionType;

    #[test]
    fn normalizes_encodings() {
        let vk = OptionDef { ty: OptionType::Key, ..Default::default() };
        assert_eq!(normalize_option(&vk, "f9").as_deref(), Some("F9"));
        assert_eq!(normalize_option(&vk, "none"), None);
        let hex = OptionDef { format: Some(FormatSpec::Name("vk-hex".into())), ..vk.clone() };
        assert_eq!(normalize_option(&hex, "0x78").as_deref(), Some("F9"));
        let rs = OptionDef { format: Some(FormatSpec::Name("reshade".into())), ..vk.clone() };
        assert_eq!(normalize_option(&rs, "36,0,0,0").as_deref(), Some("Home"));
        assert_eq!(normalize_option(&rs, "123,0,1,0").as_deref(), Some("Shift+F12"));
        assert_eq!(normalize_chord("shift_r+F12"), "Shift+F12");
        assert_eq!(normalize_chord("F3+ctrl+shift"), "Ctrl+Shift+F3");
        let mut b = known_bindings();
        b.push(KeyBinding { key: "F12".into(), kind: "option".into(), ..Default::default() });
        let c = conflicts(&b);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, "F12");
    }
}
