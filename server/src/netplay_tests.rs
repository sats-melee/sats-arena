use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn player(port: u16) -> Value {
    json!({"name": "Player", "build": "test-build", "rules": "vanilla-1v1", "endpoint": format!("127.0.0.1:{port}")})
}
async fn create_room(app: &Router, port: u16) -> (String, String) {
    let (status, body) = request(app, "POST", "/netplay/rooms", "", player(port)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["slot"], 0);
    (
        format!("/netplay/rooms/{}", body["room"]["code"].as_str().unwrap()),
        body["token"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn room_http_flow_freezes_shared_configuration_and_isolates_tokens() {
    // Exercise the same router used by the existing room server.
    let app = crate::rooms::router("frontend/dist".into());
    let (path, host) = create_room(&app, 41001).await;
    let (_, outsider) = create_room(&app, 42001).await;
    assert_eq!(
        request(&app, "GET", &path, &outsider, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let mut wrong_build = player(41002);
    wrong_build["build"] = json!("different");
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", wrong_build)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut wrong_rules = player(41002);
    wrong_rules["rules"] = json!("different");
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", wrong_rules)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", player(41001))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (status, joined) = request(&app, "POST", &format!("{path}/join"), "", player(41002)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(joined["slot"], 1);
    let guest = joined["token"].as_str().unwrap();
    assert_eq!(
        request(&app, "POST", &path, guest, json!({"type":"start"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"start"}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    for token in [&host, guest] {
        assert_eq!(
            request(
                &app,
                "POST",
                &path,
                token,
                json!({"type":"ready", "ready":true})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (status, started) = request(&app, "POST", &path, &host, json!({"type":"start"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(started["match_id"].as_str().unwrap().len(), 32);
    assert!(!started.to_string().contains(&host));
    assert!(!started.to_string().contains(guest));
    // A delayed or retried Ready acknowledgement must not tear down a started room.
    for token in [&host, guest] {
        let (status, snapshot) = request(
            &app,
            "POST",
            &path,
            token,
            json!({"type":"ready", "ready":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(snapshot, started);
    }
    assert_eq!(
        request(&app, "GET", &path, guest, Value::Null).await.1,
        started
    );
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"start"}))
            .await
            .1,
        started
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &path,
            guest,
            json!({"type":"ready","ready":false})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", player(41003))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (_, remaining) = request(&app, "POST", &path, guest, json!({"type":"leave"})).await;
    assert_eq!(remaining["players"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(&app, "GET", &path, guest, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&app, "GET", &path, &host, Value::Null).await.0,
        StatusCode::OK
    );
    request(&app, "POST", &path, &host, json!({"type":"close"})).await;
    assert_eq!(
        request(&app, "GET", &path, &host, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn roster_changes_reset_readiness_and_revoke_departed_tokens() {
    let app = router();
    let mut config = player(43001);
    config["config"] = json!({"player_count":4,"teams":false,"team_ids":[255,255,255,255]});
    let (_, admission) = request(&app, "POST", "/netplay/rooms", "", config).await;
    let path = format!(
        "/netplay/rooms/{}",
        admission["room"]["code"].as_str().unwrap()
    );
    let host = admission["token"].as_str().unwrap().to_owned();
    request(
        &app,
        "POST",
        &path,
        &host,
        json!({"type":"ready","ready":true}),
    )
    .await;
    let mut guests = vec![];
    for port in 43002..=43004 {
        let (status, body) = request(&app, "POST", &format!("{path}/join"), "", player(port)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["room"]["players"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["ready"] == false)
        );
        guests.push(body["token"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", player(43005))
            .await
            .0,
        StatusCode::CONFLICT
    );
    request(
        &app,
        "POST",
        &path,
        &host,
        json!({"type":"ready","ready":true}),
    )
    .await;
    let (_, left) = request(&app, "POST", &path, &guests[0], json!({"type":"leave"})).await;
    assert_eq!(left["players"][0]["ready"], false);
    assert_eq!(
        request(&app, "GET", &path, &guests[0], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, replacement) = request(&app, "POST", &format!("{path}/join"), "", player(43005)).await;
    assert_eq!(replacement["slot"], 1);
    assert_ne!(replacement["token"], guests[0]);
    request(&app, "POST", &path, &host, json!({"type":"leave"})).await;
    assert_eq!(
        request(&app, "GET", &path, &guests[1], Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn preserves_optional_lan_candidates_and_rejects_invalid_ones() {
    let app = router();
    let mut body = player(43113);
    body["endpoint"] = json!("203.0.113.10:54321");
    body["lan_endpoint"] = json!("192.168.1.10:43113");
    let (status, admission) = request(&app, "POST", "/netplay/rooms", "", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        admission["room"]["players"][0]["endpoint"],
        "203.0.113.10:54321"
    );
    assert_eq!(
        admission["room"]["players"][0]["lan_endpoint"],
        "192.168.1.10:43113"
    );
    for endpoint in [
        "0.0.0.0:42",
        "127.0.0.1:0",
        "224.0.0.1:42",
        "255.255.255.255:42",
    ] {
        let mut body = player(43114);
        body["lan_endpoint"] = json!(endpoint);
        assert_eq!(
            request(&app, "POST", "/netplay/rooms", "", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, old_client) = request(&app, "POST", "/netplay/rooms", "", player(43115)).await;
    assert!(
        old_client["room"]["players"][0]
            .get("lan_endpoint")
            .is_none()
    );
}

#[tokio::test]
async fn rejects_invalid_input_and_missing_auth() {
    let app = router();
    for endpoint in [
        "0.0.0.0:42",
        "127.0.0.1:0",
        "224.0.0.1:42",
        "255.255.255.255:42",
    ] {
        let mut body = player(1);
        body["endpoint"] = json!(endpoint);
        assert_eq!(
            request(&app, "POST", "/netplay/rooms", "", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut body = player(1);
    body["name"] = json!("x".repeat(65));
    assert_eq!(
        request(&app, "POST", "/netplay/rooms", "", body).await.0,
        StatusCode::BAD_REQUEST
    );
    let (path, _) = create_room(&app, 44001).await;
    assert_eq!(
        request(&app, "POST", &path, "", json!({"type":"start"}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn guest_expiry_removes_only_the_guest_and_host_expiry_closes_room() {
    let join: Join = serde_json::from_value(player(45001)).unwrap();
    let mut entry = Entry {
        view: Room {
            config: Default::default(),
            code: "TEST".into(),
            build: join.build.clone(),
            rules: join.rules.clone(),
            players: vec![],
            round: 1,
            match_complete: false,
            match_id: None,
            payment: None,
        },
        members: BTreeMap::new(),
        finished: BTreeSet::new(),
        previous_match_id: None,
    };
    assert!(entry.admit(join).is_ok());
    assert!(
        entry
            .admit(serde_json::from_value(player(45002)).unwrap())
            .is_ok()
    );
    entry.members.get_mut(&1).unwrap().seen = Instant::now() - LEASE;
    let mut rooms = BTreeMap::from([("TEST".into(), entry)]);
    assert!(prune(&mut rooms, None).await.is_ok());
    assert_eq!(rooms["TEST"].members.len(), 1);
    rooms
        .get_mut("TEST")
        .unwrap()
        .members
        .get_mut(&0)
        .unwrap()
        .seen = Instant::now() - LEASE;
    assert!(prune(&mut rooms, None).await.is_ok());
    assert!(rooms.is_empty());
}

#[tokio::test]
async fn paid_room_expiry_closes_finances_even_if_members_keep_polling_them() {
    use crate::payments::tests::{fund, prepared, settle};
    for admitted in [false, true] {
        let (_temp, payments, fake, id) = prepared().await;
        fund(&payments, &fake, &id, &[0, 1]).await;
        if admitted {
            assert!(payments.admitted(&id).await.is_ok());
        }
        let mut room = Entry {
            view: Room {
                config: Default::default(),
                code: "TEST".into(),
                build: "test".into(),
                rules: "test".into(),
                players: vec![],
                round: 1,
                match_complete: false,
                match_id: admitted.then(|| id.clone()),
                payment: Some(payments.view(&id).await.ok().unwrap()),
            },
            members: BTreeMap::new(),
            finished: BTreeSet::new(),
            previous_match_id: None,
        };
        for (slot, token) in [(0, "a".repeat(64)), (1, "b".repeat(64))] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            assert!(payments.authenticated(&id, &headers, None).await.is_ok());
            room.members.insert(
                slot,
                Member {
                    token,
                    seen: if slot == 1 {
                        Instant::now() - LEASE
                    } else {
                        Instant::now()
                    },
                },
            );
        }
        let mut rooms = BTreeMap::from([("TEST".into(), room)]);
        assert!(prune(&mut rooms, Some(&payments)).await.is_ok());
        assert_eq!(rooms["TEST"].members.len(), 1);
        if admitted {
            assert!(payments.view(&id).await.ok().unwrap().frozen);
            assert_eq!(fake.payment_attempts(), 0);
        } else {
            settle(&payments, &id).await;
            let view = payments.view(&id).await.ok().unwrap();
            assert_eq!(view.players[0].earned_sats, 500);
            assert_eq!(view.players[1].earned_sats, 500);
        }
    }
}

#[tokio::test]
async fn configured_four_player_room_never_starts_after_only_two_join() {
    let app = router();
    let mut host = player(46001);
    host["config"] = json!({"player_count":4,"teams":true,"team_ids":[0,1,0,1]});
    let (_, admission) = request(&app, "POST", "/netplay/rooms", "", host).await;
    assert_eq!(admission["room"]["config"]["player_count"], 4);
    let path = format!(
        "/netplay/rooms/{}",
        admission["room"]["code"].as_str().unwrap()
    );
    let mut tokens = vec![admission["token"].as_str().unwrap().to_owned()];
    for port in 46002..=46004 {
        let (status, admission) =
            request(&app, "POST", &format!("{path}/join"), "", player(port)).await;
        assert_eq!(status, StatusCode::OK);
        tokens.push(admission["token"].as_str().unwrap().into());
        for token in &tokens {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &path,
                    token,
                    json!({"type":"ready","ready":true})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        let (status, room) =
            request(&app, "POST", &path, &tokens[0], json!({"type":"start"})).await;
        if tokens.len() < 4 {
            assert_eq!(status, StatusCode::CONFLICT);
        } else {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(room["players"].as_array().unwrap().len(), 4);
        }
    }
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", player(46005))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn free_rematches_keep_membership_and_close_is_host_only() {
    let app = router();
    let (path, host) = create_room(&app, 47001).await;
    let (_, admission) = request(&app, "POST", &format!("{path}/join"), "", player(47002)).await;
    let guest = admission["token"].as_str().unwrap();
    assert_eq!(
        request(&app, "POST", &path, guest, json!({"type":"close"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for token in [&host, guest] {
        request(
            &app,
            "POST",
            &path,
            token,
            json!({"type":"ready","ready":true}),
        )
        .await;
    }
    let (_, first) = request(&app, "POST", &path, &host, json!({"type":"start"})).await;
    let id = first["match_id"].as_str().unwrap();
    let next = json!({"type":"next_match","match_id":id});
    assert_eq!(
        request(&app, "POST", &path, guest, next.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "POST", &path, &host, next.clone()).await.0,
        StatusCode::CONFLICT
    );
    for token in [&host, guest] {
        assert_eq!(
            request(
                &app,
                "POST",
                &path,
                token,
                json!({"type":"finish","match_id":id})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (_, second) = request(&app, "POST", &path, &host, next.clone()).await;
    assert_eq!(second["round"], 2);
    assert_eq!(second["code"], first["code"]);
    assert_eq!(second["match_id"], Value::Null);
    assert_eq!(second["match_complete"], false);
    assert!(
        second["players"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["ready"] == false)
    );
    assert_eq!(request(&app, "POST", &path, &host, next).await.1, second);
    assert_eq!(
        request(
            &app,
            "POST",
            &path,
            guest,
            json!({"type":"finish","match_id":id})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    for token in [&host, guest] {
        request(
            &app,
            "POST",
            &path,
            token,
            json!({"type":"ready","ready":true}),
        )
        .await;
    }
    let (_, started) = request(&app, "POST", &path, &host, json!({"type":"start"})).await;
    assert_ne!(started["match_id"], first["match_id"]);
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"close"}))
            .await
            .1,
        Value::Null
    );
    assert_eq!(
        request(&app, "GET", &path, guest, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

async fn paid_room(
    payments: &Payments,
    app: &Router,
    port: u16,
) -> (String, String, String, String) {
    use crate::payments::tests::pump;
    let (path, host) = create_room(app, port).await;
    let (_, admission) = request(app, "POST", &format!("{path}/join"), "", player(port + 1)).await;
    let guest = admission["token"].as_str().unwrap().to_owned();
    let id = admission["room"]["payment"]["match_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (slot, token) in [&host, &guest].into_iter().enumerate() {
        assert_eq!(
            request(
                app,
                "POST",
                &path,
                token,
                json!({"type":"set_address","address":format!("player{slot}@example.com")})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    pump(payments, 20).await;
    (path, host, guest, id)
}
fn completed(id: &str, game_number: u32) -> Value {
    json!({"type":"report","report":{"match_id":id,"manifest":{"game_number":game_number,"stage":31,"seed":42,"teams":false,"kinds":[0,0,3,3],"team_ids":[255,255,255,255],"characters":[17,17,255,255]},"credits":[],"end":{"last_sequence":0,"winner":0,"complete":true}}})
}
async fn start_paid(app: &Router, path: &str, host: &str, guest: &str) {
    for token in [host, guest] {
        assert_eq!(
            request(
                app,
                "POST",
                path,
                token,
                json!({"type":"ready","ready":true})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(app, "POST", path, host, json!({"type":"start"}))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn paid_rematch_has_fresh_entries_and_keeps_prior_records_and_membership() {
    use crate::payments::tests::{fund, prepared, pump, settle};
    let (_temp, payments, fake, _unused) = prepared().await;
    let app = router_with(Some(payments.clone()));
    let (path, host, guest, id) = paid_room(&payments, &app, 48001).await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    start_paid(&app, &path, &host, &guest).await;
    let next = json!({"type":"next_match","match_id":id});
    assert_eq!(
        request(&app, "POST", &path, &host, next.clone()).await.0,
        StatusCode::CONFLICT
    );
    for token in [&host, &guest] {
        assert_eq!(
            request(&app, "POST", &path, token, completed(&id, 1))
                .await
                .0,
            StatusCode::OK
        );
    }
    settle(&payments, &id).await;
    let previous = payments.view(&id).await.ok().unwrap();
    let (status, room) = request(&app, "POST", &path, &host, next.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let id2 = room["payment"]["match_id"].as_str().unwrap();
    assert_ne!(id2, id);
    assert_eq!(room["round"], 2);
    assert_eq!(room["match_id"], Value::Null);
    assert_eq!(room["payment"]["agreed_sequence"], 0);
    assert_eq!(room["payment"]["own_next_sequence"], json!([1, 1, 1, 1]));
    assert!(
        room["payment"]["players"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["paid_sats"] == 0 && p["earned_sats"] == 0)
    );
    assert_eq!(
        room["payment"]["players"][0]["address"],
        "player0@example.com"
    );
    assert_eq!(
        request(&app, "POST", &path, &host, next).await.1["payment"]["match_id"],
        id2
    );
    assert_eq!(
        request(&app, "POST", &path, &guest, completed(&id, 1))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/netplay/matches/{id}"),
            &guest,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/netplay/matches/{id}"),
            &guest,
            completed(&id, 1)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(payments.view(&id).await.ok().unwrap(), previous);
    pump(&payments, 20).await;
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"start"}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    fund(&payments, &fake, id2, &[0, 1]).await;
    start_paid(&app, &path, &host, &guest).await;
    for token in [&host, &guest] {
        assert_eq!(
            request(&app, "POST", &path, token, completed(id2, 2))
                .await
                .0,
            StatusCode::OK
        );
    }
    settle(&payments, id2).await;
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"close"}))
            .await
            .1,
        Value::Null
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/netplay/matches/{id2}"),
            &guest,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn pregame_guest_leave_refunds_then_host_reopens_same_room_for_replacement() {
    use crate::payments::tests::{fund, prepared, pump, settle};
    use sats_arena_core::wallet::{InvoiceState, Wallet};
    let (temp, payments, fake, _unused) = prepared().await;
    let app = router_with(Some(payments.clone()));
    let (path, host, guest, id) = paid_room(&payments, &app, 49001).await;
    let original = payments.view(&id).await.ok().unwrap();
    let late_hash = fake
        .decode_invoice(&original.players[1].invoice)
        .unwrap()
        .payment_hash;
    fund(&payments, &fake, &id, &[0]).await;
    let (status, remaining) = request(&app, "POST", &path, &guest, json!({"type":"leave"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(remaining["players"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(&app, "GET", &path, &guest, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    settle(&payments, &id).await;
    assert_eq!(
        payments.view(&id).await.ok().unwrap().players[0].earned_sats,
        500
    );
    let (status, next) = request(
        &app,
        "POST",
        &path,
        &host,
        json!({"type":"next_match","match_id":id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["code"], remaining["code"]);
    assert_eq!(next["round"], 2);
    assert_ne!(next["payment"]["match_id"], id);
    let (status, replacement) =
        request(&app, "POST", &format!("{path}/join"), "", player(49003)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replacement["slot"], 1);
    assert_ne!(replacement["token"], guest);
    assert_eq!(replacement["room"]["payment"]["players"][1]["address"], "");
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/netplay/matches/{id}"),
            replacement["token"].as_str().unwrap(),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/netplay/matches/{id}"),
            &guest,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        replacement["room"]["payment"]["players"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["paid_sats"] == 0)
    );
    fake.set_invoice_state(
        &late_hash,
        InvoiceState::Paid {
            amount_sats: original.invoice_sats,
        },
    );
    pump(&payments, 30).await;
    let current_id = next["payment"]["match_id"].as_str().unwrap();
    assert!(
        payments
            .view(current_id)
            .await
            .ok()
            .unwrap()
            .players
            .iter()
            .all(|p| p.paid_sats == 0)
    );
    // A retired invoice may need two five-second polling windows before its refund.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            pump(&payments, 1).await;
            let ledger: Value = serde_json::from_slice(
                &std::fs::read(temp.path().join(&id).join("prepaid-match.json")).unwrap(),
            )
            .unwrap();
            if ledger["retired_entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["incoming_hash"] == late_hash && entry["state"] == "Paid")
            {
                assert_eq!(fake.payment_attempts(), 2);
                assert!(
                    payments
                        .view(current_id)
                        .await
                        .ok()
                        .unwrap()
                        .players
                        .iter()
                        .all(|player| player.paid_sats == 0)
                );
                return;
            }
        }
    })
    .await
    .expect("Late original entry was not refunded on its original record");
}

#[tokio::test]
async fn closing_after_own_complete_result_keeps_final_report_grace() {
    use crate::payments::tests::{fund, prepared, settle};
    let (_temp, payments, fake, _unused) = prepared().await;
    let app = router_with(Some(payments.clone()));
    let (path, host, guest, id) = paid_room(&payments, &app, 50001).await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    start_paid(&app, &path, &host, &guest).await;
    assert_eq!(
        request(&app, "POST", &path, &host, completed(&id, 1))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "POST", &path, &host, json!({"type":"close"}))
            .await
            .1,
        Value::Null
    );
    assert!(!payments.view(&id).await.ok().unwrap().frozen);
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/netplay/matches/{id}"),
            &guest,
            completed(&id, 1)
        )
        .await
        .0,
        StatusCode::OK
    );
    settle(&payments, &id).await;
    assert!(!payments.view(&id).await.ok().unwrap().frozen);
}
