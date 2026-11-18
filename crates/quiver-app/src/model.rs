//! What the app knows about each game, assembled from daemon replies, plus the display
//! formatting shared by every screen.

use chrono::{DateTime, Datelike, Local, Utc};
use quiver_api::{GameRef, GameSettings, Job, JobKind, LibraryEntry, Playtime, Revision, StoreId};

/// How the app names a game in its own state (pages, menus, focus, maps): a [`GameRef`]
/// that prints as its key, `3539` for a Ubisoft product (so every `quiver …` hint the app
/// shows reads as before) and `steam:620` for other stores.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Gid(pub GameRef);

impl Gid {
    /// The Ubisoft product id, for views and requests only Ubisoft games reach (settings,
    /// mods, saves, Options, perf). Other stores' games never open those views; were one
    /// to get there, `0` names no product and the daemon refuses it.
    pub fn pid(self) -> u32 {
        self.0.as_ubisoft().unwrap_or(0)
    }

    pub fn ubisoft(self) -> Option<u32> {
        self.0.as_ubisoft()
    }

    pub fn store(self) -> StoreId {
        self.0.store
    }

    pub fn is_ubisoft(self) -> bool {
        self.0.is_ubisoft()
    }

    /// A number for GPUI element ids: the product id for Ubisoft games (as before), a hash
    /// of the ref with the top bit set for other stores.
    pub fn ix(self) -> usize {
        match self.0.as_ubisoft() {
            Some(id) => id as usize,
            None => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                self.0.hash(&mut h);
                (h.finish() as usize) | (1 << (usize::BITS - 1))
            }
        }
    }
}

impl Default for Gid {
    /// No game: product 0, which no store uses.
    fn default() -> Self {
        Gid::from(0)
    }
}

impl std::str::FromStr for Gid {
    type Err = quiver_api::game::GameRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        GameRef::parse(s).map(Gid)
    }
}

impl PartialEq<Gid> for u32 {
    fn eq(&self, g: &Gid) -> bool {
        g == self
    }
}

impl From<u32> for Gid {
    fn from(product_id: u32) -> Self {
        Gid(GameRef::ubisoft(product_id))
    }
}

impl From<GameRef> for Gid {
    fn from(g: GameRef) -> Self {
        Gid(g)
    }
}

impl PartialEq<u32> for Gid {
    fn eq(&self, product_id: &u32) -> bool {
        self.0.as_ubisoft() == Some(*product_id)
    }
}

impl std::fmt::Display for Gid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.key())
    }
}

impl std::fmt::Debug for Gid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gid({})", self.0)
    }
}

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

    pub fn id(&self) -> Gid {
        Gid(self.entry.game)
    }

    pub fn installed(&self) -> bool {
        self.entry.installed.is_some()
    }

    /// A Steam game (`steam:<appid>`), whose mods Quiver keeps (they reach it through
    /// `quiver steam-run`).
    pub fn steam_game(&self) -> bool {
        self.entry.game.as_steam().is_some()
    }

    /// Installed where its mods can stack on it: Quiver's install of a Ubisoft game, or a
    /// Steam game in Steam's library.
    pub fn mods_installed(&self) -> bool {
        self.installed() || (self.steam_game() && self.client().is_some())
    }

    /// Installed through Steam (whether or not Quiver has its own install too).
    pub fn steam(&self) -> Option<&quiver_api::exports::SteamGame> {
        self.entry.steam.as_ref()
    }

    /// Another store's game, installed and run by its client (a Steam game).
    pub fn client(&self) -> Option<&quiver_api::stores::ClientInstall> {
        self.entry.client.as_ref()
    }

    /// Only Steam has it installed: Steam plays, updates and uninstalls it.
    pub fn steam_only(&self) -> bool {
        self.entry.steam_only()
    }

    /// Installed by Quiver, by Steam or by another store's client (it belongs with the
    /// installed games).
    pub fn on_disk(&self) -> bool {
        self.entry.on_disk()
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
        // Another store's game (its badge says which): its client's state and play time.
        if let Some(c) = self.client() {
            let label = self.entry.store().label();
            let played = played.or(c.playtime.map(|p| p.total_seconds).filter(|s| *s > 0));
            return match (c.complete, c.update_pending, played, self.last_played().or(c.playtime.and_then(|p| p.last_played))) {
                (false, _, _, _) => format!("Downloading in {label}"),
                (true, true, _, _) => format!("Update waiting in {label}"),
                (true, false, Some(s), Some(t)) => format!("{} played · {}", hours(s), day(t)),
                (true, false, Some(s), None) => format!("{} played", hours(s)),
                (true, false, None, _) => format!("In {label} · not played yet"),
            };
        }
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
