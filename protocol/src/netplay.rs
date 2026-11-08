//! Private desktop rooms and trusted payment observations; no wallet credentials or controller inputs.
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Immutable payment roster selected by the host before the room accepts entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomConfig {
    pub player_count: u8,
    pub teams: bool,
    pub team_ids: [u8; 4],
}
impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            player_count: 2,
            teams: false,
            team_ids: [255; 4],
        }
    }
}
impl RoomConfig {
    pub fn kinds(&self) -> [u8; 4] {
        std::array::from_fn(|i| if i < self.player_count as usize { 0 } else { 3 })
    }
    pub fn is_valid(&self) -> bool {
        if !(2..=4).contains(&self.player_count) {
            return false;
        }
        let mut sides = 0u8;
        for (i, &team) in self.team_ids.iter().enumerate() {
            if self.teams && i < self.player_count as usize {
                if team > 2 {
                    return false;
                }
                sides |= 1 << team;
            } else if team != 255 {
                return false;
            }
        }
        !self.teams || sides.count_ones() >= 2
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    #[serde(default)]
    pub config: RoomConfig,
    pub name: String,
    /// Exact identifier for the emulator, game patches, and game revision.
    pub build: String,
    /// Exact identifier for agreed gameplay settings; not a payment authorization.
    pub rules: String,
    /// Public gameplay endpoint discovered from the bound UDP socket, or a LAN override.
    pub endpoint: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_endpoint: Option<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Player {
    pub slot: u8,
    pub name: String,
    pub endpoint: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_endpoint: Option<SocketAddr>,
    pub ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    #[serde(default)]
    pub config: RoomConfig,
    pub code: String,
    pub build: String,
    pub rules: String,
    pub players: Vec<Player>,
    /// Funding/game epoch within the same room and membership.
    #[serde(default = "first_round")]
    pub round: u32,
    #[serde(default)]
    pub match_complete: bool,
    /// Slot zero owns Start. A started roster is immutable.
    pub match_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Admission {
    pub room: Room,
    pub slot: u8,
    /// Private bearer token for this room membership; never broadcast.
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Ready { ready: bool },
    Start,
    Leave,
    Close,
    NextMatch { match_id: String },
    Finish { match_id: String },
    SetAddress { address: String },
    Report { report: MatchObservation },
}

fn first_round() -> u32 {
    1
}

/// The central wallet is optional; no wallet credentials cross this protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentPlayer {
    pub address: String,
    pub invoice: String,
    pub verified: bool,
    pub verification_state: u8,
    pub verification_message: String,
    pub paid_sats: u64,
    pub remaining_sats: u64,
    pub earned_sats: u64,
    pub pending_sats: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentSnapshot {
    pub mode: String,
    pub payment_log: String,
    pub match_id: String,
    /// Same display phases as the native payment panel: funding=1, ready=2,
    /// playing=3, settled=4, settling=6.
    pub phase: u32,
    pub frozen: bool,
    pub entry_sats: u64,
    pub invoice_sats: u64,
    pub sats_per_percent: u64,
    pub batch_window_ms: u64,
    pub surcharge_bps: u32,
    pub players: [PaymentPlayer; 4],
    pub teams: bool,
    pub kinds: [u8; 4],
    pub team_ids: [u8; 4],
    pub characters: [u8; 4],
    pub batch_remaining_ms: [u64; 4],
    pub damage_centi: [[u64; 4]; 4],
    pub agreed_sequence: u64,
    pub own_next_sequence: [u64; 4],
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartManifest {
    pub game_number: u32,
    pub stage: u16,
    pub seed: u32,
    pub teams: bool,
    pub kinds: [u8; 4],
    pub team_ids: [u8; 4],
    pub characters: [u8; 4],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DamageCredit {
    pub sequence: u64,
    pub frame: i32,
    pub attacker: u8,
    pub victim: u8,
    pub damage_centi: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndReport {
    pub last_sequence: u64,
    pub winner: Option<u8>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchObservation {
    pub match_id: String,
    pub manifest: StartManifest,
    pub credits: Vec<DamageCredit>,
    pub end: Option<EndReport>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn room_configuration_matches_local_team_constraints_and_legacy_defaults() {
        let join: Join = serde_json::from_value(serde_json::json!({"name":"Player","build":"test","rules":"test","endpoint":"127.0.0.1:1234"})).unwrap();
        assert_eq!(join.config, RoomConfig::default());
        for count in 2..=4 {
            let config = RoomConfig {
                player_count: count,
                teams: true,
                team_ids: std::array::from_fn(|i| {
                    if i < count as usize {
                        (i % 2) as u8
                    } else {
                        255
                    }
                }),
            };
            assert!(config.is_valid());
            assert_eq!(
                config.kinds().iter().filter(|&&k| k == 0).count(),
                count as usize
            );
        }
        assert!(
            !RoomConfig {
                player_count: 4,
                teams: true,
                team_ids: [0; 4]
            }
            .is_valid()
        );
        assert!(
            !RoomConfig {
                player_count: 3,
                teams: true,
                team_ids: [0, 1, 0, 1]
            }
            .is_valid()
        );
        assert!(
            !RoomConfig {
                player_count: 4,
                teams: false,
                team_ids: [0, 1, 0, 1]
            }
            .is_valid()
        );
        assert!(
            !RoomConfig {
                player_count: 5,
                ..RoomConfig::default()
            }
            .is_valid()
        );
    }
}
