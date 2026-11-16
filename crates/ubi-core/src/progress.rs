//! Ubisoft progression over UbiServices REST (placeholder; filled in from live shapes).

use ubi_api::info::Progress;

use crate::auth::{Auth, Session};

pub async fn fetch(_auth: &Auth, _session: &Session, space_id: Option<&str>) -> Progress {
    Progress { space_id: space_id.map(str::to_owned), fetched: chrono::Utc::now(), ..Default::default() }
}
