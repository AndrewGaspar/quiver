//! Per-game launch settings with full history.
//!
//! `<data>/games/<product_id>/history.jsonl` holds every revision (one JSON object per line);
//! the last line is current. Restoring an old revision appends a new one, so history is
//! never lost and every change records who made it (gui/cli/agent…).

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
pub use ubi_api::{GameSettings, Revision, SettingOp};

use crate::launcher::data_dir;

fn history_path(product_id: u32) -> PathBuf {
    data_dir().join("games").join(product_id.to_string()).join("history.jsonl")
}

pub fn history(product_id: u32) -> Result<Vec<Revision>> {
    let Ok(text) = std::fs::read_to_string(history_path(product_id)) else { return Ok(Vec::new()) };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("corrupt settings history"))
        .collect()
}

/// Current settings and their revision (0 = defaults, never changed).
pub fn current(product_id: u32) -> Result<(GameSettings, u32)> {
    Ok(history(product_id)?
        .pop()
        .map(|r| (r.settings, r.revision))
        .unwrap_or_default())
}

fn append(product_id: u32, settings: GameSettings, source: &str, message: Option<String>) -> Result<Revision> {
    let path = history_path(product_id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let revision = current(product_id)?.1 + 1;
    let rev = Revision { revision, at: chrono::Utc::now(), source: source.to_owned(), message, settings };
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&rev)?)?;
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
