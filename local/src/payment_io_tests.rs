use super::*;
use sats_arena_core::wallet::{DecodedInvoice, NewInvoice};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct HeldWallet {
    inner: Arc<FakeWallet>,
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    gate: Semaphore,
    hold_all: bool,
}
#[async_trait::async_trait]
impl Wallet for HeldWallet {
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
        self.inner.payment_state(hash).await
    }
    async fn pay_invoice(&self, invoice: &str, fees: u64) -> anyhow::Result<PaymentState> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        if self.hold_all || call == 0 {
            self.gate.acquire().await.unwrap().forget();
        }
        let result = self.inner.pay_invoice(invoice, fees).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}
async fn playing(path: &Path, hold_all: bool) -> (Demo, Arc<HeldWallet>) {
    let mut demo = Demo::open(path).await.unwrap();
    demo.command(Command::Addresses([
        "alice@example.test".into(),
        "bob@example.test".into(),
    ]))
    .await
    .unwrap();
    demo.command(Command::Action(2)).await.unwrap();
    demo.command(Command::Action(3)).await.unwrap();
    demo.reconcile().await.unwrap();
    demo.command(Command::Action(4)).await.unwrap();
    let wallet = Arc::new(HeldWallet {
        inner: demo.simulation.clone().unwrap(),
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        gate: Semaphore::new(0),
        hold_all,
    });
    demo.wallet = wallet.clone();
    (demo, wallet)
}
async fn drive(demo: &mut Demo, ready: impl Fn(&Demo) -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while !ready(demo) {
        demo.tick().await.unwrap();
        assert!(
            std::time::Instant::now() < until,
            "network job did not progress"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
fn hits(demo: &mut Demo, sequence: u64) {
    demo.saved
        .pot
        .credit_batch_at(
            &[
                Credit {
                    sequence,
                    attacker: 0,
                    victim: 1,
                    damage_centi: 1000,
                },
                Credit {
                    sequence: sequence + 1,
                    attacker: 1,
                    victim: 0,
                    damage_centi: 1000,
                },
            ],
            now_ms(),
        )
        .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5_000);
    demo.save().unwrap();
}
#[tokio::test]
async fn slow_payment_does_not_hold_another_recipient_or_damage_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), false).await;
    hits(&mut demo, 1);
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 10).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 2);
    assert_eq!(wallet.active.load(Ordering::SeqCst), 1);
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 2);
    let started = std::time::Instant::now();
    demo.command(Command::Credits(vec![Credit {
        sequence: 3,
        attacker: 0,
        victim: 1,
        damage_centi: 500,
    }]))
    .await
    .unwrap();
    demo.tick().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(demo.view().unwrap().last_credit_sequence, 3);
    assert_eq!(demo.saved.pot.remaining, [490, 485, 0, 0]);
    wallet.gate.add_permits(1);
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 20).await;
    demo.command(Command::Action(6)).await.unwrap();
    drive(&mut demo, |d| d.saved.pot.phase == Phase::Settled).await;
    assert_eq!(demo.saved.pot.earned.iter().sum::<u64>(), 1000);
    let log = std::fs::read_to_string(dir.path().join("payment-timings.jsonl")).unwrap();
    assert!(
        log.contains("recipient_invoice")
            && log.contains("payment_submit")
            && log.contains("payout_complete")
    );
    assert!(!log.contains("example.test") && !log.contains("lnfake:"));
}
#[tokio::test]
async fn concurrent_submissions_are_bounded_and_persisted_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    for sequence in [1, 3, 5] {
        hits(&mut demo, sequence);
    }
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 4).await;
    for _ in 0..10 {
        demo.tick().await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 4);
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 4);
    let saved: Saved =
        serde_json::from_slice(&std::fs::read(dir.path().join("prepaid-match.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved
            .pot
            .payouts
            .iter()
            .filter(|p| p.state == PayoutState::InFlight)
            .count(),
        4
    );
    for payout in saved
        .pot
        .payouts
        .iter()
        .filter(|p| p.state == PayoutState::InFlight)
    {
        assert!(
            saved
                .used_payment_hashes
                .contains(&saved.prepared[&payout.id].payment_hash)
        );
    }
    wallet.gate.add_permits(6);
    drive(&mut demo, |d| {
        d.saved
            .pot
            .payouts
            .iter()
            .all(|p| p.state == PayoutState::Paid)
    })
    .await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 6);
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 4);
}
#[tokio::test]
async fn actual_coordinator_acknowledges_credit_while_wallet_request_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    let (sender, receiver) = mpsc::sync_channel(16);
    let shared = Arc::new(Mutex::new(LocalView::default()));
    let task = tokio::spawn(run_worker(demo, receiver, shared.clone()));
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while wallet.calls.load(Ordering::SeqCst) != 2 {
        assert!(std::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let handle = LocalHandle {
        sender,
        shared: shared.clone(),
        worker: None,
    };
    let credit = LocalCredit {
        sequence: 3,
        attacker: 0,
        victim: 1,
        damage_centi: 500,
    };
    let started = std::time::Instant::now();
    assert!(unsafe { arena_local_credits(&handle, &credit, 1) });
    loop {
        let view = *shared.lock().unwrap();
        if view.busy == 0 && view.last_credit_sequence == 3 {
            assert_eq!(view.error[0], 0);
            assert_eq!(view.pending_sats, [15, 10, 0, 0]);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "credit blocked behind network I/O"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(wallet.active.load(Ordering::SeqCst), 2);
    drop(handle);
    task.await.unwrap();
}

#[tokio::test]
async fn stopping_with_network_jobs_in_flight_recovers_by_lookup_only() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 2).await;
    drop(demo); // Both durable InFlight jobs are cancelled with unknown outcomes.
    tokio::task::yield_now().await;
    let mut recovered = Demo::open(dir.path()).await.unwrap();
    recovered.reconcile().await.unwrap();
    assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 0);
    assert!(
        recovered
            .saved
            .pot
            .payouts
            .iter()
            .all(|p| p.state == PayoutState::InFlight)
    );
    assert_eq!(recovered.saved.pot.pending(), [10, 10, 0, 0]);
}

