//! Ephemeral phone-room relay. Only the authenticated desktop supplies match/payment state.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use sats_arena_protocol::rooms::{
    DesktopCommand, HostHello, HostMessage, PhoneMessage, ServerMessage, Snapshot,
};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response,
        protocol::{Role, WebSocketConfig},
    },
};

type WebSocket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

// Reuse the wallet client's WebSocket library instead of shipping a second version.
fn upgrade(
    mut request: Request,
    max_bytes: usize,
    handler: impl FnOnce(WebSocket) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    + Send
    + 'static,
) -> Response {
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .unwrap_or("");
        let origin = origin.to_str().unwrap_or("");
        if origin != format!("http://{host}") && origin != format!("https://{host}") {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let mut handshake = axum::http::Request::new(());
    *handshake.method_mut() = request.method().clone();
    *handshake.version_mut() = request.version();
    *handshake.headers_mut() = request.headers().clone();
    let Ok(response) = create_response(&handshake) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let upgrading = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        if let Ok(stream) = upgrading.await {
            let config = WebSocketConfig::default()
                .max_message_size(Some(max_bytes))
                .max_frame_size(Some(max_bytes));
            handler(
                WebSocketStream::from_raw_socket(TokioIo::new(stream), Role::Server, Some(config))
                    .await,
            )
            .await;
        }
    });
    response.map(|_| axum::body::Body::empty())
}

const MAX_ROOMS: usize = 128;
const MAX_BYTES: usize = 64 * 1024;
const RECONNECT_WINDOW: Duration = Duration::from_secs(600);

#[cfg(test)]
#[path = "rooms_tests.rs"]
mod tests;

#[derive(Clone)]
enum Event {
    State,
    Reply {
        request_id: u64,
        player: usize,
        error: Option<String>,
    },
}

struct RoomState {
    host: Option<mpsc::Sender<DesktopCommand>>,
    generation: u64,
    last_seen: Instant,
    snapshot: Option<Snapshot>,
    claims: [Option<String>; 4],
    pending: BTreeMap<u64, usize>,
    next_request: u64,
}

struct Room {
    token: String,
    state: Mutex<RoomState>,
    events: broadcast::Sender<Event>,
}

#[derive(Clone)]
pub(crate) struct Rooms {
    rooms: Arc<Mutex<BTreeMap<String, Arc<Room>>>>,
    frontend: PathBuf,
}

fn secret() -> String {
    format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    )
}

impl Rooms {
    fn host(
        &self,
        hello: HostHello,
        sender: mpsc::Sender<DesktopCommand>,
    ) -> Result<(String, Arc<Room>, u64), &'static str> {
        let mut rooms = self.rooms.lock().unwrap();
        rooms.retain(|_, room| {
            let state = room.state.lock().unwrap();
            state.host.is_some() || state.last_seen.elapsed() < RECONNECT_WINDOW
        });
        if let (Some(code), Some(token)) = (hello.code, hello.token) {
            let room = rooms
                .get(&code)
                .ok_or("Room expired; create a new room")?
                .clone();
            if room.token != token {
                return Err("Invalid desktop token");
            }
            let generation = {
                let mut state = room.state.lock().unwrap();
                state.generation += 1;
                state.host = Some(sender);
                state.pending.clear();
                state.last_seen = Instant::now();
                state.generation
            };
            let _ = room.events.send(Event::State);
            return Ok((code, room, generation));
        }
        if rooms.len() >= MAX_ROOMS {
            return Err("Room server is full");
        }
        let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let code = loop {
            let code: String = (0..6)
                .map(|_| alphabet[usize::from(rand::random::<u8>()) % alphabet.len()] as char)
                .collect();
            if !rooms.contains_key(&code) {
                break code;
            }
        };
        let (events, _) = broadcast::channel(32);
        let room = Arc::new(Room {
            token: secret(),
            events,
            state: Mutex::new(RoomState {
                host: Some(sender),
                generation: 1,
                last_seen: Instant::now(),
                snapshot: None,
                claims: Default::default(),
                pending: BTreeMap::new(),
                next_request: 0,
            }),
        });
        rooms.insert(code.clone(), room.clone());
        Ok((code, room, 1))
    }

    fn join(
        &self,
        code: &str,
        player: usize,
        token: Option<&str>,
    ) -> Result<(Arc<Room>, String), &'static str> {
        if player >= 4 {
            return Err("Choose Player 1–4");
        }
        let room = self
            .rooms
            .lock()
            .unwrap()
            .get(code)
            .cloned()
            .ok_or("Room not found")?;
        let token = {
            let mut state = room.state.lock().unwrap();
            if state.host.is_none() {
                return Err("Desktop is reconnecting; try again shortly");
            }
            if let Some(existing) = &state.claims[player] {
                if Some(existing.as_str()) != token {
                    return Err("That player slot is already taken");
                }
                existing.clone()
            } else {
                let snapshot = state.snapshot.as_ref().ok_or("Waiting for the desktop")?;
                if !matches!(snapshot.phase, 1 | 2) {
                    return Err("Wait for the next match to join");
                }
                if !matches!(snapshot.players[player].kind, 0 | 3) {
                    return Err("Choose a human player slot");
                }
                let token = secret();
                state.claims[player] = Some(token.clone());
                token
            }
        };
        let _ = room.events.send(Event::State);
        Ok((room, token))
    }
}

