//! Host-side tests of the bridge's answers (no Wine needed).

use ubi_proto::uplaydll::{self as u, OperationResult};

use crate::respond::{Reply, respond};

const UNAVAILABLE: i32 = OperationResult::Unavailable as i32;

fn overlay(o: u::OverlayReq) -> Option<u::OverlayRsp> {
    match respond(&u::Req { overlay_req: Some(o), ..Default::default() }) {
        Reply::Rsp(r) => Some(r.overlay_rsp.expect("an overlay response")),
        Reply::None => None,
        Reply::Unhandled => panic!("unhandled overlay request"),
    }
}

fn friends(f: u::FriendsReq) -> Reply {
    respond(&u::Req { friends_req: Some(f), ..Default::default() })
}

fn friends_rsp(f: u::FriendsReq) -> u::FriendsRsp {
    match friends(f) {
        Reply::Rsp(r) => r.friends_rsp.expect("a friends response"),
        _ => panic!("no reply"),
    }
}

/// SIGHT §7: every overlay request with a response message gets one (a game may wait for
/// it), echoing the request id; there is no overlay, so the shows are `Unavailable`.
#[test]
fn overlay_requests_are_answered() {
    let r = overlay(u::OverlayReq {
        overlay_show_req: Some(u::OverlayShowReq { request_id: 1, section: u::OverlaySection::Achievements as i32 }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_show_rsp, Some(u::OverlayShowRsp { request_id: 1, result: UNAVAILABLE }));

    let r = overlay(u::OverlayReq {
        overlay_show_browser_url_req: Some(u::OverlayShowBrowserUrlReq { request_id: 2, url: "https://x.example/a?token=secret".into() }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_show_browser_url_rsp, Some(u::OverlayShowBrowserUrlRsp { request_id: 2, result: UNAVAILABLE }));

    let r = overlay(u::OverlayReq {
        overlay_show_notification_req: Some(u::OverlayShowNotificationReq { request_id: 3, notification_id: 77 }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_show_notification_rsp, Some(u::OverlayShowNotificationRsp { request_id: 3, result: UNAVAILABLE }));

    let r = overlay(u::OverlayReq {
        overlay_show_friend_invitation_req: Some(u::OverlayShowFriendInvitationReq { request_id: 4, user_ids: vec!["a".into()] }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_show_friend_invitation_rsp, Some(u::OverlayShowFriendInvitationRsp { request_id: 4, result: UNAVAILABLE }));

    let r = overlay(u::OverlayReq {
        overlay_show_friend_selection_req: Some(u::OverlayShowFriendSelectionReq { request_id: 5, user_ids: vec!["a".into(), "b".into()] }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        r.overlay_show_friend_selection_rsp,
        Some(u::OverlayShowFriendSelectionRsp { request_id: 5, result: UNAVAILABLE, selected_ids: vec![] })
    );

    let r = overlay(u::OverlayReq {
        overlay_show_micro_app_req: Some(u::OverlayShowMicroAppReq {
            request_id: 6,
            micro_app_name: "challenges".into(),
            micro_app_params: vec![u::OverlayMicroAppParam { name: "id".into(), value: "1".into() }],
        }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_show_micro_app_rsp, Some(u::OverlayShowMicroAppRsp { request_id: 6, result: UNAVAILABLE }));

    // Setting the shop URL only stores it: accepted.
    let r = overlay(u::OverlayReq {
        overlay_set_shop_url_req: Some(u::OverlaySetShopUrlReq { request_id: 7, url: "https://shop.example/x".into() }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.overlay_set_shop_url_rsp, Some(u::OverlaySetShopUrlRsp { request_id: 7, result: OperationResult::Ok as i32 }));

    // No response message in the protocol: nothing to send.
    assert!(overlay(u::OverlayReq { overlay_show_shop_url_req: Some(u::OverlayShowShopUrlReq { url: "https://s".into() }), ..Default::default() }).is_none());
}

/// The replies survive the wire: encode as the bridge does, decode as the SDK would.
#[test]
fn overlay_replies_encode() {
    use prost::Message;
    let req = u::Req {
        overlay_req: Some(u::OverlayReq {
            overlay_show_browser_url_req: Some(u::OverlayShowBrowserUrlReq { request_id: 9, url: "https://a".into() }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let req = u::Req::decode(req.encode_to_vec().as_slice()).unwrap();
    let Reply::Rsp(rsp) = respond(&req) else { panic!() };
    let back = u::Rsp::decode(rsp.encode_to_vec().as_slice()).unwrap();
    assert_eq!(back.overlay_rsp.unwrap().overlay_show_browser_url_rsp.unwrap().request_id, 9);
}

#[test]
fn friend_ui_requests_are_answered() {
    let r = friends_rsp(u::FriendsReq {
        show_friend_selection_ui_req: Some(u::ShowFriendSelectionUiReq { request_id: 11, filter: None }),
        ..Default::default()
    });
    let pick = r.show_friend_selection_ui_rsp.unwrap();
    assert_eq!(pick.request_id, 11);
    assert!(pick.empty_list.is_some() && pick.success.is_none() && pick.cancelled.is_none());

    assert!(matches!(
        friends(u::FriendsReq { show_invite_friends_to_game_ui_req: Some(u::ShowInviteFriendsToGameUiReq { allowed_account_ids: vec![] }), ..Default::default() }),
        Reply::None
    ));

    let r = friends_rsp(u::FriendsReq {
        invite_friend_to_game_req: Some(u::InviteFriendToGameReq { request_id: 12, account_id: "x".into() }),
        ..Default::default()
    });
    assert_eq!(r.invite_friend_to_game_rsp, Some(u::InviteFriendToGameRsp { request_id: 12, result: UNAVAILABLE }));
    let r = friends_rsp(u::FriendsReq {
        request_friendship_req: Some(u::RequestFriendshipReq { request_id: 13, search_string: "x".into() }),
        ..Default::default()
    });
    assert_eq!(r.request_friendhip_rsp, Some(u::RequestFriendshipRsp { request_id: 13, result: UNAVAILABLE }));
    // No launch context here: nobody's name is known.
    let r = friends_rsp(u::FriendsReq { get_name_req: Some(u::GetNameReq { request_id: 14, account_id: "x".into() }), ..Default::default() });
    assert_eq!(r.get_name_rsp.unwrap().result, OperationResult::NotFound as i32);
}

fn achievement_rsp(a: u::AchievementReq) -> u::AchievementRsp {
    match respond(&u::Req { achievement_req: Some(a), ..Default::default() }) {
        Reply::Rsp(r) => r.achievement_rsp.expect("an achievement response"),
        _ => panic!("no reply"),
    }
}

fn get_achievements(request_id: u32, only_earned: bool) -> u::GetAchievementsRsp {
    let get = u::GetAchievementsReq { request_id, only_earned: Some(only_earned), account_id: None, language_country_code: Some("en-US".into()) };
    achievement_rsp(u::AchievementReq { get_req: Some(get), ..Default::default() }).get_rsp.expect("a get response")
}

fn earn_achievement(request_id: u32, id: u32) -> u::EarnAchievementRsp {
    achievement_rsp(u::AchievementReq { earn_req: Some(u::EarnAchievementReq { request_id, id }), ..Default::default() }).earn_rsp.expect("an earn response")
}

/// `GetAchievements` answers with quiverd's list (Origins: 59 "Namaste" earned through
/// Connect), `EarnAchievement` marks one earned for later reads; re-sends of earned ones
/// (Origins does it at every save load) are not news and aren't forwarded again. One test:
/// the bridge's achievement state is per process.
#[test]
fn achievements_from_quiverd_and_earns() {
    use crate::achievements as ach;
    ach::reset();
    // Not loaded yet (quiverd unreachable): an empty, successful list.
    assert!(ach::should_load());
    let r = get_achievements(1, false);
    assert_eq!((r.request_id, r.result, r.data.len()), (1, OperationResult::Ok as i32, 0));
    ach::load_failed();
    assert!(!ach::should_load(), "no retry storm after a failed load");

    let item = |id: u32, name: &str, earned_at: u64| ubi_ipc::BridgeAchievement {
        id,
        name: name.into(),
        description: format!("{name}."),
        earned: earned_at > 0,
        earned_at,
    };
    ach::set_list(vec![item(1, "First Steps", 0), item(59, "Namaste", 1_763_337_600), item(60, "Sixty", 0)]);
    assert!(!ach::should_load());
    let r = get_achievements(2, false);
    assert_eq!(r.request_id, 2);
    assert_eq!(
        r.data,
        vec![
            u::AchievementData { id: 1, name: "First Steps".into(), description: "First Steps.".into(), is_earned: false, timestamp: 0 },
            u::AchievementData { id: 59, name: "Namaste".into(), description: "Namaste.".into(), is_earned: true, timestamp: 1_763_337_600 },
            u::AchievementData { id: 60, name: "Sixty".into(), description: "Sixty.".into(), is_earned: false, timestamp: 0 },
        ]
    );
    assert_eq!(get_achievements(3, true).data.iter().map(|d| d.id).collect::<Vec<_>>(), vec![59]);

    // Earned already (quiverd says so): answered OK, not news.
    assert!(!ach::is_new(59));
    assert_eq!(earn_achievement(4, 59), u::EarnAchievementRsp { request_id: 4, result: OperationResult::Ok as i32 });
    // A new one: news once, then earned in the list.
    assert!(ach::is_new(60));
    assert_eq!(earn_achievement(5, 60).result, OperationResult::Ok as i32);
    assert!(!ach::is_new(60));
    let sixty = get_achievements(6, true).data.into_iter().find(|d| d.id == 60).expect("60 now earned");
    assert!(sixty.is_earned && sixty.timestamp > 1_700_000_000);
    // One the list doesn't have still shows (without a name).
    earn_achievement(7, 99);
    assert!(get_achievements(8, true).data.iter().any(|d| d.id == 99 && d.is_earned));

    // The response encodes and decodes as the SDK reads it.
    let rsp = match respond(&u::Req {
        achievement_req: Some(u::AchievementReq {
            get_req: Some(u::GetAchievementsReq { request_id: 9, only_earned: None, account_id: None, language_country_code: None }),
            ..Default::default()
        }),
        ..Default::default()
    }) {
        Reply::Rsp(r) => r,
        _ => panic!(),
    };
    use prost::Message;
    let back = u::Rsp::decode(rsp.encode_to_vec().as_slice()).unwrap();
    assert_eq!(back.achievement_rsp.unwrap().get_rsp.unwrap().data.len(), 4);
    ach::reset();
}

/// The broker messages the bridge sends and reads for achievements.
#[test]
fn achievement_broker_messages() {
    let req = ubi_ipc::BrokerRequest::AchievementEarned { uplay_id: 3539, account_id: Some("u".into()), achievement_id: 59 };
    assert_eq!(
        serde_json::to_value(&req).unwrap(),
        serde_json::json!({ "achievement_earned": { "uplay_id": 3539, "account_id": "u", "achievement_id": 59 } })
    );
    let rsp: ubi_ipc::BrokerResponse =
        serde_json::from_str(r#"{"achievements":{"items":[{"id":59,"name":"Namaste","earned":true,"earned_at":1763337600}],"unlocked_known":true}}"#).unwrap();
    match rsp {
        ubi_ipc::BrokerResponse::Achievements { items, unlocked_known } => {
            assert!(unlocked_known);
            assert_eq!((items[0].id, items[0].earned, items[0].description.as_str()), (59, true, ""));
        }
        other => panic!("{other:?}"),
    }
}
