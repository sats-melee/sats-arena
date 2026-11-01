use super::*;
use sats_arena_core::wallet::WalletIssue;

fn retry_limit() -> usize {
    3
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct FailedAttempt {
    pub invoice: PreparedPayout,
    pub at_ms: u64,
    pub reason: WalletIssue,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_arena_core::wallet::{DecodedInvoice, NewInvoice};
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct RecoveringWallet {
        inner: Arc<FakeWallet>,
        sends: AtomicUsize,
        replies: Mutex<VecDeque<std::result::Result<PaymentState, WalletIssue>>>,
        lookup_error: Mutex<Option<WalletIssue>>,
    }
    #[async_trait::async_trait]
    impl Wallet for RecoveringWallet {
        async fn create_invoice(
            &self,
            sats: u64,
            memo: &str,
            expiry: u64,
        ) -> anyhow::Result<NewInvoice> {
            self.inner.create_invoice(sats, memo, expiry).await
        }
        async fn invoice_state(&self, hash: &str) -> anyhow::Result<InvoiceState> {
            self.inner.invoice_state(hash).await
        }
        fn decode_invoice(&self, invoice: &str) -> anyhow::Result<DecodedInvoice> {
            self.inner.decode_invoice(invoice)
        }
        async fn payment_state(&self, hash: &str) -> anyhow::Result<PaymentState> {
            let error = *self.lookup_error.lock().unwrap();
            if let Some(error) = error {
                Err(error.into())
            } else {
                self.inner.payment_state(hash).await
            }
        }
        async fn pay_invoice(&self, invoice: &str, fees: u64) -> anyhow::Result<PaymentState> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let reply = self.replies.lock().unwrap().pop_front();
            match reply {
                Some(Ok(state)) => {
                    let hash = self.decode_invoice(invoice)?.payment_hash;
                    self.inner.set_payment_state(&hash, state.clone());
                    Ok(state)
                }
                Some(Err(error)) => Err(error.into()),
                None => self.inner.pay_invoice(invoice, fees).await,
            }
        }
    }
    async fn fixture(
        path: &Path,
        replies: Vec<std::result::Result<PaymentState, WalletIssue>>,
    ) -> (Demo, Arc<RecoveringWallet>, String) {
        let mut demo = Demo::open(path).await.unwrap();
        demo.command(Command::Addresses([
            "alice@example.test".into(),
            "bob@example.test".into(),
        ]))
        .await
        .unwrap();
        for action in [2, 3] {
            demo.command(Command::Action(action)).await.unwrap();
            demo.reconcile().await.unwrap();
        }
        demo.command(Command::Action(4)).await.unwrap();
        let wallet = Arc::new(RecoveringWallet {
            inner: demo.simulation.clone().unwrap(),
            sends: AtomicUsize::new(0),
            replies: Mutex::new(replies.into()),
            lookup_error: Mutex::new(None),
        });
        demo.wallet = wallet.clone();
        demo.io.set_submission_rate(20).unwrap();
        demo.command(Command::Credits(vec![Credit {
            sequence: 1,
            attacker: 0,
            victim: 1,
            damage_centi: 4000,
        }]))
        .await
        .unwrap();
        demo.saved.pot.flush_due(now_ms() + 5000);
        let id = demo.saved.pot.payouts[0].id.clone();
        (demo, wallet, id)
    }
    async fn drive(demo: &mut Demo, ready: impl Fn(&Demo) -> bool) {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while !ready(demo) {
            let _ = demo.tick().await;
            assert!(
                std::time::Instant::now() < until,
                "recovery did not progress"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    fn failed() -> std::result::Result<PaymentState, WalletIssue> {
        Ok(PaymentState::Failed {
            reason: "raw-private-provider-error-must-not-leak".into(),
        })
    }

    #[tokio::test]
    async fn replacement_connection_must_prove_an_original_settled_payment() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, _) = fixture(dir.path(), vec![]).await;
        assert!(demo.verify_recovery_connection().await.is_err());
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        demo.verify_recovery_connection().await.unwrap();
        *wallet.lookup_error.lock().unwrap() = Some(WalletIssue::NotFound);
        assert!(demo.verify_recovery_connection().await.is_err());
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn confirmed_failure_logs_address_and_retries_fresh_invoice_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(dir.path(), vec![failed()]).await;
        drive(&mut demo, |d| {
            d.saved.pot.payouts[0].state == PayoutState::Failed
        })
        .await;
        let original = demo.saved.prepared[&id].payment_hash.clone();
        let notice = demo.payment_notice().unwrap();
        assert!(notice.contains("40 sats to alice@example.test") && notice.contains("Still owed"));
        assert!(!notice.contains("raw-private"));
        assert_eq!(demo.saved.pot.pending()[0], 40);
        demo.recovery_mut(&id).unwrap().retry_at_ms = Some(0);
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 2);
        assert_ne!(demo.saved.prepared[&id].payment_hash, original);
        assert_eq!(
            demo.recovery(&id).unwrap().failed[0].invoice.payment_hash,
            original
        );
        assert_eq!(demo.saved.pot.pending()[0], 0);
        assert!(demo.payment_notice().is_none());
        let activity = serde_json::to_string(&demo.saved.activity).unwrap();
        assert!(
            activity.contains("FAILED")
                && activity.contains("alice@example.test")
                && activity.contains("received 40 sats")
        );
        assert!(!activity.contains("raw-private"));
        let timings = std::fs::read_to_string(dir.path().join("payment-timings.jsonl")).unwrap();
        let attempts: Vec<_> = timings
            .lines()
            .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
            .filter(|r| r["stage"] == "payment_dispatch")
            .map(|r| r["attempt"].as_u64().unwrap())
            .collect();
        assert_eq!(attempts, [1, 2]);
    }

    #[tokio::test]
    async fn retry_limit_survives_restart_and_manual_retry_does_not_erase_debt() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(dir.path(), vec![failed(), failed(), failed()]).await;
        for count in 1..=3 {
            drive(&mut demo, |d| {
                d.recovery(&id).is_some_and(|r| r.failed.len() == count)
            })
            .await;
            if count < 3 {
                demo.recovery_mut(&id).unwrap().retry_at_ms = Some(0);
            }
        }
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 3);
        assert!(demo.recovery(&id).unwrap().retry_at_ms.is_none());
        demo.save().unwrap();
        drop(demo);
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.wallet = wallet.clone();
        for _ in 0..10 {
            let _ = demo.tick().await;
            tokio::task::yield_now().await;
        }
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 3);
        assert_eq!(demo.saved.pot.pending()[0], 40);
        demo.command(Command::Action(15)).await.unwrap();
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 4);
        assert_eq!(demo.recovery(&id).unwrap().failed.len(), 3);
    }

    #[tokio::test]
    async fn unknown_payment_is_logged_once_and_manual_retry_only_rechecks_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(dir.path(), vec![Err(WalletIssue::Timeout)]).await;
        *wallet.lookup_error.lock().unwrap() = Some(WalletIssue::NotFound);
        drive(&mut demo, |d| {
            d.recovery(&id)
                .is_some_and(|r| r.issue == Some(WalletIssue::NotFound))
        })
        .await;
        let hash = demo.saved.prepared[&id].payment_hash.clone();
        let _ = demo.command(Command::Action(15)).await;
        for _ in 0..15 {
            demo.io.clear_retry(&id);
            let _ = demo.tick().await;
            tokio::task::yield_now().await;
        }
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 1);
        assert_eq!(demo.saved.prepared[&id].payment_hash, hash);
        assert_eq!(demo.saved.pot.pending()[0], 40);
        assert_eq!(
            demo.saved
                .activity
                .iter()
                .filter(|a| a.message.contains("cannot find the original"))
                .count(),
            1
        );
        assert!(demo.recovery(&id).unwrap().failed.is_empty());
        demo.save().unwrap();
        drop(demo);
        let demo = Demo::open(dir.path()).await.unwrap();
        assert!(
            demo.payment_notice()
                .unwrap()
                .contains("alice@example.test")
        );
        assert_eq!(demo.saved.pot.pending()[0], 40);
    }

    #[tokio::test]
    async fn wallet_limit_rejection_stays_visible_until_user_requests_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(
            dir.path(),
            vec![Ok(PaymentState::Failed {
                reason: WalletIssue::QuotaExceeded.to_string(),
            })],
        )
        .await;
        drive(&mut demo, |d| {
            d.saved.pot.payouts[0].state == PayoutState::Failed
        })
        .await;
        assert!(demo.recovery(&id).unwrap().retry_at_ms.is_none());
        assert!(demo.payment_notice().unwrap().contains("spending limit"));
        assert!(
            demo.payment_notice()
                .unwrap()
                .contains("alice@example.test")
        );
        demo.command(Command::Action(15)).await.unwrap();
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn late_entry_refund_retains_failure_history_across_the_next_lobby() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        let wallet = Arc::new(RecoveringWallet {
            inner: demo.simulation.clone().unwrap(),
            sends: AtomicUsize::new(0),
            replies: Mutex::new(vec![failed()].into()),
            lookup_error: Mutex::new(None),
        });
        demo.wallet = wallet.clone();
        demo.io.set_submission_rate(20).unwrap();
        demo.saved.retired_entries.push(RetiredEntry {
            incoming_hash: "old-entry".into(),
            player: 0,
            address: "alice@example.test".into(),
            expected_sats: 7,
            received_sats: Some(7),
            prepared: None,
            state: PayoutState::Created,
        });
        let id = "late-entry-old-entry";
        drive(&mut demo, |d| {
            d.recovery(id).is_some_and(|r| r.failed.len() == 1)
        })
        .await;
        let original = demo.recovery(id).unwrap().failed[0]
            .invoice
            .payment_hash
            .clone();
        demo.next_match(None).await.unwrap();
        assert_eq!(
            demo.recovery(id).unwrap().failed[0].invoice.payment_hash,
            original
        );
        demo.recovery_mut(id).unwrap().retry_at_ms = Some(0);
        drive(&mut demo, |d| {
            d.saved
                .retired_entries
                .iter()
                .any(|e| e.incoming_hash == "old-entry" && e.state == PayoutState::Paid)
        })
        .await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 2);
        assert_eq!(
            demo.saved
                .receipts
                .iter()
                .filter(|r| r.id == id)
                .map(|r| r.sats)
                .sum::<u64>(),
            7
        );
    }

    #[tokio::test]
    async fn legacy_failed_payment_that_wallet_confirms_paid_is_not_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(dir.path(), vec![Err(WalletIssue::Timeout)]).await;
        drive(&mut demo, |_| wallet.sends.load(Ordering::SeqCst) == 1).await;
        for _ in 0..10 {
            let _ = demo.tick().await;
            tokio::task::yield_now().await;
        }
        demo.saved.pot.payouts[0].state = PayoutState::Failed;
        demo.saved.recovery.clear();
        let hash = demo.saved.prepared[&id].payment_hash.clone();
        wallet
            .inner
            .set_payment_state(&hash, PaymentState::Succeeded { fee_sats: 0 });
        demo.io.clear_retry(&id);
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 1);
        assert_eq!(demo.saved.prepared[&id].payment_hash, hash);
    }

    #[tokio::test]
    async fn legacy_failed_label_requires_wallet_confirmation_before_a_new_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut demo, wallet, id) = fixture(dir.path(), vec![Err(WalletIssue::Timeout)]).await;
        drive(&mut demo, |_| wallet.sends.load(Ordering::SeqCst) == 1).await;
        for _ in 0..10 {
            let _ = demo.tick().await;
            tokio::task::yield_now().await;
        }
        demo.saved.pot.payouts[0].state = PayoutState::Failed;
        demo.saved.recovery.clear();
        *wallet.lookup_error.lock().unwrap() = Some(WalletIssue::Expired);
        demo.io.clear_retry(&id);
        drive(&mut demo, |d| {
            d.recovery(&id)
                .is_some_and(|r| r.issue == Some(WalletIssue::Expired))
        })
        .await;
        assert!(demo.recovery(&id).unwrap().failed.is_empty());
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 1);
        let hash = demo.saved.prepared[&id].payment_hash.clone();
        wallet.inner.set_payment_state(
            &hash,
            PaymentState::Failed {
                reason: "confirmed".into(),
            },
        );
        *wallet.lookup_error.lock().unwrap() = None;
        demo.io.clear_retry(&id);
        drive(&mut demo, |d| {
            d.recovery(&id).is_some_and(|r| r.failed.len() == 1)
        })
        .await;
        demo.recovery_mut(&id).unwrap().retry_at_ms = Some(0);
        drive(&mut demo, |d| d.saved.pot.earned[0] == 40).await;
        assert_eq!(wallet.sends.load(Ordering::SeqCst), 2);
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Recovery {
    #[serde(default)]
    pub missed: bool,
    #[serde(default)]
    pub submitted_at_ms: Option<u64>,
    #[serde(default)]
    pub manual_ready: bool,
    #[serde(default)]
    pub failed: Vec<FailedAttempt>,
    #[serde(default)]
    pub issue: Option<WalletIssue>,
    #[serde(default)]
    pub retry_at_ms: Option<u64>,
    #[serde(default)]
    pub retry_prepared: bool,
    #[serde(default = "retry_limit")]
    pub limit: usize,
}
impl Default for Recovery {
    fn default() -> Self {
        Self {
            missed: false,
            submitted_at_ms: None,
            manual_ready: false,
            failed: vec![],
            issue: None,
            retry_at_ms: None,
            retry_prepared: false,
            limit: retry_limit(),
        }
    }
}
impl Recovery {
    pub fn confirmed_failed(&self, prepared: &PreparedPayout) -> bool {
        self.failed
            .last()
            .is_some_and(|attempt| attempt.invoice.payment_hash == prepared.payment_hash)
    }
    pub fn retry_due(&self) -> bool {
        self.failed.len() < self.limit && self.retry_at_ms.is_some_and(|at| now_ms() >= at)
    }
    pub fn description(&self) -> String {
        let reason = self
            .issue
            .map(|issue| issue.message())
            .unwrap_or(if self.failed.is_empty() {
                "Wallet reports processing"
            } else {
                "Retry in progress"
            });
        if self.missed && !self.manual_ready {
            format!("{reason}. Still owed; on the missed-payments list")
        } else if let Some(at) = self.retry_at_ms {
            format!(
                "{reason}. Still owed; retry {} of {} in {}s",
                self.failed.len() + 1,
                self.limit,
                at.saturating_sub(now_ms()).div_ceil(1000)
            )
        } else if self.failed.len() >= self.limit {
            format!("{reason}. Still owed; automatic retry limit reached")
        } else {
            format!("{reason}. Still owed")
        }
    }
}

