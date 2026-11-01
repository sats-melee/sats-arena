use super::*;
use sats_arena_protocol::rooms::Player;
use tokio_tungstenite::{client_async, tungstenite::client::IntoClientRequest};

fn snapshot() -> Snapshot {
    Snapshot {
        match_id: "match-one".into(),
        phase: 1,
        mode: "simulated".into(),
        roster_ready: true,
        locked: false,
        entry_sats: 400,
        invoice_sats: 408,
        players: std::array::from_fn(|i| Player {
            kind: if i < 2 { 0 } else { 3 },
            invoice: format!("invoice-{i}"),
            ..Player::default()
        }),
        damage_centi: [[0; 4]; 4],
        logs: vec![],
    }
}

fn rooms() -> Rooms {
    Rooms {
        rooms: Default::default(),
        frontend: "frontend/dist".into(),
    }
}

#[test]
fn rooms_and_slot_claims_are_isolated_and_resume_requires_the_correct_token() {
    let rooms = rooms();
    let (host, mut commands) = mpsc::channel(8);
    let (code, room, _) = rooms
        .host(
            HostHello {
                code: None,
                token: None,
            },
            host,
        )
        .unwrap();
    room.state.lock().unwrap().snapshot = Some(snapshot());
    let (_, token) = rooms.join(&code, 0, None).unwrap();
    assert!(rooms.join(&code, 0, None).is_err());
    assert!(rooms.join(&code, 0, Some("wrong-token")).is_err());
    assert!(rooms.join(&code, 0, Some(&token)).is_ok());
    assert!(matches!(
        room.snapshot(0, "wrong-token"),
        ServerMessage::Error { .. }
    ));
    assert!(rooms.join(&code, 2, None).is_err());
    assert!(rooms.join(&code, 4, None).is_err());
    let ServerMessage::State {
        snapshot: Some(view),
        ..
    } = room.snapshot(0, &token)
    else {
        panic!()
    };
    assert_eq!(view.players[0].invoice, "invoice-0");
    assert!(view.players[1].invoice.is_empty());
    room.command(
        0,
        &token,
        PhoneMessage::RequestInvoice {
            match_id: "match-one".into(),
        },
    )
    .unwrap();
    let command = commands.try_recv().unwrap();
    assert!(matches!(
        command,
        DesktopCommand::RequestInvoice { player: 0, .. }
    ));
    assert!(room.command(0, "wrong-token", PhoneMessage::Leave).is_err());
    let (host, _) = mpsc::channel(8);
    assert!(
        rooms
            .host(
                HostHello {
                    code: Some(code.clone()),
                    token: Some("wrong-token".into())
                },
                host
            )
            .is_err()
    );
    let (host, _) = mpsc::channel(8);
    let (other_code, other, _) = rooms
        .host(
            HostHello {
                code: None,
                token: None,
            },
            host,
        )
        .unwrap();
    assert_ne!(code, other_code);
    other.state.lock().unwrap().snapshot = Some(snapshot());
    assert!(rooms.join(&other_code, 0, None).is_ok());
    room.state.lock().unwrap().snapshot.as_mut().unwrap().locked = true;
    assert!(rooms.join(&code, 1, None).is_err());
    assert!(room.command(0, &token, PhoneMessage::Leave).is_err());
}

async fn read(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) => socket.flush().await.unwrap(),
                message => panic!("Unexpected message: {message:?}"),
            }
        }
    })
    .await
    .expect("WebSocket response timed out")
}

#[tokio::test]
async fn websocket_protocol_relays_requests_snapshots_and_host_reconnection() {
    let app = router("frontend/dist".into());
    let host_url = "ws://rooms.test/rooms/host";
    let join_url = "ws://rooms.test/rooms/join";
    let (mut host, _) = connect(app.clone(), host_url.into_client_request().unwrap())
        .await
        .unwrap();
    host.send(Message::Text(
        serde_json::to_string(&HostHello {
            code: None,
            token: None,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let hosted = read(&mut host).await;
    let code = hosted["code"].as_str().unwrap().to_owned();
    let host_token = hosted["token"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 6);
    host.send(Message::Text(
        serde_json::to_string(&HostMessage::Snapshot {
            snapshot: Box::new(snapshot()),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let (mut phone, _) = connect(app.clone(), join_url.into_client_request().unwrap())
        .await
        .unwrap();
    phone
        .send(Message::Text(
            serde_json::to_string(&PhoneMessage::Join {
                code: code.clone(),
                player: 0,
                token: None,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let joined = read(&mut phone).await;
    assert_eq!(joined["type"], "joined");
    let state = read(&mut phone).await;
    assert_eq!(state["snapshot"]["players"][0]["invoice"], "invoice-0");
    assert_eq!(state["snapshot"]["players"][1]["invoice"], "");
    phone
        .send(Message::Text(
            serde_json::to_string(&PhoneMessage::SetAddress {
                match_id: "match-one".into(),
                address: "player@example.com".into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let command = read(&mut host).await;
    assert_eq!(command["type"], "set_address");
    assert_eq!(command["player"], 0);
    host.send(Message::Text(
        serde_json::to_string(&HostMessage::Reply {
            request_id: command["request_id"].as_u64().unwrap(),
            error: None,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    assert_eq!(read(&mut phone).await["type"], "result");
    let mut update = snapshot();
    update.damage_centi[0][1] = 4200;
    host.send(Message::Text(
        serde_json::to_string(&HostMessage::Snapshot {
            snapshot: Box::new(update),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        read(&mut phone).await["snapshot"]["damage_centi"][0][1],
        4200
    );
    host.close(None).await.unwrap();
    assert_eq!(read(&mut phone).await["online"], false);
    let (mut resumed, _) = connect(app.clone(), host_url.into_client_request().unwrap())
        .await
        .unwrap();
    resumed
        .send(Message::Text(
            serde_json::to_string(&HostHello {
                code: Some(code.clone()),
                token: Some(host_token),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(read(&mut resumed).await["code"], code);
    assert_eq!(read(&mut phone).await["online"], true);
    let mut hostile = join_url.into_client_request().unwrap();
    hostile
        .headers_mut()
        .insert("Origin", "https://another.example".parse().unwrap());
    let error = connect(app, hostile).await.unwrap_err();
    assert!(
        matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::FORBIDDEN)
    );
}

// Exercise HTTP Upgrade and real WebSocket frames without depending on OS socket permissions.
async fn connect(
    app: Router,
    request: axum::http::Request<()>,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
        axum::http::Response<Option<Vec<u8>>>,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    use tower::ServiceExt;
    let (client, server) = tokio::io::duplex(128 * 1024);
    tokio::spawn(async move {
        let service = hyper::service::service_fn(move |request| app.clone().oneshot(request));
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), service)
            .with_upgrades()
            .await;
    });
    client_async(request, client).await
}
