//! In-memory desktop room coordination, mounted on the existing room server.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use sats_arena_protocol::netplay::{Admission, Command, Join, Player, Room};

use crate::{api::ApiError, payments::Payments};
use tokio::sync::Mutex;

const LEASE: Duration = Duration::from_secs(120);
const MAX_ROOMS: usize = 128;
#[derive(Clone, Default)]
struct Store {
    rooms: Arc<Mutex<BTreeMap<String, Entry>>>,
    payments: Option<Payments>,
}

struct Member {
    token: String,
    seen: Instant,
}
struct Entry {
    view: Room,
    members: BTreeMap<u8, Member>,
    finished: BTreeSet<u8>,
    previous_match_id: Option<String>,
}

fn error(status: StatusCode, message: &str) -> ApiError {
    ApiError::Status(status, message.into())
}
fn conflict(message: &str) -> ApiError {
    error(StatusCode::CONFLICT, message)
}
fn secret() -> String {
    format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    )
}

fn validate(join: &Join) -> Result<(), ApiError> {
    if !join.config.is_valid() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid room roster or teams",
        ));
    }
    for (value, max) in [(&join.name, 64), (&join.build, 128), (&join.rules, 128)] {
        if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Invalid name, build, or rules",
            ));
        }
    }
    for endpoint in std::iter::once(join.endpoint).chain(join.lan_endpoint) {
        let ip = endpoint.ip();
        if endpoint.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip == std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
        {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid gameplay endpoint"));
        }
    }
    Ok(())
}

