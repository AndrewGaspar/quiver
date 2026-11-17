//! Pure helpers for the Options screens (tested below): who made a change, what an apply
//! mode and an outcome say, staged edits as daemon ops, filtering, the function-key strip.

use std::collections::BTreeMap;

use quiver_api::options::{
    Apply, KeysReport, Level, LiveChannel, OpOutcome, OptionDef, OptionOp, OptionState, OptionType, Outcome, Pending, SurfaceState, ValueSource,
    as_f64, same_value, scalar_text,
};
use serde_json::Value;

/// Surface key and option id.
pub type RowKey = (String, String);

/// A change waiting in the unsaved bar.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// A value as typed or picked (typed JSON or text; the daemon normalizes it).
    Set(Value),
    /// Back to the default (deletes the profile's value).
    Reset,
}

/// Who made a change, from an options revision's `source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Who {
    App,
    Cli,
    /// The in-game overlay (Sight).
    InGame,
    /// The program wrote its own file (the game's menu, ReShade's UI).
    Game,
    /// A hand edit of a file Quiver renders.
    File,
    /// A mod over the SDK (its own menu).
    Mod(String),
    System,
    Agent(String),
}

pub fn who(source: &str) -> Who {
    match source {
        "gui" | "app" => Who::App,
        "cli" => Who::Cli,
        "in-game" | "sight" => Who::InGame,
        "game" => Who::Game,
        "file" => Who::File,
        "quiverd" | "system" | "daemon" | "" => Who::System,
        s if s.starts_with("mod:") => Who::Mod(s[4..].to_owned()),
        s => Who::Agent(s.to_owned()),
    }
}

impl Who {
    /// The badge text.
    pub fn badge(&self) -> String {
        match self {
            Who::App => "app".into(),
            Who::Cli => "CLI".into(),
            Who::InGame => "in game".into(),
            Who::Game => "game wrote it".into(),
            Who::File => "file edit".into(),
            Who::Mod(_) => "mod's menu".into(),
            Who::System => "quiverd".into(),
            Who::Agent(a) => format!("agent {a}"),
        }
    }

    /// Who, in a sentence ("you, in the app").
    pub fn phrase(&self) -> String {
        match self {
            Who::App => "you, in the app".into(),
            Who::Cli => "you, in a terminal".into(),
            Who::InGame => "you, in game".into(),
            Who::Game => "the game".into(),
            Who::File => "an edit of the file".into(),
            Who::Mod(m) => format!("{m}'s own menu"),
            Who::System => "Quiver".into(),
            Who::Agent(a) => format!("the agent {a}"),
        }
    }

    /// The program or a file changed it (an import, MOD-CONFIG §4).
    pub fn is_import(&self) -> bool {
        matches!(self, Who::Game | Who::File | Who::Mod(_))
    }
}

/// Visual tone for badges and outcome pills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Muted,
    Accent,
}

/// An option's apply badge ("LIVE", "RESTART", "AT SWAPCHAIN CREATION").
pub fn apply_badge(def: &OptionDef) -> (String, Tone) {
    match def.apply {
        Apply::Live => match &def.latency {
            Some(l) => (format!("LIVE · {}", short_latency(l)).to_uppercase(), Tone::Ok),
            None => ("LIVE".into(), Tone::Ok),
        },
        Apply::At => (format!("AT {}", short_moment(def.at.as_deref().unwrap_or("a moment"))).to_uppercase(), Tone::Warn),
        Apply::Restart => ("RESTART".into(), Tone::Muted),
        Apply::Launch => ("NEXT LAUNCH".into(), Tone::Muted),
    }
}

/// "about a minute" → "~1 min".
pub fn short_latency(l: &str) -> String {
    let l = l.trim();
    match l {
        "about a minute" => "~1 min".into(),
        "about 10 seconds" => "~10 s".into(),
        _ => l.strip_prefix("about ").map(|r| format!("~{r}")).unwrap_or_else(|| l.to_owned()),
    }
}

/// The moment without its explanation: "sampler creation (the game …)" → "sampler creation".
pub fn short_moment(at: &str) -> String {
    at.split(" (").next().unwrap_or(at).trim().to_owned()
}

