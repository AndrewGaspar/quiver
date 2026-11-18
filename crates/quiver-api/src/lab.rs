//! `quiver lab`: disposable headless instances of an installed game for agents
//! (design/LAB.md). Only the licensing part goes through quiverd: a lab launch asks for
//! [`LaunchMaterial`] (`Request::LaunchMaterial`) per launch, so the account's one login
//! stays in quiverd and an instance never sees a refresh token or a password.

use serde::{Deserialize, Serialize};

/// Session ids of lab launches start with this (`lab-t1-r3`). The bridge passes the
/// session to the broker, which holds such play sessions as [`LAB_HOLDER`]: tracked (they
/// keep quiverd from restarting under them) but never reported to the store as play time
/// and never recorded in the game's play history.
pub const SESSION_PREFIX: &str = "lab-";

/// The play-session holder name of a lab launch's bridge.
pub const LAB_HOLDER: &str = "lab";

/// A lab launch's session id: `lab-` and then `[A-Za-z0-9_-]`, at most 40 bytes (what
/// `quiver_proto::modsdk::is_session_id` accepts, so it can name the mod pipe too).
pub fn is_lab_session(s: &str) -> bool {
    s.len() <= 40
        && s.strip_prefix(SESSION_PREFIX).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

/// `lab-<instance>-<run>`, cut to fit.
pub fn session_id(instance: &str, run: u32) -> String {
    let inst: String = instance.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let tail = format!("-r{run}");
    let room = 40 - SESSION_PREFIX.len() - tail.len();
    let inst = if inst.len() > room { &inst[..room] } else { &inst };
    format!("{SESSION_PREFIX}{inst}{tail}")
}

/// What a lab launch needs from the store to start the game with the user's licence,
/// minted for this launch only (`Request::LaunchMaterial`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchMaterial {
    /// A Ubisoft game: the bridge's launch context (`quiver_ipc::LaunchContext` as JSON;
    /// holds a ticket: write it 0600, the bridge deletes it once read). The bridge answers
    /// the game's SDK and relays Denuvo to quiverd's broker (`context.broker`).
    Ubisoft { context: serde_json::Value },
    /// An Epic game: EGL's arguments with a fresh single-use exchange code (secret; `shown`
    /// is the same list redacted, for logs), and the ownership token when the game wants
    /// one, to write 0600 at the path its `-epicovt=` argument names.
    Epic {
        args: Vec<String>,
        shown: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ownership_token: Option<OwnershipToken>,
    },
    /// Nothing to mint: the store has no licence quiverd can hand out (why, in `reason`).
    None { reason: String },
}

/// An Epic ownership token (`<namespace><item>.ovt`), as Epic issued it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnershipToken {
    pub file_name: String,
    /// The token's bytes, base64 (standard alphabet).
    pub base64: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_sessions() {
        assert!(is_lab_session("lab-t1-r3"));
        assert!(!is_lab_session("lab-") && !is_lab_session("s-4kq2") && !is_lab_session("lab-a/b"));
        let s = session_id("origins-dlss", 12);
        assert_eq!(s, "lab-origins-dlss-r12");
        assert!(is_lab_session(&s));
        let long = session_id(&"x".repeat(80), 99999);
        assert!(long.len() <= 40 && is_lab_session(&long) && long.ends_with("-r99999"));
    }

    #[test]
    fn material_wire() {
        let m = LaunchMaterial::Epic { args: vec!["-AUTH_PASSWORD=c".into()], shown: vec!["-AUTH_PASSWORD=<redacted>".into()], ownership_token: None };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "epic");
        assert_eq!(serde_json::from_value::<LaunchMaterial>(v).unwrap(), m);
        let n: LaunchMaterial = serde_json::from_str(r#"{"kind":"none","reason":"r"}"#).unwrap();
        assert!(matches!(n, LaunchMaterial::None { .. }));
    }
}
