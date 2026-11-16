//! The `uplaydll` SDK API (`uplay_r164.dll`): init handshake, then subsystem requests.
//!
//! Requests without a response message in the protocol (subsystem inits, game sessions,
//! telemetry) are fire-and-forget: they are logged and get no reply.

use anyhow::Result;
use prost::Message;
use tokio::net::windows::named_pipe::NamedPipeServer;
use ubi_proto::uplaydll::{self as u, OperationResult};

use super::{context, hex, read_frame, write_frame};

const OK: i32 = OperationResult::Ok as i32;
const UNAVAILABLE: i32 = OperationResult::Unavailable as i32;

pub async fn serve(n: u32, mut pipe: NamedPipeServer) {
    let result: Result<()> = async {
        loop {
            let body = read_frame(&mut pipe).await?;
            let req = match u::Req::decode(body.as_slice()) {
                Ok(r) => r,
                Err(e) => {
                    log!("[conn {n}] undecodable {}-byte frame ({e}): {}", body.len(), hex(&body));
                    continue;
                }
            };
            log!("[conn {n}] <- {}", describe(&req));
            match respond(&req) {
                Reply::Rsp(rsp) => {
                    log!("[conn {n}] -> {}", summarize(&rsp));
                    write_frame(&mut pipe, &rsp).await?;
                }
                Reply::None => {}
                Reply::Unhandled => log!("[conn {n}]    (unhandled, no reply)"),
            }
        }
    }
    .await;
    log!("[conn {n}] closed: {result:?}");
}

enum Reply {
    Rsp(u::Rsp),
    /// Fire-and-forget request; the protocol has no response for it.
    None,
    Unhandled,
}

fn rsp(f: impl FnOnce(&mut u::Rsp)) -> Reply {
    let mut r = u::Rsp::default();
    f(&mut r);
    Reply::Rsp(r)
}

fn respond(req: &u::Req) -> Reply {
    if let Some(r) = &req.init_process_req {
        log!("    init_process for uplay id {:?}", r.uplay_id);
        return rsp(|x| {
            x.init_process_rsp = Some(u::InitProcessRsp {
                result: u::InitResult::Success as i32,
                uplay_pid: Some(std::process::id()),
                overlay_enabled: Some(false),
                overlay_injection_method: Some(u::OverlayInjectionMethod::None as i32),
                sdk_monitoring_config: None,
                devmode: Some(false),
            })
        });
    }
    if let Some(r) = &req.init_req {
        let init = init_rsp(r.uplay_id.unwrap_or_default());
        return rsp(|x| x.init_rsp = Some(init));
    }
    if let Some(f) = &req.friends_req {
        return friends(f);
    }
    if let Some(i) = &req.installer_req {
        return installer(i);
    }
    if let Some(a) = &req.achievement_req {
        return achievements(a);
    }
    if let Some(set) = req.rich_presence_req.as_ref().and_then(|r| r.set_req.as_ref()) {
        return rich_presence(set);
    }
    if let Some(v2) = &req.user_req_v2 {
        if let Some(set) = &v2.set_rich_presence_req {
            return rich_presence(set);
        }
        return Reply::Unhandled;
    }
    if let Some(user) = &req.user_req {
        return user_req(user);
    }
    if let Some(p) = req.product_req.as_ref().and_then(|p| p.get_product_list_req.as_ref()) {
        let products = product_list();
        let request_id = p.request_id;
        return rsp(|x| {
            x.product_rsp = Some(u::ProductRsp {
                get_product_list_rsp: Some(u::GetProductListRsp { request_id, result: OK, product_list: products }),
                ..Default::default()
            })
        });
    }
    if let Some(o) = &req.overlay_req {
        // No overlay: report it unavailable so the game falls back to its own UI.
        if let Some(show) = &o.overlay_show_req {
            let request_id = show.request_id;
            return rsp(|x| {
                x.overlay_rsp = Some(u::OverlayRsp {
                    overlay_show_rsp: Some(u::OverlayShowRsp { request_id, result: UNAVAILABLE }),
                    ..Default::default()
                })
            });
        }
        return Reply::None;
    }
    if let Some(s) = req.store_req.as_ref().and_then(|s| s.get_store_products_req.as_ref()) {
        let request_id = s.request_id;
        return rsp(|x| {
            x.store_rsp = Some(u::StoreRsp {
                get_store_products_rsp: Some(u::GetStoreProductsRsp { request_id, result: OK, products_list: vec![] }),
                ..Default::default()
            })
        });
    }
    if req.set_game_session_req.is_some()
        || req.clear_game_session_req.is_some()
        || req.metadata_req.is_some()
        || req.sdk_monitoring_req.is_some()
        || req.set_language_country_code_req.is_some()
        || req.store_req.is_some()
    {
        return Reply::None;
    }
    Reply::Unhandled
}

