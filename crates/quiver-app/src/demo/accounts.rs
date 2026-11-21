//! Demo accounts across machines (design/REMOTE.md §6): the Ubisoft account `demo` is signed
//! in on this PC ("laptop") and on Tower; Office is asleep. Flags
//! (`QUIVER_DEMO_GLOBAL`): `tower-playing` has Tower playing Origins on `demo`;
//! `epic-account` signs this PC in to Epic as "PlayerOne", whom **Add to Tower** hands over.
//!
//! Answers `Request::Accounts` and, with `epic-account`, Epic's account list on both
//! machines.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use chrono::{Duration as Span, Utc};
use quiver_api::accounts::{AccountsRequest, HandoverResult, LocalAccount, MachineAccounts, MachineReach, Playing, Presence};
use quiver_api::epic::{EpicAccount, EpicRequest};
use quiver_api::stores::AccountState;
use quiver_api::{Event, GameRef, Request, StoreId};
use serde_json::{Value, json};

use super::Demo;
use super::remote::{LAPTOP, OFFICE, TOWER};

const EPIC_ID: &str = "e7c1a9d24b3f4e0c8a6d5b2f1c9e0a7d";

/// PlayerOne was handed to Tower.
static HANDED: AtomicBool = AtomicBool::new(false);

fn flag(name: &str) -> bool {
    crate::ui::global::demo_flag(name).is_some()
}

fn epic_account() -> EpicAccount {
    EpicAccount { account_id: EPIC_ID.into(), display_name: "PlayerOne".into(), default: true, state: AccountState::SignedIn, ubisoft: None }
}

fn ubisoft(playing: bool) -> LocalAccount {
    LocalAccount {
        store: StoreId::Ubisoft,
        account_id: "demo".into(),
        name: "demo".into(),
        state: AccountState::SignedIn,
        default: true,
        playing: playing.then(|| Playing { game: GameRef::ubisoft(3539), name: "Assassin's Creed Origins".into(), started: Utc::now() - Span::minutes(42) }),
    }
}

fn epic() -> LocalAccount {
    LocalAccount { store: StoreId::Epic, account_id: EPIC_ID.into(), name: "PlayerOne".into(), state: AccountState::SignedIn, default: true, playing: None }
}

fn presence() -> Presence {
    let mut laptop = vec![ubisoft(false)];
    let mut tower = vec![ubisoft(flag("tower-playing"))];
    if flag("epic-account") {
        laptop.push(epic());
        if HANDED.load(Ordering::SeqCst) {
            tower.push(epic());
        }
    }
    let machines = [
        MachineAccounts { machine: "laptop".into(), id: LAPTOP.into(), accounts: laptop, at: Utc::now() },
        MachineAccounts { machine: "Tower".into(), id: TOWER.into(), accounts: tower, at: Utc::now() },
    ];
    let reach = vec![
        MachineReach { machine: "laptop".into(), id: LAPTOP.into(), here: true, reached: true, error: None, at: Some(Utc::now()) },
        MachineReach { machine: "Tower".into(), id: TOWER.into(), here: false, reached: true, error: None, at: Some(Utc::now()) },
        MachineReach { machine: "Office".into(), id: OFFICE.into(), here: false, reached: false, error: Some("asleep".into()), at: None },
    ];
    Presence { accounts: quiver_api::accounts::merge(&machines, LAPTOP), machines: reach }
}

/// Answer an accounts request, or hand it back.
pub(super) fn handle(demo: &Demo, req: Request) -> Result<std::result::Result<Value, Request>> {
    Ok(Ok(match req {
        Request::Accounts(AccountsRequest::Presence { .. }) => json!(presence()),
        Request::Accounts(AccountsRequest::Handover { store: StoreId::Epic, from: None, to: Some(to), .. }) if flag("epic-account") && to == TOWER => {
            HANDED.store(true, Ordering::SeqCst);
            demo.emit(Event::AccountsPresenceChanged);
            json!(HandoverResult { store: StoreId::Epic, account_id: EPIC_ID.into(), name: "PlayerOne".into(), from: "laptop".into(), to: "Tower".into() })
        }
        Request::Accounts(AccountsRequest::Handover { .. }) => bail!("the demo hands over PlayerOne from this PC to Tower only"),
        Request::Accounts(_) => bail!("the demo answers presence and one hand-over only"),
        Request::Epic(EpicRequest::Accounts) if flag("epic-account") => match demo.machine {
            None => json!([epic_account()]),
            Some(_) if HANDED.load(Ordering::SeqCst) => json!([epic_account()]),
            Some(_) => json!([]),
        },
        other => return Ok(Err(other)),
    }))
}