impl Room {
    fn snapshot(&self, player: usize, token: &str) -> ServerMessage {
        let state = self.state.lock().unwrap();
        if state.claims[player].as_deref() != Some(token) {
            return ServerMessage::Error {
                message: "This player slot is no longer yours; join again".into(),
            };
        }
        let mut snapshot = state.snapshot.clone();
        if let Some(snapshot) = &mut snapshot {
            for (i, slot) in snapshot.players.iter_mut().enumerate() {
                if i != player {
                    slot.invoice.clear();
                }
            }
        }
        ServerMessage::State {
            online: state.host.is_some(),
            claimed: state.claims.each_ref().map(Option::is_some),
            snapshot: snapshot.map(Box::new),
        }
    }

    fn command(
        &self,
        player: usize,
        token: &str,
        message: PhoneMessage,
    ) -> Result<bool, &'static str> {
        let mut state = self.state.lock().unwrap();
        if state.claims[player].as_deref() != Some(token) {
            return Err("Join the room again");
        }
        let snapshot = state.snapshot.as_ref().ok_or("Waiting for the desktop")?;
        if matches!(message, PhoneMessage::Leave) {
            if snapshot.locked {
                return Err("Your player stays bound until the match finishes");
            }
            state.claims[player] = None;
            let _ = self.events.send(Event::State);
            return Ok(true);
        }
        let sender = state.host.clone().ok_or("Desktop is reconnecting")?;
        if state.pending.values().any(|&p| p == player) {
            return Err("Your previous request is still being processed");
        }
        let request_id = state.next_request + 1;
        let command = match message {
            PhoneMessage::SetAddress { match_id, address } => {
                if address.len() > 127
                    || !address.contains('@')
                    || address.chars().any(char::is_whitespace)
                {
                    return Err("Enter a Lightning Address such as name@example.com");
                }
                DesktopCommand::SetAddress {
                    request_id,
                    player,
                    match_id,
                    address,
                }
            }
            PhoneMessage::RequestInvoice { match_id } => DesktopCommand::RequestInvoice {
                request_id,
                player,
                match_id,
            },
            _ => return Err("Unsupported room request"),
        };
        sender
            .try_send(command)
            .map_err(|_| "Desktop is busy; try again")?;
        state.next_request = request_id;
        state.pending.insert(request_id, player);
        Ok(false)
    }
}

async fn send(socket: &mut WebSocket, message: &impl serde::Serialize) -> Result<(), ()> {
    socket
        .send(Message::Text(
            serde_json::to_string(message).map_err(|_| ())?.into(),
        ))
        .await
        .map_err(|_| ())
}

async fn first<T: serde::de::DeserializeOwned>(socket: &mut WebSocket) -> Option<T> {
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .ok()??
        .ok()?;
    let Message::Text(text) = message else {
        return None;
    };
    serde_json::from_str(&text).ok()
}

async fn host_socket(mut socket: WebSocket, rooms: Rooms) {
    let Some(hello) = first::<HostHello>(&mut socket).await else {
        return;
    };
    let (sender, mut commands) = mpsc::channel(8);
    let (code, room, generation) = match rooms.host(hello, sender) {
        Ok(value) => value,
        Err(message) => {
            let _ = send(
                &mut socket,
                &ServerMessage::Error {
                    message: message.into(),
                },
            )
            .await;
            return;
        }
    };
    if send(
        &mut socket,
        &ServerMessage::Hosted {
            code,
            token: room.token.clone(),
        },
    )
    .await
    .is_ok()
    {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        let mut last_received = Instant::now();
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    if send(&mut socket, &command).await.is_err() { break; }
                }
                message = socket.next() => {
                    let Some(Ok(message)) = message else { break; };
                    last_received = Instant::now();
                    let Message::Text(text) = message else {
                        if matches!(message, Message::Close(_)) { break; }
                        continue;
                    };
                    let Ok(message) = serde_json::from_str::<HostMessage>(&text) else { break; };
                    let event = {
                        let mut state = room.state.lock().unwrap();
                        if state.generation != generation { break; }
                        state.last_seen = Instant::now();
                        match message {
                            HostMessage::Snapshot { snapshot } => {
                                if snapshot.logs.len() > 100 || snapshot.players.iter().any(|p| p.invoice.len() > 2048 || p.address.len() > 127) { break; }
                                state.snapshot = Some(*snapshot);
                                Event::State
                            }
                            HostMessage::Reply { request_id, error } => {
                                let Some(player) = state.pending.remove(&request_id) else { continue; };
                                Event::Reply { request_id, player, error }
                            }
                        }
                    };
                    let _ = room.events.send(event);
                }
                _ = heartbeat.tick() => {
                    if last_received.elapsed() > Duration::from_secs(45) || socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                }
            }
        }
    }
    {
        let mut state = room.state.lock().unwrap();
        if state.generation == generation {
            state.host = None;
            state.last_seen = Instant::now();
            state.pending.clear();
        }
    }
    let _ = room.events.send(Event::State);
}