/// What one operation's outcome says, for the row and the result line. Never optimistic.
pub fn outcome_text(o: &Outcome) -> (String, Tone) {
    match o {
        Outcome::AppliedLive { channel: LiveChannel::Sdk, .. } => ("Applied live · the mod confirmed".into(), Tone::Ok),
        Outcome::AppliedLive { latency: Some(l), .. } => (format!("Applied live · the mod picks it up within {l}"), Tone::Ok),
        Outcome::AppliedLive { .. } => ("Applied live".into(), Tone::Ok),
        Outcome::SavedRestart { running: true } => ("Saved · needs a restart".into(), Tone::Warn),
        Outcome::SavedRestart { running: false } => ("Saved · applies next launch".into(), Tone::Muted),
        Outcome::SavedAt { at } => (format!("Saved · applies at {}", short_moment(at)), Tone::Warn),
        Outcome::SavedPending { detail } => (format!("Saved · {detail}"), Tone::Warn),
        Outcome::Dormant { reason } => (format!("Kept, not applied: {reason}"), Tone::Warn),
        Outcome::Rejected { reason } => (format!("Rejected: {reason}"), Tone::Bad),
        Outcome::Unchanged => ("Unchanged".into(), Tone::Muted),
        Outcome::TimedOut => ("Saved · the mod didn't confirm in time".into(), Tone::Warn),
    }
}

