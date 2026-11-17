//! Guessing a schema from an INI-like file (design/MOD-CONFIG.md, "Inference"): good enough
//! for "Other keys" and as a starting point for authors (`options infer`). Everything guessed
//! is `apply = "restart"` (unknown).

use std::collections::BTreeMap;

use serde_json::Value;
use quiver_api::options::{EnumValue, FileDef, FileFormat, FileOwner, Level, OptionDef, OptionType, PageDef, Schema};

/// Guess one key's description from its value and the comment lines right above it.
pub fn guess(key: &str, value: &str, comment: &str) -> OptionDef {
    let v = value.trim();
    let lc = comment.to_lowercase();
    let mut d = OptionDef { ty: OptionType::String, ..Default::default() };
    // BepInEx-style metadata is exact.
    let meta = |name: &str| comment.lines().find_map(|l| l.trim().strip_prefix(name).map(|r| r.trim().to_owned()));
    if let Some(t) = meta("Setting type:") {
        d.ty = match t.as_str() {
            "Boolean" => OptionType::Bool,
            "Int32" | "Int64" | "Byte" | "UInt32" => OptionType::Int,
            "Single" | "Double" => OptionType::Float,
            "KeyboardShortcut" | "KeyCode" => OptionType::Key,
            _ => OptionType::String,
        };
        if d.ty == OptionType::Bool {
            d.format = Some(quiver_api::options::FormatSpec::Name("truefalse".into()));
        }
        if let Some(r) = meta("# Acceptable value range: From").or_else(|| meta("Acceptable value range: From"))
            && let Some((a, b)) = r.split_once(" to ")
        {
            d.min = a.trim().parse().ok();
            d.max = b.trim().parse().ok();
        }
        if let Some(list) = meta("Acceptable values:") {
            d.ty = OptionType::Enum;
            d.values = list.split(',').map(|s| EnumValue { value: Value::String(s.trim().to_owned()), ..Default::default() }).collect();
        }
    } else if let Some(choices) = enum_choices(comment, v) {
        d.ty = OptionType::Enum;
        d.values = choices.into_iter().map(|c| EnumValue { value: Value::String(c), ..Default::default() }).collect();
    } else if matches!(v, "0" | "1") && !lc.contains("count") {
        d.ty = OptionType::Bool;
    } else if matches!(v.to_ascii_lowercase().as_str(), "true" | "false") {
        d.ty = OptionType::Bool;
        let name = if v.starts_with(char::is_uppercase) { "TrueFalse" } else { "truefalse" };
        d.format = Some(quiver_api::options::FormatSpec::Name(name.into()));
    } else if matches!(v.to_ascii_lowercase().as_str(), "on" | "off") {
        d.ty = OptionType::Bool;
        d.format = Some(quiver_api::options::FormatSpec::Name("onoff".into()));
    } else if v.parse::<i64>().is_ok() {
        d.ty = OptionType::Int;
    } else if v.parse::<f64>().is_ok() {
        d.ty = OptionType::Float;
    } else if is_fkey(v) || v.to_ascii_uppercase().starts_with("VK_") {
        d.ty = OptionType::Key;
        d.format = Some(quiver_api::options::FormatSpec::Name("vk-name".into()));
    } else if v.starts_with('#') && v.len() == 7 && v[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        d.ty = OptionType::Color;
        d.format = Some(quiver_api::options::FormatSpec::Name("#rrggbb".into()));
    }
    let help: String = comment.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains("Setting type:") && !l.contains("Default value:")).collect::<Vec<_>>().join(" ");
    if !help.is_empty() {
        d.help = Some(help);
    }
    let words = format!("{key} {lc}");
    if ["debug", "experiment", "trace", "diagnos"].iter().any(|w| words.to_lowercase().contains(w)) {
        d.level = Level::Debug;
    }
    d
}

fn is_fkey(v: &str) -> bool {
    let u = v.to_ascii_uppercase();
    u.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| (1..=24).contains(&n))
}

/// `a | b | c` (or `a, b or c`) in a comment, when the value is one of them.
fn enum_choices(comment: &str, value: &str) -> Option<Vec<String>> {
    for line in comment.lines() {
        let l = line.trim();
        let parts: Vec<String> = if l.contains('|') {
            l.split('|').map(|p| p.trim().trim_end_matches(['.', ',', ';', ')']).trim_start_matches('(').to_owned()).collect()
        } else if l.contains(" or ") && l.contains(',') {
            l.replace(" or ", ",").split(',').map(|p| p.trim().to_owned()).collect()
        } else {
            continue;
        };
        // Each choice is the first word of its part ("boost (on + keep …)" is "boost").
        // The first part may carry the description ("Reflex: off | on"): its last word.
        let mixed: Vec<String> = parts
            .iter()
            .enumerate()
            .filter_map(|(i, p)| if i == 0 { p.split_whitespace().last() } else { p.split_whitespace().next() })
            .map(|w| w.trim_end_matches([':', ',']).to_owned())
            .collect();
        let firsts: Vec<String> = parts.iter().filter_map(|p| p.split_whitespace().next()).map(|w| w.trim_end_matches(':').to_owned()).collect();
        for cand in [mixed, firsts] {
            if cand.len() >= 2 && cand.iter().all(|c| !c.is_empty() && c.len() <= 24) && cand.iter().any(|c| c.eq_ignore_ascii_case(value)) {
                return Some(cand);
            }
        }
    }
    None
}

