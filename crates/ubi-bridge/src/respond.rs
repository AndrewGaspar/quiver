//! Answers to the `uplaydll` SDK API (`uplay_r164.dll`): init handshake, then subsystem
//! requests. Pure (no pipes) so it is tested on the host; `win::api` serves it.
//!
//! Requests without a response message in the protocol (subsystem inits, game sessions,
//! telemetry) are fire-and-forget: they are logged and get no reply. Every request that has
//! a response message gets one, even for features Quiver doesn't have (the overlay, friend
//! pickers): a game waiting for an answer that never comes can hang.

use ubi_proto::uplaydll::{self as u, OperationResult};

use crate::context;

const OK: i32 = OperationResult::Ok as i32;
const UNAVAILABLE: i32 = OperationResult::Unavailable as i32;

#[allow(clippy::large_enum_variant)] // one per request, moved straight into the encoder
pub enum Reply {
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

pub fn respond(req: &u::Req) -> Reply {
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
    if let Some(c) = &req.club_req {
        return club(c);
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
        return overlay(o);
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
    // Newer (R2) requests. Cross-boot is how a game starts another one (AC game pickers):
    // not wired to quiverd yet, so refuse it rather than leave the game waiting.
    if let Some(r) = &req.cross_boot_allowed_req {
        log!("    cross-boot to {} asked: not allowed yet", r.product_id);
        let request_id = r.request_id;
        return rsp(|x| x.cross_boot_allowed_rsp = Some(u::CrossBootAllowedRsp { request_id, is_allowed: false }));
    }
    if let Some(r) = &req.cross_boot_status_req {
        let request_id = r.request_id;
        let status = r
            .product_ids
            .iter()
            .map(|&id| u::cross_boot_status_rsp::Status { product_id: Some(id), is_allowed: Some(false) })
            .collect();
        return rsp(|x| x.cross_boot_status_rsp = Some(u::CrossBootStatusRsp { request_id, status }));
    }
    if let Some(r) = &req.launch_app_req {
        let params: Vec<String> = r.params.iter().map(|p| format!("{}={}", p.name, p.value)).collect();
        log!("    LaunchApp {} {params:?}: not supported yet", r.product_id);
        return rsp(|x| x.launch_app_rsp = Some(u::LaunchAppRsp { result: u::GameLaunchResult::Unavailable as i32 }));
    }
    if let Some(s) = &req.streaming_req {
        // Not cloud/remote streaming: say so for the type, "unavailable" for the rest.
        let request_id = s.request_id;
        let streaming_type = s.get_streaming_type_req.is_some();
        return rsp(|x| {
            x.streaming_rsp = Some(u::StreamingRsp {
                request_id,
                result: if streaming_type { OK } else { UNAVAILABLE },
                get_streaming_type_rsp: streaming_type
                    .then_some(u::GetStreamingTypeRsp { streaming_type: Some(u::StreamingType::None as i32) }),
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
            email: ctx.email.clone(),
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
        // Launch parameters another game passed when it cross-booted this one: none.
        cross_boot_init: Some(u::CrossBootInit { launch_params: vec![] }),
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

fn overlay_rsp(r: u::OverlayRsp) -> Reply {
    rsp(|x| x.overlay_rsp = Some(r))
}

/// The scheme and host of a URL a game asked to open, for the log (paths and queries can
/// carry tokens).
fn url_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host: String = host.rsplit('@').next().unwrap_or_default().chars().filter(|c| c.is_ascii_graphic()).take(80).collect();
    format!("{}://{host}", scheme.chars().filter(char::is_ascii_alphanumeric).take(16).collect::<String>())
}

/// URL the game set for the overlay's shop page (`OverlaySetShopUrlReq`), kept for Sight.
static SHOP_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Connect's in-game overlay. Quiver has none yet (Sight will be it): every request that
/// has a response gets `Unavailable` so the game falls back to its own UI or carries on,
/// instead of waiting for an answer that never comes (SIGHT §7, "fix now").
fn overlay(o: &u::OverlayReq) -> Reply {
    if let Some(r) = &o.overlay_show_req {
        let section = u::OverlaySection::try_from(r.section).map(|s| s.as_str_name()).unwrap_or("unknown");
        log!("    overlay {section} asked: no overlay (unavailable)");
        return overlay_rsp(u::OverlayRsp {
            overlay_show_rsp: Some(u::OverlayShowRsp { request_id: r.request_id, result: UNAVAILABLE }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_set_shop_url_req {
        // Only remembered: nothing is shown, so accepting it is the truthful answer.
        log!("    overlay shop URL set ({})", url_origin(&r.url));
        *SHOP_URL.lock().unwrap() = Some(r.url.chars().take(2048).collect());
        return overlay_rsp(u::OverlayRsp {
            overlay_set_shop_url_rsp: Some(u::OverlaySetShopUrlRsp { request_id: r.request_id, result: OK }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_show_shop_url_req {
        // No response message: fire-and-forget.
        log!("    overlay shop page asked ({}): no overlay", url_origin(&r.url));
        return Reply::None;
    }
    if let Some(r) = &o.overlay_show_browser_url_req {
        log!("    overlay browser asked for {}: no overlay (unavailable)", url_origin(&r.url));
        return overlay_rsp(u::OverlayRsp {
            overlay_show_browser_url_rsp: Some(u::OverlayShowBrowserUrlRsp { request_id: r.request_id, result: UNAVAILABLE }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_show_notification_req {
        log!("    overlay notification {} asked: no overlay (unavailable)", r.notification_id);
        return overlay_rsp(u::OverlayRsp {
            overlay_show_notification_rsp: Some(u::OverlayShowNotificationRsp { request_id: r.request_id, result: UNAVAILABLE }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_show_friend_invitation_req {
        log!("    overlay friend invitation for {} user(s) asked: no overlay (unavailable)", r.user_ids.len());
        return overlay_rsp(u::OverlayRsp {
            overlay_show_friend_invitation_rsp: Some(u::OverlayShowFriendInvitationRsp { request_id: r.request_id, result: UNAVAILABLE }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_show_friend_selection_req {
        log!("    overlay friend picker ({} candidates) asked: no overlay (unavailable)", r.user_ids.len());
        return overlay_rsp(u::OverlayRsp {
            overlay_show_friend_selection_rsp: Some(u::OverlayShowFriendSelectionRsp {
                request_id: r.request_id,
                result: UNAVAILABLE,
                selected_ids: vec![],
            }),
            ..Default::default()
        });
    }
    if let Some(r) = &o.overlay_show_micro_app_req {
        let params: Vec<&str> = r.micro_app_params.iter().map(|p| p.name.as_str()).collect();
        log!("    overlay micro-app {:?} asked (params {params:?}): no overlay (unavailable)", r.micro_app_name);
        return overlay_rsp(u::OverlayRsp {
            overlay_show_micro_app_rsp: Some(u::OverlayShowMicroAppRsp { request_id: r.request_id, result: UNAVAILABLE }),
            ..Default::default()
        });
    }
    Reply::Unhandled
}

fn friends_rsp(r: u::FriendsRsp) -> Reply {
    rsp(|x| x.friends_rsp = Some(r))
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
    // The friend picker over an empty friend list: "empty list" is the protocol's own
    // answer for that, and what Connect says to an account without friends.
    if let Some(r) = &f.show_friend_selection_ui_req {
        log!("    friend picker asked: no friends to pick from (empty list)");
        return friends_rsp(u::FriendsRsp {
            show_friend_selection_ui_rsp: Some(u::ShowFriendSelectionUiRsp {
                request_id: r.request_id,
                empty_list: Some(u::ShowFriendSelectionUiEmptyList {}),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if f.show_invite_friends_to_game_ui_req.is_some() {
        // No response message: fire-and-forget.
        log!("    invite-friends UI asked: not available");
        return Reply::None;
    }
    if f.game_invite_accept_req.is_some() {
        // Answered by a push only if an invite was accepted; there are none.
        return Reply::None;
    }
    // Friends-service actions Quiver has no friends service for: refuse each.
    let unavailable = |request_id: u32| (request_id, UNAVAILABLE);
    if let Some(r) = &f.request_friendship_req {
        let (request_id, result) = unavailable(r.request_id);
        return friends_rsp(u::FriendsRsp { request_friendhip_rsp: Some(u::RequestFriendshipRsp { request_id, result }), ..Default::default() });
    }
    if let Some(r) = &f.add_to_black_list_req {
        let (request_id, result) = unavailable(r.request_id);
        return friends_rsp(u::FriendsRsp { add_to_black_list_rsp: Some(u::AddToBlackListRsp { request_id, result }), ..Default::default() });
    }
    if let Some(r) = &f.remove_from_black_list_req {
        let (request_id, result) = unavailable(r.request_id);
        return friends_rsp(u::FriendsRsp {
            remove_from_black_list_rsp: Some(u::RemoveFromBlackListRsp { request_id, result }),
            ..Default::default()
        });
    }
    if let Some(r) = &f.invite_friend_to_game_req {
        let (request_id, result) = unavailable(r.request_id);
        return friends_rsp(u::FriendsRsp { invite_friend_to_game_rsp: Some(u::InviteFriendToGameRsp { request_id, result }), ..Default::default() });
    }
    if let Some(r) = &f.remove_friendship_req {
        let (request_id, result) = unavailable(r.request_id);
        return friends_rsp(u::FriendsRsp { remove_friendship_rsp: Some(u::RemoveFriendshipRsp { request_id, result }), ..Default::default() });
    }
    if let Some(r) = &f.get_name_req {
        // The player's own name is known; anyone else's isn't.
        let own = context().filter(|c| c.account_id == r.account_id);
        let (result, name) = match own {
            Some(c) => (OK, c.name_on_platform.clone()),
            None => (OperationResult::NotFound as i32, String::new()),
        };
        return friends_rsp(u::FriendsRsp { get_name_rsp: Some(u::GetNameRsp { request_id: r.request_id, result, name }), ..Default::default() });
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

// TODO: Ubisoft Connect rewards/actions live in UbiServices; report none for now.
fn club(c: &u::ClubReq) -> Reply {
    let club_rsp = |f: &dyn Fn(&mut u::ClubRsp)| {
        let mut c = u::ClubRsp::default();
        f(&mut c);
        rsp(|x| x.club_rsp = Some(c))
    };
    if c.refresh_actions_req.is_some() {
        return Reply::None;
    }
    if let Some(r) = &c.get_actions_req {
        let request_id = r.request_id;
        return club_rsp(&|c| c.get_actions_rsp = Some(u::GetActionsRsp { request_id, result: OK, actions: vec![] }));
    }
    if let Some(r) = &c.get_rewards_req {
        let request_id = r.request_id;
        return club_rsp(&|c| c.get_rewards_rsp = Some(u::GetRewardsRsp { request_id, result: OK, rewards: vec![] }));
    }
    if let Some(r) = &c.get_unit_balance_req {
        let request_id = r.request_id;
        return club_rsp(&|c| {
            c.get_unit_balance_rsp = Some(u::GetUnitBalanceRsp { request_id, result: OK, balance: Some(0) })
        });
    }
    if let Some(r) = &c.set_actions_completed_req {
        log!("    club actions completed (not reported to Ubisoft yet): {:?}", r.action_ids);
        let request_id = r.request_id;
        return club_rsp(&|c| {
            c.set_actions_completed_rsp = Some(u::SetActionsCompletedRsp { request_id, result: OK })
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
pub fn summarize(rsp: &u::Rsp) -> String {
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
pub fn describe(req: &u::Req) -> String {
    format!("{req:#?}")
        .lines()
        .filter(|l| !l.trim_end().ends_with(": None,"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}
