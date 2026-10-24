//! Durable pre-game consent and budget reservations. No Lightning hold invoices.
use crate::{
    Error, MAX_BALANCE_SATS, Result, Rules,
    ledger::{Ledger, apply},
    settle,
};
use rusqlite::{Connection, OptionalExtension, params};
use sats_arena_protocol::{
    Lobby, LobbyPhase, LobbyPlayer, LobbyRequest, MatchMode, MatchReport, PROTOCOL_VERSION,
    PlayerSlot,
};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lobbies (
 id TEXT PRIMARY KEY, creator TEXT NOT NULL REFERENCES players(id),
 mode TEXT NOT NULL, roster TEXT NOT NULL, budget INTEGER NOT NULL CHECK(budget > 0),
 phase TEXT NOT NULL DEFAULT 'funding'
);
CREATE TABLE IF NOT EXISTS lobby_funds (
 lobby_id TEXT NOT NULL REFERENCES lobbies(id), player_id TEXT NOT NULL REFERENCES players(id),
 amount INTEGER NOT NULL DEFAULT 0 CHECK(amount >= 0), PRIMARY KEY(lobby_id, player_id)
);";

impl Ledger {
    pub fn create_lobby(
        &mut self,
        player: &str,
        request: &LobbyRequest,
        rules: &Rules,
    ) -> Result<Lobby> {
        if request.budget_sats == 0 || request.budget_sats > MAX_BALANCE_SATS {
            return Err(Error::Rejected("invalid match budget".into()));
        }
        let reporter = request
            .players
            .iter()
            .find(|slot| slot.player_id == player)
            .ok_or(Error::Unauthorized)?;
        settle::validate(&MatchReport {
            protocol_version: PROTOCOL_VERSION,
            match_id: request.id.clone(),
            mode: request.mode,
            reporter_port: reporter.port,
            players: request.players.clone(),
            hits: vec![],
            winner_port: None,
        })
        .map_err(Error::Rejected)?;
        let mut roster = request.players.clone();
        roster.sort_by_key(|slot| slot.port);
        let tx = self.conn.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT creator FROM lobbies WHERE id = ?1",
                [&request.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(creator) = existing {
            let lobby = read(&tx, &request.id)?;
            let saved: Vec<_> = lobby
                .players
                .iter()
                .map(|p| PlayerSlot {
                    port: p.port,
                    player_id: p.player_id.clone(),
                })
                .collect();
            if creator != player
                || lobby.mode != request.mode
                || saved != roster
                || lobby.budget_sats != request.budget_sats
            {
                return Err(Error::Rejected(
                    "lobby ID already exists with different terms".into(),
                ));
            }
            return Ok(lobby);
        }
        let used: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM match_reports WHERE match_id = ?1)",
            [&request.id],
            |r| r.get(0),
        )?;
        if used {
            return Err(Error::Rejected("game ID already has reports".into()));
        }
        for slot in &roster {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM players WHERE id = ?1)",
                [&slot.player_id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(Error::Rejected("unknown lobby player".into()));
            }
        }
        tx.execute(
            "INSERT INTO lobbies(id, creator, mode, roster, budget) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                request.id,
                player,
                serde_json::to_string(&request.mode)?,
                serde_json::to_string(&roster)?,
                request.budget_sats as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO match_terms(match_id, rules) VALUES (?1, ?2)",
            params![request.id, serde_json::to_string(rules)?],
        )?;
        for slot in &roster {
            tx.execute(
                "INSERT INTO lobby_funds(lobby_id, player_id) VALUES (?1, ?2)",
                params![request.id, slot.player_id],
            )?;
        }
        let lobby = read(&tx, &request.id)?;
        tx.commit()?;
        Ok(lobby)
    }

    pub fn lobby(&self, player: &str, id: &str) -> Result<Lobby> {
        let lobby = read(&self.conn, id)?;
        member(&lobby, player)?;
        Ok(lobby)
    }

    pub fn ready_lobby(&mut self, player: &str, id: &str) -> Result<Lobby> {
        let tx = self.conn.transaction()?;
        let lobby = read(&tx, id)?;
        let slot = member(&lobby, player)?;
        if !matches!(lobby.phase, LobbyPhase::Funding | LobbyPhase::Ready) {
            return Err(Error::Rejected(
                "lobby is not accepting ready requests".into(),
            ));
        }
        if slot.reserved_sats == 0 {
            if slot.available_sats < lobby.budget_sats {
                return Err(Error::Rejected(
                    "player needs a confirmed deposit before readying".into(),
                ));
            }
            apply(
                &tx,
                player,
                -(lobby.budget_sats as i64),
                "match_reserve",
                id,
            )?;
            tx.execute(
                "UPDATE lobby_funds SET amount = ?3 WHERE lobby_id = ?1 AND player_id = ?2",
                params![id, player, lobby.budget_sats as i64],
            )?;
        }
        let waiting: i64 = tx.query_row(
            "SELECT COUNT(*) FROM lobby_funds WHERE lobby_id = ?1 AND amount = 0",
            [id],
            |r| r.get(0),
        )?;
        if waiting == 0 {
            tx.execute("UPDATE lobbies SET phase = 'ready' WHERE id = ?1", [id])?;
        }
        let lobby = read(&tx, id)?;
        tx.commit()?;
        Ok(lobby)
    }

    pub fn start_lobby(&mut self, player: &str, id: &str) -> Result<Lobby> {
        let tx = self.conn.transaction()?;
        let lobby = read(&tx, id)?;
        member(&lobby, player)?;
        let creator: String =
            tx.query_row("SELECT creator FROM lobbies WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        if creator != player {
            return Err(Error::Unauthorized);
        }
        if lobby.phase != LobbyPhase::Ready {
            return Err(Error::Rejected(
                "both players must be ready; start authorization is single-use".into(),
            ));
        }
        tx.execute(
            "UPDATE lobbies SET phase = 'playing' WHERE id = ?1 AND phase = 'ready'",
            [id],
        )?;
        let lobby = read(&tx, id)?;
        tx.commit()?;
        Ok(lobby)
    }

    pub fn cancel_lobby(&mut self, player: &str, id: &str) -> Result<Lobby> {
        let tx = self.conn.transaction()?;
        let lobby = read(&tx, id)?;
        member(&lobby, player)?;
        if lobby.phase == LobbyPhase::Cancelled {
            return Ok(lobby);
        }
        if !matches!(lobby.phase, LobbyPhase::Funding | LobbyPhase::Ready) {
            return Err(Error::Rejected(
                "started or disputed games cannot be refunded by cancelling".into(),
            ));
        }
        for slot in &lobby.players {
            if slot.reserved_sats > 0 {
                apply(
                    &tx,
                    &slot.player_id,
                    slot.reserved_sats as i64,
                    "match_cancel",
                    id,
                )?;
            }
        }
        tx.execute(
            "UPDATE lobby_funds SET amount = 0 WHERE lobby_id = ?1",
            [id],
        )?;
        tx.execute("UPDATE lobbies SET phase = 'cancelled' WHERE id = ?1", [id])?;
        let lobby = read(&tx, id)?;
        tx.commit()?;
        Ok(lobby)
    }
}

