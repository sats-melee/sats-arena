//! One payment engine, two hosts: embedded in Dolphin or wrapped by an HTTP server.
//!
//! Deposits use ordinary invoices, never hold invoices. A wallet-confirmed payment
//! credits a durable player balance exactly once. Matches only move ledger funds;
//! withdrawals are separate Lightning payments. See `examples/local.rs`.
//!
//! Run this engine on a background Tokio runtime, NOT on Dolphin's emulation
//! thread. Its bounded command queue serializes wallet and ledger operations.

mod engine;
mod ledger;
mod lobby;
mod rules;
mod settle;
pub mod wallet;

pub use engine::Engine;
pub use rules::{MAX_BALANCE_SATS, Rules};
pub use sats_arena_protocol as protocol;

#[derive(Debug)]
pub enum Error {
    Rejected(String),
    Unauthorized,
    NotFound,
    Closed,
    WalletUnavailable,
    Internal(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => f.write_str(message),
            Self::Unauthorized => f.write_str("missing or invalid player token"),
            Self::NotFound => f.write_str("not found"),
            Self::Closed => f.write_str("the payment engine is closed"),
            Self::WalletUnavailable => {
                f.write_str("the wallet is unavailable; pending funds remain reserved")
            }
            Self::Internal(_) => f.write_str("internal payment engine error"),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
