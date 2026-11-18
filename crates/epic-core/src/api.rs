//! Epic's HTTP calls and their JSON (design/STORE-EPIC.md §3, §5).
//!
//! The call surface is small and the shapes are well mapped by legendary, Heroic and
//! egs-api (which were read as a protocol reference; none of their code is used). Every
//! reply type takes `#[serde(default)]` so a field Epic adds or drops doesn't break a read.
//!
//! | Call | Endpoint |
//! |---|---|
//! | token (code, refresh) | `POST account …/account/api/oauth/token` |
//! | end a session | `DELETE account …/account/api/oauth/sessions/kill/<access token>` |
//! | account, linked accounts | `GET account …/account/api/public/account/<id>[/externalAuths]` |
//! | owned items | `GET library-service …/library/api/public/items?includeMetadata=true[&cursor=]` |
//! | catalog | `GET catalog …/catalog/api/shared/namespace/<ns>/bulk/items?id=<ids>` |

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ClientConfig;
use crate::http::{Method, Request, Response, Transport};

pub const ACCOUNT_HOST: &str = "https://account-public-service-prod03.ol.epicgames.com";
pub const LIBRARY_HOST: &str = "https://library-service.live.use1a.on.epicgames.com";
pub const CATALOG_HOST: &str = "https://catalog-public-service-prod06.ol.epicgames.com";

/// Library pages followed at most (a page holds hundreds of records).
const MAX_PAGES: usize = 100;

/// An error Epic answered with (`errorCode`, `errorMessage`, …).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EpicError {
    #[serde(skip)]
    pub status: u16,
    pub error_code: String,
    pub error_message: String,
    pub numeric_error_code: Option<i64>,
    /// OAuth's own field (`invalid_grant`).
    #[serde(rename = "error")]
    pub oauth_error: Option<String>,
    /// `errors.com.epicgames.oauth.corrective_action_required`: what Epic wants the person
    /// to do (`PRIVACY_POLICY_ACCEPTANCE`, …) and the page where they do it.
    pub corrective_action: Option<String>,
    pub continuation_url: Option<String>,
}

impl EpicError {
    pub(crate) fn from_response(label: &str, r: &Response) -> EpicError {
        let mut e: EpicError = serde_json::from_slice(&r.body).unwrap_or_default();
        e.status = r.status;
        // Epic quotes the token it refused ("the refresh token 'eg1~…' is invalid").
        e.error_message = scrub(&e.error_message);
        if e.error_message.is_empty() {
            e.error_message = format!("Epic {label} failed (HTTP {})", r.status);
        }
        e
    }

    /// The session (or the code being exchanged) is no good any more: sign in again.
    pub fn is_invalid_grant(&self) -> bool {
        self.oauth_error.as_deref() == Some("invalid_grant")
            || self.error_code.contains("invalid_refresh_token")
            || self.error_code.contains("refresh_token")
            || self.error_code.contains("token_verification_failed")
            || (self.status == 401 && self.error_code.contains("authentication"))
    }

    /// Epic wants the person to accept something on its site first (never done for them).
    pub fn is_corrective_action(&self) -> bool {
        self.error_code.ends_with("corrective_action_required") || self.continuation_url.is_some()
    }
}

impl fmt::Display for EpicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_corrective_action() {
            write!(f, "Epic needs you to {} first", self.corrective_action.as_deref().map(corrective_text).unwrap_or("finish something on its site"))?;
            if let Some(u) = &self.continuation_url {
                write!(f, ": open {u}, then sign in again")?;
            }
            return Ok(());
        }
        f.write_str(&self.error_message)?;
        if !self.error_code.is_empty() {
            write!(f, " ({})", self.error_code)?;
        }
        Ok(())
    }
}

impl std::error::Error for EpicError {}

