//! Host-side tests of the bridge's answers (no Wine needed).

use quiver_proto::uplaydll::{self as u, OperationResult};

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
