//! SQLite-backed ledger. Every balance change goes through `apply` inside a
//! transaction and leaves a row in `entries`, so balances can be audited.

use std::{collections::HashMap, path::Path};

use anyhow::Context;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sats_arena_protocol::{
    Account, Deposit, DepositStatus, MatchReport, MatchStatus, Transfer, Withdrawal,
    WithdrawalStatus,
};
use sha2::{Digest, Sha256};

use crate::{Error, MAX_BALANCE_SATS, Result, Rules, settle};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS players (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    balance_sats INTEGER NOT NULL DEFAULT 0 CHECK (balance_sats >= 0),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY,
    player_id TEXT NOT NULL REFERENCES players(id),
    delta_sats INTEGER NOT NULL,
    kind TEXT NOT NULL,
    reference TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS deposits (
    id TEXT PRIMARY KEY,
    player_id TEXT NOT NULL REFERENCES players(id),
    amount_sats INTEGER NOT NULL,
    bolt11 TEXT NOT NULL,
    payment_hash TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS withdrawals (
    id TEXT PRIMARY KEY,
    player_id TEXT NOT NULL REFERENCES players(id),
    amount_sats INTEGER NOT NULL,
    reserved_sats INTEGER NOT NULL,
    bolt11 TEXT NOT NULL,
    payment_hash TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS match_reports (
    match_id TEXT NOT NULL,
    player_id TEXT NOT NULL REFERENCES players(id),
    report TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (match_id, player_id)
);
CREATE TABLE IF NOT EXISTS matches (
    match_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    transfers TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS match_terms (
    match_id TEXT PRIMARY KEY,
    rules TEXT NOT NULL
);
";

fn rejected<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}

pub struct Ledger {
    pub(crate) conn: Connection,
}

pub struct PendingWithdrawal {
    pub id: String,
    pub payment_hash: String,
}

impl Ledger {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::lobby::SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn register(&mut self, name: &str) -> Result<(String, String)> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 32 {
            return rejected("name must be 1-32 characters");
        }
        let id = random_id();
        let token = format!("{}{}", random_id(), random_id());
        self.conn.execute(
            "INSERT INTO players (id, name, token_hash) VALUES (?1, ?2, ?3)",
            params![id, name, hash_token(&token)],
        )?;
        Ok((id, token))
    }

    pub fn authenticate(&self, token: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM players WHERE token_hash = ?1",
                [hash_token(token)],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn account(&self, player_id: &str) -> Result<Account> {
        Ok(self.conn.query_row(
            "SELECT id, name, balance_sats FROM players WHERE id = ?1",
            [player_id],
            |r| {
                Ok(Account {
                    player_id: r.get(0)?,
                    name: r.get(1)?,
                    balance_sats: r.get::<_, i64>(2)? as u64,
                })
            },
        )?)
    }

    pub fn insert_deposit(
        &mut self,
        player_id: &str,
        amount_sats: u64,
        bolt11: &str,
        payment_hash: &str,
    ) -> Result<Deposit> {
        if amount_sats == 0 || amount_sats > MAX_BALANCE_SATS {
            return rejected("invalid deposit amount");
        }
        let id = random_id();
        self.conn.execute(
            "INSERT INTO deposits (id, player_id, amount_sats, bolt11, payment_hash, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
            params![id, player_id, amount_sats as i64, bolt11, payment_hash],
        )?;
        Ok(Deposit {
            id,
            amount_sats,
            bolt11: bolt11.into(),
            status: DepositStatus::Pending,
        })
    }

    /// Returns the deposit and its payment hash, if it belongs to the player.
    pub fn deposit(&self, player_id: &str, id: &str) -> Result<Option<(Deposit, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, amount_sats, bolt11, status, payment_hash FROM deposits WHERE id = ?1 AND player_id = ?2",
                [id, player_id],
                |r| {
                    Ok((
                        Deposit {
                            id: r.get(0)?,
                            amount_sats: r.get::<_, i64>(1)? as u64,
                            bolt11: r.get(2)?,
                            status: deposit_status(&r.get::<_, String>(3)?),
                        },
                        r.get(4)?,
                    ))
                },
            )
            .optional()?)
    }

    pub fn pending_deposits(&self) -> Result<Vec<(String, String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, payment_hash, amount_sats FROM deposits WHERE status = 'pending'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Credits a paid deposit exactly once, however many times it is called.
    pub fn credit_deposit(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "UPDATE deposits SET status = 'credited' WHERE id = ?1 AND status = 'pending'
                 RETURNING player_id, amount_sats",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((player_id, amount)) = row {
            apply(&tx, &player_id, amount, "deposit", id)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn expire_deposit(&mut self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE deposits SET status = 'expired' WHERE id = ?1 AND status = 'pending'",
            [id],
        )?;
        Ok(())
    }

    /// Takes the amount plus a fee reserve out of the balance before paying.
    pub fn reserve_withdrawal(
        &mut self,
        player_id: &str,
        amount_sats: u64,
        fee_reserve_sats: u64,
        bolt11: &str,
        payment_hash: &str,
    ) -> Result<String> {
        let tx = self.conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM withdrawals WHERE payment_hash = ?1)",
            [payment_hash],
            |r| r.get(0),
        )?;
        if exists {
            return rejected("this invoice has already been submitted");
        }
        let reserved = amount_sats
            .checked_add(fee_reserve_sats)
            .filter(|value| amount_sats > 0 && *value <= MAX_BALANCE_SATS)
            .ok_or_else(|| Error::Rejected("invalid withdrawal amount or fee".into()))?;
        let balance: i64 = tx.query_row(
            "SELECT balance_sats FROM players WHERE id = ?1",
            [player_id],
            |r| r.get(0),
        )?;
        if (balance as u64) < reserved {
            return rejected(format!(
                "withdrawing {amount_sats} sats needs {reserved} including the fee reserve"
            ));
        }
        let id = random_id();
        tx.execute(
            "INSERT INTO withdrawals (id, player_id, amount_sats, reserved_sats, bolt11, payment_hash, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
            params![id, player_id, amount_sats as i64, reserved as i64, bolt11, payment_hash],
        )?;
        apply(&tx, player_id, -(reserved as i64), "withdrawal", &id)?;
        tx.commit()?;
        Ok(id)
    }

    /// Records a definite payment result and refunds whatever wasn't spent.
    pub fn finish_withdrawal(
        &mut self,
        id: &str,
        fee_sats: Option<u64>,
        error: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let status = if fee_sats.is_some() {
            "completed"
        } else {
            "failed"
        };
        let row: Option<(String, i64, i64)> = tx
            .query_row(
                "UPDATE withdrawals SET status = ?2, error = ?3 WHERE id = ?1 AND status = 'pending'
                 RETURNING player_id, amount_sats, reserved_sats",
                params![id, status, error],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((player_id, amount, reserved)) = row {
            if fee_sats.is_some_and(|fee| fee > (reserved - amount) as u64) {
                // Returning before commit rolls back the status change too.
                return rejected(
                    "wallet exceeded the reserved routing fee; manual review is required",
                );
            }
            let refund = match fee_sats {
                Some(fee) => (reserved - amount - fee as i64).max(0),
                None => reserved,
            };
            if refund > 0 {
                apply(&tx, &player_id, refund, "withdrawal_refund", id)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn withdrawal(&self, player_id: &str, id: &str) -> Result<Option<Withdrawal>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, amount_sats, status, error FROM withdrawals WHERE id = ?1 AND player_id = ?2",
                [id, player_id],
                |r| {
                    Ok(Withdrawal {
                        id: r.get(0)?,
                        amount_sats: r.get::<_, i64>(1)? as u64,
                        status: withdrawal_status(&r.get::<_, String>(2)?),
                        error: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn pending_withdrawals(&self) -> Result<Vec<PendingWithdrawal>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, payment_hash FROM withdrawals WHERE status = 'pending'")?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingWithdrawal {
                id: r.get(0)?,
                payment_hash: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Invoice hash is the idempotency key for a withdrawal. Retrying the same
    /// request returns its existing state; it must never send a second payment.
    pub fn withdrawal_for_invoice(
        &self,
        player_id: &str,
        payment_hash: &str,
    ) -> Result<Option<Withdrawal>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id, player_id FROM withdrawals WHERE payment_hash = ?1",
                [payment_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((id, owner)) if owner == player_id => self.withdrawal(player_id, &id),
            Some(_) => rejected("this invoice has already been submitted"),
            None => Ok(None),
        }
    }

    /// Stores one player's report and settles once every player has sent a
    /// matching one. Local and netplay matches follow the same rule.
    pub fn submit_report(
        &mut self,
        player_id: &str,
        report: &MatchReport,
        rules: &Rules,
    ) -> Result<MatchStatus> {
        settle::validate(report).map_err(Error::Rejected)?;
        let reporter = report
            .players
            .iter()
            .find(|p| p.port == report.reporter_port);
        if reporter.is_none_or(|p| p.player_id != player_id) {
            return rejected("reporter_port must be the port you are playing on");
        }

        let tx = self.conn.transaction()?;
        crate::lobby::check_report(&tx, report)?;
        if let Some(status) = match_status(&tx, &report.match_id)? {
            return Ok(status);
        }
        for slot in &report.players {
            let known: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM players WHERE id = ?1)",
                [&slot.player_id],
                |r| r.get(0),
            )?;
            if !known {
                return rejected(format!("unknown player {}", slot.player_id));
            }
        }

        // Use the immutable terms saved at lobby creation, even after restart
        // with a different host configuration.
        tx.execute(
            "INSERT OR IGNORE INTO match_terms (match_id, rules) VALUES (?1, ?2)",
            params![report.match_id, serde_json::to_string(rules)?],
        )?;
        let terms: String = tx.query_row(
            "SELECT rules FROM match_terms WHERE match_id = ?1",
            [&report.match_id],
            |r| r.get(0),
        )?;
        let match_rules: Rules = serde_json::from_str(&terms)?;

        let previous: Option<String> = tx
            .query_row(
                "SELECT report FROM match_reports WHERE match_id = ?1 AND player_id = ?2",
                [&report.match_id, player_id],
                |r| r.get(0),
            )
            .optional()?;
        match previous {
            Some(json) if serde_json::from_str::<MatchReport>(&json)? == *report => {}
            Some(_) => return rejected("you already sent a different report for this match"),
            None => {
                tx.execute(
                    "INSERT INTO match_reports (match_id, player_id, report) VALUES (?1, ?2, ?3)",
                    params![report.match_id, player_id, serde_json::to_string(report)?],
                )?;
            }
        }

        let reports: Vec<MatchReport> = {
            let mut stmt = tx.prepare("SELECT report FROM match_reports WHERE match_id = ?1")?;
            let rows = stmt.query_map([&report.match_id], |r| r.get::<_, String>(0))?;
            rows.map(|json| Ok(serde_json::from_str(&json?)?))
                .collect::<anyhow::Result<_>>()?
        };

        let status = if reports.iter().any(|other| !other.same_outcome(report)) {
            tx.execute(
                "INSERT INTO matches (match_id, status) VALUES (?1, 'disputed')",
                [&report.match_id],
            )?;
            tx.execute(
                "UPDATE lobbies SET phase = 'disputed' WHERE id = ?1",
                [&report.match_id],
            )?;
            MatchStatus::Disputed
        } else if reports.len() < report.players.len() {
            MatchStatus::WaitingForOpponent
        } else {
            let transfers = settle_match(&tx, report, &match_rules)?;
            MatchStatus::Settled { transfers }
        };
        tx.commit()?;
        Ok(status)
    }
}

fn settle_match(tx: &Transaction, report: &MatchReport, rules: &Rules) -> Result<Vec<Transfer>> {
    let mut balances = HashMap::new();
    for slot in &report.players {
        let balance: i64 = tx.query_row(
            "SELECT amount FROM lobby_funds WHERE lobby_id = ?1 AND player_id = ?2",
            params![report.match_id, slot.player_id],
            |r| r.get(0),
        )?;
        balances.insert(slot.player_id.clone(), balance as u64);
    }
    let transfers = settle::compute(report, rules, &balances);
    for t in &transfers {
        *balances.get_mut(&t.from_player_id).unwrap() -= t.sats;
        *balances.get_mut(&t.to_player_id).unwrap() += t.sats;
    }
    for (player, amount) in balances {
        apply(
            tx,
            &player,
            amount as i64,
            "match_release",
            &report.match_id,
        )?;
    }
    tx.execute(
        "UPDATE lobby_funds SET amount = 0 WHERE lobby_id = ?1",
        [&report.match_id],
    )?;
    tx.execute(
        "UPDATE lobbies SET phase = 'settled' WHERE id = ?1",
        [&report.match_id],
    )?;
    tx.execute(
        "INSERT INTO matches (match_id, status, transfers) VALUES (?1, 'settled', ?2)",
        params![report.match_id, serde_json::to_string(&transfers)?],
    )?;
    Ok(transfers)
}

fn match_status(tx: &Transaction, match_id: &str) -> Result<Option<MatchStatus>> {
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT status, transfers FROM matches WHERE match_id = ?1",
            [match_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        None => None,
        Some((status, _)) if status == "disputed" => Some(MatchStatus::Disputed),
        Some((_, transfers)) => Some(MatchStatus::Settled {
            transfers: serde_json::from_str(transfers.as_deref().unwrap_or("[]"))?,
        }),
    })
}

pub(crate) fn apply(
    tx: &Transaction,
    player_id: &str,
    delta: i64,
    kind: &str,
    reference: &str,
) -> Result<()> {
    let balance: i64 = tx.query_row(
        "SELECT balance_sats FROM players WHERE id = ?1",
        [player_id],
        |r| r.get(0),
    )?;
    let next = balance
        .checked_add(delta)
        .filter(|next| *next >= 0 && (*next as u64) <= MAX_BALANCE_SATS)
        .ok_or_else(|| Error::Rejected("balance would be outside the supported range".into()))?;
    tx.execute(
        "UPDATE players SET balance_sats = ?2 WHERE id = ?1",
        params![player_id, next],
    )?;
    tx.execute(
        "INSERT INTO entries (player_id, delta_sats, kind, reference) VALUES (?1, ?2, ?3, ?4)",
        params![player_id, delta, kind, reference],
    )?;
    Ok(())
}

fn deposit_status(s: &str) -> DepositStatus {
    match s {
        "credited" => DepositStatus::Credited,
        "expired" => DepositStatus::Expired,
        _ => DepositStatus::Pending,
    }
}

fn withdrawal_status(s: &str) -> WithdrawalStatus {
    match s {
        "completed" => WithdrawalStatus::Completed,
        "failed" => WithdrawalStatus::Failed,
        _ => WithdrawalStatus::Pending,
    }
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_arena_protocol::{Hit, MatchMode, PROTOCOL_VERSION, PlayerSlot};

    fn rules() -> Rules {
        Rules {
            sats_per_percent: 1,
            win_sats: 100,
            max_deposit_sats: 1_000_000,
            invoice_expiry_secs: 600,
        }
    }

    fn setup() -> (Ledger, String, String) {
        let mut ledger = Ledger::in_memory().unwrap();
        let (a, _) = ledger.register("alice").unwrap();
        let (b, _) = ledger.register("bob").unwrap();
        for (player, n) in [(&a, 1), (&b, 2)] {
            let hash = format!("hash{n}");
            let d = ledger
                .insert_deposit(player, 1000, "lnfake", &hash)
                .unwrap();
            ledger.credit_deposit(&d.id).unwrap();
            ledger.credit_deposit(&d.id).unwrap(); // Crediting twice must be a no-op.
        }
        (ledger, a, b)
    }

    fn report(a: &str, b: &str, reporter_port: u8, winner_port: Option<u8>) -> MatchReport {
        MatchReport {
            protocol_version: PROTOCOL_VERSION,
            match_id: "match-1".into(),
            mode: MatchMode::Netplay,
            reporter_port,
            players: vec![
                PlayerSlot {
                    port: 0,
                    player_id: a.into(),
                },
                PlayerSlot {
                    port: 1,
                    player_id: b.into(),
                },
            ],
            hits: vec![Hit {
                frame: 100,
                victim_port: 1,
                attacker_port: Some(0),
                damage_centi: 5000,
            }],
            winner_port,
        }
    }

    fn balance(ledger: &Ledger, id: &str) -> u64 {
        ledger.account(id).unwrap().balance_sats
    }

    fn admit(ledger: &mut Ledger, a: &str, b: &str) {
        let report = report(a, b, 0, Some(0));
        ledger
            .create_lobby(
                a,
                &sats_arena_protocol::LobbyRequest {
                    id: report.match_id,
                    mode: report.mode,
                    players: report.players,
                    budget_sats: 1000,
                },
                &rules(),
            )
            .unwrap();
        ledger.ready_lobby(a, "match-1").unwrap();
        ledger.ready_lobby(b, "match-1").unwrap();
        ledger.start_lobby(a, "match-1").unwrap();
    }

    #[test]
    fn settles_only_when_both_players_agree() {
        let (mut ledger, a, b) = setup();
        assert_eq!(balance(&ledger, &a), 1000);
        admit(&mut ledger, &a, &b);
        let status = ledger
            .submit_report(&a, &report(&a, &b, 0, Some(0)), &rules())
            .unwrap();
        assert_eq!(status, MatchStatus::WaitingForOpponent);
        assert_eq!(balance(&ledger, &a), 0); // Budget is reserved, not spendable.

        let status = ledger
            .submit_report(&b, &report(&a, &b, 1, Some(0)), &rules())
            .unwrap();
        assert!(matches!(status, MatchStatus::Settled { .. }));
        assert_eq!(balance(&ledger, &a), 1150); // 50 for damage + 100 for the win.
        assert_eq!(balance(&ledger, &b), 850);

        // Resending is harmless and doesn't pay twice.
        ledger
            .submit_report(&b, &report(&a, &b, 1, Some(0)), &rules())
            .unwrap();
        assert_eq!(balance(&ledger, &a), 1150);
    }

    #[test]
    fn disagreement_moves_nothing() {
        let (mut ledger, a, b) = setup();
        admit(&mut ledger, &a, &b);
        ledger
            .submit_report(&a, &report(&a, &b, 0, Some(0)), &rules())
            .unwrap();
        let status = ledger
            .submit_report(&b, &report(&a, &b, 1, Some(1)), &rules())
            .unwrap();
        assert_eq!(status, MatchStatus::Disputed);
        assert_eq!((balance(&ledger, &a), balance(&ledger, &b)), (0, 0)); // Still reserved for review.
    }

    #[test]
    fn cant_report_for_someone_else() {
        let (mut ledger, a, b) = setup();
        assert!(
            ledger
                .submit_report(&a, &report(&a, &b, 1, Some(0)), &rules())
                .is_err()
        );
    }

    #[test]
    fn withdrawals_reserve_and_refund() {
        let (mut ledger, a, _) = setup();
        let ok = ledger
            .reserve_withdrawal(&a, 500, 10, "inv1", "h1")
            .unwrap();
        assert_eq!(balance(&ledger, &a), 490);
        ledger.finish_withdrawal(&ok, Some(3), None).unwrap();
        assert_eq!(balance(&ledger, &a), 497);

        let failed = ledger
            .reserve_withdrawal(&a, 400, 10, "inv2", "h2")
            .unwrap();
        ledger
            .finish_withdrawal(&failed, None, Some("no route"))
            .unwrap();
        ledger
            .finish_withdrawal(&failed, None, Some("no route"))
            .unwrap();
        assert_eq!(balance(&ledger, &a), 497);

        assert!(
            ledger
                .reserve_withdrawal(&a, 400, 10, "inv1", "h1")
                .is_err()
        ); // Same invoice twice.
        assert!(
            ledger
                .reserve_withdrawal(&a, 495, 10, "inv3", "h3")
                .is_err()
        ); // Not enough with fees.
    }
}