impl Demo {
    pub(super) async fn verify_recovery_connection(&self) -> Result<()> {
        let prepared = std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .find_map(|owner| {
                owner.pot.payouts.iter().find_map(|p| {
                    (p.state == PayoutState::Paid)
                        .then(|| owner.prepared.get(&p.id))
                        .flatten()
                })
            })
            .ok_or_else(|| {
                Error::Rejected(
                    "No original settled payment to verify this replacement connection".into(),
                )
            })?;
        if !matches!(
            self.wallet.payment_state(&prepared.payment_hash).await,
            Ok(PaymentState::Succeeded { .. })
        ) {
            return Err(Error::Rejected("Replacement connection could not prove an original settled payment; original payments retained".into()));
        }
        Ok(())
    }
    pub(super) fn recovery(&self, id: &str) -> Option<&Recovery> {
        if id.starts_with("late-entry-") {
            self.saved.recovery.get(id)
        } else {
            self.payout_owner(id)?.recovery.get(id)
        }
    }
    pub(super) fn recovery_mut(&mut self, id: &str) -> Result<&mut Recovery> {
        let owner = if id.starts_with("late-entry-") {
            &mut self.saved
        } else {
            self.payout_owner_mut(id)?
        };
        Ok(owner.recovery.entry(id.into()).or_default())
    }
    pub(super) fn payment_details(
        &self,
        id: &str,
    ) -> Option<(u64, String, Option<PreparedPayout>)> {
        if let Some(hash) = id.strip_prefix("late-entry-") {
            let e = self
                .saved
                .retired_entries
                .iter()
                .find(|e| e.incoming_hash == hash)?;
            Some((e.received_sats?, e.address.clone(), e.prepared.clone()))
        } else {
            let owner = self.payout_owner(id)?;
            let p = owner.pot.payouts.iter().find(|p| p.id == id)?;
            let invoice = owner.prepared.get(id).cloned();
            let address = invoice
                .as_ref()
                .map(|p| p.address.clone())
                .unwrap_or_else(|| owner.addresses[p.player].clone());
            Some((owner.group_sats(id).unwrap_or(p.sats), address, invoice))
        }
    }
    pub(super) fn record_issue(&mut self, id: &str, error: &anyhow::Error) -> Result<()> {
        let issue = error
            .downcast_ref::<WalletIssue>()
            .copied()
            .unwrap_or(WalletIssue::Unavailable);
        if self.recovery(id).and_then(|r| r.issue) == Some(issue) {
            return Ok(());
        }
        let (sats, address, _) = self.payment_details(id).ok_or(Error::NotFound)?;
        self.recovery_mut(id)?.issue = Some(issue);
        self.event(format!(
            "{sats} sats to {address}: {issue}. Still owed; original payment retained"
        ));
        self.save()
    }
    pub(super) fn record_failure(&mut self, id: &str, reason: &str) -> Result<()> {
        let (sats, address, invoice) = self.payment_details(id).ok_or(Error::NotFound)?;
        let invoice = invoice.ok_or(Error::NotFound)?;
        let issue = WalletIssue::failure_reason(reason);
        let recovery = self.recovery_mut(id)?;
        if recovery.confirmed_failed(&invoice) {
            return Ok(());
        }
        recovery.failed.push(FailedAttempt {
            invoice,
            at_ms: now_ms(),
            reason: issue,
        });
        recovery.issue = Some(issue);
        recovery.retry_prepared = false;
        recovery.retry_at_ms =
            (!recovery.missed && issue.automatic_retry() && recovery.failed.len() < recovery.limit)
                .then(|| {
                    now_ms()
                        + if recovery.failed.len() == 1 {
                            5_000
                        } else {
                            15_000
                        }
                });
        let description = recovery.description();
        self.event(format!("{sats} sats to {address} FAILED: {description}"));
        self.save()
    }
    pub(super) fn retry_failed(&mut self) -> Result<()> {
        let mut ids: Vec<_> = std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .flat_map(|owner| {
                owner
                    .pot
                    .payouts
                    .iter()
                    .filter(|p| matches!(p.state, PayoutState::Failed | PayoutState::InFlight))
                    .map(|p| p.id.clone())
            })
            .collect();
        ids.extend(
            self.saved
                .retired_entries
                .iter()
                .filter(|e| matches!(e.state, PayoutState::Failed | PayoutState::InFlight))
                .map(|e| format!("late-entry-{}", e.incoming_hash)),
        );
        let mut scheduled = 0;
        for id in ids {
            if self.recovery(&id).is_some_and(|r| r.missed) {
                self.io.clear_retry(&id);
                continue;
            }
            let Some((sats, address, Some(invoice))) = self.payment_details(&id) else {
                continue;
            };
            // A legacy Failed label is not proof: query its original hash first.
            let failed = if let Some(hash) = id.strip_prefix("late-entry-") {
                self.saved
                    .retired_entries
                    .iter()
                    .any(|e| e.incoming_hash == hash && e.state == PayoutState::Failed)
            } else {
                self.payout_owner(&id).is_some_and(|s| {
                    s.pot
                        .payouts
                        .iter()
                        .any(|p| p.id == id && p.state == PayoutState::Failed)
                })
            };
            let proven = failed
                && self
                    .recovery(&id)
                    .is_some_and(|r| r.confirmed_failed(&invoice) || r.retry_prepared);
            if proven {
                let recovery = self.recovery_mut(&id)?;
                recovery.limit = recovery.limit.max(recovery.failed.len() + 1);
                recovery.retry_at_ms = Some(now_ms());
                self.event(format!(
                    "Retry requested: {sats} sats still owed to {address}"
                ));
                scheduled += 1;
            }
            self.io.clear_retry(&id);
        }
        self.save()?;
        if scheduled == 0 {
            self.event("Checking original payments; unknown payments remain owed and will not be sent again".into());
            self.save()?;
        }
        Ok(())
    }
    pub(super) fn payment_notice(&self) -> Option<String> {
        let mut rows = Vec::new();
        for owner in std::iter::once(&self.saved).chain(self.saved.pending_matches.iter()) {
            for p in &owner.pot.payouts {
                if p.state.complete() {
                    continue;
                }
                if let Some(r) = owner
                    .recovery
                    .get(&p.id)
                    .filter(|r| r.missed || r.issue.is_some() || !r.failed.is_empty())
                {
                    let address = owner
                        .prepared
                        .get(&p.id)
                        .map(|p| p.address.as_str())
                        .unwrap_or(&owner.addresses[p.player]);
                    rows.push((
                        p.state != PayoutState::Failed,
                        format!("{} sats to {}: {}", p.sats, address, r.description()),
                    ));
                }
            }
        }
        for entry in &self.saved.retired_entries {
            if entry.state.complete() {
                continue;
            }
            if let (Some(sats), Some(r)) = (
                entry.received_sats,
                self.saved
                    .recovery
                    .get(&format!("late-entry-{}", entry.incoming_hash)),
            ) {
                if r.missed || r.issue.is_some() || !r.failed.is_empty() {
                    rows.push((
                        entry.state != PayoutState::Failed,
                        format!("{sats} sats to {}: {}", entry.address, r.description()),
                    ));
                }
            }
        }
        rows.sort_by_key(|(not_failed, _)| *not_failed);
        rows.into_iter().next().map(|(_, text)| text)
    }
}