/// One line for a whole save: "r16 · 2 applied live · 1 needs a restart · 1 rejected".
pub fn outcome_summary(revision: u32, outcomes: &[OpOutcome]) -> String {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let order = ["applied live", "needs a restart", "applies next launch", "applies later", "waits for the mod", "kept, not applied", "unchanged", "rejected"];
    for o in outcomes {
        let k = match &o.outcome {
            Outcome::AppliedLive { .. } => "applied live",
            Outcome::SavedRestart { running: true } => "needs a restart",
            Outcome::SavedRestart { running: false } => "applies next launch",
            Outcome::SavedAt { .. } => "applies later",
            Outcome::SavedPending { .. } | Outcome::TimedOut => "waits for the mod",
            Outcome::Dormant { .. } => "kept, not applied",
            Outcome::Unchanged => "unchanged",
            Outcome::Rejected { .. } => "rejected",
        };
        *counts.entry(k).or_default() += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    if revision > 0 {
        parts.push(format!("Saved as r{revision}"));
    } else {
        parts.push("Nothing saved".into());
    }
    for k in order {
        if let Some(n) = counts.get(k) {
            parts.push(format!("{n} {k}"));
        }
    }
    parts.join(" · ")
}

/// A saved value the running game hasn't picked up yet.
pub fn pending_text(p: &Pending) -> Option<String> {
    match p {
        Pending::None => None,
        Pending::Restart => Some("Saved · applies when the game restarts".into()),
        Pending::At { at } => Some(format!("Saved · applies at {}", short_moment(at))),
        Pending::Reread => Some("Saved · applies when the mod re-reads its file".into()),
    }
}

/// The value a row shows: the staged edit's, else the effective one.
pub fn draft_value(st: &OptionState, edit: Option<&Edit>) -> Value {
    match edit {
        Some(Edit::Set(v)) => st.def.normalize(v).unwrap_or_else(|_| v.clone()),
        Some(Edit::Reset) => st.default.clone(),
        None => st.value.clone(),
    }
}

/// The profile changed it (or the session did), as opposed to the shipped default.
pub fn is_changed(st: &OptionState) -> bool {
    !matches!(st.from, ValueSource::Default)
}

/// Staged edits as the daemon's ops, in a stable order. Rows of "Other keys" are keyed
/// `raw:<binding>`.
pub fn ops_of(staged: &BTreeMap<RowKey, Edit>) -> Vec<OptionOp> {
    staged
        .iter()
        .map(|((surface, id), e)| match (id.strip_prefix("raw:"), e) {
            (Some(b), Edit::Set(v)) => OptionOp::Raw { surface: surface.clone(), binding: b.to_owned(), value: Some(scalar_text(v)) },
            (Some(b), Edit::Reset) => OptionOp::Raw { surface: surface.clone(), binding: b.to_owned(), value: None },
            (None, Edit::Set(v)) => OptionOp::Set { surface: surface.clone(), id: id.clone(), value: v.clone() },
            (None, Edit::Reset) => OptionOp::Reset { surface: surface.clone(), id: id.clone() },
        })
        .collect()
}

/// Whether an option shows at this level (`basic` < `advanced` < `debug`).
pub fn at_level(def: &OptionDef, level: Level) -> bool {
    def.level <= level
}

/// Search over labels, ids, help and the value as shown.
pub fn matches_search(q: &str, id: &str, st: &OptionState) -> bool {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = [id.to_lowercase(), st.def.label_or(id).to_lowercase(), st.def.help.clone().unwrap_or_default().to_lowercase(), st.def.display(&st.value).to_lowercase()];
    q.split_whitespace().all(|w| hay.iter().any(|h| h.contains(w)))
}

/// Visibility with staged values taken into account (`when` holds on what will be saved).
pub fn visible_with(s: &SurfaceState, staged: &BTreeMap<RowKey, Edit>, def: &OptionDef) -> bool {
    let get = |id: &str| -> Option<Value> {
        let st = s.options.get(id)?;
        Some(draft_value(st, staged.get(&(s.key.clone(), id.to_owned()))))
    };
    def.visible(&get)
}

/// A number one step up or down, clamped and rounded to the option's precision.
pub fn step_number(def: &OptionDef, v: &Value, dir: f64) -> Value {
    let cur = as_f64(v).filter(|_| v.is_number()).unwrap_or_else(|| def.min.unwrap_or(0.0));
    let step = def.step.unwrap_or(match def.ty {
        OptionType::Int => 1.0,
        _ => match (def.min, def.max) {
            (Some(a), Some(b)) if b > a => ((b - a) / 20.0).max(0.01),
            _ => 0.1,
        },
    });
    let mut n = cur + dir * step;
    if let Some(m) = def.min {
        n = n.max(m);
    }
    if let Some(m) = def.max {
        n = n.min(m);
    }
    if def.ty == OptionType::Int {
        return Value::from(n.round() as i64);
    }
    let p = def.precision.unwrap_or(3).min(6) as i32;
    let f = 10f64.powi(p);
    serde_json::Number::from_f64((n * f).round() / f).map(Value::Number).unwrap_or(Value::Null)
}

/// Where a number sits on its range (0..1), when it has one.
pub fn fraction(def: &OptionDef, v: &Value) -> Option<f32> {
    let (a, b) = (def.min?, def.max?);
    let x = as_f64(v).filter(|_| v.is_number() && def.special_of(v).is_none())?;
    (b > a).then(|| ((x - a) / (b - a)).clamp(0.0, 1.0) as f32)
}

/// Keys a key option may take, when its `allowed` is a short list or range (`F1-F24`).
pub fn allowed_keys(def: &OptionDef) -> Option<Vec<String>> {
    let a = def.allowed.as_deref()?;
    let mut out = Vec::new();
    for part in a.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part.split_once('-') {
            Some((x, y)) => {
                let split = |s: &str| {
                    let s = s.trim().to_ascii_uppercase();
                    let i = s.find(|c: char| c.is_ascii_digit())?;
                    Some((s[..i].to_owned(), s[i..].parse::<u32>().ok()?))
                };
                let ((px, nx), (py, ny)) = (split(x)?, split(y)?);
                if px != py || ny < nx || ny - nx > 40 {
                    return None;
                }
                out.extend((nx..=ny).map(|n| format!("{px}{n}")));
            }
            None => out.push(part.to_owned()),
        }
    }
    (!out.is_empty() && out.len() <= 48).then_some(out)
}

/// The function keys to show (F1–F12, and F13–F24 when anything uses them), with the
/// indexes of the bindings on each (any modifiers).
pub fn function_keys(r: &KeysReport) -> Vec<(String, Vec<usize>)> {
    let base_of = |k: &str| k.rsplit('+').next().unwrap_or(k).to_ascii_uppercase();
    let high = r.bindings.iter().any(|b| base_of(&b.key).strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n > 12));
    let n = if high { 24 } else { 12 };
    (1..=n)
        .map(|i| {
            let k = format!("F{i}");
            let on: Vec<usize> = r.bindings.iter().enumerate().filter(|(_, b)| base_of(&b.key) == k).map(|(j, _)| j).collect();
            (k, on)
        })
        .collect()
}

/// The modifiers of a chord, for the strip ("Shift: ").
pub fn chord_prefix(key: &str) -> String {
    let parts: Vec<&str> = key.split('+').collect();
    if parts.len() > 1 { format!("{}: ", parts[..parts.len() - 1].join("+")) } else { String::new() }
}

