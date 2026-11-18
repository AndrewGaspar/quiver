//! Signing in to Epic (design/STORE-EPIC.md §5): the browser plus a pasted code.
//!
//! 1. [`login_url`] is Epic's own sign-in page, redirecting to `id/api/redirect` for EGL's
//!    client. Password, 2FA and captcha all happen on Epic's pages.
//! 2. That redirect page shows JSON: `{"redirectUrl": …, "authorizationCode": "<32 hex>",
//!    "sid": null}`. The person pastes the code, or the whole JSON, or a URL with `code=`;
//!    [`parse_code`] takes any of them.
//! 3. The code is exchanged once for a session ([`Tokens`]). Epic rotates the refresh token
//!    on every refresh, so the new set is written to the keyring before the new access
//!    token is used (`Epic::access_token`).

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use quiver_core::secret::Secret;
use serde::{Deserialize, Serialize};

use crate::api::{TokenResponse, urlencode};
use crate::config::ClientConfig;

/// Refresh when less than this is left on the access token (legendary's margin).
pub const REFRESH_MARGIN: Duration = Duration::minutes(10);

/// Epic's sign-in page for the configured client.
pub fn login_url(c: &ClientConfig) -> String {
    let redirect = format!("https://www.epicgames.com/id/api/redirect?clientId={}&responseType=code", c.client_id);
    format!("https://www.epicgames.com/id/login?redirectUrl={}", urlencode(&redirect))
}

/// What to do on the sign-in page, for people.
pub const INSTRUCTIONS: &str = "Sign in on Epic's page. It ends on a page of text with \"authorizationCode\" in it: copy the code (or the whole page) and paste it here. The code works once, for a few minutes.";

/// The authorization code in what the person pasted: the bare code, the redirect page's
/// JSON (whole or in part), or a URL with `code=`.
pub fn parse_code(input: &str) -> Result<String> {
    let s = input.trim().trim_matches(|c| c == '"' || c == '\'');
    if s.is_empty() {
        bail!("nothing was pasted");
    }
    // The whole page; a partial copy is looked through below.
    if s.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(s)
    {
        return match v.get("authorizationCode") {
            Some(serde_json::Value::String(c)) if is_code(c) => Ok(c.clone()),
            Some(serde_json::Value::Null) => bail!("Epic's page has no code yet (\"authorizationCode\": null): sign in on that page first, then open the link again"),
            _ => bail!("that JSON has no \"authorizationCode\""),
        };
    }
    if let Some(i) = s.find("authorizationCode") {
        let rest = &s[i + "authorizationCode".len()..];
        let code: String = rest.trim_start_matches(|c: char| c == '"' || c == ':' || c.is_whitespace()).chars().take_while(char::is_ascii_alphanumeric).collect();
        if is_code(&code) {
            return Ok(code);
        }
        bail!("Epic's page has no code yet: sign in on that page first, then open the link again");
    }
    if let Some(i) = s.find("code=") {
        let code: String = s[i + 5..].chars().take_while(char::is_ascii_alphanumeric).collect();
        if is_code(&code) {
            return Ok(code);
        }
    }
    if is_code(s) {
        return Ok(s.to_owned());
    }
    bail!("that doesn't look like Epic's code (32 letters and digits, from the page after you sign in)")
}

