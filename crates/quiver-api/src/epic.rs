//! Epic Games Store accounts (`Request::Epic`; design/STORE-EPIC.md §5).
//!
//! Sign-in is the browser plus a pasted code: [`EpicRequest::LoginBegin`] gives the URL
//! of Epic's own sign-in page, which ends on a JSON page holding an `authorizationCode`;
//! the person pastes that code (or the whole page) into [`EpicRequest::LoginComplete`].
//! Quiver never sees the password. Signing in and out are a person's actions only: quiverd
//! refuses them from any source but the app and the terminal (STORES.md §7, item 10).
//!
//! Epic's games themselves come through the store-generic requests (`Library`,
//! `GameDetail`, `Stores`), as `epic:<app name>`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::stores::AccountState;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EpicRequest {
    /// Where to sign in ([`LoginStart`]). Nothing is sent to Epic.
    LoginBegin,
    /// Finish a sign-in with the code from Epic's page (the bare code, the page's JSON, or
    /// a URL carrying `code=`). Adds the account; the first one becomes the default.
    /// Returns [`EpicAccount`].
    LoginComplete { code: String },
    /// Sign an Epic account out and delete its stored session (`None`: the default).
    /// `account` is an account id, display name or unique id prefix.
    Logout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Epic accounts Quiver knows (`Vec<EpicAccount>`), default first.
    Accounts,
    /// Make an Epic account the default.
    AccountDefault { account: String },
    /// An Epic game's cloud saves, play time and achievements (appended; phase 3).
    Game(crate::epicgame::EpicGameRequest),
}

impl fmt::Debug for EpicRequest {
    /// The code is a credential for a minute or two: never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EpicRequest::LoginBegin => f.write_str("LoginBegin"),
            EpicRequest::LoginComplete { .. } => f.write_str("LoginComplete { code: <redacted> }"),
            EpicRequest::Logout { account } => f.debug_struct("Logout").field("account", account).finish(),
            EpicRequest::Accounts => f.write_str("Accounts"),
            EpicRequest::AccountDefault { account } => f.debug_struct("AccountDefault").field("account", account).finish(),
            EpicRequest::Game(r) => f.debug_tuple("Game").field(r).finish(),
        }
    }
}

/// How to sign in (`EpicRequest::LoginBegin`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStart {
    /// Epic's sign-in page. It ends on a page of JSON with `"authorizationCode"`.
    pub url: String,
    /// What to do there, for people.
    pub instructions: String,
}

/// An Epic account in Quiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpicAccount {
    pub account_id: String,
    pub display_name: String,
    #[serde(default)]
    pub default: bool,
    /// `signed_in`, or `needs_sign_in` once Epic stopped accepting its session (Quiver
    /// never signs in again on its own).
    #[serde(default)]
    pub state: AccountState,
    /// The Ubisoft account linked to it on Epic's side, which receives Ubisoft games
    /// bought on Epic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubisoft: Option<LinkedAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedAccount {
    /// The other store's account id (Ubisoft's user id).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_and_no_code_in_debug() {
        let r = crate::Request::Epic(EpicRequest::LoginComplete { code: "0123abcd".into() });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({"method": "epic", "params": {"op": "login_complete", "code": "0123abcd"}}));
        assert!(!format!("{r:?}").contains("0123abcd"));
        let back: crate::Request = serde_json::from_value(serde_json::json!({"method": "epic", "params": {"op": "logout"}})).unwrap();
        assert!(matches!(back, crate::Request::Epic(EpicRequest::Logout { account: None })));
        let a: EpicAccount = serde_json::from_str(r#"{"account_id":"a","display_name":"A"}"#).unwrap();
        assert_eq!((a.default, a.state, a.ubisoft), (false, AccountState::NeedsSignIn, None));
    }
}
