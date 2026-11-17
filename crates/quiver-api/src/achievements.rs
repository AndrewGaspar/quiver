//! Achievements: per-account unlock status, and reporting the unlocks games earn through a
//! Quiver bridge to Ubisoft (`Request::Achievements`).
//!
//! Status merges the definitions (`info::Achievements`) with what Ubisoft says this account
//! unlocked and with Quiver's **report journal**: every `EarnAchievement` a game sent that
//! Ubisoft didn't already have, with whether it was reported. Reporting is per game
//! (`achievements` setting: on by default, `off`, `dry-run`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::info::Achievements;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AchievementsRequest {
    /// The game's achievements with unlock status, the journal and the reporting mode
    /// (`AchievementsReport`). `refresh` asks Ubisoft again instead of the cache.
    List {
        product_id: u32,
        #[serde(default)]
        refresh: bool,
    },
    /// Send a journaled achievement to Ubisoft again (`ReportResult`). Only achievements the
    /// game earned through Quiver (in the journal) can be sent. Agents need a person's
    /// approval (`confirmed`); `dry_run` only shows the write and needs none.
    Report {
        product_id: u32,
        achievement_id: u32,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        confirmed: bool,
    },
    /// Read-only, for checking the upstream protocol: one raw `ach_frontend` read, with
    /// optional overrides (`platform_id`; `framing` `raw` or `wrapped`; `all_products`:
    /// read without naming a product). Returns what was sent and what came back.
    Probe {
        product_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        framing: Option<String>,
        #[serde(default)]
        all_products: bool,
    },
}

/// Where an achievement the game earned stands with Ubisoft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportState {
    /// Waiting to be sent (offline, or not answered yet); retried.
    Pending,
    /// Ubisoft accepted the write.
    Reported,
    /// Gave up after repeated attempts that reached Ubisoft; `report` sends it again.
    Failed,
    /// Reporting is off for this game: kept here only.
    Off,
    /// Reporting is `dry-run` for this game: the write was logged, not sent.
    DryRun,
    /// Ubisoft already had it when Quiver checked (nothing sent).
    AlreadyEarned,
}

impl ReportState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Reported => "reported",
            Self::Failed => "failed",
            Self::Off => "kept locally (reporting off)",
            Self::DryRun => "dry run (not sent)",
            Self::AlreadyEarned => "already on Ubisoft",
        }
    }
}

/// One journal entry: an achievement a game earned through a Quiver bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub achievement_id: u32,
    pub state: ReportState,
    /// When the game earned it (the bridge's `EarnAchievement`).
    pub earned_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<DateTime<Utc>>,
    /// Attempts that reached Ubisoft (offline attempts don't count).
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// After a write: whether a read-back showed it unlocked (`None`: not checked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    /// Ubisoft's state was unknown when it was earned (no read had worked): it is checked
    /// before it is sent, and announced only if Ubisoft didn't have it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
    /// `game` (from the bridge) or `manual` (a `report` request).
    #[serde(default)]
    pub via: String,
}

/// `AchievementsRequest::List`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AchievementsReport {
    pub product_id: u32,
    #[serde(default)]
    pub account: Option<String>,
    /// Definitions with unlock status (`unlocked_known` false: no source answered).
    #[serde(default)]
    pub achievements: Option<Achievements>,
    /// When Ubisoft's unlock status was last read.
    #[serde(default)]
    pub fetched: Option<DateTime<Utc>>,
    /// The `ach_frontend` product (`uplay.achievements_sync_id`) and platform used.
    #[serde(default)]
    pub sync_id: Option<String>,
    #[serde(default)]
    pub platform_id: Option<String>,
    /// `on`, `off` or `dry-run`.
    pub reporting: String,
    #[serde(default)]
    pub journal: Vec<JournalEntry>,
    /// Sources that failed, with why.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// `AchievementsRequest::Report`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportResult {
    pub achievement_id: u32,
    /// The journal entry afterwards.
    pub entry: JournalEntry,
    /// The `ach_frontend` request (protobuf, hex) that was or would be sent.
    pub request_hex: String,
    /// The same request, decoded.
    pub request: serde_json::Value,
    pub sent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire() {
        let r: AchievementsRequest = serde_json::from_str(r#"{"action":"report","product_id":3539,"achievement_id":59}"#).unwrap();
        assert!(matches!(r, AchievementsRequest::Report { product_id: 3539, achievement_id: 59, dry_run: false, confirmed: false }));
        let r: crate::Request = serde_json::from_str(r#"{"method":"achievements","params":{"action":"list","product_id":1}}"#).unwrap();
        assert!(matches!(r, crate::Request::Achievements(AchievementsRequest::List { product_id: 1, refresh: false })));
        assert_eq!(serde_json::to_value(ReportState::DryRun).unwrap(), "dry_run");
        let e = crate::Event::AchievementUnlocked {
            product_id: 3539,
            account: "u".into(),
            achievement_id: 59,
            name: Some("Namaste".into()),
            description: None,
            image: None,
            unlocked_at: "2026-11-17T00:00:00Z".parse().unwrap(),
            report: ReportState::Pending,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "achievement_unlocked");
        assert_eq!(v["report"], "pending");
    }

    #[test]
    fn setting() {
        let mut s = crate::GameSettings::default();
        s.apply(&crate::SettingOp::parse("achievements=off").unwrap()).unwrap();
        assert_eq!(s.achievements.as_deref(), Some("off"));
        s.apply(&crate::SettingOp::parse("achievements=dry_run").unwrap()).unwrap();
        assert_eq!(s.achievements.as_deref(), Some("dry-run"));
        s.apply(&crate::SettingOp::parse("achievements=on").unwrap()).unwrap();
        assert_eq!(s.achievements, None);
        assert!(s.apply(&crate::SettingOp::parse("achievements=maybe").unwrap()).is_err());
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({}));
    }
}
