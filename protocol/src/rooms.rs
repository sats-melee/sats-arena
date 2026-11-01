//! Phone rooms carry observations and address requests, never wallet credentials or game inputs.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Player {
    pub kind: u8,
    pub address: String,
    pub verification: u8,
    pub message: String,
    pub invoice: String,
    pub paid_sats: u64,
    pub earned_sats: u64,
    pub pending_sats: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub at_ms: u64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub match_id: String,
    pub phase: u8,
    pub mode: String,
    pub roster_ready: bool,
    pub locked: bool,
    pub entry_sats: u64,
    pub invoice_sats: u64,
    pub players: [Player; 4],
    pub damage_centi: [[u64; 4]; 4],
    pub logs: Vec<LogEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostHello {
    pub code: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Snapshot {
        snapshot: Box<Snapshot>,
    },
    Reply {
        request_id: u64,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopCommand {
    SetAddress {
        request_id: u64,
        player: usize,
        match_id: String,
        address: String,
    },
    RequestInvoice {
        request_id: u64,
        player: usize,
        match_id: String,
    },
}

impl DesktopCommand {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::SetAddress { request_id, .. } | Self::RequestInvoice { request_id, .. } => {
                *request_id
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhoneMessage {
    Join {
        code: String,
        player: usize,
        token: Option<String>,
    },
    SetAddress {
        match_id: String,
        address: String,
    },
    RequestInvoice {
        match_id: String,
    },
    Leave,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hosted {
        code: String,
        token: String,
    },
    Joined {
        code: String,
        player: usize,
        token: String,
    },
    State {
        online: bool,
        claimed: [bool; 4],
        snapshot: Option<Box<Snapshot>>,
    },
    Result {
        request_id: u64,
        error: Option<String>,
    },
    Error {
        message: String,
    },
    Left,
}
