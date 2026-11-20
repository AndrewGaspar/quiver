//! The panel's sections, each in its own module: what it lists, the hints it gives
//! ([`super::hints`]), and how it draws. The shell (`panel/view.rs`) picks the section's
//! rows and layout through [`rows`] and `PanelView::section_body`.
//!
//! - [`mods`]: the Mods and Tweaks sections (option surfaces, pages, presets, the session's
//!   changes, the change chip, A/B compare).
//! - Session, Performance and Achievements still draw from `panel::state::rows`.

pub mod mods;
pub mod mods_view;

use chrono::{DateTime, Utc};
use quiver_api::sight::PanelSection;

use super::state::{Nav, PanelData, Row};
use mods::SessionLog;

/// The rows of the current section (in focus order).
pub fn rows(nav: &Nav, d: &PanelData, log: &SessionLog, now: DateTime<Utc>) -> Vec<Row> {
    match nav.section {
        PanelSection::Mods => mods::rows(d, nav, log),
        PanelSection::Tweaks => mods::tweak_rows(d, nav, log),
        s => super::state::rows(s, d, now),
    }
}