/// `s` with anything token-shaped replaced: `eg1~…` tokens, and runs of 24 or more
/// letters, digits and `-_.~` (codes, ids, JWT parts).
pub fn scrub(s: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~');
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 24 || run.contains("eg1~") {
            out.push_str("<redacted>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn corrective_text(action: &str) -> &str {
    match action {
        "PRIVACY_POLICY_ACCEPTANCE" => "accept Epic's privacy policy",
        "EULA_ACCEPTANCE" | "TOS_ACCEPTANCE" => "accept Epic's terms",
        "DATE_OF_BIRTH" => "confirm your date of birth",
        "DISPLAY_NAME_CHANGE" => "choose a display name",
        _ => "finish something on its site",
    }
}

/// `oauth/token`'s reply. Lifetimes come from the reply, never assumed.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub expires_in: Option<i64>,
    pub refresh_token: String,
    pub refresh_expires_at: Option<DateTime<Utc>>,
    pub refresh_expires: Option<i64>,
    pub account_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub token_type: String,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse").field("account_id", &self.account_id).field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

/// `account/api/public/account/<id>`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccountInfo {
    pub id: String,
    pub display_name: String,
}

/// `…/externalAuths`: accounts in other services linked to this Epic account.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExternalAuth {
    #[serde(rename = "type")]
    pub kind: String,
    pub external_auth_id: String,
    pub external_display_name: Option<String>,
}