struct ReusedInvoice;
#[async_trait::async_trait]
impl AddressInvoices for ReusedInvoice {
    async fn invoice(&self, _: &LightningAddress, sats: u64, _: &str) -> anyhow::Result<String> {
        Ok(format!("lnfake:{sats}:same-invoice"))
    }
}
#[tokio::test]
async fn concurrent_reused_hash_is_paid_once_and_network_error_does_not_reject_credits() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), false).await;
    demo.provider = Arc::new(ReusedInvoice);
    hits(&mut demo, 1);
    wallet.gate.add_permits(1);
    let (sender, receiver) = mpsc::sync_channel(16);
    let shared = Arc::new(Mutex::new(LocalView::default()));
    let task = tokio::spawn(run_worker(demo, receiver, shared.clone()));
    let until = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let view = *shared.lock().unwrap();
        if view.payment_error[0] != 0 && view.earned_sats.iter().sum::<u64>() == 10 {
            break;
        }
        assert!(std::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let handle = LocalHandle {
        sender,
        shared: shared.clone(),
        worker: None,
    };
    let credit = LocalCredit {
        sequence: 3,
        attacker: 0,
        victim: 1,
        damage_centi: 500,
    };
    assert!(unsafe { arena_local_credits(&handle, &credit, 1) });
    loop {
        let view = *shared.lock().unwrap();
        if view.busy == 0 && view.last_credit_sequence == 3 {
            assert_eq!(view.error[0], 0);
            assert_ne!(view.payment_error[0], 0);
            break;
        }
        assert!(std::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    drop(handle);
    task.await.unwrap();
}

#[tokio::test]
async fn old_invoice_monitoring_has_a_global_budget_as_history_grows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, _) = playing(dir.path(), false).await;
    let mut hashes = BTreeSet::new();
    for _ in 0..20 {
        let invoice = demo
            .wallet
            .create_invoice(500, "old unpaid", 600)
            .await
            .unwrap();
        hashes.insert(format!("incoming-{}", invoice.payment_hash));
        demo.saved.retired_entries.push(RetiredEntry {
            incoming_hash: invoice.payment_hash,
            player: 0,
            address: "alice@example.test".into(),
            expected_sats: 500,
            received_sats: None,
            prepared: None,
            state: PayoutState::Created,
        });
    }
    for _ in 0..20 {
        demo.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let log = std::fs::read_to_string(dir.path().join("payment-timings.jsonl")).unwrap();
    let count = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|r| r["stage"] == "incoming_lookup" && hashes.contains(r["id"].as_str().unwrap()))
        .count();
    assert_eq!(count, 4);
}