fn init_rsp(id: u32) -> u::InitRsp {
    let Some(ctx) = context() else {
        return u::InitRsp {
            result: u::InitResult::Success as i32,
            account: Some(u::Account {
                account_id: Some("00000000-0000-0000-0000-000000000000".into()),
                username: Some("recon".into()),
                name_on_platform: Some("recon".into()),
                ..Default::default()
            }),
            connected: Some(true),
            game_uplay_id: Some(id),
            uplay_ids: vec![id],
            is_in_offline_mode: Some(false),
            upc_ticket: Some("recon-placeholder-ticket".into()),
            uplay_pid: Some(std::process::id()),
            ..Default::default()
        };
    };
    if ctx.uplay_id != id {
        log!("    warning: game asked for uplay id {id}, context is for {}", ctx.uplay_id);
    }
    u::InitRsp {
        result: u::InitResult::Success as i32,
        account: Some(u::Account {
            account_id: Some(ctx.account_id.clone()),
            username: Some(ctx.username.clone()),
            name_on_platform: Some(ctx.name_on_platform.clone()),
            ..Default::default()
        }),
        connected: Some(true),
        game_uplay_id: Some(id),
        uplay_ids: ctx.owned_uplay_ids.clone(),
        game_cd_keys: ctx
            .cd_keys
            .iter()
            .map(|k| u::GameCdKey { uplay_id: k.uplay_id, cd_key: Some(k.key.clone()) })
            .collect(),
        is_in_offline_mode: Some(false),
        upc_ticket: Some(ctx.upc_ticket.clone()),
        uplay_pid: Some(std::process::id()),
        game_install: Some(u::GameInstall {
            success: Some(true),
            chunks: Some(chunks()),
            language: ctx.language.clone(),
        }),
        storage: ctx.savegame_path.as_deref().map(|p| u::Storage { savegame_storage_path: Some(save_dir(p)) }),
        ubi_services: ctx.ubiservices_app_id.clone().map(|a| u::UbiServices { app_id: Some(a) }),
        ..subsystems()
    }
}

/// Subsystem init blocks Connect sends with `InitRsp`. Without them some SDK calls
/// (e.g. `UPLAY_FRIENDS_Init`) wait for state that never arrives and time out.
fn subsystems() -> u::InitRsp {
    u::InitRsp {
        overlay: Some(u::Overlay {
            enabled: Some(false),
            injection_method: Some(u::OverlayInjectionMethod::None as i32),
        }),
        // Mid-range defaults; Connect gets these from its hardware benchmark.
        hw_benchmark: Some(u::HardwareBenchmark {
            cpu_score: Some(1000),
            gpu_score: Some(1000),
            gpu_score_confidence_level: Some(1.0),
        }),
        friend_init: Some(u::FriendInit { friend_list: vec![], is_available: Some(true) }),
        store_init: Some(u::StoreInit {
            is_available: false,
            target_partner: u::TargetPartner::None as i32,
        }),
        multiplayer_init: Some(u::MultiplayerInit { session: None, is_available: Some(false) }),
        user_init: Some(u::UserInit { blacklisted_list: vec![], is_available: Some(true) }),
        ..Default::default()
    }
}

/// The SDK appends `<slot>.save` to this path directly, so it must end in a separator.
fn save_dir(path: &str) -> String {
    if path.ends_with(['\\', '/']) { path.to_owned() } else { format!("{path}\\") }
}

/// Every chunk in the install manifest, reported as downloaded.
fn chunks() -> u::Chunks {
    let chunks = context()
        .map(|c| c.chunks.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|c| u::ChunkInfo { chunk_id: c.id, downloaded: Some(true), tags: c.tags.clone() })
        .collect();
    u::Chunks { chunks }
}

fn friends(f: &u::FriendsReq) -> Reply {
    if f.init_req.is_some() {
        // Connect answers with the (here: empty) friend list.
        return rsp(|x| {
            x.friends_rsp = Some(u::FriendsRsp {
                friend_list_updated_push: Some(u::FriendListUpdatedPush { friend_list: vec![] }),
                ..Default::default()
            })
        });
    }
    if let Some(list) = &f.get_friend_list_req {
        let request_id = list.request_id;
        return rsp(|x| {
            x.friends_rsp = Some(u::FriendsRsp {
                get_friend_list_rsp: Some(u::GetFriendListRsp { request_id, result: OK, friend_list: vec![] }),
                ..Default::default()
            })
        });
    }
    Reply::Unhandled
}

