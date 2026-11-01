//! Network jobs return facts to the single ledger owner. They never mutate funds.
use super::*;
use sats_arena_core::wallet::{IncomingReceiptError, NewInvoice, WalletIssue};
use std::{future::Future, io::Write};
use tokio::time::Instant;

const MAX_PAYMENTS: usize = 4;
const MAX_PREPARATIONS: usize = 4;
const MAX_LOOKUPS: usize = 2;
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
    Submit {
        target: Target,
        address: String,
    },
    Lookup(Target),
}
impl Operation {
    fn key(&self) -> String {
        match self {
            Self::Create {
                match_id, player, ..
            } => format!("entry-{match_id}-{player}"),
            Self::Incoming(hash) => format!("incoming-{hash}"),
            Self::Prepare(t) | Self::Lookup(t) | Self::Submit { target: t, .. } => {
                format!("payout-{}", t.id())
            }
        }
    }
    fn stage(&self) -> &'static str {
        match self {
            Self::Create { .. } => "entry_invoice",
            Self::Incoming(_) => "incoming_lookup",
            Self::Prepare(_) => "recipient_invoice",
            Self::Submit { .. } => "payment_submit",
            Self::Lookup(_) => "payment_lookup",
        }
    }
    fn lane(&self) -> u8 {
        match self {
            Self::Submit { .. } => 0,
            Self::Prepare(_) => 1,
            Self::Lookup(_) => 2,
            _ => 3,
        }
    }
    fn failure_message(&self) -> &'static str {
        match self {
            Self::Create { .. } => {
                "Could not create an entry invoice; check the wallet connection. Retrying automatically."
            }
            Self::Incoming(_) => {
                "Could not check an entry payment yet. Retrying automatically; do not pay again."
            }
            Self::Prepare(_) => {
                "Could not request a receiving invoice; see missed payments. Funds remain reserved."
            }
            Self::Submit { .. } => {
                "Waiting for wallet confirmation; checking the original payment. Funds remain reserved."
            }
            Self::Lookup(_) => {
                "Could not check payout status. Retrying automatically; the original payment remains reserved."
            }
        }
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
struct VerificationJob {
    address: String,
    task: tokio::task::JoinHandle<AddressCheck>,
}
#[derive(Default)]
pub(super) struct PaymentIo {
    pub(super) serialize_submissions: bool,
    pub(super) track_missed: bool,
    active: BTreeMap<String, Active>,
    retry: BTreeMap<String, Instant>,
    waiting: BTreeMap<String, Instant>,
    ready_since: BTreeMap<String, Instant>,
    errors: BTreeMap<String, String>,
    retired_budget: u8,
    retired_refill: Option<Instant>,
    verifications: BTreeMap<usize, VerificationJob>,
    submit_interval: Option<Duration>,
    next_submit: Option<Instant>,
    lookup_attempts: BTreeMap<String, u32>,
}
impl Drop for PaymentIo {
    fn drop(&mut self) {
        // Submitted payments were durably marked InFlight before spawning.
        // Recovery will look up their hashes, never submit them a second time.
        for active in self.active.values() {
            active.task.abort();
        }
        for job in self.verifications.values() {
            job.task.abort();
        }
    }
}
impl PaymentIo {
    pub(super) fn cancel_preparations(&mut self, ids: &[String]) {
        for id in ids {
            let key = format!("payout-{id}");
            if self
                .active
                .get(&key)
                .is_some_and(|j| matches!(j.operation, Operation::Prepare(_)))
            {
                self.active.remove(&key).unwrap().task.abort();
            }
            self.clear_retry(id);
        }
    }
    pub(super) fn clear_retry(&mut self, id: &str) {
        let key = format!("payout-{id}");
        self.retry.remove(&key);
        self.lookup_attempts.remove(&key);
        self.errors.remove(&key);
    }
    pub(super) fn set_submission_rate(&mut self, per_second: u32) -> Result<()> {
        if !(1..=20).contains(&per_second) {
            return Err(Error::Rejected(
                "Payment starts per second must be 1–20".into(),
            ));
        }
        self.submit_interval = Some(Duration::from_secs_f64(1.0 / f64::from(per_second)));
        Ok(())
    }