/// A change line ("DLSS world.npc_density 1 × → 2 ×") split for a diff view: what, from, to.
pub fn split_change(line: &str) -> (String, Option<(String, String)>) {
    let Some((left, to)) = line.split_once(" → ") else { return (line.to_owned(), None) };
    // "<title> <id> <from…>": the id is the first word with a dot or the second word.
    let words: Vec<&str> = left.split(' ').collect();
    let id_at = words.iter().position(|w| w.contains('.')).unwrap_or(1.min(words.len().saturating_sub(1)));
    let what = words[..=id_at].join(" ");
    let from = words[id_at + 1..].join(" ");
    (what, Some((from.trim().to_owned(), to.trim().to_owned())))
}

/// Count of options a profile changes on a page (or a whole surface), staged edits included.
pub fn changed_on(s: &SurfaceState, ids: &[String], staged: &BTreeMap<RowKey, Edit>) -> usize {
    ids.iter()
        .filter(|id| {
            let k = (s.key.clone(), (*id).clone());
            staged.contains_key(&k) || s.options.get(*id).is_some_and(is_changed)
        })
        .count()
}

/// Every option id on a page, groups included, in order.
pub fn page_ids(s: &SurfaceState, page: &str) -> Vec<String> {
    s.pages.iter().find(|p| p.id == page).map(|p| p.groups.iter().flat_map(|g| g.options.iter().cloned()).collect()).unwrap_or_default()
}

/// The page an option is on.
pub fn page_of(s: &SurfaceState, id: &str) -> Option<String> {
    s.pages.iter().find(|p| p.groups.iter().any(|g| g.options.iter().any(|o| o == id))).map(|p| p.id.clone())
}