/// Every key of an INI text with its comment block, value and section, in order.
pub fn ini_entries(text: &str) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    let (mut section, mut comment) = (String::new(), String::new());
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            comment.clear();
        } else if let Some(c) = t.strip_prefix(';').or_else(|| t.strip_prefix('#')) {
            comment.push_str(c.trim());
            comment.push('\n');
        } else if t.starts_with('[') && t.contains(']') {
            section = t[1..t.rfind(']').unwrap()].trim().to_owned();
            comment.clear();
        } else if let Some((k, v)) = t.split_once('=') {
            out.push((section.clone(), k.trim().to_owned(), v.trim().to_owned(), std::mem::take(&mut comment)));
        }
    }
    out
}

/// A whole schema guessed from one INI file (`path` relative to the install folder).
pub fn infer_schema(surface: &str, title: &str, path: &str, text: &str) -> Schema {
    let mut s = Schema {
        format: quiver_api::options::FORMAT,
        surface: surface.to_owned(),
        title: Some(title.to_owned()),
        files: [("ini".to_owned(), FileDef { path: path.to_owned(), format: FileFormat::Ini, owner: FileOwner::Program, ..Default::default() })].into(),
        ..Default::default()
    };
    let mut pages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for (section, key, value, comment) in ini_entries(text) {
        let id = if section.is_empty() { key.clone() } else { format!("{section}.{key}") };
        let mut d = guess(&key, &value, &comment);
        if section.contains('.') || key.contains('.') {
            d.section = Some(section.clone());
            d.key = Some(key.clone());
        }
        let id = id.replace(' ', "_");
        if s.options.contains_key(&id) {
            continue;
        }
        s.options.insert(id.clone(), d);
        if !pages.contains_key(&section) {
            order.push(section.clone());
        }
        pages.entry(section).or_default().push(id);
    }
    for sec in order {
        let options = pages.remove(&sec).unwrap_or_default();
        let id = if sec.is_empty() { "general".to_owned() } else { sec.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-") };
        s.pages.push(PageDef { title: if sec.is_empty() { "General".into() } else { sec.clone() }, id, options, groups: Vec::new() });
    }
    s
}

/// A schema as TOML text (for `options infer` and local schemas).
pub fn to_toml(s: &Schema) -> String {
    let v = serde_json::to_value(s).unwrap_or_default();
    let mut text = toml::to_string_pretty(&v).unwrap_or_default();
    text.insert_str(0, "# Guessed by `quiver mods <game> options infer`: check every type, range and `apply`.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_from_aco_dlss_style_comments() {
        let ini = "; header\n[upscaler]\nenabled=1\n; dlss | bilinear\nbackend=dlss\n; key (F1..F24, or none)\ntoggle_key=F10\nmip_bias_offset=0.5\nname=x y\n\n[reflex]\n; NVIDIA Reflex: off | on | boost (on + keep GPU clocks high)\nmode=on\n; 1 = log which threads read input (diagnostics)\nprobe=0\nfps_limit=160\n";
        let s = infer_schema("aco-dlss", "DLSS", "aco_dlss.ini", ini);
        let o = |id: &str| &s.options[id];
        assert_eq!(o("upscaler.enabled").ty, OptionType::Bool);
        assert_eq!(o("upscaler.backend").ty, OptionType::Enum);
        assert_eq!(o("upscaler.backend").values.len(), 2);
        assert_eq!(o("upscaler.toggle_key").ty, OptionType::Key);
        assert_eq!(o("upscaler.mip_bias_offset").ty, OptionType::Float);
        assert_eq!(o("upscaler.name").ty, OptionType::String);
        assert_eq!(o("reflex.mode").ty, OptionType::Enum);
        assert_eq!(o("reflex.mode").values.iter().map(|v| v.value.as_str().unwrap()).collect::<Vec<_>>(), ["off", "on", "boost"]);
        assert_eq!(o("reflex.probe").level, Level::Debug);
        assert_eq!(o("reflex.fps_limit").ty, OptionType::Int);
        assert_eq!(o("reflex.mode").help.as_deref(), Some("NVIDIA Reflex: off | on | boost (on + keep GPU clocks high)"));
        assert_eq!(s.pages.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["upscaler", "reflex"]);
        // Round trip through TOML.
        let back = super::super::schema::parse(&to_toml(&s), false).unwrap();
        assert_eq!(back.options.len(), s.options.len());
    }

    #[test]
    fn bepinex_metadata_is_exact() {
        let ini = "[General]\n## Spawn more\n# Setting type: Int32\n# Default value: 1\n# Acceptable value range: From 0 to 5\nCount = 2\n# Setting type: Boolean\nOn = true\n";
        let s = infer_schema("x", "X", "BepInEx/config/x.cfg", ini);
        let c = &s.options["General.Count"];
        assert_eq!((c.ty, c.min, c.max), (OptionType::Int, Some(0.0), Some(5.0)));
        assert_eq!(s.options["General.On"].ty, OptionType::Bool);
    }
}