async fn prune(
    rooms: &mut BTreeMap<String, Entry>,
    payments: Option<&Payments>,
) -> Result<(), ApiError> {
    let expired: Vec<_> = rooms
        .iter()
        .filter(|(_, room)| room.members.values().any(|m| m.seen.elapsed() >= LEASE))
        .map(|(code, room)| {
            (
                code.clone(),
                room.members
                    .iter()
                    .filter(|(_, m)| m.seen.elapsed() >= LEASE)
                    .map(|(&slot, _)| slot)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    for (code, slots) in expired {
        let room = rooms.get_mut(&code).unwrap();
        if let (Some(payments), Some(snapshot)) = (payments, &room.view.payment) {
            payments.abandon(&snapshot.match_id).await?;
        }
        if slots.contains(&0) {
            rooms.remove(&code);
        } else {
            for slot in slots {
                room.remove_member(slot);
            }
        }
    }
    Ok(())
}

impl Entry {
    fn remove_member(&mut self, slot: u8) {
        self.members.remove(&slot);
        self.view.players.retain(|p| p.slot != slot);
        self.finished.remove(&slot);
        for player in &mut self.view.players {
            player.ready = false;
        }
        if self.view.payment.is_none() {
            // A departed free-game peer ends this roster; the host may explicitly prepare a replacement round.
            self.view.match_complete = self.view.match_id.is_some();
        }
    }
    fn update_payment(&mut self, payment: sats_arena_protocol::netplay::PaymentSnapshot) {
        self.view.match_complete = payment.phase == 4
            && !payment.frozen
            && payment
                .players
                .iter()
                .all(|player| player.pending_sats == 0);
        self.view.payment = Some(payment);
    }
    fn admit(&mut self, join: Join) -> Result<Admission, ApiError> {
        if self.view.match_id.is_some() {
            return Err(conflict("Match already started"));
        }
        if join.build != self.view.build || join.rules != self.view.rules {
            return Err(conflict("Build or rules do not match the room"));
        }
        if self
            .view
            .players
            .iter()
            .any(|p| p.endpoint == join.endpoint)
        {
            return Err(conflict("Gameplay endpoint already claimed"));
        }
        let slot = (0..self.view.config.player_count)
            .find(|s| !self.members.contains_key(s))
            .ok_or_else(|| conflict("Room is full"))?;
        for player in &mut self.view.players {
            player.ready = false;
        }
        self.view.players.push(Player {
            slot,
            name: join.name,
            endpoint: join.endpoint,
            lan_endpoint: join.lan_endpoint,
            ready: false,
        });
        self.view.players.sort_by_key(|p| p.slot);
        let token = secret();
        self.members.insert(
            slot,
            Member {
                token: token.clone(),
                seen: Instant::now(),
            },
        );
        Ok(Admission {
            room: self.view.clone(),
            slot,
            token,
        })
    }
    fn authenticate(&mut self, headers: &HeaderMap) -> Result<u8, ApiError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Room token required"))?;
        let (slot, member) = self
            .members
            .iter_mut()
            .find(|(_, m)| m.token == token)
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Invalid room token"))?;
        member.seen = Instant::now();
        Ok(*slot)
    }
}

pub(crate) fn router() -> Router {
    router_with(None)
}

pub(crate) fn router_with(payments: Option<Payments>) -> Router {
    Router::new()
        .route("/netplay/rooms", post(create))
        .route("/netplay/rooms/{code}/join", post(join))
        .route("/netplay/rooms/{code}", get(view).post(command))
        .route(
            "/netplay/matches/{id}",
            get(payment_view).post(payment_command),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(Store {
            payments,
            ..Store::default()
        })
}

async fn create(
    State(store): State<Store>,
    Json(join): Json<Join>,
) -> Result<Json<Admission>, ApiError> {
    validate(&join)?;
    let mut rooms = store.rooms.lock().await;
    prune(&mut rooms, store.payments.as_ref()).await?;
    if rooms.len() >= MAX_ROOMS {
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "Room server full"));
    }
    let code = loop {
        let code = format!("{:08X}", rand::random::<u32>());
        if !rooms.contains_key(&code) {
            break code;
        }
    };
    let mut room = Entry {
        view: Room {
            config: join.config.clone(),
            code: code.clone(),
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
    let mut admission = room.admit(join)?;
    if let Some(payments) = &store.payments {
        let id = format!("{:032x}", rand::random::<u128>());
        room.view.payment = Some(
            payments
                .create(&id, &admission.token, room.view.config.clone())
                .await?,
        );
        admission.room = room.view.clone();
    }
    rooms.insert(code, room);
    Ok(Json(admission))
}

async fn join(
    State(store): State<Store>,
    Path(code): Path<String>,
    Json(join): Json<Join>,
) -> Result<Json<Admission>, ApiError> {
    validate(&join)?;
    let mut rooms = store.rooms.lock().await;
    prune(&mut rooms, store.payments.as_ref()).await?;
    let room = rooms
        .get_mut(&code)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Room expired or missing"))?;
    let mut admission = room.admit(join)?;
    if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
        if let Err(error) = payments
            .add_member(&snapshot.match_id, admission.slot, &admission.token)
            .await
        {
            room.members.remove(&admission.slot);
            room.view.players.retain(|p| p.slot != admission.slot);
            return Err(error);
        }
        let payment = payments.view(&snapshot.match_id).await?;
        room.update_payment(payment);
        admission.room = room.view.clone();
    }
    Ok(Json(admission))
}

async fn view(
    State(store): State<Store>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Room>, ApiError> {
    let mut rooms = store.rooms.lock().await;
    prune(&mut rooms, store.payments.as_ref()).await?;
    let room = rooms
        .get_mut(&code)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Room expired or missing"))?;
    room.authenticate(&headers)?;
    if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
        let payment = payments
            .authenticated(&snapshot.match_id, &headers, None)
            .await?;
        room.update_payment(payment);
    }
    Ok(Json(room.view.clone()))
}

async fn command(
    State(store): State<Store>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Result<Json<Option<Room>>, ApiError> {
    let mut rooms = store.rooms.lock().await;
    prune(&mut rooms, store.payments.as_ref()).await?;
    let room = rooms
        .get_mut(&code)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Room expired or missing"))?;
    let slot = room.authenticate(&headers)?;
    if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
        let payment = payments
            .authenticated(&snapshot.match_id, &headers, None)
            .await?;
        room.update_payment(payment);
    }
    let close = matches!(&command, Command::Close);
    match command {
        Command::NextMatch { match_id } => {
            if slot != 0 {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "Only the host can prepare the next match",
                ));
            }
            if room.previous_match_id.as_deref() == Some(&match_id) {
                return Ok(Json(Some(room.view.clone())));
            }
            let current = room
                .view
                .payment
                .as_ref()
                .map(|p| p.match_id.as_str())
                .or(room.view.match_id.as_deref());
            if current != Some(match_id.as_str()) {
                return Err(conflict("Room already advanced to another match"));
            }
            if !room.view.match_complete {
                return Err(conflict(
                    "Previous match must finish settling before the next entry",
                ));
            }
            let round = room
                .view
                .round
                .checked_add(1)
                .ok_or_else(|| conflict("Room round limit reached"))?;
            if let Some(payments) = &store.payments {
                let members: Vec<_> = room
                    .members
                    .iter()
                    .map(|(&slot, member)| (slot, member.token.clone()))
                    .collect();
                let id = format!("{:032x}", rand::random::<u128>());
                let payment = payments
                    .next_match(&match_id, &id, room.view.config.clone(), &members)
                    .await?;
                room.view.payment = Some(payment);
            }
            room.previous_match_id = Some(match_id);
            room.view.round = round;
            room.view.match_id = None;
            room.view.match_complete = false;
            room.finished.clear();
            for player in &mut room.view.players {
                player.ready = false;
            }
        }
        Command::Finish { match_id } => {
            if room.view.payment.is_some() {
                return Err(conflict(
                    "Paid match completion requires the agreed payment reports",
                ));
            }
            if room.view.match_id.as_deref() != Some(&match_id) {
                return Err(conflict("Match identity changed"));
            }
            room.finished.insert(slot);
            room.view.match_complete = room.members.keys().all(|slot| room.finished.contains(slot));
        }
        Command::Ready { ready } => {
            let player = room
                .view
                .players
                .iter_mut()
                .find(|p| p.slot == slot)
                .unwrap();
            if room.view.match_id.is_some() && player.ready != ready {
                return Err(conflict("Match already started"));
            }
            player.ready = ready;
        }
        Command::Start => {
            if slot != 0 {
                return Err(error(StatusCode::FORBIDDEN, "Only the host can start"));
            }
            if room.view.match_id.is_none() {
                if room.view.players.len() != room.view.config.player_count as usize
                    || room.view.players.iter().any(|p| !p.ready)
                {
                    return Err(conflict(
                        "The configured number of players must all be ready",
                    ));
                }
                if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
                    payments.admitted(&snapshot.match_id).await?;
                    room.view.match_id = Some(snapshot.match_id.clone());
                } else {
                    // Slippi carries at most 51 bytes of match ID through the game/replay protocol.
                    room.view.match_id = Some(format!("{:032x}", rand::random::<u128>()));
                }
            }
        }
        Command::SetAddress { address } => {
            let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) else {
                return Err(conflict("This room has no payment host"));
            };
            room.view.payment = Some(
                payments
                    .authenticated(
                        &snapshot.match_id,
                        &headers,
                        Some(Command::SetAddress { address }),
                    )
                    .await?,
            );
        }
        Command::Report { report } => {
            let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) else {
                return Err(conflict("This room has no payment host"));
            };
            room.view.payment = Some(
                payments
                    .authenticated(
                        &snapshot.match_id,
                        &headers,
                        Some(Command::Report { report }),
                    )
                    .await?,
            );
        }
        Command::Leave | Command::Close => {
            if close && slot != 0 {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "Only the host can close the room",
                ));
            }
            if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
                let payment = payments
                    .authenticated(&snapshot.match_id, &headers, Some(Command::Leave))
                    .await?;
                room.update_payment(payment);
            }
            if slot == 0 {
                rooms.remove(&code);
                return Ok(Json(None));
            }
            room.remove_member(slot);
        }
    }
    if let (Some(payments), Some(snapshot)) = (&store.payments, &room.view.payment) {
        let payment = payments.view(&snapshot.match_id).await?;
        room.update_payment(payment);
    }
    Ok(Json(Some(room.view.clone())))
}

async fn payment_view(
    State(store): State<Store>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<sats_arena_protocol::netplay::PaymentSnapshot>, ApiError> {
    let payments = store
        .payments
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Payment host is not configured"))?;
    Ok(Json(payments.authenticated(&id, &headers, None).await?))
}
async fn payment_command(
    State(store): State<Store>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Result<Json<sats_arena_protocol::netplay::PaymentSnapshot>, ApiError> {
    let payments = store
        .payments
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Payment host is not configured"))?;
    Ok(Json(
        payments.authenticated(&id, &headers, Some(command)).await?,
    ))
}

#[cfg(test)]
#[path = "netplay_tests.rs"]
mod tests;
