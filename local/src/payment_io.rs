//! Network jobs return facts to the single ledger owner. They never mutate funds.
use super::*;
use sats_arena_core::wallet::NewInvoice;
use std::{future::Future, io::Write, time::Instant};

const MAX_PAYMENTS: usize = 4;
const MAX_INCOMING: usize = 4;
const RETRY: Duration = Duration::from_secs(2);

#[derive(Clone)]
enum Target {
    Match(String),
    Late(String),
}
impl Target {
    fn id(&self) -> String {
        match self {
            Self::Match(id) => id.clone(),
            Self::Late(hash) => format!("late-entry-{hash}"),
        }
    }
}
#[derive(Clone)]
enum Operation {
    Create {
        match_id: String,
        player: usize,
        address: String,
        sats: u64,
    },
    Incoming(String),
    Prepare(Target),
    Submit(Target),
    Lookup(Target),
}
impl Operation {
    fn key(&self) -> String {
        match self {
            Self::Create {
                match_id, player, ..
            } => format!("entry-{match_id}-{player}"),
            Self::Incoming(hash) => format!("incoming-{hash}"),
            Self::Prepare(t) | Self::Submit(t) | Self::Lookup(t) => format!("payout-{}", t.id()),
        }
    }
    fn stage(&self) -> &'static str {
        match self {
            Self::Create { .. } => "entry_invoice",
            Self::Incoming(_) => "incoming_lookup",
            Self::Prepare(_) => "recipient_invoice",
            Self::Submit(_) => "payment_submit",
            Self::Lookup(_) => "payment_lookup",
        }
    }
    fn payment(&self) -> bool {
        matches!(self, Self::Prepare(_) | Self::Submit(_) | Self::Lookup(_))
    }
}
enum Value {
    Invoice(NewInvoice),
    Incoming(InvoiceState),
    Prepared(PreparedPayout),
    Payment(PaymentState),
}
struct Active {
    operation: Operation,
    queued_ms: u64,
    task: tokio::task::JoinHandle<(anyhow::Result<Value>, u64, u64)>,
}
#[derive(Default)]
pub(super) struct PaymentIo {
    active: BTreeMap<String, Active>,
    retry: BTreeMap<String, Instant>,
    waiting: BTreeMap<String, Instant>,
    ready_since: BTreeMap<String, Instant>,
    errors: BTreeMap<String, String>,
    retired_budget: u8,
    retired_refill: Option<Instant>,
}
impl Drop for PaymentIo {
    fn drop(&mut self) {
        // Submitted payments were durably marked InFlight before spawning.
        // Recovery will look up their hashes, never submit them a second time.
        for active in self.active.values() {
            active.task.abort();
        }
    }
}
impl PaymentIo {
    fn available(&mut self, op: &Operation) -> bool {
        let key = op.key();
        if self.active.contains_key(&key) {
            return false;
        }
        if self
            .retry
            .get(&key)
            .is_some_and(|until| Instant::now() < *until)
        {
            return false;
        }
        self.waiting.entry(key.clone()).or_insert_with(Instant::now);
        let count = self
            .active
            .values()
            .filter(|j| j.operation.payment() == op.payment())
            .count();
        count
            < if op.payment() {
                MAX_PAYMENTS
            } else {
                MAX_INCOMING
            }
    }
    fn spawn(
        &mut self,
        op: Operation,
        future: impl Future<Output = anyhow::Result<Value>> + Send + 'static,
    ) {
        let key = op.key();
        let queued_ms = self
            .waiting
            .remove(&key)
            .map(|at| millis(at.elapsed()))
            .unwrap_or(0);
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let started_at_ms = now_ms();
            let result = tokio::time::timeout(Duration::from_secs(15), future)
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("wallet operation timed out")));
            (result, millis(started.elapsed()), started_at_ms)
        });
        self.active.insert(
            key,
            Active {
                operation: op,
                queued_ms,
                task,
            },
        );
    }
}
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

