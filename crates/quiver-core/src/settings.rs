//! Per-game launch settings with full history.
//!
//! `<data>/games/<dir>/history.jsonl` (`GameRef::dir_name`: `3539`, `steam-620`) holds every revision (one JSON object per line);
//! the last line is current. Restoring an old revision appends a new one, so history is
//! never lost and every change records who made it (gui/cli/agent…).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use quiver_api::{GameSettings, Revision, SettingOp};
use quiver_api::GameRef;

use crate::launcher::data_dir;

fn history_path(game: GameRef) -> PathBuf {
    crate::paths::game_dir_in(&data_dir(), &game).join("history.jsonl")
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

pub fn history(game: impl Into<GameRef>) -> Result<Vec<Revision>> {
    let game: GameRef = game.into();
    read_jsonl(&history_path(game))
}

/// Current settings and their revision (0 = defaults, never changed).
pub fn current(game: impl Into<GameRef>) -> Result<(GameSettings, u32)> {
    let game: GameRef = game.into();
    Ok(history(game)?
        .pop()
        .map(|r| (r.settings, r.revision))
        .unwrap_or_default())
}

fn append(game: GameRef, settings: GameSettings, source: &str, message: Option<String>) -> Result<Revision> {
    let revision = current(game)?.1 + 1;
    let rev = Revision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, settings };
    append_jsonl(&history_path(game), &rev)?;
    Ok(rev)
}

/// Apply edits and record a revision. No-op edits don't create one.
pub fn apply(game: impl Into<GameRef>, ops: &[SettingOp], source: &str, message: Option<String>) -> Result<Revision> {
    let game: GameRef = game.into();
    let (mut settings, rev) = current(game)?;
    let before = settings.clone();
    for op in ops {
        if let Err(e) = settings.apply(op) {
            bail!(e);
        }
    }
    if settings == before && rev > 0 {
        return Ok(history(game)?.pop().unwrap());
    }
    append(game, settings, source, message)
}

/// Append a finished play session to `<data>/games/<id>/playtime.jsonl`.
/// [`record_session`] with extra fields stored beside it (readers that don't know them
/// ignore them).
pub fn record_session_with(game: impl Into<GameRef>, session: &quiver_api::PlaySession, extra: serde_json::Value) -> Result<()> {
    let game: GameRef = game.into();
    let mut v = serde_json::to_value(session)?;
    if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in x {
            if !val.is_null() {
                o.insert(k.clone(), val.clone());
            }
        }
    }
    let path = history_path(game).with_file_name("playtime.jsonl");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&v)?)?;
    Ok(())
}

pub fn record_session(game: impl Into<GameRef>, session: &quiver_api::PlaySession) -> Result<()> {
    let game: GameRef = game.into();
    let path = history_path(game).with_file_name("playtime.jsonl");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(session)?)?;
    Ok(())
}

pub fn playtime(game: impl Into<GameRef>) -> Result<quiver_api::Playtime> {
    let game: GameRef = game.into();
    let path = history_path(game).with_file_name("playtime.jsonl");
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
pub fn restore(game: impl Into<GameRef>, revision: u32, source: &str) -> Result<Revision> {
    let game: GameRef = game.into();
    let target = if revision == 0 {
        GameSettings::default()
    } else {
        history(game)?
            .into_iter()
            .find(|r| r.revision == revision)
            .with_context(|| format!("no revision {revision}"))?
            .settings
    };
    append(game, target, source, Some(format!("restore revision {revision}")))
}
