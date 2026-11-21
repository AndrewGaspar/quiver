//! Accounts across machines (`Request::Accounts`; design/REMOTE.md §6).
//!
//! Every machine keeps its own store sessions; nothing here moves a stored secret. What
//! this adds:
//!
//! - **Presence.** [`AccountsRequest::Local`] is one machine's accounts (every store) and
//!   what each is playing, with no secret in it. [`AccountsRequest::Presence`] asks this
//!   machine and each paired server for theirs, adds what paired clients last told this
//!   machine ([`AccountsRequest::Announce`]), and merges them per account
//!   ([`merge`]): which machines are signed in, and which one is playing.
//! - **Epic hand-over.** [`AccountsRequest::Handover`] gives another machine its own Epic
//!   session from one that is signed in: the signed-in machine asks Epic for a one-time
//!   exchange code (`GET /account/api/oauth/exchange`, as Epic's own launcher and
//!   legendary do), and the other machine redeems it (`grant_type=exchange_code`) for a
//!   refresh-token chain of its own. The code is the only thing that travels, once, inside
//!   the pinned mutual TLS between paired machines; it is a [`Secret`], never printed or
//!   logged, and dead once redeemed. The two machines' sessions then rotate independently.
//! - **"Playing on <machine>".** Ubisoft allows one live demux connection per account, so
//!   before a Ubisoft game starts, quiverd asks its paired machines whether that account is
//!   playing there ([`live_elsewhere`]) and refuses with the machine's name if so.
//!
//! Sign-in and hand-over are the person's actions (STORES.md §7, item 10): quiverd takes
//! them from the app and the terminal, here or over the tunnel, never from an agent
//! ([`crate::remote::is_person`]).

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::secret::Secret;
use crate::stores::AccountState;
use crate::{GameRef, StoreId};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AccountsRequest {
    /// This machine's accounts in every store and what each is playing
    /// ([`MachineAccounts`]). No secrets; it is what paired machines ask each other.
    Local,
    /// Every account on this machine and its paired machines, merged ([`Presence`]).
    /// Paired servers are asked now (each for at most `wait_ms`, default 3000); paired
    /// clients count with what they last announced.
    Presence {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_ms: Option<u64>,
    },
    /// Give machine `to` its own session of `account` from machine `from`'s
    /// ([`HandoverResult`]). `from` and `to` name paired machines as this machine lists
    /// them (`quiver remote peers`); `None` is this machine. Epic only: Ubisoft has no
    /// session hand-over (sign in on the other machine instead). The person's action.
    Handover {
        store: StoreId,
        /// Account id, name or unique id prefix on `from` (`None`: its default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// A one-time hand-over code from this machine's session of `account`
    /// ([`HandoverCode`]), for the paired machine that asks. Refused unless it came over the
    /// remote tunnel from the person: only another machine has a use for it.
    Export {
        store: StoreId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Redeem a hand-over code for this machine's own session; adds the account
    /// ([`ImportedAccount`]). The person's action.
    Import { store: StoreId, code: Secret<String> },
    /// A paired client telling this machine what it is signed in to and playing (sent when
    /// its sessions change, and again while a game runs). Kept in memory for
    /// [`ANNOUNCE_TTL`].
    Announce(MachineAccounts),
}

impl fmt::Debug for AccountsRequest {
    /// Spelled out so a code can never be printed, whatever `Secret` does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountsRequest::Local => f.write_str("Local"),
            AccountsRequest::Presence { wait_ms } => f.debug_struct("Presence").field("wait_ms", wait_ms).finish(),
            AccountsRequest::Handover { store, account, from, to } => {
                f.debug_struct("Handover").field("store", store).field("account", account).field("from", from).field("to", to).finish()
            }
            AccountsRequest::Export { store, account } => f.debug_struct("Export").field("store", store).field("account", account).finish(),
            AccountsRequest::Import { store, .. } => f.debug_struct("Import").field("store", store).field("code", &"<redacted>").finish(),
            AccountsRequest::Announce(m) => f.debug_tuple("Announce").field(m).finish(),
        }
    }
}

/// How long a paired client's announcement counts: a little more than two of its
/// refreshes, so a client that went away stops counting as playing.
pub const ANNOUNCE_TTL: std::time::Duration = std::time::Duration::from_secs(75);
/// How often a machine with a game running announces again.
pub const ANNOUNCE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// One machine's accounts (`AccountsRequest::Local`, `Announce`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineAccounts {
    /// The machine's name and id (`RemoteStatus`).
    pub machine: String,
    pub id: String,
    #[serde(default)]
    pub accounts: Vec<LocalAccount>,
    /// When it was read.
    pub at: DateTime<Utc>,
}