async fn phone_socket(mut socket: WebSocket, rooms: Rooms) {
    let Some(PhoneMessage::Join {
        code,
        player,
        token,
    }) = first(&mut socket).await
    else {
        return;
    };
    let code = code.trim().to_ascii_uppercase();
    let (room, token) = match rooms.join(&code, player, token.as_deref()) {
        Ok(value) => value,
        Err(message) => {
            let _ = send(
                &mut socket,
                &ServerMessage::Error {
                    message: message.into(),
                },
            )
            .await;
            return;
        }
    };
    let mut events = room.events.subscribe();
    if send(
        &mut socket,
        &ServerMessage::Joined {
            code,
            player,
            token: token.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }
    if send(&mut socket, &room.snapshot(player, &token))
        .await
        .is_err()
    {
        return;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_received = Instant::now();
    let mut last_request = Instant::now() - Duration::from_secs(1);
    loop {
        if room.state.lock().unwrap().claims[player].as_deref() != Some(token.as_str()) {
            break;
        }
        let message = tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Reply { request_id, player: recipient, error }) if recipient == player => ServerMessage::Result { request_id, error },
                Ok(Event::Reply { .. }) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
                _ => room.snapshot(player, &token),
            },
            incoming = socket.next() => {
                let Some(Ok(incoming)) = incoming else { break; };
                last_received = Instant::now();
                let Message::Text(text) = incoming else {
                    if matches!(incoming, Message::Close(_)) { break; }
                    continue;
                };
                if last_request.elapsed() < Duration::from_millis(500) {
                    let _ = send(&mut socket, &ServerMessage::Error { message: "Please wait a moment before another request".into() }).await;
                    continue;
                }
                last_request = Instant::now();
                let result = serde_json::from_str(&text).map_err(|_| "Invalid room request").and_then(|message| room.command(player, &token, message));
                match result {
                    Ok(true) => { let _ = send(&mut socket, &ServerMessage::Left).await; break; }
                    Ok(false) => continue,
                    Err(message) => ServerMessage::Error { message: message.into() },
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(45) || socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                continue;
            }
        };
        if send(&mut socket, &message).await.is_err() {
            break;
        }
    }
}

async fn asset(rooms: Rooms, file: &str) -> Response {
    let content_type = if file == "index.html" {
        "text/html; charset=utf-8"
    } else if file.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if file.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(rooms.frontend.join(file)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "Build server/frontend first: npm ci && npm run build",
        )
            .into_response(),
    }
}

pub(crate) fn router(frontend: PathBuf) -> Router {
    let rooms = Rooms {
        rooms: Default::default(),
        frontend,
    };
    Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(
                    serde_json::json!({"ok": true, "service": "sats-melee-rooms", "version": 1}),
                )
            }),
        )
        .route(
            "/rooms/host",
            get(|State(rooms): State<Rooms>, request: Request| async move {
                upgrade(request, MAX_BYTES, |socket| {
                    Box::pin(host_socket(socket, rooms))
                })
            }),
        )
        .route(
            "/rooms/join",
            get(|State(rooms): State<Rooms>, request: Request| async move {
                upgrade(request, 4096, |socket| {
                    Box::pin(phone_socket(socket, rooms))
                })
            }),
        )
        .route(
            "/",
            get(|State(rooms): State<Rooms>| async move { asset(rooms, "index.html").await }),
        )
        .route(
            "/assets/{file}",
            get(
                |State(rooms): State<Rooms>, Path(file): Path<String>| async move {
                    if !file
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    asset(rooms, &format!("assets/{file}")).await
                },
            ),
        )
        .with_state(rooms)
}

pub(crate) async fn serve(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut bind = "127.0.0.1:8738".to_owned();
    let mut frontend = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("frontend/dist");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bind requires an address"))?
            }
            "--frontend" => {
                frontend = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--frontend requires a directory"))?
                    .into()
            }
            _ => anyhow::bail!(
                "Usage: sats-arena-server rooms [--bind 127.0.0.1:8738] [--frontend server/frontend/dist]"
            ),
        }
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("Phone rooms listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(frontend))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
