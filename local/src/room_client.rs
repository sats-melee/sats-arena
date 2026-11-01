//! Optional outbound room connection. Losing this connection never stops the wallet worker.
use futures_util::{SinkExt, StreamExt};
use sats_arena_protocol::rooms::{DesktopCommand, HostHello, HostMessage, ServerMessage, Snapshot};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub(super) struct Bridge {
    pub commands: mpsc::Receiver<DesktopCommand>,
    pub replies: mpsc::Sender<HostMessage>,
    pub snapshots: watch::Sender<Option<Snapshot>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn websocket_url(server: &str) -> Result<String, &'static str> {
    let mut url = url::Url::parse(server).map_err(|_| "Enter a valid phone room server URL")?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => return Err("Phone rooms require an http or https server URL"),
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(
            "Phone room server URL must contain only its origin, without credentials or a path",
        );
    }
    if scheme == "ws" && !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) {
        return Err("Use HTTPS for a remote phone room server");
    }
    url.set_scheme(scheme)
        .map_err(|_| "Invalid phone room server URL")?;
    url.set_path("/rooms/host");
    Ok(url.into())
}

impl Bridge {
    pub fn start(server: &str, status: Arc<Mutex<String>>) -> Result<Self, &'static str> {
        let url = websocket_url(server)?;
        let server = server.trim_end_matches('/').to_owned();
        let (commands_tx, commands) = mpsc::channel(8);
        let (replies, mut replies_rx) = mpsc::channel::<HostMessage>(8);
        let (snapshots, mut snapshots_rx) = watch::channel::<Option<Snapshot>>(None);
        let task = tokio::spawn(async move {
            let mut code: Option<String> = None;
            let mut token = None;
            loop {
                *status.lock().unwrap() = match &code {
                    Some(code) => format!("Phone room {code} • reconnecting to {server}"),
                    None => format!("Phone room • connecting to {server}"),
                };
                if let Ok(Ok((mut socket, _))) =
                    tokio::time::timeout(Duration::from_secs(5), connect_async(&url)).await
                {
                    let hello = HostHello {
                        code: code.clone(),
                        token: token.clone(),
                    };
                    if socket
                        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
                        .await
                        .is_ok()
                    {
                        let greeting =
                            tokio::time::timeout(Duration::from_secs(5), socket.next()).await;
                        let greeting = match greeting {
                            Ok(Some(Ok(Message::Text(text)))) => {
                                serde_json::from_str::<ServerMessage>(&text).ok()
                            }
                            _ => None,
                        };
                        if let Some(ServerMessage::Hosted {
                            code: assigned,
                            token: assigned_token,
                        }) = greeting
                        {
                            *status.lock().unwrap() = format!("Phone room {assigned} • {server}");
                            code = Some(assigned);
                            token = Some(assigned_token);
                            // Responses from a previous socket cannot acknowledge a new request.
                            while replies_rx.try_recv().is_ok() {}
                            let latest = snapshots_rx.borrow().clone();
                            if let Some(snapshot) = latest {
                                let message = HostMessage::Snapshot {
                                    snapshot: Box::new(snapshot),
                                };
                                if socket
                                    .send(Message::Text(
                                        serde_json::to_string(&message).unwrap().into(),
                                    ))
                                    .await
                                    .is_err()
                                {
                                    continue;
                                }
                            }
                            let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
                            let mut last_received = tokio::time::Instant::now();
                            loop {
                                let outgoing = tokio::select! {
                                    message = socket.next() => {
                                        let Some(Ok(message)) = message else { break; };
                                        last_received = tokio::time::Instant::now();
                                        match message {
                                            Message::Text(text) => {
                                                let Ok(command) = serde_json::from_str::<DesktopCommand>(&text) else { break; };
                                                let request_id = command.request_id();
                                                if commands_tx.try_send(command).is_err() {
                                                    Some(HostMessage::Reply { request_id, error: Some("Desktop is busy; try again".into()) })
                                                } else { None }
                                            }
                                            Message::Close(_) => break,
                                            _ => None,
                                        }
                                    }
                                    changed = snapshots_rx.changed() => {
                                        if changed.is_err() { return; }
                                        let latest = snapshots_rx.borrow_and_update().clone();
                                        latest.map(|snapshot| HostMessage::Snapshot { snapshot: Box::new(snapshot) })
                                    }
                                    reply = replies_rx.recv() => reply,
                                    _ = heartbeat.tick() => {
                                        if last_received.elapsed() > Duration::from_secs(45) || socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                                        None
                                    }
                                };
                                if let Some(message) = outgoing {
                                    if socket
                                        .send(Message::Text(
                                            serde_json::to_string(&message).unwrap().into(),
                                        ))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        } else if matches!(greeting, Some(ServerMessage::Error { .. })) {
                            code = None;
                            token = None;
                        }
                    }
                }
                *status.lock().unwrap() = match &code {
                    Some(code) => format!("Phone room {code} • disconnected; retrying"),
                    None => "Phone room unavailable; retrying".into(),
                };
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(Self {
            commands,
            replies,
            snapshots,
            task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Demo, Roster};
    #[test]
    fn only_credential_free_secure_origins_or_local_development_are_accepted() {
        assert_eq!(
            websocket_url("https://arena.example.com/").unwrap(),
            "wss://arena.example.com/rooms/host"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:8738").unwrap(),
            "ws://127.0.0.1:8738/rooms/host"
        );
        for url in [
            "http://example.com",
            "https://secret@example.com",
            "https://example.com/path",
            "https://example.com?token=secret",
            "file:///tmp/server",
        ] {
            assert!(websocket_url(url).is_err());
        }
    }

    #[tokio::test]
    async fn phone_requests_reuse_entries_and_cannot_change_a_funded_or_stale_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(Roster {
            teams: false,
            kinds: [0, 0, 3, 3],
            team_ids: [255; 4],
            characters: [1, 2, 255, 255],
        }))
        .await
        .unwrap();
        let match_id = demo.saved.pot.id.clone();
        for player in 0..2 {
            demo.room_command(DesktopCommand::SetAddress {
                request_id: player as u64,
                player,
                match_id: match_id.clone(),
                address: format!("player{player}@example.test"),
            })
            .await
            .unwrap();
        }
        let invoice = demo.saved.invoices[0].clone();
        assert!(!invoice.is_empty());
        for _ in 0..2 {
            demo.room_command(DesktopCommand::RequestInvoice {
                request_id: 5,
                player: 0,
                match_id: match_id.clone(),
            })
            .await
            .unwrap();
            assert_eq!(demo.saved.invoices[0], invoice);
        }
        assert!(!demo.room_snapshot().players[0].invoice.is_empty());
        assert!(
            demo.room_command(DesktopCommand::RequestInvoice {
                request_id: 6,
                player: 0,
                match_id: "old-match".into()
            })
            .await
            .is_err()
        );
        assert!(
            demo.room_command(DesktopCommand::RequestInvoice {
                request_id: 7,
                player: 2,
                match_id: match_id.clone()
            })
            .await
            .is_err()
        );
        demo.command(Command::Action(2)).await.unwrap();
        demo.reconcile().await.unwrap();
        assert!(demo.room_snapshot().locked);
        assert!(
            demo.room_command(DesktopCommand::SetAddress {
                request_id: 8,
                player: 1,
                match_id,
                address: "different@example.test".into()
            })
            .await
            .is_err()
        );
        assert_eq!(demo.saved.addresses[1], "player1@example.test");
        assert!(demo.room_snapshot().players[0].invoice.is_empty());
    }
}