/// An account on one machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalAccount {
    pub store: StoreId,
    /// The store's id for the account (Ubisoft user id, Epic account id): the same on every
    /// machine, so presence merges on it.
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub state: AccountState,
    /// The store's default account on that machine.
    #[serde(default)]
    pub default: bool,
    /// A game of this account running on that machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<Playing>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playing {
    pub game: GameRef,
    pub name: String,
    pub started: DateTime<Utc>,
}

/// `AccountsRequest::Presence`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Every account, each once, with the machines it is signed in on.
    pub accounts: Vec<AccountPresence>,
    /// The machines that were asked (or last heard from), this one first.
    pub machines: Vec<MachineReach>,
}

impl Presence {
    pub fn account(&self, store: StoreId, account_id: &str) -> Option<&AccountPresence> {
        self.accounts.iter().find(|a| a.store == store && a.account_id == account_id)
    }
}

/// One account across machines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountPresence {
    pub store: StoreId,
    pub account_id: String,
    pub name: String,
    /// The machines it is signed in on, this one first.
    pub machines: Vec<SignedInOn>,
}

impl AccountPresence {
    /// The machine playing one of its games, if one is.
    pub fn playing(&self) -> Option<&SignedInOn> {
        self.machines.iter().find(|m| m.playing.is_some())
    }

    /// Signed in on this machine.
    pub fn here(&self) -> Option<&SignedInOn> {
        self.machines.iter().find(|m| m.here)
    }

    /// Machines that have it, other than this one.
    pub fn elsewhere(&self) -> impl Iterator<Item = &SignedInOn> {
        self.machines.iter().filter(|m| !m.here)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedInOn {
    pub machine: String,
    pub id: String,
    /// This machine.
    #[serde(default)]
    pub here: bool,
    #[serde(default)]
    pub state: AccountState,
    #[serde(default)]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<Playing>,
}

/// A machine `Presence` asked or heard from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineReach {
    pub machine: String,
    pub id: String,
    #[serde(default)]
    pub here: bool,
    /// It answered (a server), or announced within [`ANNOUNCE_TTL`] (a client).
    pub reached: bool,
    /// Why not, when it wasn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When its accounts were read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

/// `AccountsRequest::Export`: a one-time code to redeem on another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoverCode {
    pub store: StoreId,
    pub account_id: String,
    pub name: String,
    /// Epic's exchange code: single use, minutes long. Never printed.
    pub code: Secret<String>,
}

/// `AccountsRequest::Import`: the account the code signed in on this machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedAccount {
    pub store: StoreId,
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub default: bool,
}

/// `AccountsRequest::Handover`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoverResult {
    pub store: StoreId,
    pub account_id: String,
    pub name: String,
    /// The machines' names.
    pub from: String,
    pub to: String,
}

/// Merge machines' accounts into one entry per account, this machine (`here`) first and
/// the rest by name. A machine listed twice (asked, and announced) counts once, with its
/// newest read.
pub fn merge(machines: &[MachineAccounts], here_id: &str) -> Vec<AccountPresence> {
    let mut latest: Vec<&MachineAccounts> = Vec::new();
    for m in machines {
        match latest.iter_mut().find(|x| x.id == m.id) {
            Some(x) if x.at < m.at => *x = m,
            Some(_) => {}
            None => latest.push(m),
        }
    }
    latest.sort_by(|a, b| (a.id != here_id, a.machine.to_lowercase()).cmp(&(b.id != here_id, b.machine.to_lowercase())));
    let mut out: Vec<AccountPresence> = Vec::new();
    for m in latest {
        for a in &m.accounts {
            let on = SignedInOn {
                machine: m.machine.clone(),
                id: m.id.clone(),
                here: m.id == here_id,
                state: a.state,
                default: a.default,
                playing: a.playing.clone(),
            };
            match out.iter_mut().find(|p| p.store == a.store && p.account_id == a.account_id) {
                Some(p) => p.machines.push(on),
                None => out.push(AccountPresence { store: a.store, account_id: a.account_id.clone(), name: a.name.clone(), machines: vec![on] }),
            }
        }
    }
    out
}

/// The other machine playing `account`'s game, if one is (for the "playing on <machine>"
/// refusal): from machines other than `here_id`.
pub fn live_elsewhere<'a>(machines: &'a [MachineAccounts], here_id: &str, store: StoreId, account_id: &str) -> Option<(&'a MachineAccounts, &'a Playing)> {
    machines.iter().filter(|m| m.id != here_id).find_map(|m| {
        m.accounts.iter().find(|a| a.store == store && a.account_id == account_id).and_then(|a| a.playing.as_ref()).map(|p| (m, p))
    })
}