/// The value as a short text for change lists.
pub fn short_value(def: &OptionDef, v: &Value) -> String {
    if v.is_null() {
        return "—".into();
    }
    match (def.ty, v) {
        (OptionType::Enum, _) => def.values.iter().find(|e| same_value(&e.value, v)).and_then(|e| e.label.clone()).unwrap_or_else(|| scalar_text(v)),
        _ => def.display(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::options::{EnumValue, KeyBinding, Mark};
    use serde_json::json;

    fn def(ty: OptionType) -> OptionDef {
        OptionDef { ty, ..Default::default() }
    }

    #[test]
    fn actors() {
        assert_eq!(who("gui"), Who::App);
        assert_eq!(who("in-game"), Who::InGame);
        assert_eq!(who("aco-dlss"), Who::Agent("aco-dlss".into()));
        assert_eq!(who("mod:aco-dlss"), Who::Mod("aco-dlss".into()));
        assert!(who("game").is_import() && who("file").is_import() && !who("gui").is_import());
        assert_eq!(who("claude-code").badge(), "agent claude-code");
    }

    #[test]
    fn badges_and_outcomes() {
        let mut d = def(OptionType::Float);
        d.apply = Apply::Live;
        d.latency = Some("about a minute".into());
        assert_eq!(apply_badge(&d).0, "LIVE · ~1 MIN");
        d.apply = Apply::At;
        d.at = Some("sampler creation (the game creates its samplers while loading)".into());
        assert_eq!(apply_badge(&d), ("AT SAMPLER CREATION".into(), Tone::Warn));
        assert_eq!(outcome_text(&Outcome::SavedRestart { running: true }).0, "Saved · needs a restart");
        assert_eq!(outcome_text(&Outcome::Rejected { reason: "too big".into() }).1, Tone::Bad);
        let o = |outcome| OpOutcome { surface: "s".into(), id: "i".into(), outcome, before: None, after: None };
        let s = outcome_summary(16, &[o(Outcome::AppliedLive { channel: LiveChannel::File, latency: None }), o(Outcome::AppliedLive { channel: LiveChannel::File, latency: None }), o(Outcome::SavedRestart { running: true }), o(Outcome::Rejected { reason: "x".into() })]);
        assert_eq!(s, "Saved as r16 · 2 applied live · 1 needs a restart · 1 rejected");
        assert_eq!(outcome_summary(0, &[o(Outcome::Unchanged)]), "Nothing saved · 1 unchanged");
    }

    #[test]
    fn stepping_numbers() {
        let d = OptionDef { min: Some(1.0), max: Some(5.0), step: Some(0.5), ..def(OptionType::Float) };
        assert_eq!(step_number(&d, &json!(4.5), 1.0), json!(5.0));
        assert_eq!(step_number(&d, &json!(5.0), 1.0), json!(5.0), "clamped");
        assert_eq!(step_number(&d, &json!(1.0), -1.0), json!(1.0));
        let i = OptionDef { min: Some(30.0), max: Some(500.0), special: vec![Mark { value: json!(0), label: "Not set".into() }], ..def(OptionType::Int) };
        assert_eq!(step_number(&i, &json!(165), 1.0), json!(166));
        assert_eq!(fraction(&i, &json!(0)), None, "a sentinel isn't on the track");
        assert!((fraction(&d, &json!(3.0)).unwrap() - 0.5).abs() < 1e-6);
        let p = OptionDef { min: Some(0.33), max: Some(1.0), step: Some(0.01), precision: Some(3), ..def(OptionType::Float) };
        assert_eq!(step_number(&p, &json!(0.58), 1.0), json!(0.59));
    }

    #[test]
    fn keys_allowed_and_strip() {
        let k = OptionDef { allowed: Some("F1-F24".into()), ..def(OptionType::Key) };
        let keys = allowed_keys(&k).unwrap();
        assert_eq!((keys.len(), keys[8].as_str()), (24, "F9"));
        assert!(allowed_keys(&def(OptionType::Key)).is_none());
        let b = |key: &str| KeyBinding { key: key.into(), ..Default::default() };
        let r = KeysReport { bindings: vec![b("F9"), b("Shift+F2"), b("Shift+Tab")], ..Default::default() };
        let strip = function_keys(&r);
        assert_eq!(strip.len(), 12);
        assert_eq!(strip[8].1, vec![0]);
        assert_eq!(strip[1].1, vec![1]);
        assert_eq!(chord_prefix("Shift+F2"), "Shift: ");
        let r = KeysReport { bindings: vec![b("F13")], ..Default::default() };
        assert_eq!(function_keys(&r).len(), 24);
    }

    #[test]
    fn staged_edits_and_visibility() {
        let mut s = SurfaceState { key: "m/aco-dlss".into(), ..Default::default() };
        let mode = OptionDef {
            values: vec![EnumValue { value: json!("off"), ..Default::default() }, EnumValue { value: json!("d3d12"), ..Default::default() }],
            ..def(OptionType::Enum)
        };
        s.options.insert("fg.mode".into(), OptionState { def: mode, value: json!("d3d12"), default: json!("d3d12"), ..Default::default() });
        let key = OptionDef { when: Some([("fg.mode".to_owned(), quiver_api::options::WhenCond::One(json!("d3d12")))].into()), ..def(OptionType::Key) };
        let mut staged = BTreeMap::new();
        assert!(visible_with(&s, &staged, &key));
        staged.insert(("m/aco-dlss".to_owned(), "fg.mode".to_owned()), Edit::Set(json!("off")));
        assert!(!visible_with(&s, &staged, &key), "the staged value hides it");
        staged.insert(("m/aco-dlss".to_owned(), "fg.toggle_key".to_owned()), Edit::Reset);
        let ops = ops_of(&staged);
        assert!(matches!(&ops[0], OptionOp::Set { id, .. } if id == "fg.mode"));
        assert!(matches!(&ops[1], OptionOp::Reset { id, .. } if id == "fg.toggle_key"));
    }

    #[test]
    fn change_lines() {
        let (what, ft) = split_change("DLSS world.npc_density 1 × → 2 ×");
        assert_eq!(what, "DLSS world.npc_density");
        assert_eq!(ft, Some(("1 ×".into(), "2 ×".into())));
        let (what, ft) = split_change("AC Origins Graphics.Fov 1.00 → 1.15");
        assert_eq!(what, "AC Origins Graphics.Fov");
        assert_eq!(ft.unwrap().0, "1.00");
        assert_eq!(split_change("restored r6").1, None);
    }

    #[test]
    fn search() {
        let st = OptionState { def: OptionDef { label: Some("Crowd density".into()), help: Some("Multiplies the cap".into()), ..def(OptionType::Float) }, value: json!(2.0), ..Default::default() };
        assert!(matches_search("crowd", "world.npc_density", &st));
        assert!(matches_search("npc cap", "world.npc_density", &st));
        assert!(!matches_search("reflex", "world.npc_density", &st));
    }
}
