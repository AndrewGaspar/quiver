//! UbiServices REST authentication: password login, 2FA, and remember-me refresh.
//!
//! All three are `POST /v3/profiles/sessions`; only the `Authorization` scheme differs:
//! `Basic` (email:password), `ubi_2fa_v1 t=<2fa ticket>` + `Ubi-2faCode`, or
//! `rm_v1 t=<remember-me ticket>`.

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const SESSIONS_URL: &str = "https://public-ubiservices.ubi.com/v3/profiles/sessions";
/// Ubi-AppId of the Ubisoft Connect PC client.
pub const CONNECT_APP_ID: &str = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6";
const USER_AGENT: &str = "Massgate";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    ticket: Option<String>,
    two_factor_authentication_ticket: Option<String>,
    masked_phone: Option<String>,
    #[serde(default)]
    code_generation_preference: Vec<String>,
    profile_id: Option<String>,
    user_id: Option<String>,
    name_on_platform: Option<String>,
    session_id: Option<String>,
    expiration: Option<DateTime<Utc>>,
    remember_me_ticket: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    error_code: Option<i64>,
    message: Option<String>,
}

/// A live UbiServices session plus the long-lived remember-me ticket that renews it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub ticket: String,
    pub session_id: String,
    pub profile_id: String,
    pub user_id: String,
    pub name: String,
    pub expiration: DateTime<Utc>,
    pub remember_me_ticket: Option<String>,
}

impl Session {
    /// True if the ticket expires within `margin`.
    pub fn expires_within(&self, margin: chrono::Duration) -> bool {
        Utc::now() + margin >= self.expiration
    }
}

/// Result of a password login.
pub enum LoginOutcome {
    Done(Session),
    /// The account has 2FA; pass the challenge to [`Auth::complete_2fa`] with the code.
    TwoFactor(TwoFactorChallenge),
}

#[derive(Debug, Clone)]
pub struct TwoFactorChallenge {
    pub ticket: String,
    pub masked_phone: Option<String>,
    pub methods: Vec<String>,
}

pub struct Auth {
    pub(crate) http: reqwest::Client,
    pub(crate) app_id: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self::new(CONNECT_APP_ID)
    }
}

impl Auth {
    pub fn new(app_id: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("build HTTP client");
        Self { http, app_id: app_id.to_owned() }
    }

    async fn create_session(
        &self,
        authorization: String,
        two_fa_code: Option<&str>,
    ) -> Result<SessionResponse> {
        let mut req = self
            .http
            .post(SESSIONS_URL)
            .header("Ubi-AppId", &self.app_id)
            .header("Ubi-RequestedPlatformType", "uplay")
            .header("Authorization", authorization)
            .json(&serde_json::json!({ "rememberMe": true }));
        if let Some(code) = two_fa_code {
            req = req.header("Ubi-2faCode", code);
        }
        let resp = req.send().await.context("UbiServices request failed")?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            let detail = serde_json::from_str::<ErrorResponse>(&body)
                .ok()
                .map(|e| format!("{} (code {})", e.message.unwrap_or_default(), e.error_code.unwrap_or(0)))
                .unwrap_or(body);
            bail!("UbiServices returned {status}: {detail}");
        }
        serde_json::from_str(&body).context("unexpected UbiServices session response")
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<LoginOutcome> {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{password}"));
        let rsp = self.create_session(format!("Basic {basic}"), None).await?;
        if rsp.ticket.is_none() {
            if let Some(ticket) = rsp.two_factor_authentication_ticket {
                return Ok(LoginOutcome::TwoFactor(TwoFactorChallenge {
                    ticket,
                    masked_phone: rsp.masked_phone,
                    methods: rsp.code_generation_preference,
                }));
            }
        }
        Ok(LoginOutcome::Done(into_session(rsp, None)?))
    }

    pub async fn complete_2fa(&self, challenge: &TwoFactorChallenge, code: &str) -> Result<Session> {
        let rsp = self
            .create_session(format!("ubi_2fa_v1 t={}", challenge.ticket), Some(code.trim()))
            .await?;
        into_session(rsp, None)
    }

    /// Exchange a remember-me ticket for a fresh session. Ubisoft may rotate the
    /// remember-me ticket; the returned session carries whichever is current.
    pub async fn refresh(&self, remember_me_ticket: &str) -> Result<Session> {
        let rsp = self
            .create_session(format!("rm_v1 t={remember_me_ticket}"), None)
            .await?;
        into_session(rsp, Some(remember_me_ticket))
    }
}

impl Auth {
    /// The account's email address (`GET /v3/users/{userId}`), which games read via
    /// `UPLAY_USER_GetEmailUtf8`.
    pub async fn account_email(&self, session: &Session) -> Result<String> {
        #[derive(Deserialize)]
        struct User {
            email: Option<String>,
        }
        let resp = self
            .http
            .get(format!("https://public-ubiservices.ubi.com/v3/users/{}", session.user_id))
            .header("Ubi-AppId", &self.app_id)
            .header("Ubi-SessionId", &session.session_id)
            .header("Authorization", format!("Ubi_v1 t={}", session.ticket))
            .send()
            .await
            .context("UbiServices request failed")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET /v3/users returned {status}: {}", resp.text().await.unwrap_or_default());
        }
        resp.json::<User>().await?.email.context("no email in user profile")
    }
}

fn into_session(rsp: SessionResponse, prev_remember_me: Option<&str>) -> Result<Session> {
    Ok(Session {
        ticket: rsp.ticket.context("session response had no ticket")?,
        session_id: rsp.session_id.unwrap_or_default(),
        profile_id: rsp.profile_id.unwrap_or_default(),
        user_id: rsp.user_id.unwrap_or_default(),
        name: rsp.name_on_platform.unwrap_or_default(),
        expiration: rsp.expiration.unwrap_or_else(|| Utc::now() + chrono::Duration::hours(1)),
        remember_me_ticket: rsp
            .remember_me_ticket
            .or_else(|| prev_remember_me.map(str::to_owned)),
    })
}