/// The refusal a launch gets when the account is playing on another machine.
pub fn playing_elsewhere_message(account: &str, machine: &str, game: &str) -> String {
    format!(
        "{account} is playing {game} on {machine}. Ubisoft allows one live session per account, so starting here would end that one: stop it on {machine} first (quiver --machine {machine} stop …)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn acct(store: StoreId, id: &str, playing: Option<&str>) -> LocalAccount {
        LocalAccount {
            store,
            account_id: id.into(),
            name: format!("{id}-name"),
            state: AccountState::SignedIn,
            default: true,
            playing: playing.map(|g| Playing { game: GameRef::ubisoft(3539), name: g.into(), started: at("2026-11-21T10:00:00Z") }),
        }
    }

    fn machine(name: &str, id: &str, when: &str, accounts: Vec<LocalAccount>) -> MachineAccounts {
        MachineAccounts { machine: name.into(), id: id.into(), accounts, at: at(when) }
    }

    #[test]
    fn merges_per_account_with_this_machine_first() {
        let ms = vec![
            machine("tower", "t1", "2026-11-21T10:00:00Z", vec![acct(StoreId::Ubisoft, "u1", Some("Origins")), acct(StoreId::Epic, "e1", None)]),
            machine("laptop", "l1", "2026-11-21T10:00:00Z", vec![acct(StoreId::Ubisoft, "u1", None)]),
            // An older announcement of the tower: the newer read wins.
            machine("tower", "t1", "2026-11-21T09:00:00Z", vec![]),
        ];
        let p = merge(&ms, "l1");
        assert_eq!(p.len(), 2);
        let u = &p[0];
        assert_eq!((u.store, u.account_id.as_str()), (StoreId::Ubisoft, "u1"));
        assert_eq!(u.machines.iter().map(|m| (m.machine.as_str(), m.here)).collect::<Vec<_>>(), [("laptop", true), ("tower", false)]);
        assert_eq!(u.playing().unwrap().machine, "tower");
        assert_eq!(u.here().unwrap().machine, "laptop");
        assert_eq!(u.elsewhere().count(), 1);
        assert_eq!(p[1].machines.len(), 1);
        assert!(p[1].here().is_none());

        let (m, g) = live_elsewhere(&ms, "l1", StoreId::Ubisoft, "u1").unwrap();
        assert_eq!((m.machine.as_str(), g.name.as_str()), ("tower", "Origins"));
        assert!(live_elsewhere(&ms, "t1", StoreId::Ubisoft, "u1").is_none(), "a machine's own game isn't elsewhere");
        assert!(live_elsewhere(&ms, "l1", StoreId::Epic, "e1").is_none());
        let msg = playing_elsewhere_message("u1-name", "tower", "Origins");
        assert!(msg.starts_with("u1-name is playing Origins on tower."), "{msg}");
    }

    #[test]
    fn wire_forms_and_no_code_in_debug() {
        let r = crate::Request::Accounts(AccountsRequest::Import { store: StoreId::Epic, code: Secret::new("deadbeefcafe".into()) });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({"method": "accounts", "params": {"op": "import", "store": "epic", "code": "deadbeefcafe"}}));
        assert!(!format!("{r:?}").contains("deadbeefcafe"), "{r:?}");
        let back: crate::Request = serde_json::from_value(v).unwrap();
        assert!(matches!(back, crate::Request::Accounts(AccountsRequest::Import { code, .. }) if code.expose() == "deadbeefcafe"));
        let h: crate::Request = serde_json::from_str(r#"{"method":"accounts","params":{"op":"handover","store":"epic","to":"tower"}}"#).unwrap();
        assert!(matches!(h, crate::Request::Accounts(AccountsRequest::Handover { from: None, to: Some(ref t), account: None, .. }) if t == "tower"));
        let c = HandoverCode { store: StoreId::Epic, account_id: "a".into(), name: "A".into(), code: Secret::new("0123456789abcdef".into()) };
        assert!(!format!("{c:?}").contains("0123456789abcdef"));
        let login = crate::Request::Login { email: "me@example.com".into(), password: Secret::new("hunter2".into()) };
        let two = crate::Request::Login2fa { code: Secret::new("135790".into()) };
        let dbg = format!("{login:?} {two:?}");
        assert!(!dbg.contains("hunter2") && !dbg.contains("135790"), "{dbg}");
        assert_eq!(serde_json::to_value(&login).unwrap()["params"]["password"], "hunter2");
    }
}
