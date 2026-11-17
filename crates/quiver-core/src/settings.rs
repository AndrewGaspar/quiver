//! Per-game launch settings with full history.
//!
//! `<data>/games/<product_id>/history.jsonl` holds every revision (one JSON object per line);
//! the last line is current. Restoring an old revision appends a new one, so history is
//! never lost and every change records who made it (gui/cli/agent…).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use quiver_api::{GameSettings, Revision, SettingOp};

use crate::launcher::data_dir;

fn history_path(product_id: u32) -> PathBuf {
    crate::paths::game_dir_in(&data_dir(), &quiver_api::GameRef::ubisoft(product_id)).join("history.jsonl")
}

/// Every entry of a JSON-lines history file (empty if it doesn't exist yet). Shared by
/// game settings and mod profiles (`mods`).
pub(crate) fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(Vec::new()) };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).with_context(|| format!("corrupt history {}", path.display())))
        .collect()
}

/// Append one entry to a JSON-lines history file.
pub(crate) fn append_jsonl<T: serde::Serialize>(path: &Path, entry: &T) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("history path")?)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

pub fn history(product_id: u32) -> Result<Vec<Revision>> {
    read_jsonl(&history_path(product_id))
}

/// Current settings and their revision (0 = defaults, never changed).
pub fn current(product_id: u32) -> Result<(GameSettings, u32)> {
    Ok(history(product_id)?
        .pop()
        .map(|r| (r.settings, r.revision))
        .unwrap_or_default())
}

fn append(product_id: u32, settings: GameSettings, source: &str, message: Option<String>) -> Result<Revision> {
    let revision = current(product_id)?.1 + 1;
    let rev = Revision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, settings };
    append_jsonl(&history_path(product_id), &rev)?;
    Ok(rev)
}

/// Apply edits and record a revision. No-op edits don't create one.
pub fn apply(product_id: u32, ops: &[SettingOp], source: &str, message: Option<String>) -> Result<Revision> {
    let (mut settings, rev) = current(product_id)?;
    let before = settings.clone();
    for op in ops {
        if let Err(e) = settings.apply(op) {
            bail!(e);
        }
    }
    if settings == before && rev > 0 {
        return Ok(history(product_id)?.pop().unwrap());
    }
    append(product_id, settings, source, message)
}

/// Append a finished play session to `<data>/games/<id>/playtime.jsonl`.
/// [`record_session`] with extra fields stored beside it (readers that don't know them
/// ignore them).
pub fn record_session_with(product_id: u32, session: &quiver_api::PlaySession, extra: serde_json::Value) -> Result<()> {
    let mut v = serde_json::to_value(session)?;
    if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in x {
            if !val.is_null() {
                o.insert(k.clone(), val.clone());
            }
        }
    }
    let path = history_path(product_id).with_file_name("playtime.jsonl");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&v)?)?;
    Ok(())
}

pub fn record_session(product_id: u32, session: &quiver_api::PlaySession) -> Result<()> {
    let path = history_path(product_id).with_file_name("playtime.jsonl");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(session)?)?;
    Ok(())
}

pub fn playtime(product_id: u32) -> Result<quiver_api::Playtime> {
    let path = history_path(product_id).with_file_name("playtime.jsonl");
    let sessions: Vec<quiver_api::PlaySession> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    Ok(quiver_api::Playtime {
        total_seconds: sessions.iter().map(|s| s.seconds).sum(),
        last_played: sessions.iter().map(|s| s.ended).max(),
        sessions,
    })
}

/// Make an old revision current again (recorded as a new revision).
pub fn restore(product_id: u32, revision: u32, source: &str) -> Result<Revision> {
    let target = if revision == 0 {
        GameSettings::default()
    } else {
        history(product_id)?
            .into_iter()
            .find(|r| r.revision == revision)
            .with_context(|| format!("no revision {revision}"))?
            .settings
    };
    append(product_id, target, source, Some(format!("restore revision {revision}")))
}