/// One owned item from the library service.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryRecord {
    pub namespace: String,
    pub catalog_item_id: String,
    pub app_name: String,
    pub product_id: String,
    pub sandbox_name: String,
    pub sandbox_type: String,
    pub record_type: String,
    pub acquisition_date: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LibraryPage {
    response_metadata: PageMeta,
    records: Vec<LibraryRecord>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PageMeta {
    next_cursor: Option<String>,
}

/// A catalog item (`bulk/items`), as much of it as Quiver uses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CatalogItem {
    pub id: String,
    pub namespace: String,
    pub title: String,
    pub description: Option<String>,
    pub long_description: Option<String>,
    pub key_images: Vec<KeyImage>,
    pub categories: Vec<Category>,
    pub custom_attributes: HashMap<String, Attribute>,
    pub developer: Option<String>,
    pub release_info: Vec<ReleaseInfo>,
    pub dlc_item_list: Vec<CatalogItem>,
    pub main_game_item: Option<Box<CatalogItem>>,
    pub creation_date: Option<DateTime<Utc>>,
    pub last_modified_date: Option<DateTime<Utc>>,
    pub entitlement_type: Option<String>,
    pub item_type: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyImage {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Category {
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attribute {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub id: String,
    pub app_id: String,
    pub platform: Vec<String>,
    pub date_added: Option<DateTime<Utc>>,
}

/// The token grants Quiver uses.
pub enum Grant<'a> {
    /// The code from Epic's sign-in page.
    AuthorizationCode(&'a str),
    RefreshToken(&'a str),
}

/// Epic's API for one client config, over a transport.
#[derive(Clone)]
pub struct Api {
    transport: Arc<dyn Transport>,
    pub config: ClientConfig,
}

impl Api {
    pub fn new(transport: Arc<dyn Transport>, config: ClientConfig) -> Self {
        Api { transport, config }
    }

    pub(crate) async fn send(&self, req: Request) -> Result<Response> {
        self.transport.send(req).await
    }

    /// Send, and turn an error reply into an [`EpicError`].
    pub(crate) async fn call(&self, req: Request) -> Result<Response> {
        let label = req.label;
        let r = self.send(req).await?;
        if !r.ok() {
            return Err(EpicError::from_response(label, &r).into());
        }
        Ok(r)
    }

    pub(crate) async fn json<T: serde::de::DeserializeOwned>(&self, req: Request) -> Result<T> {
        let label = req.label;
        let r = self.call(req).await?;
        serde_json::from_slice(&r.body).with_context(|| format!("Epic {label}: unexpected reply"))
    }

    /// `oauth/token` with the client's basic auth. An error is an [`EpicError`].
    pub async fn token(&self, grant: Grant<'_>) -> Result<TokenResponse> {
        let form: Vec<(&str, &str)> = match grant {
            Grant::AuthorizationCode(code) => vec![("grant_type", "authorization_code"), ("code", code), ("token_type", "eg1")],
            Grant::RefreshToken(t) => vec![("grant_type", "refresh_token"), ("refresh_token", t), ("token_type", "eg1")],
        };
        let basic = basic_auth(&self.config.client_id, &self.config.client_secret);
        let req = Request::new("oauth token", Method::Post, format!("{ACCOUNT_HOST}/account/api/oauth/token")).header("Authorization", basic).form(&form);
        let t: TokenResponse = self.json(req).await?;
        anyhow::ensure!(!t.access_token.is_empty() && !t.account_id.is_empty(), "Epic's sign-in reply had no session in it");
        Ok(t)
    }

    /// End the session the access token belongs to (best effort, at sign-out).
    pub async fn kill_session(&self, access_token: &str) -> Result<()> {
        let req = Request::new("end session", Method::Delete, format!("{ACCOUNT_HOST}/account/api/oauth/sessions/kill/{access_token}")).bearer(access_token);
        self.call(req).await.map(|_| ())
    }

    pub async fn account(&self, access_token: &str, account_id: &str) -> Result<AccountInfo> {
        self.json(Request::new("account", Method::Get, format!("{ACCOUNT_HOST}/account/api/public/account/{account_id}")).bearer(access_token)).await
    }

    pub async fn external_auths(&self, access_token: &str, account_id: &str) -> Result<Vec<ExternalAuth>> {
        self.json(Request::new("linked accounts", Method::Get, format!("{ACCOUNT_HOST}/account/api/public/account/{account_id}/externalAuths")).bearer(access_token)).await
    }

    /// Every owned item, following the library's cursor.
    pub async fn library_items(&self, access_token: &str) -> Result<Vec<LibraryRecord>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!("{LIBRARY_HOST}/library/api/public/items?includeMetadata=true");
            if let Some(c) = &cursor {
                url.push_str("&cursor=");
                url.push_str(&urlencode(c));
            }
            let page: LibraryPage = self.json(Request::new("library items", Method::Get, url).bearer(access_token)).await?;
            let n = page.records.len();
            out.extend(page.records);
            match page.response_metadata.next_cursor.filter(|c| !c.is_empty()) {
                Some(c) if n > 0 && cursor.as_ref() != Some(&c) => cursor = Some(c),
                _ => return Ok(out),
            }
        }
        tracing::warn!("Epic library: stopped after {MAX_PAGES} pages");
        Ok(out)
    }

    /// One catalog item, with its DLC and main game. The reply maps ids to items; an id
    /// Epic doesn't know is left out.
    ///
    /// One id per call, as legendary asks. Stage 1 sent a namespace's ids comma-separated,
    /// and on a real library every namespace asked for more than one item (a game and its
    /// DLC) came back without any of them, while every single-id namespace resolved.
    pub async fn catalog_item(&self, access_token: &str, namespace: &str, id: &str) -> Result<HashMap<String, CatalogItem>> {
        let c = &self.config;
        let url = format!(
            "{CATALOG_HOST}/catalog/api/shared/namespace/{}/bulk/items?id={}&includeDLCDetails=true&includeMainGameDetails=true&country={}&locale={}",
            urlencode(namespace),
            urlencode(id),
            urlencode(&c.country),
            urlencode(&c.locale),
        );
        self.json(Request::new("catalog", Method::Get, url).bearer(access_token)).await
    }
}

fn basic_auth(id: &str, secret: &str) -> String {
    use base64::Engine;
    format!("basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}")))
}