    pub(super) fn outbound_status(&self, id: &str, state: PayoutState, prepared: bool) -> String {
        let key = format!("payout-{id}");
        let active = self.active.get(&key).map(|job| &job.operation);
        let stage = match state {
            PayoutState::Paid => "Confirmed",
            PayoutState::Failed => "Failed; needs attention",
            PayoutState::Superseded => "Consolidated; this original payment will not be sent",
            PayoutState::Discarded => "Discarded without sending; funds retained by host",
            PayoutState::Created if matches!(active, Some(Operation::Prepare(_))) => {
                "Requesting receiving invoice"
            }
            PayoutState::Created if prepared => "Queued to send",
            PayoutState::Created => "Queued for receiving invoice",
            PayoutState::InFlight if matches!(active, Some(Operation::Submit { .. })) => "Sending",
            PayoutState::InFlight if matches!(active, Some(Operation::Lookup(_))) => {
                "Checking confirmation"
            }
            PayoutState::InFlight => "Awaiting confirmation",
        };
        let mut description = stage.to_owned();
        // Fixed descriptions only: raw provider errors may contain credentials.
        if !state.complete() && state != PayoutState::Failed && self.errors.contains_key(&key) {
            description.push_str("; retrying after an error");
        }
        if let Some(ready) = self.ready_since.get(id) {
            description.push_str(&format!(
                "; observed outstanding {}s",
                ready.elapsed().as_secs()
            ));
        }
        description
    }