fn installer(i: &u::InstallerReq) -> Reply {
    if i.init_installer_req.is_some() {
        let language = context().and_then(|c| c.language.clone());
        return rsp(|x| {
            x.installer_rsp = Some(u::InstallerRsp {
                init_installer_rsp: Some(u::InitInstallerRsp { success: true, chunks: Some(chunks()), language }),
                ..Default::default()
            })
        });
    }
    if let Some(order) = &i.update_install_order_req {
        let request_id = order.request_id;
        return rsp(|x| {
            x.installer_rsp = Some(u::InstallerRsp {
                update_install_order_rsp: Some(u::UpdateInstallOrderRsp { request_id, ..Default::default() }),
                ..Default::default()
            })
        });
    }
    Reply::Unhandled
}

// TODO: real achievements live in UbiServices; until then report none and log earned ones.
fn achievements(a: &u::AchievementReq) -> Reply {
    if let Some(get) = &a.get_req {
        let request_id = get.request_id;
        return rsp(|x| {
            x.achievement_rsp = Some(u::AchievementRsp {
                get_rsp: Some(u::GetAchievementsRsp { request_id, result: OK, data: vec![] }),
                ..Default::default()
            })
        });
    }
    if let Some(earn) = &a.earn_req {
        log!("    achievement {} earned (not reported to Ubisoft yet)", earn.id);
        let request_id = earn.request_id;
        return rsp(|x| {
            x.achievement_rsp = Some(u::AchievementRsp {
                earn_rsp: Some(u::EarnAchievementRsp { request_id, result: OK }),
                ..Default::default()
            })
        });
    }
    if let Some(img) = &a.image_req {
        let request_id = img.request_id;
        return rsp(|x| {
            x.achievement_rsp = Some(u::AchievementRsp {
                image_rsp: Some(u::GetAchievementImageRsp {
                    request_id,
                    result: OperationResult::NotFound as i32,
                    data: None,
                }),
                ..Default::default()
            })
        });
    }
    Reply::Unhandled
}

fn rich_presence(set: &u::SetRichPresenceReq) -> Reply {
    let request_id = set.request_id;
    rsp(|x| {
        x.user_rsp_v2 = Some(u::UserRspV2 {
            set_rich_presence_rsp: Some(u::SetRichPresenceRsp { request_id, result: OK }),
            ..Default::default()
        })
    })
}

fn user_req(user: &u::UserReq) -> Reply {
    let ctx = context();
    if let Some(p) = &user.get_profile_req {
        let request_id = p.request_id;
        let own = ctx.filter(|c| c.account_id == p.account_id);
        let (result, name) = match own {
            Some(c) => (OK, Some(c.name_on_platform.clone())),
            None => (OperationResult::NotFound as i32, None),
        };
        return rsp(|x| {
            x.user_rsp = Some(u::UserRsp {
                get_profile_rsp: Some(u::GetProfileRsp { request_id, result, name }),
                ..Default::default()
            })
        });
    }
    if let Some(c) = &user.get_credentials_req {
        // Connect would hand over the password; the bridge never has it.
        let request_id = c.request_id;
        let ctx = ctx.cloned().unwrap_or_default();
        return rsp(|x| {
            x.user_rsp = Some(u::UserRsp {
                get_credentials_rsp: Some(u::GetCredentialsRsp {
                    request_id,
                    username: ctx.username,
                    password: String::new(),
                    account_id: ctx.account_id,
                    email: String::new(),
                }),
                ..Default::default()
            })
        });
    }
    Reply::Unhandled
}

fn product_list() -> Vec<u::Product> {
    let Some(ctx) = context() else { return vec![] };
    ctx.owned_uplay_ids
        .iter()
        .map(|&id| u::Product {
            id,
            r#type: if id == ctx.uplay_id { u::ProductType::Game } else { u::ProductType::Addon } as i32,
            ownership: u::ProductOwnership::Owned as i32,
            state: u::ProductState::Playable as i32,
            balance: None,
            activation: u::ProductActivation::Purchased as i32,
        })
        .collect()
}

/// Name the populated response fields without printing tickets or keys.
fn summarize(rsp: &u::Rsp) -> String {
    let full = format!("{rsp:?}");
    let fields: Vec<&str> = full
        .trim_start_matches("Rsp { ")
        .split(", ")
        .filter_map(|f| f.split_once(": Some(").map(|(k, _)| k))
        .filter(|k| k.ends_with("_rsp") || k.ends_with("_push") || k.ends_with("_v2"))
        .collect();
    format!("Rsp {{ {} }}", fields.join(", "))
}

/// Debug-print only the populated fields of a request.
fn describe(req: &u::Req) -> String {
    format!("{req:#?}")
        .lines()
        .filter(|l| !l.trim_end().ends_with(": None,"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}
