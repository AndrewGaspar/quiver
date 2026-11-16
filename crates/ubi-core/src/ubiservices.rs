//! Plain UbiServices REST reads (`GET public-ubiservices.ubi.com/...`) with a stored session,
//! sent with the headers Ubisoft Connect uses. These don't touch the demux login.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::auth::{Auth, Session};

pub const BASE_URL: &str = "https://public-ubiservices.ubi.com";

/// Replace `{userId}` and `{profileId}` in a path with the session's ids.
pub fn expand(path: &str, session: &Session) -> String {
    path.replace("{userId}", &session.user_id).replace("{profileId}", &session.profile_id)
}

/// `GET {BASE_URL}{path}`: status and body, whatever the status.
pub async fn get_raw(auth: &Auth, session: &Session, path: &str) -> Result<(u16, String)> {
    anyhow::ensure!(path.starts_with('/'), "expected a path on {BASE_URL}, got {path:?}");
    let url = format!("{BASE_URL}{path}");
    let rsp = auth
        .http
        .get(&url)
        .header("Ubi-AppId", &auth.app_id)
        .header("Ubi-SessionId", &session.session_id)
        .header("Authorization", format!("Ubi_v1 t={}", session.ticket))
        .header("Ubi-LocaleCode", "en-US")
        .send()
        .await
        .context("UbiServices request failed")?;
    let status = rsp.status().as_u16();
    Ok((status, rsp.text().await?))
}

/// `GET` and decode a JSON body; non-2xx statuses are errors.
pub async fn get_json<T: DeserializeOwned>(auth: &Auth, session: &Session, path: &str) -> Result<T> {
    let (status, body) = get_raw(auth, session, path).await?;
    if !(200..300).contains(&status) {
        let short: String = body.chars().take(300).collect();
        bail!("GET {} returned {status}: {short}", path.split('?').next().unwrap_or(path));
    }
    serde_json::from_str(&body).with_context(|| format!("unexpected response from {path}"))
}