fn is_code(s: &str) -> bool {
    (16..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// An Epic session, as stored in the keyring entry `epic:<account id>` (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub account_id: String,
    pub display_name: String,
    pub access_token: Secret<String>,
    pub access_expires_at: DateTime<Utc>,
    pub refresh_token: Secret<String>,
    pub refresh_expires_at: DateTime<Utc>,
}

impl Tokens {
    pub fn from_response(t: &TokenResponse, now: DateTime<Utc>) -> Result<Tokens> {
        if t.refresh_token.is_empty() {
            bail!("Epic's reply had no refresh token, so the session couldn't be kept");
        }
        let at = |abs: Option<DateTime<Utc>>, rel: Option<i64>, what: &str| -> Result<DateTime<Utc>> {
            abs.or_else(|| rel.map(|s| now + Duration::seconds(s))).ok_or_else(|| anyhow::anyhow!("Epic's reply didn't say when the {what} expires"))
        };
        Ok(Tokens {
            account_id: t.account_id.clone(),
            display_name: t.display_name.clone(),
            access_token: Secret::new(t.access_token.clone()),
            access_expires_at: at(t.expires_at, t.expires_in, "session")?,
            refresh_token: Secret::new(t.refresh_token.clone()),
            refresh_expires_at: at(t.refresh_expires_at, t.refresh_expires, "refresh token")?,
        })
    }

    /// The access token has less than [`REFRESH_MARGIN`] left.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.access_expires_at - now < REFRESH_MARGIN
    }

    /// The refresh token itself has run out: only signing in again helps.
    pub fn refresh_expired(&self, now: DateTime<Utc>) -> bool {
        self.refresh_expires_at <= now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn login_url_names_the_client() {
        let u = login_url(&ClientConfig::default());
        assert_eq!(
            u,
            "https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode"
        );
    }

    #[test]
    fn every_way_of_pasting_the_code() {
        let page = format!(r#"{{"warning":"Do not share this code with any 3rd party service.","redirectUrl":"https://localhost/launcher/authorized?code={CODE}","authorizationCode":"{CODE}","exchangeCode":null,"sid":null}}"#);
        assert_eq!(parse_code(&page).unwrap(), CODE);
        assert_eq!(parse_code(&format!("  {CODE}\n")).unwrap(), CODE);
        assert_eq!(parse_code(&format!("\"{CODE}\"")).unwrap(), CODE);
        assert_eq!(parse_code(&format!(r#""authorizationCode":"{CODE}","#)).unwrap(), CODE);
        assert_eq!(parse_code(&format!("https://localhost/launcher/authorized?code={CODE}&x=1")).unwrap(), CODE);
        // Pretty-printed JSON, as a browser's JSON viewer copies it.
        assert_eq!(parse_code(&format!("{{\n  \"authorizationCode\": \"{CODE}\",\n  \"sid\": null\n}}")).unwrap(), CODE);
    }

    #[test]
    fn unusable_pastes_say_why() {
        let null = parse_code(r#"{"redirectUrl":"https://www.epicgames.com/id/api/redirect","authorizationCode":null,"sid":null}"#).unwrap_err();
        assert!(null.to_string().contains("sign in on that page first"), "{null}");
        assert!(parse_code("").is_err());
        assert!(parse_code("hunter2").unwrap_err().to_string().contains("doesn't look like"));
        assert!(parse_code("my password is 1234").is_err());
    }

    #[test]
    fn tokens_from_reply_and_refresh_timing() {
        let t: TokenResponse = serde_json::from_str(include_str!("../tests/fixtures/token.json")).unwrap();
        let now: DateTime<Utc> = "2026-11-17T12:00:00Z".parse().unwrap();
        let tok = Tokens::from_response(&t, now).unwrap();
        assert_eq!(tok.access_expires_at.to_rfc3339(), "2026-11-17T20:00:00+00:00");
        assert!(!tok.needs_refresh(now));
        assert!(tok.needs_refresh("2026-11-17T19:55:00Z".parse().unwrap()));
        assert!(!tok.refresh_expired(now));
        assert!(!format!("{tok:?}").contains("eg1~"));
        // Relative lifetimes when Epic leaves out the timestamps.
        let mut rel = t.clone();
        (rel.expires_at, rel.refresh_expires_at) = (None, None);
        (rel.expires_in, rel.refresh_expires) = (Some(3600), Some(86400));
        let tok = Tokens::from_response(&rel, now).unwrap();
        assert_eq!(tok.access_expires_at - now, Duration::hours(1));
        rel.refresh_token.clear();
        assert!(Tokens::from_response(&rel, now).is_err());
    }
}
