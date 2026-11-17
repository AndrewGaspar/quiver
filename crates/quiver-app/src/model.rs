//! What the app knows about each game, assembled from daemon replies, plus the display
//! formatting shared by every screen.

use chrono::{DateTime, Datelike, Local, Utc};
use quiver_api::{GameSettings, Job, JobKind, LibraryEntry, Playtime, Revision};

#[derive(Clone, Debug)]
pub struct GameRow {
    pub entry: LibraryEntry,
    /// Latest install/launch job for this game.
    pub job: Option<Job>,
    pub playtime: Option<Playtime>,
    pub settings: Option<GameSettings>,
    pub history: Option<Vec<Revision>>,
}

impl GameRow {
    pub fn new(entry: LibraryEntry) -> Self {
        Self { entry, job: None, playtime: None, settings: None, history: None }
    }

    pub fn id(&self) -> u32 {
        self.entry.product_id
    }

    pub fn installed(&self) -> bool {
        self.entry.installed.is_some()
    }

    /// Installed through Steam (whether or not Quiver has its own install too).
    pub fn steam(&self) -> Option<&quiver_api::exports::SteamGame> {
        self.entry.steam.as_ref()
    }

    /// Only Steam has it installed: Steam plays, updates and uninstalls it.
    pub fn steam_only(&self) -> bool {
        self.entry.steam_only()
    }

    /// Installed by Quiver or by Steam (it belongs with the installed games).
    pub fn on_disk(&self) -> bool {
        self.installed() || self.entry.steam.is_some()
    }

    /// An install job that hasn't finished.
    pub fn install_job(&self) -> Option<&Job> {
        self.job.as_ref().filter(|j| j.kind == JobKind::Install && !j.done)
    }

    pub fn queued(&self) -> bool {
        self.install_job().is_some_and(|j| j.status.starts_with("Queued"))
    }

    /// Paused in the download queue (it keeps its place).
    pub fn paused(&self) -> bool {
        self.install_job().is_some_and(|j| j.status == "Paused")
    }

    pub fn installing(&self) -> Option<f32> {
        self.install_job().filter(|_| !self.queued() && !self.paused()).map(job_fraction)
    }

    pub fn running(&self) -> bool {
        self.entry.running || self.job.as_ref().is_some_and(|j| j.kind == JobKind::Play && !j.done)
    }

    pub fn failed(&self) -> Option<&str> {
        self.job.as_ref().and_then(|j| j.error.as_deref())
    }

    pub fn last_played(&self) -> Option<DateTime<Utc>> {
        self.playtime.as_ref().and_then(|p| p.last_played.or_else(|| p.sessions.first().map(|s| s.ended)))
    }

    pub fn current_revision(&self) -> Option<u32> {
        self.history.as_ref().and_then(|h| h.iter().map(|r| r.revision).max())
    }

    /// The one-line status under a tile.
    pub fn tile_meta(&self) -> String {
        if let Some(e) = self.failed() {
            return format!("Failed: {e}");
        }
        if self.running() {
            return "Playing now".into();
        }
        if let Some(j) = self.install_job() {
            if self.queued() {
                return if j.total > 0 { format!("Queued · {}", gb(j.total)) } else { "Queued".into() };
            }
            if self.paused() {
                return if j.total > 0 { format!("Paused · {} of {}", gb(j.downloaded), gb(j.total)) } else { "Paused".into() };
            }
            return format!("{} of {}", gb(j.downloaded), gb(j.total));
        }
        let played = self.playtime.as_ref().map(|p| p.total_seconds).filter(|s| *s > 0);
        match (self.installed(), self.entry.update_available, played, self.last_played()) {
            (true, true, Some(s), _) => format!("Update available · {} played", hours(s)),
            (true, true, None, _) => "Update available".into(),
            (true, false, Some(s), Some(t)) => format!("{} played · {}", hours(s), day(t)),
            (true, false, Some(s), None) => format!("{} played", hours(s)),
            (true, false, None, _) => "Installed · not played yet".into(),
            (false, _, Some(s), _) if self.steam_only() => format!("Installed via Steam · {} played", hours(s)),
            (false, _, _, _) if self.steam_only() => "Installed via Steam".into(),
            (false, _, _, _) => "Not installed".into(),
        }
    }
}

pub fn job_fraction(j: &Job) -> f32 {
    if j.total == 0 { 0.0 } else { (j.downloaded as f64 / j.total as f64).clamp(0.0, 1.0) as f32 }
}