/// Percent-encoding for query values and path segments (unreserved characters kept).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fake;

    fn api(fake: Arc<Fake>) -> Api {
        Api::new(fake, ClientConfig::default())
    }

    #[tokio::test]
    async fn token_request_and_reply() {
        let fake = Arc::new(Fake::new());
        fake.reply("/oauth/token", 200, include_str!("../tests/fixtures/token.json"));
        let t = api(fake.clone()).token(Grant::AuthorizationCode("c0ffee")).await.unwrap();
        assert_eq!((t.account_id.as_str(), t.display_name.as_str()), ("8645b4947bbc4c0092a8b7236df169d1", "Player One"));
        assert_eq!(t.expires_at.unwrap().to_rfc3339(), "2026-11-17T20:00:00+00:00");
        assert!(!format!("{t:?}").contains("eg1~"));
        let req = &fake.requests()[0];
        assert_eq!(req.method, Method::Post);
        let form = req.form.clone().unwrap();
        assert!(form.contains(&("grant_type".into(), "authorization_code".into())) && form.contains(&("code".into(), "c0ffee".into())));
        // EGL's client, as Basic auth.
        let auth = req.headers.iter().find(|(k, _)| k == "Authorization").unwrap();
        assert_eq!(auth.1, "basic MzRhMDJjZjhmNDQxNGUyOWIxNTkyMTg3NmRhMzZmOWE6ZGFhZmJjY2M3Mzc3NDUwMzlkZmZlNTNkOTRmYzc2Y2Y=");
        assert_eq!(format!("{req:?}"), "Post oauth token");
    }

    #[tokio::test]
    async fn errors_say_what_happened() {
        let fake = Arc::new(Fake::new());
        fake.reply("/oauth/token", 400, include_str!("../tests/fixtures/error_code_not_found.json"));
        fake.reply("/oauth/token", 400, include_str!("../tests/fixtures/error_corrective.json"));
        fake.reply("/oauth/token", 502, "<html>bad gateway</html>");
        let a = api(fake);
        let e = a.token(Grant::AuthorizationCode("x")).await.unwrap_err();
        let e = e.downcast_ref::<EpicError>().unwrap();
        assert!(e.is_invalid_grant() && !e.is_corrective_action());
        assert!(e.to_string().contains("authorization code"), "{e}");
        let e = a.token(Grant::AuthorizationCode("x")).await.unwrap_err();
        let e = e.downcast_ref::<EpicError>().unwrap();
        assert!(e.is_corrective_action());
        assert!(e.to_string().starts_with("Epic needs you to accept Epic's privacy policy first: open https://www.epicgames.com/id/"), "{e}");
        let e = a.token(Grant::RefreshToken("r")).await.unwrap_err();
        assert_eq!(e.downcast_ref::<EpicError>().unwrap().status, 502);
        assert!(e.to_string().contains("HTTP 502"));
    }

    #[tokio::test]
    async fn library_follows_the_cursor() {
        let fake = Arc::new(Fake::new());
        fake.reply("includeMetadata=true&cursor=page2", 200, include_str!("../tests/fixtures/library_page2.json"));
        fake.reply("includeMetadata=true", 200, include_str!("../tests/fixtures/library_page1.json"));
        let items = api(fake.clone()).library_items("tok").await.unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(fake.requests().len(), 2);
        assert!(fake.unused().is_empty());
        assert!(fake.requests()[0].headers.contains(&("Authorization".into(), "bearer tok".into())));
    }

    #[tokio::test]
    async fn catalog_reads_items_with_dlc() {
        let fake = Arc::new(Fake::new());
        fake.reply("/namespace/ns-alanwake/bulk/items?id=item-awr&includeDLCDetails=true", 200, include_str!("../tests/fixtures/catalog_alanwake.json"));
        let items = api(fake).catalog_item("tok", "ns-alanwake", "item-awr").await.unwrap();
        let aw = &items["item-awr"];
        assert_eq!(aw.title, "Alan Wake Remastered");
        assert_eq!(aw.key_images.iter().find(|k| k.kind == "DieselGameBoxTall").unwrap().width, Some(1200));
        assert_eq!(aw.custom_attributes["CanRunOffline"].value, "true");
    }

    #[test]
    fn scrubbed_messages() {
        assert_eq!(scrub("Sorry the refresh token 'eg1~abc' is invalid"), "Sorry the refresh token '<redacted>' is invalid");
        assert_eq!(scrub("code 0a1b2c3d4e5f60718293a4b5c6d7e8f9 not found."), "code <redacted> not found.");
        assert_eq!(scrub("Sorry, no."), "Sorry, no.");
    }

    #[test]
    fn urlencoding() {
        assert_eq!(urlencode("a b/c+d=é"), "a%20b%2Fc%2Bd%3D%C3%A9");
        assert_eq!(urlencode("Abc-1_2.3~"), "Abc-1_2.3~");
    }
}