    pub(super) fn verifying(&self, player: usize, address: &str) -> bool {
        self.verifications
            .get(&player)
            .is_some_and(|job| job.address == address)
    }
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
        if matches!(op, Operation::Submit { .. })
            && self.next_submit.is_some_and(|at| Instant::now() < at)
        {
            return false;
        }
        let count = self
            .active
            .values()
            .filter(|j| j.operation.lane() == op.lane())
            .count();
        count
            < match op.lane() {
                0 if self.serialize_submissions => 1,
                0 => MAX_PAYMENTS,
                1 => MAX_PREPARATIONS,
                2 => MAX_LOOKUPS,
                _ => MAX_INCOMING,
            }
    }
    fn spawn(
        &mut self,
        op: Operation,
        future: impl Future<Output = anyhow::Result<Value>> + Send + 'static,
    ) {
        let key = op.key();
        if matches!(op, Operation::Submit { .. }) {
            self.next_submit =
                Some(Instant::now() + self.submit_interval.unwrap_or(Duration::from_secs(1)));
        }
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
    pub(super) fn unsent_sats(&self) -> u64 {
        std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .map(|saved| {
                saved.pot.unbatched.iter().sum::<u64>()
                    + saved
                        .pot
                        .payouts
                        .iter()
                        .filter(|p| p.state == PayoutState::Created)
                        .map(|p| p.sats)
                        .sum::<u64>()
            })
            .sum::<u64>()
            + self
                .saved
                .retired_entries
                .iter()
                .filter(|e| e.state == PayoutState::Created)
                .map(|e| e.received_sats.unwrap_or(0))
                .sum::<u64>()
    }

    pub(super) fn discard_unsent(&mut self) -> Result<()> {
        let before = self.saved.clone();
        let mut sats = self.saved.pot.discard_unsent();
        for saved in &mut self.saved.pending_matches {
            sats += saved.pot.discard_unsent();
        }
        let mut ids: Vec<_> = std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .flat_map(|saved| {
                saved
                    .pot
                    .payouts
                    .iter()
                    .filter(|p| p.state == PayoutState::Discarded)
                    .map(|p| p.id.clone())
            })
            .collect();
        for entry in &mut self.saved.retired_entries {
            if entry.state == PayoutState::Created && entry.received_sats.is_some() {
                sats += entry.received_sats.unwrap();
                entry.state = PayoutState::Discarded;
                ids.push(format!("late-entry-{}", entry.incoming_hash));
            }
        }
        self.event(format!(
            "Discarded {sats} unsent sats • retained in host wallet; submitted payments unchanged"
        ));
        if let Err(error) = self.save() {
            self.saved = before;
            return Err(error);
        }
        // Persist the discard before cancelling any invoice-only work.
        for id in ids {
            let key = format!("payout-{id}");
            if let Some(job) = self.io.active.remove(&key) {
                debug_assert!(matches!(job.operation, Operation::Prepare(_)));
                job.task.abort();
            }
            self.io.ready_since.remove(&id);
            self.io.errors.remove(&key);
            self.io.retry.remove(&key);
            self.io.waiting.remove(&key);
        }
        Ok(())
    }

    /// Only Created intents can be replaced. Their optional invoice-resolution
    /// tasks cannot spend funds and are cancelled after the replacement is saved.
    pub(super) fn consolidate_outbound(&mut self) -> Result<()> {
        let mut replacements = Vec::new();
        if self
            .saved
            .pot
            .consolidate_created_except(&self.saved.held_payouts())
        {
            replacements.extend(
                self.saved
                    .pot
                    .payout_replacements
                    .iter()
                    .map(|(old, new)| (old.clone(), new.clone())),
            );
        }
        for saved in &mut self.saved.pending_matches {
            if saved.pot.consolidate_created_except(&saved.held_payouts()) {
                replacements.extend(
                    saved
                        .pot
                        .payout_replacements
                        .iter()
                        .map(|(old, new)| (old.clone(), new.clone())),
                );
            }
        }
        if replacements.is_empty() {
            return Ok(());
        }
        self.save()?;
        for (old, new) in replacements {
            let key = format!("payout-{old}");
            if let Some(job) = self.io.active.remove(&key) {
                // InFlight is persisted before Submit is spawned, so a Created
                // intent can only have an invoice-preparation job here.
                debug_assert!(matches!(job.operation, Operation::Prepare(_)));
                job.task.abort();
            }
            if let Some(ready) = self.io.ready_since.remove(&old) {
                let next = self.io.ready_since.entry(new.clone()).or_insert(ready);
                *next = (*next).min(ready);
            }
            self.io.errors.remove(&key);
            self.io.retry.remove(&key);
            self.io.waiting.remove(&key);
        }
        Ok(())
    }

    pub(super) fn start_verification(&mut self, player: usize, address: LightningAddress) {
        if let Some(previous) = self.io.verifications.remove(&player) {
            previous.task.abort();
        }
        if let Some(report) = self.cached_receiver(&address) {
            self.saved.verification[player] = AddressCheck {
                report: Some(report),
                error: String::new(),
            };
            self.remember_receiver(player);
            return;
        }
        let verifier = self.verifier.clone();
        let bound_address = address.as_str().to_owned();
        let task = tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), verifier.verify(&address)).await {
                Ok(Ok(report)) if report.address == address.as_str() => AddressCheck {
                    report: Some(report),
                    error: String::new(),
                },
                Ok(Ok(_)) => AddressCheck {
                    report: None,
                    error: "Verification returned a different receiving address".into(),
                },
                Ok(Err(error)) => AddressCheck {
                    report: None,
                    error: format!("{error:#}"),
                },
                Err(_) => AddressCheck {
                    report: None,
                    error:
                        "Receiver did not respond within 15 seconds. Check the address or retry."
                            .into(),
                },
            }
        });
        self.io.verifications.insert(
            player,
            VerificationJob {
                address: bound_address,
                task,
            },
        );
    }

    pub(super) async fn finish_verifications(&mut self) -> Result<()> {
        let finished: Vec<_> = self
            .io
            .verifications
            .iter()
            .filter(|(_, job)| job.task.is_finished())
            .map(|(player, _)| *player)
            .collect();
        for player in finished {
            let job = self.io.verifications.remove(&player).unwrap();
            let check = job.task.await.unwrap_or_else(|_| AddressCheck {
                report: None,
                error: "Receiving check was interrupted. Retry this address.".into(),
            });
            // Edits and room changes cannot attach an old result to a new input.
            if self.saved.addresses[player] == job.address {
                self.saved.verification[player] = check;
                self.remember_receiver(player);
                self.save()?;
            }
        }
        Ok(())
    }
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
            let id = key.strip_prefix("payout-").unwrap_or(key);
            let attempt = self
                .recovery(id)
                .map(|r| {
                    let already_failed = self
                        .payment_details(id)
                        .and_then(|(_, _, p)| p)
                        .is_some_and(|p| r.confirmed_failed(&p));
                    r.failed.len() + usize::from(!already_failed || stage == "recipient_invoice")
                })
                .unwrap_or(1);
            let line = serde_json::json!({"at_ms": started_at_ms.saturating_add(elapsed_ms), "started_at_ms": started_at_ms, "observed_at_ms": now_ms(), "id": key, "stage": stage,
                "elapsed_ms": elapsed_ms, "queued_ms": queued_ms, "result": result, "attempt": attempt, "transfer_id": self.payout_owner(id).and_then(|o| o.group_leader(id)).unwrap_or(id)});
            let _ = writeln!(file, "{line}");
        }
    }

    /// A short accounting tick: drain finished work and launch bounded I/O.
    /// Only command handlers and this method may change or persist the ledger.
    pub(super) async fn tick(&mut self) -> Result<()> {
        self.finish_verifications().await?;
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
            // Back off failed lookups, not a wallet's healthy processing response.
            // An uncertain send can be checked immediately without sending again.
            let delay = if matches!(&result, Ok(Value::Prepared(_)) | Ok(Value::Invoice(_)))
                || (matches!(job.operation, Operation::Submit { .. })
                    && matches!(&result, Ok(Value::Payment(PaymentState::Pending)) | Err(_)))
            {
                Duration::ZERO
            } else if matches!(
                job.operation,
                Operation::Lookup(_)
                    | Operation::Prepare(_)
                    | Operation::Create { .. }
                    | Operation::Incoming(_)
            ) && result.is_err()
            {
                let attempts = self.io.lookup_attempts.entry(key.clone()).or_default();
                *attempts = attempts.saturating_add(1);
                Duration::from_secs((2u64 << (*attempts - 1).min(4)).min(30))
            } else {
                self.io.lookup_attempts.remove(&key);
                if matches!(&job.operation, Operation::Incoming(hash) if self.saved.retired_entries.iter().any(|entry| &entry.incoming_hash == hash))
                {
                    // Keep late-payment recovery, without continuously polling
                    // abandoned invoices throughout every subsequent game.
                    if matches!(&result, Ok(Value::Incoming(InvoiceState::Expired))) {
                        Duration::from_secs(300)
                    } else {
                        Duration::from_secs(30)
                    }
                } else {
                    RETRY
                }
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
        self.consolidate_outbound()?;
        if previous != self.saved.pot.payouts.len() {
            self.save()?;
        }
        for payout in std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .flat_map(|saved| &saved.pot.payouts)
        {
            if !payout.state.complete() {
                self.io
                    .ready_since
                    .entry(payout.id.clone())
                    .or_insert_with(Instant::now);
            }
        }
        self.plan_payments()?;
        self.plan_entries();
        self.finish_pending_matches()?;
        if matches!(self.saved.pot.phase, Phase::Settled | Phase::Settling)
            && let Some(roster) = self.saved.next_roster.clone()
        {
            self.next_match(Some(roster)).await?;
        }
        // A failed request for an abandoned room must not leave a permanent
        // error on its replacement. Keep late-refund jobs across room changes.
        let mut current_keys = BTreeSet::new();
        for player in 0..4 {
            if self.saved.pot.participates(player)
                && !self.saved.pot.paid[player]
                && self.saved.pot.phase == Phase::Funding
            {
                current_keys.insert(format!("entry-{}-{player}", self.saved.pot.id));
                current_keys.insert(format!("incoming-{}", self.saved.hashes[player]));
            }
        }
        for payout in std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .flat_map(|saved| &saved.pot.payouts)
        {
            if !payout.state.complete() {
                current_keys.insert(format!("payout-{}", payout.id));
            }
        }
        for entry in &self.saved.retired_entries {
            if !entry.state.complete() {
                current_keys.insert(format!("payout-late-entry-{}", entry.incoming_hash));
                current_keys.insert(format!("incoming-{}", entry.incoming_hash));
            }
        }
        self.io.errors.retain(|key, _| current_keys.contains(key));
        self.io.retry.retain(|key, _| current_keys.contains(key));
        self.io.waiting.retain(|key, _| current_keys.contains(key));
        self.io
            .lookup_attempts
            .retain(|key, _| current_keys.contains(key));
        if let Some((_, message)) = self.io.errors.iter().find(|(key, _)| {
            !key.strip_prefix("payout-")
                .and_then(|id| self.recovery(id))
                .is_some_and(|r| r.missed)
        }) {
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
                if !self.saved.pot.participates(player)
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
                if self.saved.pot.participates(player)
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
                && !entry.state.complete()
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
        // Current payouts take the first available slots; a backlog of slow
        // recoveries must not starve the current game indefinitely.
        let mut jobs: Vec<_> = std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .flat_map(|saved| {
                saved.pot.payouts.iter().map(|payout| {
                    (
                        payout.clone(),
                        saved
                            .prepared
                            .get(&payout.id)
                            .map(|p| p.address.clone())
                            .unwrap_or_else(|| saved.addresses[payout.player].clone()),
                        payout_memo(&saved.pot, payout),
                        saved.prepared.get(&payout.id).cloned(),
                    )
                })
            })
            .collect();
        // Final settlements get precedence over damage batches. Lookup requests
        // have their own lane and cannot consume submission/preparation slots.
        let scheduling_now = Instant::now();
        jobs.sort_by_key(|(p, _, _, prepared)| {
            let finalizing = self
                .payout_owner(&p.id)
                .is_some_and(|saved| saved.pot.phase == Phase::Settling);
            (
                !finalizing,
                prepared.is_none(),
                self.io
                    .ready_since
                    .get(&p.id)
                    .copied()
                    .unwrap_or(scheduling_now),
            )
        });
        for (payout, address, memo, prepared) in jobs {
            if payout.state.complete() {
                continue;
            }
            let owner = self.payout_owner(&payout.id).ok_or(Error::NotFound)?;
            if owner
                .group_leader(&payout.id)
                .is_some_and(|leader| leader != payout.id)
            {
                continue;
            }
            let sats = owner.group_sats(&payout.id).unwrap_or(payout.sats);
            let memo = if let Some(members) = owner.payout_groups.get(&payout.id) {
                format!(
                    "Sats Melee — Missed payouts: {}",
                    owner
                        .pot
                        .payouts
                        .iter()
                        .filter(|p| members.contains(&p.id))
                        .map(|p| format!(
                            "P{} {}",
                            p.player + 1,
                            payout_memo(&owner.pot, p).trim_start_matches("Sats Melee — ")
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            } else {
                memo
            };
            let target = Target::Match(payout.id.clone());
            self.plan_payment(
                target,
                payout.player,
                sats,
                address,
                memo,
                payout.state,
                prepared,
            )?;
        }
        for index in 0..self.saved.retired_entries.len() {
            let entry = self.saved.retired_entries[index].clone();
            let Some(sats) = entry.received_sats else {
                continue;
            };
            if entry.state.complete() {
                continue;
            }
            if sats == 0 {
                self.saved.retired_entries[index].state = PayoutState::Paid;
                self.save()?;
                continue;
            }
            let target = Target::Late(entry.incoming_hash.clone());
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
        let recovery = self.recovery(&target.id()).cloned().unwrap_or_default();
        let proven_failure = prepared
            .as_ref()
            .is_some_and(|p| recovery.confirmed_failed(p))
            || recovery.retry_prepared;
        if state == PayoutState::Failed && proven_failure && !recovery.retry_due() {
            return Ok(());
        }
        let retrying = state == PayoutState::Failed && proven_failure;
        let sendable = state == PayoutState::Created || retrying;
        if sendable && recovery.missed && !recovery.manual_ready {
            return Ok(());
        }
        let prepare_new = prepared.is_none() || (retrying && !recovery.retry_prepared);
        if state == PayoutState::Failed && !proven_failure && prepared.is_none() {
            return Ok(()); // No original hash to reconcile; never guess a replacement.
        }
        // Keep live damage batched while this match's receiver is still processing.
        // Final unsent amounts are independent debts; an unknown earlier send
        // must not hold them indefinitely or cause its own hash to be replaced.
        // Older uncertain matches must not block a fresh match's own payouts.
        let processing = sendable
            && match &target {
                Target::Match(id) => self.payout_owner(id).is_some_and(|owner| {
                    owner.pot.phase != Phase::Settling
                        && owner.pot.payouts.iter().any(|p| {
                            p.state == PayoutState::InFlight
                                && !owner.recovery.get(&p.id).is_some_and(|r| r.missed)
                                && owner
                                    .prepared
                                    .get(&p.id)
                                    .is_some_and(|invoice| invoice.address == address)
                        })
                }),
                Target::Late(_) => false,
            };
        if sendable
            && (processing || self.io.active.values().any(|job| {
                matches!(&job.operation, Operation::Submit { address: active, .. } if active == &address)
            }))
        {
            return Ok(());
        }
        let op = if prepare_new {
            Operation::Prepare(target.clone())
        } else if sendable {
            Operation::Submit {
                target: target.clone(),
                address: address.clone(),
            }
        } else {
            Operation::Lookup(target.clone())
        };
        if !self.io.available(&op) {
            return Ok(());
        }
        let wallet = self.wallet.clone();
        if !prepare_new {
            let prepared = prepared.ok_or(Error::NotFound)?;
            let fees = self.saved.max_fee_sats;
            if sendable {
                match &target {
                    Target::Match(id) => {
                        let owner = self.payout_owner_mut(id)?;
                        let index = owner
                            .pot
                            .payouts
                            .iter()
                            .position(|p| &p.id == id)
                            .ok_or(Error::NotFound)?;
                        if retrying {
                            if owner.pot.payouts[index].state != PayoutState::Failed {
                                return Err(Error::Rejected("Retry is no longer eligible".into()));
                            }
                            owner.pot.payouts[index].state = PayoutState::InFlight;
                        } else {
                            owner.pot.begin_payout(index)?;
                        }
                        if let Some(members) = owner.payout_groups.get(id).cloned() {
                            for member in members.iter().filter(|member| *member != id) {
                                let part = owner
                                    .pot
                                    .payouts
                                    .iter_mut()
                                    .find(|p| p.id == *member)
                                    .ok_or(Error::NotFound)?;
                                part.state = PayoutState::InFlight;
                            }
                        }
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
                if self.recovery(&target.id()).is_some() {
                    let record = self.recovery_mut(&target.id())?;
                    record.issue = None;
                    record.retry_at_ms = None;
                    record.retry_prepared = false;
                }
                self.recovery_mut(&target.id())?.submitted_at_ms = Some(now_ms());
                if retrying {
                    let attempt = recovery.failed.len() + 1;
                    self.event(format!(
                        "Retry {attempt} of {}: sending {sats} sats still owed to {address}",
                        recovery.limit
                    ));
                }
                self.save()?; // Invoice/hash and InFlight must be durable before dispatch.
                let queued_ms = self
                    .io
                    .waiting
                    .get(&op.key())
                    .map(|at| millis(at.elapsed()))
                    .unwrap_or(0);
                self.timing(
                    &target.id(),
                    "payment_dispatch",
                    0,
                    queued_ms,
                    "dispatched",
                    now_ms(),
                );
                self.io.spawn(op, async move {
                    wallet
                        .pay_invoice(&prepared.invoice, fees)
                        .await
                        .map(Value::Payment)
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
            let id = if retrying {
                format!("{}-attempt-{}", target.id(), recovery.failed.len() + 1)
            } else {
                target.id()
            };
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
        if let Operation::Prepare(target)
        | Operation::Submit { target, .. }
        | Operation::Lookup(target) = op
        {
            let pending_stalled = self
                .recovery(&target.id())
                .and_then(|r| r.submitted_at_ms)
                .is_none_or(|at| now_ms().saturating_sub(at) >= 12_000);
            if result.is_err()
                || matches!(&result, Ok(Value::Payment(PaymentState::Failed { .. })))
                || (pending_stalled && matches!(&result, Ok(Value::Payment(PaymentState::Pending))))
            {
                self.mark_missed(&target.id())?;
            }
        }
        if let Err(error) = &result {
            match op {
                Operation::Submit { target, .. } | Operation::Lookup(target) => {
                    self.record_issue(&target.id(), error)?
                }
                Operation::Prepare(target) => {
                    self.record_issue(&target.id(), &WalletIssue::ReceivingInvoice.into())?
                }
                _ => (),
            }
        }
        let value = match result {
            Ok(value) => value,
            // Submission may have reached the wallet. Its durable InFlight state
            // makes the next operation a lookup, even after a timeout or crash.
            Err(_) if matches!(op, Operation::Submit { .. }) => {
                Value::Payment(PaymentState::Pending)
            }
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
            // Provider responses can contain credentials. Describe the failed operation,
            // without exposing the provider error or treating uncertainty as a failed payment.
            Err(error) => {
                let message = error
                    .downcast_ref::<IncomingReceiptError>()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| op.failure_message().into());
                return Err(Error::Rejected(message));
            }
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
                    && self.saved.pot.participates(*player)
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
            (Operation::Lookup(target), Value::Payment(PaymentState::Pending)) => {
                if self.recovery(&target.id()).and_then(|r| r.issue).is_some() {
                    self.recovery_mut(&target.id())?.issue = None;
                    if let Some((sats, address, _)) = self.payment_details(&target.id()) {
                        self.event(format!(
                            "{sats} sats to {address}: wallet reports processing; still owed"
                        ));
                    }
                }
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
                            .payout_owner(id)
                            .ok_or(Error::NotFound)?
                            .pot
                            .payouts
                            .iter()
                            .any(|p| {
                                &p.id == id
                                    && (p.state == PayoutState::Created
                                        || (p.state == PayoutState::Failed
                                            && self.recovery(id).is_some_and(|r| {
                                                r.retry_due() && !r.retry_prepared
                                            })))
                            })
                        {
                            return Err(Error::Rejected("Stale payout preparation".into()));
                        }
                        self.saved
                            .used_payment_hashes
                            .insert(prepared.payment_hash.clone());
                        let owner = self.payout_owner_mut(id)?;
                        owner
                            .used_payment_hashes
                            .insert(prepared.payment_hash.clone());
                        owner.prepared.insert(id.clone(), prepared);
                        if let Some(members) = owner.payout_groups.get(id).cloned() {
                            let invoice = owner.prepared[id].clone();
                            for member in members.iter().filter(|member| *member != id) {
                                owner.prepared.insert(member.clone(), invoice.clone());
                            }
                        }
                        if owner
                            .pot
                            .payouts
                            .iter()
                            .any(|p| &p.id == id && p.state == PayoutState::Failed)
                        {
                            owner.recovery.entry(id.clone()).or_default().retry_prepared = true;
                        }
                    }
                    Target::Late(hash) => {
                        let entry = self
                            .saved
                            .retired_entries
                            .iter_mut()
                            .find(|e| &e.incoming_hash == hash)
                            .ok_or(Error::NotFound)?;
                        let retrying = entry.state == PayoutState::Failed;
                        if entry.state != PayoutState::Created && !retrying {
                            return Err(Error::Rejected("Stale entry return preparation".into()));
                        }
                        self.saved
                            .used_payment_hashes
                            .insert(prepared.payment_hash.clone());
                        entry.prepared = Some(prepared);
                        if retrying {
                            self.saved
                                .recovery
                                .entry(target.id())
                                .or_default()
                                .retry_prepared = true;
                        }
                    }
                }
            }
            (
                Operation::Submit { target, .. } | Operation::Lookup(target),
                Value::Payment(state),
            ) => self.apply_payment(target, state)?,
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
            && entry.received_sats.is_none()
            && !entry.state.complete()
            && let InvoiceState::Paid { amount_sats } = state
        {
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
        if let Target::Match(id) = target {
            let members = self
                .payout_owner(id)
                .and_then(|o| o.payout_groups.get(id))
                .cloned();
            if let Some(members) = members {
                match &state {
                    PaymentState::Pending => return Ok(()),
                    PaymentState::Failed { reason } => {
                        let owner = self.payout_owner_mut(id)?;
                        for member in &members {
                            let part = owner
                                .pot
                                .payouts
                                .iter_mut()
                                .find(|p| p.id == *member)
                                .ok_or(Error::NotFound)?;
                            if part.state == PayoutState::Paid {
                                return Err(Error::Rejected(
                                    "A paid transfer cannot fail later".into(),
                                ));
                            }
                            part.state = PayoutState::Failed;
                            let r = owner.recovery.entry(member.clone()).or_default();
                            r.missed = true;
                            r.manual_ready = false;
                        }
                        return self.record_failure(id, reason);
                    }
                    PaymentState::Succeeded { .. } => {
                        for member in members {
                            self.apply_payment_single(&Target::Match(member), state.clone())?;
                        }
                        return Ok(());
                    }
                }
            }
        }
        self.apply_payment_single(target, state)
    }
    fn apply_payment_single(&mut self, target: &Target, state: PaymentState) -> Result<()> {
        if matches!(state, PaymentState::Pending) {
            return Ok(());
        }
        let id = target.id();
        if let PaymentState::Failed { reason } = &state {
            match target {
                Target::Match(id) => {
                    let owner = self.payout_owner_mut(id)?;
                    let payout = owner
                        .pot
                        .payouts
                        .iter_mut()
                        .find(|p| &p.id == id)
                        .ok_or(Error::NotFound)?;
                    if payout.state == PayoutState::Paid {
                        return Ok(());
                    }
                    payout.state = PayoutState::Failed;
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
                    entry.state = PayoutState::Failed;
                }
            }
            return self.record_failure(&id, reason);
        }
        let at_ms = now_ms();
        let receipt = match target {
            Target::Match(id) => {
                let owner = self.payout_owner_mut(id)?;
                let index = owner
                    .pot
                    .payouts
                    .iter()
                    .position(|p| &p.id == id)
                    .ok_or(Error::NotFound)?;
                if owner.pot.payouts[index].state == PayoutState::Paid {
                    return Ok(());
                }
                // Reconciliation may prove an old locally-Failed attempt actually paid.
                if owner.pot.payouts[index].state == PayoutState::Failed {
                    owner.pot.payouts[index].state = PayoutState::InFlight;
                }
                owner.pot.confirm_payout(index)?;
                let payout = &owner.pot.payouts[index];
                let memo = payout_memo(&owner.pot, payout);
                let prepared = owner.prepared.get_mut(id).ok_or(Error::NotFound)?;
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
                if !matches!(entry.state, PayoutState::InFlight | PayoutState::Failed) {
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
        if self.recovery(&id).is_some() {
            let record = self.recovery_mut(&id)?;
            record.issue = None;
            record.retry_at_ms = None;
            record.missed = false;
            record.manual_ready = false;
        }
        if let Some(previous) = self
            .saved
            .pending_matches
            .iter_mut()
            .find(|saved| saved.pot.payouts.iter().any(|p| p.id == receipt.id))
        {
            previous.receipts.push(receipt.clone());
        }
        let message = receipt_message(&receipt, self.saved.simulated);
        self.saved.receipts.push(receipt);
        // Keep every receipt needed to audit this game, including long games.
        // Earlier games have their own durable archive/recovery snapshots.
        while self.saved.receipts.len() > 100 {
            let Some(index) = self.saved.receipts.iter().position(|receipt| {
                !self
                    .saved
                    .pot
                    .payouts
                    .iter()
                    .any(|payout| payout.id == receipt.id)
            }) else {
                break;
            };
            self.saved.receipts.remove(index);
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
                let queued = std::iter::once(&self.saved)
                    .chain(self.saved.pending_matches.iter())
                    .any(|saved| {
                        saved
                            .pot
                            .payouts
                            .iter()
                            .any(|p| p.state == PayoutState::Created)
                    })
                    || self.saved.retired_entries.iter().any(|entry| {
                        entry.received_sats.is_some_and(|sats| sats > 0)
                            && entry.state == PayoutState::Created
                    });
                if !queued {
                    return Ok(());
                }
                let delay = self
                    .io
                    .next_submit
                    .map(|at| at.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_millis(20));
                tokio::time::sleep(delay.max(Duration::from_millis(1))).await;
                continue;
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
    async fn discard_cancels_invoice_preparation_before_it_can_advance_to_submission() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.saved.pot.confirm_entry(0, 500).unwrap();
        demo.saved.pot.confirm_entry(1, 500).unwrap();
        demo.saved.pot.start().unwrap();
        demo.saved.pot.credit_at([0, 1000], now_ms()).unwrap();
        demo.saved.pot.flush_due(u64::MAX);
        let id = demo.saved.pot.payouts[0].id.clone();
        demo.io.spawn(
            Operation::Prepare(Target::Match(id)),
            std::future::pending::<anyhow::Result<Value>>(),
        );
        assert_eq!(demo.io.active.len(), 1);
        demo.command(Command::Action(14)).await.unwrap();
        assert!(demo.io.active.is_empty());
        demo.reconcile().await.unwrap();
        assert_eq!(demo.simulation.as_ref().unwrap().payment_attempts(), 0);
        assert_eq!(demo.saved.pot.discarded_sats(), 10);
        assert!(demo.saved.receipts.is_empty());
    }

    #[tokio::test]
    async fn lookup_error_is_retryable_and_never_credits_or_exposes_provider_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        let before = serde_json::to_value(&demo.saved).unwrap();
        let op = Operation::Incoming(demo.saved.hashes[0].clone());
        let error = demo
            .apply_result(
                &op,
                Err(anyhow::anyhow!(
                    "provider echoed nostr+walletconnect://private?secret=DO_NOT_DISCLOSE"
                )),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("check an entry payment") && error.contains("Retrying automatically")
        );
        assert!(!error.contains("DO_NOT_DISCLOSE") && !error.contains("walletconnect"));
        let error = demo
            .apply_result(&op, Err(IncomingReceiptError::HistoryPermission.into()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("list_transactions permission") && error.contains("Do not pay again")
        );
        assert_eq!(serde_json::to_value(&demo.saved).unwrap(), before);
        demo.apply_result(&op, Ok(Value::Incoming(InvoiceState::Pending)))
            .unwrap();
        assert!(!demo.saved.pot.paid[0]);
        assert!(demo.saved.receipts.is_empty());
    }
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