fn read(conn: &Connection, id: &str) -> Result<Lobby> {
    let row: Option<(String, String, i64, String)> = conn
        .query_row(
            "SELECT mode, roster, budget, phase FROM lobbies WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (mode, roster, budget, phase) = row.ok_or(Error::NotFound)?;
    let phase = match phase.as_str() {
        "funding" => LobbyPhase::Funding,
        "ready" => LobbyPhase::Ready,
        "playing" => LobbyPhase::Playing,
        "settled" => LobbyPhase::Settled,
        "cancelled" => LobbyPhase::Cancelled,
        "disputed" => LobbyPhase::Disputed,
        _ => return Err(Error::Internal(anyhow::anyhow!("invalid lobby phase"))),
    };
    let terms: String = conn.query_row(
        "SELECT rules FROM match_terms WHERE match_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    let rules: Rules = serde_json::from_str(&terms)?;
    let mut players = Vec::new();
    for slot in serde_json::from_str::<Vec<PlayerSlot>>(&roster)? {
        let (available, reserved): (i64, i64) = conn.query_row("SELECT p.balance_sats, f.amount FROM players p JOIN lobby_funds f ON p.id = f.player_id WHERE f.lobby_id = ?1 AND p.id = ?2",
            params![id, slot.player_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        players.push(LobbyPlayer {
            port: slot.port,
            player_id: slot.player_id,
            available_sats: available as u64,
            reserved_sats: reserved as u64,
        });
    }
    Ok(Lobby {
        id: id.into(),
        mode: serde_json::from_str::<MatchMode>(&mode)?,
        phase,
        budget_sats: budget as u64,
        sats_per_percent: rules.sats_per_percent,
        win_sats: rules.win_sats,
        players,
    })
}

fn member<'a>(lobby: &'a Lobby, player: &str) -> Result<&'a LobbyPlayer> {
    lobby
        .players
        .iter()
        .find(|slot| slot.player_id == player)
        .ok_or(Error::Unauthorized)
}

pub(crate) fn check_report(conn: &Connection, report: &MatchReport) -> Result<()> {
    let lobby = read(conn, &report.match_id)?;
    if !matches!(
        lobby.phase,
        LobbyPhase::Playing | LobbyPhase::Settled | LobbyPhase::Disputed
    ) {
        return Err(Error::Rejected(
            "game has not been funded and started".into(),
        ));
    }
    let mut players = report.players.clone();
    players.sort_by_key(|slot| slot.port);
    let expected: Vec<_> = lobby
        .players
        .iter()
        .map(|slot| PlayerSlot {
            port: slot.port,
            player_id: slot.player_id.clone(),
        })
        .collect();
    if report.mode != lobby.mode || players != expected {
        return Err(Error::Rejected(
            "report doesn't match the admitted lobby".into(),
        ));
    }
    Ok(())
}