impl Demo {
    fn timing(
        &self,
        key: &str,
        stage: &str,
        elapsed_ms: u64,
        queued_ms: u64,
        result: &str,
        started_at_ms: u64,
    ) {
        // Diagnostics contain IDs/timings only: no credentials, invoices, URLs or addresses.
        let path = self.path.with_file_name("payment-timings.jsonl");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
            let _ = std::fs::rename(&path, path.with_extension("previous.jsonl"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(path) {
            let line = serde_json::json!({"at_ms": started_at_ms.saturating_add(elapsed_ms), "started_at_ms": started_at_ms, "observed_at_ms": now_ms(), "id": key, "stage": stage,
                "elapsed_ms": elapsed_ms, "queued_ms": queued_ms, "result": result});
            let _ = writeln!(file, "{line}");
        }
    }

    /// A short accounting tick: drain finished work and launch bounded I/O.
    /// Only command handlers and this method may change or persist the ledger.
    pub(super) async fn tick(&mut self) -> Result<()> {
        let finished: Vec<_> = self
            .io
            .active
            .iter()
            .filter(|(_, j)| j.task.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in finished {
            let job = self.io.active.remove(&key).unwrap();
            let (result, elapsed, started_at_ms) = job
                .task
                .await
                .unwrap_or_else(|_| (Err(anyhow::anyhow!("wallet job interrupted")), 0, now_ms()));
            let outcome = match &result {
                Err(_) => "error",
                Ok(Value::Payment(PaymentState::Pending)) => "pending",
                Ok(Value::Payment(PaymentState::Failed { .. })) => "failed",
                _ => "ok",
            };
            self.timing(
                &key,
                job.operation.stage(),
                elapsed,
                job.queued_ms,
                outcome,
                started_at_ms,
            );
            // Poll uncertain payments and incoming receipts with a backoff.
            // A successful preparation can advance to submission immediately.
            let delay = if matches!(&result, Ok(Value::Prepared(_)) | Ok(Value::Invoice(_))) {
                Duration::ZERO
            } else {
                RETRY
            };
            self.io.retry.insert(key.clone(), Instant::now() + delay);
            match self.apply_result(&job.operation, result) {
                Ok(()) => {
                    self.io.errors.remove(&key);
                }
                Err(error) => {
                    self.io.errors.insert(key, error.to_string());
                }
            }
        }
        let previous = self.saved.pot.payouts.len();
        self.saved.pot.flush_due(now_ms());
        if previous != self.saved.pot.payouts.len() {
            self.save()?;
        }
        for payout in &self.saved.pot.payouts {
            if payout.state != PayoutState::Paid {
                self.io
                    .ready_since
                    .entry(payout.id.clone())
                    .or_insert_with(Instant::now);
            }
        }
        self.plan_payments()?;
        self.plan_entries();
        if self.saved.pot.phase == Phase::Settled
            && let Some(roster) = self.saved.next_roster.clone() {
                self.next_match(Some(roster)).await?;
            }
        // A failed request for an abandoned room must not leave a permanent
        // error on its replacement. Keep late-refund jobs across room changes.
        let mut current_keys = BTreeSet::new();
        for player in 0..4 {
            if self.saved.pot.kinds[player] == 0
                && !self.saved.pot.paid[player]
                && self.saved.pot.phase == Phase::Funding
            {
                current_keys.insert(format!("entry-{}-{player}", self.saved.pot.id));
                current_keys.insert(format!("incoming-{}", self.saved.hashes[player]));
            }
        }
        for payout in &self.saved.pot.payouts {
            if payout.state != PayoutState::Paid {
                current_keys.insert(format!("payout-{}", payout.id));
            }
        }
        for entry in &self.saved.retired_entries {
            if entry.state != PayoutState::Paid {
                current_keys.insert(format!("payout-late-entry-{}", entry.incoming_hash));
                current_keys.insert(format!("incoming-{}", entry.incoming_hash));
            }
        }
        self.io.errors.retain(|key, _| current_keys.contains(key));
        self.io.retry.retain(|key, _| current_keys.contains(key));
        self.io.waiting.retain(|key, _| current_keys.contains(key));
        if let Some(message) = self.io.errors.values().next() {
            return Err(Error::Rejected(message.clone()));
        }
        Ok(())
    }

    fn plan_entries(&mut self) {
        if self.simulation.is_none()
            && self.saved.pot.phase == Phase::Funding
            && self.require_addresses().is_ok()
        {
            for player in 0..4 {
                if self.saved.pot.kinds[player] != 0
                    || self.saved.pot.paid[player]
                    || !self.saved.hashes[player].is_empty()
                {
                    continue;
                }
                let op = Operation::Create {
                    match_id: self.saved.pot.id.clone(),
                    player,
                    address: self.saved.addresses[player].clone(),
                    sats: self.saved.pot.invoice_sats(),
                };
                if !self.io.available(&op) {
                    continue;
                }
                let wallet = self.wallet.clone();
                let sats = self.saved.pot.invoice_sats();
                let memo = format!("{} Player {}", self.saved.pot.id, player + 1);
                self.io.spawn(op, async move {
                    Ok(Value::Invoice(
                        wallet.create_invoice(sats, &memo, 600).await?,
                    ))
                });
            }
        }
        if self.saved.pot.phase == Phase::Funding {
            for player in 0..4 {
                if self.saved.pot.kinds[player] == 0
                    && !self.saved.pot.paid[player]
                    && !self.saved.hashes[player].is_empty()
                {
                    self.plan_incoming(self.saved.hashes[player].clone());
                }
            }
        }
        // Round-robin old invoices. Per-hash backoff leaves capacity for current entries.
        if self.io.retired_refill.is_none_or(|at| Instant::now() >= at) {
            self.io.retired_budget = 4;
            self.io.retired_refill = Some(Instant::now() + RETRY);
        }
        let count = self.saved.retired_entries.len();
        for _ in 0..count.min(4) {
            if self.io.retired_budget == 0 {
                break;
            }
            let index = self.retired_cursor % count;
            self.retired_cursor = (self.retired_cursor + 1) % count;
            let entry = &self.saved.retired_entries[index];
            if entry.received_sats.is_none()
                && entry.state != PayoutState::Paid
                && self.plan_incoming(entry.incoming_hash.clone())
            {
                self.io.retired_budget -= 1;
            }
        }
    }
    fn plan_incoming(&mut self, hash: String) -> bool {
        let op = Operation::Incoming(hash.clone());
        if !self.io.available(&op) {
            return false;
        }
        let wallet = self.wallet.clone();
        self.io.spawn(op, async move {
            Ok(Value::Incoming(wallet.invoice_state(&hash).await?))
        });
        true
    }
    fn plan_payments(&mut self) -> Result<()> {
        for index in 0..self.saved.pot.payouts.len() {
            let payout = self.saved.pot.payouts[index].clone();
            if payout.state == PayoutState::Paid {
                continue;
            }
            let target = Target::Match(payout.id.clone());
            if payout.state == PayoutState::Failed {
                self.io.errors.insert(
                    format!("payout-{}", payout.id),
                    "Payout failed; needs attention".into(),
                );
                continue;
            }
            self.plan_payment(
                target,
                payout.player,
                payout.sats,
                self.saved.addresses[payout.player].clone(),
                payout_memo(&self.saved.pot, &payout),
                payout.state,
                self.saved.prepared.get(&payout.id).cloned(),
            )?;
        }
        for index in 0..self.saved.retired_entries.len() {
            let entry = self.saved.retired_entries[index].clone();
            let Some(sats) = entry.received_sats else {
                continue;
            };
            if entry.state == PayoutState::Paid {
                continue;
            }
            if sats == 0 {
                self.saved.retired_entries[index].state = PayoutState::Paid;
                self.save()?;
                continue;
            }
            let target = Target::Late(entry.incoming_hash.clone());
            if entry.state == PayoutState::Failed {
                self.io.errors.insert(
                    format!("payout-{}", target.id()),
                    "A late entry return failed and needs attention".into(),
                );
                continue;
            }
            self.io
                .ready_since
                .entry(target.id())
                .or_insert_with(Instant::now);
            self.plan_payment(
                target,
                entry.player,
                sats,
                entry.address,
                "Sats Melee — Entry refunded".into(),
                entry.state,
                entry.prepared,
            )?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn plan_payment(
        &mut self,
        target: Target,
        _player: usize,
        sats: u64,
        address: String,
        memo: String,
        state: PayoutState,
        prepared: Option<PreparedPayout>,
    ) -> Result<()> {
        let op = if prepared.is_none() {
            Operation::Prepare(target.clone())
        } else if state == PayoutState::Created {
            Operation::Submit(target.clone())
        } else {
            Operation::Lookup(target.clone())
        };
        if !self.io.available(&op) {
            return Ok(());
        }
        let wallet = self.wallet.clone();
        if let Some(prepared) = prepared {
            let fees = self.saved.max_fee_sats;
            if state == PayoutState::Created {
                match &target {
                    Target::Match(id) => {
                        let index = self
                            .saved
                            .pot
                            .payouts
                            .iter()
                            .position(|p| &p.id == id)
                            .ok_or(Error::NotFound)?;
                        self.saved.pot.begin_payout(index)?;
                    }
                    Target::Late(hash) => {
                        let entry = self
                            .saved
                            .retired_entries
                            .iter_mut()
                            .find(|e| &e.incoming_hash == hash)
                            .ok_or(Error::NotFound)?;
                        entry.state = PayoutState::InFlight;
                    }
                }
                self.save()?; // Invoice/hash and InFlight must be durable before dispatch.
                self.io.spawn(op, async move {
                    Ok(Value::Payment(
                        wallet
                            .pay_invoice(&prepared.invoice, fees)
                            .await
                            .unwrap_or(PaymentState::Pending),
                    ))
                });
            } else {
                self.io.spawn(op, async move {
                    Ok(Value::Payment(
                        wallet.payment_state(&prepared.payment_hash).await?,
                    ))
                });
            }
        } else {
            if state == PayoutState::InFlight {
                self.io.errors.insert(
                    op.key(),
                    "Missing persisted invoice for pending payout".into(),
                );
                return Ok(());
            }
            let address = match parse_address(&address) {
                Ok(address) => address,
                Err(error) => {
                    self.io.errors.insert(op.key(), error.to_string());
                    return Ok(());
                }
            };
            let provider = self.provider.clone();
            let id = target.id();
            self.io.spawn(op, async move {
                Ok(Value::Prepared(
                    prepare_payout_with_comment(
                        wallet.as_ref(),
                        provider.as_ref(),
                        &address,
                        sats,
                        &id,
                        &memo,
                    )
                    .await?,
                ))
            });
        }
        Ok(())
    }

    fn apply_result(&mut self, op: &Operation, result: anyhow::Result<Value>) -> Result<()> {
        let value = match result {
            Ok(value) => value,
            // Submission may have reached the wallet. Its durable InFlight state
            // makes the next operation a lookup, even after a timeout or crash.
            Err(_) if matches!(op, Operation::Submit(_)) => Value::Payment(PaymentState::Pending),
            Err(_) if matches!(op, Operation::Incoming(hash) if self.saved.retired_entries.iter().any(|e| &e.incoming_hash == hash)) =>
            {
                return Ok(());
            }
            Err(_)
                if matches!(op, Operation::Create {match_id, player, address, sats}
                if self.saved.pot.id != *match_id || self.saved.addresses[*player] != *address || self.saved.pot.invoice_sats() != *sats) =>
            {
                return Ok(());
            }
            Err(error) => return Err(Error::Internal(error)),
        };
        match (op, value) {
            (
                Operation::Create {
                    match_id,
                    player,
                    address,
                    sats,
                },
                Value::Invoice(invoice),
            ) => {
                if !self
                    .saved
                    .used_entry_hashes
                    .insert(invoice.payment_hash.clone())
                {
                    return Err(Error::Rejected(
                        "Wallet reused an entry invoice hash".into(),
                    ));
                }
                if self.saved.pot.id == *match_id
                    && self.saved.pot.phase == Phase::Funding
                    && self.saved.pot.kinds[*player] == 0
                    && self.saved.addresses[*player] == *address
                    && self.saved.pot.invoice_sats() == *sats
                    && self.saved.hashes[*player].is_empty()
                {
                    self.saved.hashes[*player] = invoice.payment_hash;
                    self.saved.invoices[*player] = invoice.bolt11;
                    self.saved.invoice_addresses[*player] = address.clone();
                } else {
                    // A roster/address change raced invoice creation. Keep the
                    // original binding so a late receipt is returned in full.
                    self.saved.retired_entries.push(RetiredEntry {
                        incoming_hash: invoice.payment_hash,
                        player: *player,
                        address: address.clone(),
                        expected_sats: *sats,
                        received_sats: None,
                        prepared: None,
                        state: PayoutState::Created,
                    });
                }
            }
            (Operation::Incoming(hash), Value::Incoming(state)) => {
                self.apply_incoming(hash, state)?
            }
            (Operation::Prepare(target), Value::Prepared(prepared)) => {
                if self
                    .saved
                    .used_payment_hashes
                    .contains(&prepared.payment_hash)
                {
                    return Err(Error::Rejected("Recipient reused a payment hash".into()));
                }
                match target {
                    Target::Match(id) => {
                        if !self
                            .saved
                            .pot
                            .payouts
                            .iter()
                            .any(|p| &p.id == id && p.state == PayoutState::Created)
                        {
                            return Err(Error::Rejected("Stale payout preparation".into()));
                        }
                        self.saved
                            .used_payment_hashes
                            .insert(prepared.payment_hash.clone());
                        self.saved.prepared.insert(id.clone(), prepared);
                    }
                    Target::Late(hash) => {
                        let entry = self
                            .saved
                            .retired_entries
                            .iter_mut()
                            .find(|e| &e.incoming_hash == hash)
                            .ok_or(Error::NotFound)?;
                        if entry.state != PayoutState::Created {
                            return Err(Error::Rejected("Stale entry return preparation".into()));
                        }
                        self.saved
                            .used_payment_hashes
                            .insert(prepared.payment_hash.clone());
                        entry.prepared = Some(prepared);
                    }
                }
            }
            (Operation::Submit(target) | Operation::Lookup(target), Value::Payment(state)) => {
                self.apply_payment(target, state)?
            }
            _ => return Err(Error::Rejected("Unexpected wallet job response".into())),
        }
        self.save()
    }
    fn apply_incoming(&mut self, hash: &str, state: InvoiceState) -> Result<()> {
        if let Some(player) = self.saved.hashes.iter().position(|h| h == hash) {
            if self.saved.pot.paid[player] {
                return Ok(());
            }
            match state {
                InvoiceState::Paid { amount_sats } => {
                    if self.saved.pot.phase != Phase::Funding
                        || amount_sats != self.saved.pot.invoice_sats()
                        || (!self.saved.invoice_addresses[player].is_empty()
                            && self.saved.invoice_addresses[player] != self.saved.addresses[player])
                    {
                        self.retire_entry(player, Some(amount_sats));
                    } else {
                        self.saved.pot.confirm_entry(player, amount_sats)?;
                        self.event(format!(
                            "Player {} Invoice Paid — {} sats ({} pot + {} fees)",
                            player + 1,
                            amount_sats,
                            self.saved.pot.entry_sats,
                            self.saved.pot.surcharge_sats()
                        ));
                    }
                }
                InvoiceState::Expired => self.retire_entry(player, None),
                InvoiceState::Pending => (),
            }
        } else if let Some(entry) = self
            .saved
            .retired_entries
            .iter_mut()
            .find(|e| e.incoming_hash == hash)
            && entry.received_sats.is_none() && entry.state != PayoutState::Paid
                && let InvoiceState::Paid { amount_sats } = state {
                    entry.received_sats = Some(amount_sats);
                    let player = entry.player;
                    self.event(format!(
                        "Late Player {} invoice paid — {} sats; returning to original address",
                        player + 1,
                        amount_sats
                    ));
                }
        Ok(())
    }
    fn apply_payment(&mut self, target: &Target, state: PaymentState) -> Result<()> {
        if matches!(state, PaymentState::Pending) {
            return Ok(());
        }
        let id = target.id();
        let at_ms = now_ms();
        let receipt = match target {
            Target::Match(id) => {
                let index = self
                    .saved
                    .pot
                    .payouts
                    .iter()
                    .position(|p| &p.id == id)
                    .ok_or(Error::NotFound)?;
                if self.saved.pot.payouts[index].state == PayoutState::Paid {
                    return Ok(());
                }
                if let PaymentState::Failed { reason } = state {
                    self.saved.pot.payouts[index].state = PayoutState::Failed;
                    self.save()?;
                    return Err(Error::Rejected(reason));
                }
                self.saved.pot.confirm_payout(index)?;
                let payout = &self.saved.pot.payouts[index];
                let memo = payout_memo(&self.saved.pot, payout);
                let prepared = self.saved.prepared.get_mut(id).ok_or(Error::NotFound)?;
                prepared.confirmed_at_ms = Some(at_ms);
                Receipt {
                    id: id.clone(),
                    at_ms,
                    player: payout.player,
                    address: prepared.address.clone(),
                    sats: payout.sats,
                    detail: format!("{:?}", payout.reason),
                    memo: if prepared.memo.is_empty() {
                        memo
                    } else {
                        prepared.memo.clone()
                    },
                }
            }
            Target::Late(hash) => {
                let entry = self
                    .saved
                    .retired_entries
                    .iter_mut()
                    .find(|e| &e.incoming_hash == hash)
                    .ok_or(Error::NotFound)?;
                if entry.state == PayoutState::Paid {
                    return Ok(());
                }
                if let PaymentState::Failed { .. } = state {
                    entry.state = PayoutState::Failed;
                    self.save()?;
                    return Err(Error::Rejected(
                        "A late entry return failed and needs attention".into(),
                    ));
                }
                if entry.state != PayoutState::InFlight {
                    return Err(Error::Rejected("Entry return was not submitted".into()));
                }
                entry.state = PayoutState::Paid;
                let prepared = entry.prepared.as_mut().ok_or(Error::NotFound)?;
                prepared.confirmed_at_ms = Some(at_ms);
                Receipt {
                    id: id.clone(),
                    at_ms,
                    player: entry.player,
                    address: entry.address.clone(),
                    sats: entry.received_sats.unwrap(),
                    detail: "Late entry return".into(),
                    memo: if prepared.memo.is_empty() {
                        "Sats Melee — Entry refunded".into()
                    } else {
                        prepared.memo.clone()
                    },
                }
            }
        };
        let message = receipt_message(&receipt, self.saved.simulated);
        self.saved.receipts.push(receipt);
        if self.saved.receipts.len() > 100 {
            self.saved.receipts.remove(0);
        }
        self.record_activity(at_ms, message);
        if let Some(ready) = self.io.ready_since.remove(&id) {
            let elapsed = millis(ready.elapsed());
            self.timing(
                &id,
                "payout_complete",
                elapsed,
                0,
                "paid",
                now_ms().saturating_sub(elapsed),
            );
        }
        Ok(())
    }

    /// Tests explicitly drive network completion; production uses only short ticks.
    #[cfg(test)]
    pub(super) async fn reconcile(&mut self) -> Result<()> {
        self.io.retry.clear();
        self.io.retired_refill = None;
        for _ in 0..100 {
            self.tick().await?;
            if self.io.active.is_empty() {
                return Ok(());
            }
            while !self.io.active.values().any(|j| j.task.is_finished()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        Err(Error::Rejected("wallet test did not become idle".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn invoice_creation_racing_a_new_room_is_bound_to_its_original_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        let op = Operation::Create {
            match_id: demo.saved.pot.id.clone(),
            player: 0,
            address: "alice@example.test".into(),
            sats: 500,
        };
        demo.next_match(None).await.unwrap();
        demo.saved.addresses[0] = "bob@example.test".into();
        let invoice = demo
            .wallet
            .create_invoice(500, "old room", 600)
            .await
            .unwrap();
        let hash = invoice.payment_hash.clone();
        demo.apply_result(&op, Ok(Value::Invoice(invoice))).unwrap();
        let retired = demo
            .saved
            .retired_entries
            .iter()
            .find(|e| e.incoming_hash == hash)
            .unwrap();
        assert_eq!(retired.address, "alice@example.test");
        assert_eq!(retired.expected_sats, 500);
        assert!(!demo.saved.pot.paid[0]);
        demo.apply_result(&op, Err(anyhow::anyhow!("old request failed")))
            .unwrap();
        assert_eq!(demo.saved.addresses[0], "bob@example.test");
    }
    #[tokio::test]
    async fn a_generated_entry_hash_cannot_fund_two_slots_or_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.retire_entries();
        demo.saved.addresses[0] = "alice@example.test".into();
        demo.saved.addresses[1] = "bob@example.test".into();
        let invoice = demo
            .wallet
            .create_invoice(500, "shared", 600)
            .await
            .unwrap();
        let copy = || NewInvoice {
            bolt11: invoice.bolt11.clone(),
            payment_hash: invoice.payment_hash.clone(),
        };
        let first = Operation::Create {
            match_id: demo.saved.pot.id.clone(),
            player: 0,
            address: demo.saved.addresses[0].clone(),
            sats: 500,
        };
        demo.apply_result(&first, Ok(Value::Invoice(copy())))
            .unwrap();
        let second = Operation::Create {
            match_id: demo.saved.pot.id.clone(),
            player: 1,
            address: demo.saved.addresses[1].clone(),
            sats: 500,
        };
        assert!(
            demo.apply_result(&second, Ok(Value::Invoice(copy())))
                .is_err()
        );
        assert!(demo.saved.hashes[1].is_empty());
        demo.next_match(None).await.unwrap();
        demo.retire_entry(0, None);
        let next = Operation::Create {
            match_id: demo.saved.pot.id.clone(),
            player: 0,
            address: demo.saved.addresses[0].clone(),
            sats: 500,
        };
        assert!(
            demo.apply_result(&next, Ok(Value::Invoice(copy())))
                .is_err()
        );
        assert!(!demo.saved.pot.paid.iter().any(|&p| p));
    }
}
