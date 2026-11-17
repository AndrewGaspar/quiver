//! The SDK's achievements (`AchievementReq`): the game's list with this account's unlock
//! status comes from quiverd (`BrokerRequest::Achievements`, loaded by `win::api` before
//! the first `GetAchievements` is answered); `EarnAchievement`s are forwarded to quiverd
//! (`BrokerRequest::AchievementEarned`), which deduplicates and reports them, and are
//! marked earned here at once so the game's next read sees them.
//!
//! Pure state and answers, tested on the host; `win::api` does the broker round trips.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ubi_ipc::BridgeAchievement;
use ubi_proto::uplaydll::{self as u, OperationResult};

/// How long after a failed load the next `GetAchievements` tries quiverd again (until then
/// it answers from what this process knows: the achievements it earned).
const RELOAD_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct State {
    /// From quiverd (`None`: not loaded).
    list: Option<Vec<BridgeAchievement>>,
    /// Earned by the game in this process: (id, unix seconds).
    earned: Vec<(u32, u64)>,
    failed_at: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { list: None, earned: Vec::new(), failed_at: None });

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Whether the next `GetAchievements` should load the list from quiverd first.
pub fn should_load() -> bool {
    let s = STATE.lock().unwrap();
    s.list.is_none() && s.failed_at.is_none_or(|t| t.elapsed() >= RELOAD_AFTER_FAILURE)
}

pub fn set_list(items: Vec<BridgeAchievement>) {
    let mut s = STATE.lock().unwrap();
    s.list = Some(items);
    s.failed_at = None;
}

pub fn load_failed() {
    STATE.lock().unwrap().failed_at = Some(Instant::now());
}

fn known(s: &State, id: u32) -> bool {
    let listed = s.list.as_ref().and_then(|l| l.iter().find(|a| a.id == id)).is_some_and(|a| a.earned);
    listed || s.earned.iter().any(|&(e, _)| e == id)
}

/// Whether an `EarnAchievement` for `id` is news to this bridge (not earned per quiverd's
/// list, not earned in this process): only those are forwarded to quiverd.
pub fn is_new(id: u32) -> bool {
    !known(&STATE.lock().unwrap(), id)
}

/// The game earned `id` at `at`: marked earned for later reads. False if it already was
/// (the game re-sends earned ones, e.g. Origins at every save load).
pub fn earn(id: u32, at: u64) -> bool {
    let mut s = STATE.lock().unwrap();
    if known(&s, id) {
        return false;
    }
    s.earned.push((id, at));
    true
}

/// `GetAchievementsRsp`: quiverd's list (ids, names, descriptions in the account's
/// language, earned state and when), with this process's earns on top. `onlyEarned`
/// filters. Without a list from quiverd, only what was earned here.
pub fn get_rsp(get: &u::GetAchievementsReq) -> u::GetAchievementsRsp {
    let s = STATE.lock().unwrap();
    let mut data: Vec<u::AchievementData> = s
        .list
        .iter()
        .flatten()
        .map(|a| {
            let here = s.earned.iter().find(|&&(e, _)| e == a.id).map(|&(_, t)| t);
            let earned = a.earned || here.is_some();
            u::AchievementData {
                id: a.id,
                name: a.name.clone(),
                description: a.description.clone(),
                is_earned: earned,
                timestamp: if a.earned { a.earned_at } else { here.unwrap_or(0) },
            }
        })
        .collect();
    for &(id, at) in &s.earned {
        if !data.iter().any(|d| d.id == id) {
            data.push(u::AchievementData { id, name: String::new(), description: String::new(), is_earned: true, timestamp: at });
        }
    }
    if get.only_earned == Some(true) {
        data.retain(|d| d.is_earned);
    }
    u::GetAchievementsRsp { request_id: get.request_id, result: OperationResult::Ok as i32, data }
}

/// Reset for tests.
#[cfg(test)]
pub fn reset() {
    *STATE.lock().unwrap() = State::default();
}
