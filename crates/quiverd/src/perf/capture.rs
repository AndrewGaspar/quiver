//! What the Sight panel's Screenshot needs from the hub (`crate::sightinfo`): the session, its
//! dir, and the status of the layer drawing on its largest swapchain.

use anyhow::{Context, Result};

use super::PerfHub;
use crate::sightinfo::CaptureTarget;

impl PerfHub {
    /// The session named, else the focused game, else the only one running.
    pub fn capture_target(&self, session: Option<&str>) -> Result<CaptureTarget> {
        let st = self.lock();
        let id = Self::pick(&st, session, None)?.to_owned();
        let s = st.sessions.get(&id).context("the session went away")?;
        let dir = s.dir.as_ref().map(|d| d.path().to_owned()).context("the game has no session dir (no Sight layer in its launch)")?;
        Ok(CaptureTarget { game: s.game, dir, status: s.key_layer().and_then(|l| l.status) })
    }
}
