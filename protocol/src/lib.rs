//! Wire types shared by the Sats Arena server and the Dolphin client.
//!
//! Everything here is plain data. The server owns all money logic; clients
//! only describe what happened in a match.

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to these types.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Every player is on one Dolphin, which reports for all of them.
    Local,
    /// Each player is on their own Dolphin and reports independently.
    Netplay,
}

/// Which arena account is playing on a controller port (0-3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSlot {
    pub port: u8,
    pub player_id: String,
}

/// One finalized damage event. Damage is in hundredths of a percent so both
/// netplay clients produce bit-identical reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub frame: i32,
    pub victim_port: u8,
    /// `None` when the damage can't be attributed to another player.
    pub attacker_port: Option<u8>,
    pub damage_centi: u32,
}

/// Sent by a client when a game ends. For netplay, every player sends one and
/// the server settles only when they all agree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchReport {
    pub protocol_version: u32,
    /// Unique per game. A netplay host must coordinate this between clients
    /// (e.g. session ID + game number); a matchmaking session ID alone is not enough.
    pub match_id: String,
    pub mode: MatchMode,
    /// The port the reporting account is playing on.
    pub reporter_port: u8,
    pub players: Vec<PlayerSlot>,
    pub hits: Vec<Hit>,
    /// `None` for a no-contest (quit, disconnect, timeout tie).
    pub winner_port: Option<u8>,
}

impl MatchReport {
    /// True when two reports describe the same game, ignoring who sent them.
    pub fn same_outcome(&self, other: &Self) -> bool {
        self.protocol_version == other.protocol_version
            && self.match_id == other.match_id
            && self.mode == other.mode
            && self.players == other.players
            && self.hits == other.hits
            && self.winner_port == other.winner_port
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub invite_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub player_id: String,
    /// Bearer token for every later request. Shown once; the server stores a hash.
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub player_id: String,
    pub name: String,
    pub balance_sats: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount_sats: u64,
}

/// Unique per game; the ID also makes repeated create requests retry-safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyRequest {
    pub id: String,
    pub mode: MatchMode,
    pub players: Vec<PlayerSlot>,
    pub budget_sats: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LobbyPhase {
    Funding,
    Ready,
    Playing,
    Settled,
    Cancelled,
    Disputed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyPlayer {
    pub port: u8,
    pub player_id: String,
    pub available_sats: u64,
    pub reserved_sats: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lobby {
    pub id: String,
    pub mode: MatchMode,
    pub phase: LobbyPhase,
    pub budget_sats: u64,
    pub sats_per_percent: u64,
    pub win_sats: u64,
    pub players: Vec<LobbyPlayer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepositStatus {
    Pending,
    Credited,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deposit {
    pub id: String,
    pub amount_sats: u64,
    pub bolt11: String,
    pub status: DepositStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalRequest {
    pub bolt11: String,
    pub amount_sats: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawalStatus {
    /// Funds are reserved and the payment result isn't known yet.
    Pending,
    Completed,
    /// The payment definitely failed and the funds were returned.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withdrawal {
    pub id: String,
    pub amount_sats: u64,
    pub status: WithdrawalStatus,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferReason {
    Damage,
    Win,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub from_player_id: String,
    pub to_player_id: String,
    pub sats: u64,
    pub reason: TransferReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MatchStatus {
    /// A netplay report was accepted; other players haven't reported yet.
    WaitingForOpponent,
    Settled {
        transfers: Vec<Transfer>,
    },
    /// Reports disagreed. Nothing moved; the operator has to resolve it.
    Disputed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(reporter_port: u8) -> MatchReport {
        MatchReport {
            protocol_version: PROTOCOL_VERSION,
            match_id: "m1".into(),
            mode: MatchMode::Netplay,
            reporter_port,
            players: vec![
                PlayerSlot {
                    port: 0,
                    player_id: "a".into(),
                },
                PlayerSlot {
                    port: 1,
                    player_id: "b".into(),
                },
            ],
            hits: vec![Hit {
                frame: 10,
                victim_port: 1,
                attacker_port: Some(0),
                damage_centi: 1200,
            }],
            winner_port: Some(0),
        }
    }

    #[test]
    fn reports_from_both_sides_agree() {
        assert!(report(0).same_outcome(&report(1)));
        let mut other = report(1);
        other.winner_port = Some(1);
        assert!(!report(0).same_outcome(&other));
    }

    #[test]
    fn match_status_is_tagged() {
        let json = serde_json::to_value(MatchStatus::Settled { transfers: vec![] }).unwrap();
        assert_eq!(json["status"], "settled");
        let json = serde_json::to_value(MatchStatus::WaitingForOpponent).unwrap();
        assert_eq!(json["status"], "waiting_for_opponent");
    }
}