/// Product names without trademark signs and odd whitespace.
pub fn clean_name(s: &str) -> String {
    s.replace(['®', '™'], "").replace('’', "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Short names for the side list and dense rows: "Assassin's Creed Origins" -> "AC Origins".
pub fn short_name(s: &str) -> String {
    let s = clean_name(s);
    let s = s.replace(" - ", " ");
    for (long, short) in [("Assassin's Creed ", "AC "), ("Discovery Tour by Assassin's Creed: ", "Discovery Tour: ")] {
        if let Some(rest) = s.strip_prefix(long) {
            return format!("{short}{rest}");
        }
    }
    s.trim_end_matches(" by Ubisoft").to_owned()
}

/// Initials for an account avatar.
pub fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|p| !p.is_empty()).collect();
    let s: String = match parts.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    s.to_uppercase()
}

pub fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g >= 100.0 {
        format!("{g:.0} GB")
    } else if g >= 1.0 {
        format!("{g:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

pub fn hours(seconds: u64) -> String {
    let h = seconds / 3600;
    if h >= 1 { format!("{h} h") } else { format!("{} min", (seconds / 60).max(1)) }
}

/// Session length: "2 h 14 m", "4 m".
pub fn duration(seconds: u64) -> String {
    let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
    if h > 0 { format!("{h} h {m:02} m") } else { format!("{m} m") }
}

/// "Today", "Yesterday", "Nov 9", "Nov 9, 2025".
pub fn day(t: DateTime<Utc>) -> String {
    day_relative(t, Local::now())
}

pub fn day_relative(t: DateTime<Utc>, now: DateTime<Local>) -> String {
    let d = t.with_timezone(&Local).date_naive();
    let today = now.date_naive();
    match (today - d).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ if d.year() == today.year() => d.format("%b %-d").to_string(),
        _ => d.format("%b %-d, %Y").to_string(),
    }
}

pub fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

/// "4 min ago", "3 h ago", or the day.
pub fn ago(t: DateTime<Utc>) -> String {
    let mins = (Utc::now() - t).num_minutes();
    match mins {
        i64::MIN..=0 => "just now".into(),
        1..=59 => format!("{mins} min ago"),
        60..=1439 => format!("{} h ago", mins / 60),
        _ => day(t),
    }
}

/// Replace the home directory with `~`.
pub fn tilde(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

/// Who made a change, from a request's source label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    App,
    Cli,
    Agent,
    System,
}

pub fn actor(source: &str) -> Actor {
    match source {
        "gui" | "app" => Actor::App,
        "cli" => Actor::Cli,
        "system" | "daemon" | "quiverd" | "" => Actor::System,
        _ => Actor::Agent,
    }
}

pub fn actor_label(source: &str) -> String {
    match actor(source) {
        Actor::App => "You · app".into(),
        Actor::Cli => "You · CLI".into(),
        Actor::System => "System".into(),
        Actor::Agent => source.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn names() {
        assert_eq!(short_name("Assassin's Creed® Origins"), "AC Origins");
        assert_eq!(short_name("Assassin's Creed® III - Remastered"), "AC III Remastered");
        assert_eq!(short_name("Discovery Tour by Assassin's Creed: Ancient Egypt"), "Discovery Tour: Ancient Egypt");
        assert_eq!(short_name("Discovery Tour: Ancient Greece by Ubisoft"), "Discovery Tour: Ancient Greece");
        assert_eq!(short_name("Far Cry® 5"), "Far Cry 5");
        assert_eq!(clean_name("Assassin’s Creed®  Chronicles India"), "Assassin's Creed Chronicles India");
        assert_eq!(initials("demo"), "DE");
        assert_eq!(initials("Andrew Gaspar"), "AG");
    }

    #[test]
    fn numbers() {
        assert_eq!(gb(61_800_000_000), "61.8 GB");
        assert_eq!(gb(131_000_000_000), "131 GB");
        assert_eq!(gb(250_000_000), "250 MB");
        assert_eq!(hours(87 * 3600 + 100), "87 h");
        assert_eq!(duration(2 * 3600 + 14 * 60), "2 h 14 m");
        assert_eq!(duration(240), "4 m");
    }

    #[test]
    fn days() {
        let now = Local.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
        let t = |y, m, d| Local.with_ymd_and_hms(y, m, d, 20, 0, 0).unwrap().with_timezone(&Utc);
        assert_eq!(day_relative(t(2026, 11, 15), now), "Today");
        assert_eq!(day_relative(t(2026, 11, 14), now), "Yesterday");
        assert_eq!(day_relative(t(2026, 11, 9), now), "Nov 9");
        assert_eq!(day_relative(t(2025, 6, 3), now), "Jun 3, 2025");
    }

    #[test]
    fn actors() {
        assert_eq!(actor("gui"), Actor::App);
        assert_eq!(actor("cli"), Actor::Cli);
        assert_eq!(actor("claude-code"), Actor::Agent);
        assert_eq!(actor_label("system"), "System");
    }
}
