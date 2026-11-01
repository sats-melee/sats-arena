use super::*;
use sats_arena_core::wallet::{DecodedInvoice, NewInvoice};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct HeldVerifier {
    gate: Semaphore,
}
#[async_trait::async_trait]
impl AddressVerifier for HeldVerifier {
    async fn verify(&self, address: &LightningAddress) -> anyhow::Result<AddressVerification> {
        if address.as_str() == "nic@lawallet.io" {
            self.gate.acquire().await.unwrap().forget();
        }
        if address.as_str() == "unavailable@example.test" {
            anyhow::bail!("Receiving service is unavailable; retry this player");
        }
        TestVerifier.verify(address).await
    }
}

#[tokio::test]
async fn slow_address_check_does_not_hold_other_players_or_the_coordinator() {
    let dir = tempfile::tempdir().unwrap();
    let mut demo = Demo::open(dir.path()).await.unwrap();
    let verifier = Arc::new(HeldVerifier {
        gate: Semaphore::new(0),
    });
    demo.verifier = verifier.clone();
    let started = std::time::Instant::now();
    demo.command(Command::Address(0, "nic@lawallet.io".into()))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(demo.view().unwrap().verification_state[0], 1);
    demo.command(Command::Address(1, "agustin@lacrypta.ar".into()))
        .await
        .unwrap();
    drive(&mut demo, |d| d.view().unwrap().verification_state[1] == 2).await;
    assert_eq!(demo.view().unwrap().verification_state[0], 1);
    assert_eq!(demo.view().unwrap().busy, 0);

    // Replacing an in-flight input cancels its result without changing its neighbor.
    demo.command(Command::Address(0, "unavailable@example.test".into()))
        .await
        .unwrap();
    drive(&mut demo, |d| d.view().unwrap().verification_state[0] == 3).await;
    assert_eq!(demo.view().unwrap().verification_state[1], 2);
    assert_eq!(demo.saved.addresses[0], "unavailable@example.test");
    verifier.gate.add_permits(1);
    demo.tick().await.unwrap();
    assert_eq!(demo.saved.addresses[0], "unavailable@example.test");
    assert_eq!(demo.view().unwrap().verification_state[0], 3);
    demo.command(Command::Address(0, "recovered@example.test".into()))
        .await
        .unwrap();
    drive(&mut demo, |d| d.view().unwrap().verification_state[0] == 2).await;
}

#[tokio::test(start_paused = true)]
async fn address_timeout_ends_checking_and_keeps_successful_neighbors() {
    let dir = tempfile::tempdir().unwrap();
    let mut demo = Demo::open(dir.path()).await.unwrap();
    demo.verifier = Arc::new(HeldVerifier {
        gate: Semaphore::new(0),
    });
    demo.command(Command::Address(0, "nic@lawallet.io".into()))
        .await
        .unwrap();
    demo.command(Command::Address(1, "agustin@lacrypta.ar".into()))
        .await
        .unwrap();
    assert_eq!(demo.view().unwrap().verification_state[..2], [1, 2]);
    tokio::time::advance(Duration::from_secs(16)).await;
    tokio::task::yield_now().await;
    demo.tick().await.unwrap();
    assert_eq!(demo.view().unwrap().verification_state[..2], [3, 2]);
    assert!(demo.saved.verification[0].error.contains("15 seconds"));
    assert_eq!(demo.view().unwrap().busy, 0);
}

struct HeldWallet {
    inner: Arc<FakeWallet>,
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    gate: Semaphore,
    hold_all: bool,
    lose_response: AtomicBool,
    lookup_calls: AtomicUsize,
    lookup_replies: Mutex<std::collections::VecDeque<anyhow::Result<PaymentState>>>,
}
impl HeldWallet {
    fn new(inner: Arc<FakeWallet>, hold_all: bool) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            gate: Semaphore::new(0),
            hold_all,
            lose_response: AtomicBool::new(false),
            lookup_calls: AtomicUsize::new(0),
            lookup_replies: Mutex::new(Default::default()),
        }
    }
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
        self.lookup_calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.lookup_replies.lock().unwrap().pop_front();
        match reply {
            Some(reply) => reply,
            None => self.inner.payment_state(hash).await,
        }
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
        if self.lose_response.load(Ordering::SeqCst) {
            Ok(PaymentState::Pending)
        } else {
            result
        }
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
    let wallet = Arc::new(HeldWallet::new(demo.simulation.clone().unwrap(), hold_all));
    demo.wallet = wallet.clone();
    (demo, wallet)
}

async fn four_playing(path: &Path, addresses: [&str; 4]) -> (Demo, Arc<HeldWallet>) {
    let mut demo = Demo::open(path).await.unwrap();
    demo.command(Command::Roster(Roster {
        teams: false,
        kinds: [0; 4],
        team_ids: [255; 4],
        characters: [0, 2, 4, 6],
    }))
    .await
    .unwrap();
    for (player, address) in addresses.into_iter().enumerate() {
        demo.command(Command::Address(player, address.into()))
            .await
            .unwrap();
    }
    for action in [2, 3, 9, 10] {
        demo.command(Command::Action(action)).await.unwrap();
        demo.reconcile().await.unwrap();
    }
    demo.command(Command::Action(4)).await.unwrap();
    let wallet = Arc::new(HeldWallet::new(demo.simulation.clone().unwrap(), true));
    demo.wallet = wallet.clone();
    demo.command(Command::Credits(
        (0..4)
            .map(|attacker| Credit {
                sequence: attacker as u64 + 1,
                attacker,
                victim: (attacker + 1) % 4,
                damage_centi: 1000,
            })
            .collect(),
    ))
    .await
    .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5_000);
    (demo, wallet)
}

#[tokio::test]
async fn four_recipients_can_submit_without_waiting_for_two_slow_sends() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = four_playing(
        dir.path(),
        [
            "p1@example.test",
            "p2@example.test",
            "p3@example.test",
            "p4@example.test",
        ],
    )
    .await;
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 4).await;
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 4);
    wallet.gate.add_permits(4);
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 40).await;
}

#[tokio::test]
async fn a_serial_wallet_queues_all_four_recipients_without_starting_unsent_payments() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = four_playing(
        dir.path(),
        [
            "p1@example.test",
            "p2@example.test",
            "p3@example.test",
            "p4@example.test",
        ],
    )
    .await;
    demo.io.serialize_submissions = true;
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 1).await;
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 1);
    assert_eq!(
        demo.saved
            .pot
            .payouts
            .iter()
            .filter(|p| p.state == PayoutState::InFlight)
            .count(),
        1
    );
    for expected in 2..=4 {
        wallet.gate.add_permits(1);
        drive(&mut demo, |_| {
            wallet.calls.load(Ordering::SeqCst) == expected
        })
        .await;
        assert_eq!(wallet.peak.load(Ordering::SeqCst), 1);
    }
    wallet.gate.add_permits(1);
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 40).await;
}

#[tokio::test]
async fn shared_address_uses_one_send_slot_and_leaves_other_recipients_free() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = four_playing(
        dir.path(),
        [
            "shared@example.test",
            "shared@example.test",
            "p3@example.test",
            "p4@example.test",
        ],
    )
    .await;
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 3).await;
    for _ in 0..10 {
        demo.tick().await.unwrap();
        tokio::task::yield_now().await;
    }
    let sending: Vec<_> = demo
        .saved
        .pot
        .payouts
        .iter()
        .filter(|p| p.state == PayoutState::InFlight)
        .map(|p| demo.saved.prepared[&p.id].address.as_str())
        .collect();
    assert_eq!(
        sending
            .iter()
            .filter(|&&a| a == "shared@example.test")
            .count(),
        1
    );
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 3);
    wallet.gate.add_permits(4);
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 40).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 4);
}

async fn lost_response(
    path: &Path,
    replies: Vec<anyhow::Result<PaymentState>>,
) -> (Demo, Arc<HeldWallet>) {
    let (mut demo, wallet) = playing(path, false).await;
    wallet.lose_response.store(true, Ordering::SeqCst);
    wallet.lookup_replies.lock().unwrap().extend(replies);
    wallet.gate.add_permits(1);
    demo.command(Command::Credits(vec![Credit {
        sequence: 1,
        attacker: 0,
        victim: 1,
        damage_centi: 1000,
    }]))
    .await
    .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5_000);
    for _ in 0..20 {
        let _ = demo.tick().await;
        tokio::task::yield_now().await;
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        wallet.lookup_calls.load(Ordering::SeqCst),
        1,
        "unknown send must be looked up immediately"
    );
    (demo, wallet)
}

async fn advance_io(demo: &mut Demo, seconds: u64) {
    tokio::time::advance(Duration::from_secs(seconds)).await;
    for _ in 0..20 {
        let _ = demo.tick().await;
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn processing_payments_are_confirmed_promptly_without_resending() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = lost_response(
        dir.path(),
        vec![
            Ok(PaymentState::Pending),
            Ok(PaymentState::Pending),
            Ok(PaymentState::Pending),
        ],
    )
    .await;
    let original = demo
        .saved
        .prepared
        .values()
        .next()
        .unwrap()
        .payment_hash
        .clone();
    for expected in 2..=4 {
        advance_io(&mut demo, 2).await;
        assert_eq!(wallet.lookup_calls.load(Ordering::SeqCst), expected);
    }
    assert_eq!(demo.saved.pot.earned[0], 10);
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        demo.saved.prepared.values().next().unwrap().payment_hash,
        original
    );
}

#[tokio::test(start_paused = true)]
async fn processing_recipient_batches_new_damage_until_its_payment_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = lost_response(dir.path(), vec![Ok(PaymentState::Pending)]).await;
    wallet.lose_response.store(false, Ordering::SeqCst);
    demo.command(Command::Credits(vec![Credit {
        sequence: 2,
        attacker: 0,
        victim: 1,
        damage_centi: 1000,
    }]))
    .await
    .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5_000);
    advance_io(&mut demo, 1).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    assert_eq!(demo.saved.pot.pending()[0], 20);
    advance_io(&mut demo, 1).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 2);
    assert_eq!(demo.saved.pot.earned[0], 20);
    assert_eq!(demo.saved.pot.pending()[0], 0);
}

#[tokio::test]
async fn an_unknown_damage_payment_does_not_hold_separate_final_refunds() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = lost_response(
        dir.path(),
        (0..20).map(|_| Ok(PaymentState::Pending)).collect(),
    )
    .await;
    let original = demo.saved.prepared.values().next().unwrap().clone();
    let original_id = demo.saved.pot.payouts[0].id.clone();
    wallet.lose_response.store(false, Ordering::SeqCst);
    demo.command(Command::Action(8)).await.unwrap();
    drive(&mut demo, |d| d.saved.pot.earned.iter().sum::<u64>() == 990).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 3);
    assert_eq!(demo.saved.pot.pending().iter().sum::<u64>(), 10);
    assert_eq!(
        demo.saved.prepared[&original_id].payment_hash,
        original.payment_hash
    );
    assert_eq!(
        demo.saved
            .pot
            .payouts
            .iter()
            .find(|p| p.id == original_id)
            .unwrap()
            .state,
        PayoutState::InFlight
    );
}

#[tokio::test]
async fn previous_uncertain_payment_does_not_block_new_match_to_the_same_address() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 2).await;
    demo.command(Command::Action(8)).await.unwrap();
    demo.command(Command::Action(1)).await.unwrap();
    let original_addresses = demo.saved.addresses.clone();
    drop(demo);
    tokio::task::yield_now().await;

    let mut recovered = Demo::open(dir.path()).await.unwrap();
    for (player, action) in [2, 3].into_iter().enumerate() {
        recovered.command(Command::Action(action)).await.unwrap();
        drive(&mut recovered, |d| d.saved.pot.paid[player]).await;
    }
    assert_eq!(recovered.saved.addresses, original_addresses);
    recovered.command(Command::Action(4)).await.unwrap();
    hits(&mut recovered, 1);
    recovered.command(Command::Action(8)).await.unwrap();
    drive(&mut recovered, |d| d.saved.pot.phase == Phase::Settled).await;
    assert_eq!(recovered.saved.pot.phase, Phase::Settled);
    assert_eq!(recovered.saved.pot.earned.iter().sum::<u64>(), 1000);
    assert_eq!(recovered.saved.pending_matches.len(), 1);
    assert!(
        recovered.saved.pending_matches[0]
            .pot
            .payouts
            .iter()
            .any(|p| p.state == PayoutState::InFlight)
    );
}

#[tokio::test(start_paused = true)]
async fn unavailable_lookup_backs_off_but_processing_reply_restores_prompt_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = lost_response(
        dir.path(),
        vec![
            Err(anyhow::anyhow!("lookup unavailable")),
            Err(anyhow::anyhow!("lookup unavailable")),
            Ok(PaymentState::Pending),
        ],
    )
    .await;
    advance_io(&mut demo, 2).await;
    assert_eq!(wallet.lookup_calls.load(Ordering::SeqCst), 2);
    advance_io(&mut demo, 2).await;
    assert_eq!(wallet.lookup_calls.load(Ordering::SeqCst), 2);
    advance_io(&mut demo, 2).await;
    assert_eq!(wallet.lookup_calls.load(Ordering::SeqCst), 3);
    advance_io(&mut demo, 2).await;
    assert_eq!(wallet.lookup_calls.load(Ordering::SeqCst), 4);
    assert_eq!(demo.saved.pot.earned[0], 10);
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
}
async fn drive(demo: &mut Demo, ready: impl Fn(&Demo) -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(5);
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
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 2).await;
    for sequence in [7, 9] {
        hits(&mut demo, sequence);
    }
    for _ in 0..10 {
        demo.tick().await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 2);
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 2);
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
        2
    );
    assert_eq!(
        saved
            .pot
            .payouts
            .iter()
            .filter(|p| p.state == PayoutState::Created)
            .count(),
        2
    );
    assert_eq!(saved.pot.pending(), [50, 50, 0, 0]);
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
        d.saved.pot.payouts.iter().all(|p| p.state.complete())
    })
    .await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 4);
    assert_eq!(wallet.peak.load(Ordering::SeqCst), 2);
    assert_eq!(demo.saved.pot.earned, [50, 50, 0, 0]);
}
#[tokio::test]
async fn actual_coordinator_acknowledges_credit_while_wallet_request_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    let (sender, receiver) = mpsc::sync_channel(16);
    let shared = Arc::new(Mutex::new(LocalView::default()));
    let outbound = Arc::new(outbound_debug::DebugView::default());
    let task = tokio::spawn(run_worker(
        demo,
        receiver,
        shared.clone(),
        outbound.clone(),
        Default::default(),
    ));
    let until = std::time::Instant::now() + Duration::from_secs(3);
    while wallet.calls.load(Ordering::SeqCst) != 2 {
        assert!(std::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let handle = LocalHandle {
        room_status: Default::default(),
        sender,
        shared: shared.clone(),
        outbound,
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

#[tokio::test]
async fn discard_clears_current_and_previous_queues_but_preserves_submitted_payments_across_restart()
 {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 2).await;
    let submitted = serde_json::to_value(&demo.saved.prepared).unwrap();
    demo.command(Command::Action(8)).await.unwrap();
    demo.command(Command::Action(1)).await.unwrap();
    // The old final settlements are queued; this new match has an open batch.
    demo.saved.pot.confirm_entry(0, 500).unwrap();
    demo.saved.pot.confirm_entry(1, 500).unwrap();
    demo.saved.pot.start().unwrap();
    demo.saved.pot.credit_at([0, 800], now_ms()).unwrap();
    demo.saved.retired_entries.push(RetiredEntry {
        incoming_hash: "late-received".into(),
        player: 0,
        address: "alice@example.test".into(),
        expected_sats: 7,
        received_sats: Some(7),
        prepared: None,
        state: PayoutState::Created,
    });
    assert_eq!(demo.view().unwrap().unsent_sats, 995);
    demo.command(Command::Action(14)).await.unwrap();
    assert_eq!(demo.view().unwrap().unsent_sats, 0);
    assert_eq!(demo.view().unwrap().pending_sats, [0; 4]);
    assert_eq!(demo.view().unwrap().background_pending_sats, 20);
    assert_eq!(demo.saved.pot.discarded_sats(), 8);
    assert_eq!(demo.saved.pending_matches[0].pot.discarded_sats(), 980);
    assert_eq!(
        serde_json::to_value(&demo.saved.pending_matches[0].prepared).unwrap(),
        submitted
    );
    assert_eq!(
        demo.saved.retired_entries.last().unwrap().state,
        PayoutState::Discarded
    );
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 2);
    drop(demo);
    tokio::task::yield_now().await;
    let mut recovered = Demo::open(dir.path()).await.unwrap();
    recovered.reconcile().await.unwrap();
    assert_eq!(recovered.view().unwrap().unsent_sats, 0);
    assert_eq!(recovered.view().unwrap().background_pending_sats, 20);
    assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 0);
    assert!(recovered.saved.receipts.is_empty());
    recovered.command(Command::Action(14)).await.unwrap();
    assert_eq!(recovered.saved.pot.discarded_sats(), 8);
}

#[tokio::test]
async fn failed_discard_save_restores_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, _) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    let before = serde_json::to_value(&demo.saved).unwrap();
    let bad_parent = dir.path().join("not-a-directory");
    std::fs::write(&bad_parent, "fixture").unwrap();
    demo.path = bad_parent.join("state.json");
    assert!(demo.command(Command::Action(14)).await.is_err());
    assert_eq!(serde_json::to_value(&demo.saved).unwrap(), before);
    assert_eq!(demo.view().unwrap().unsent_sats, 20);
}

#[tokio::test]
async fn pending_previous_game_survives_restart_and_editing_without_blocking_a_fresh_paid_game() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), true).await;
    hits(&mut demo, 1);
    demo.command(Command::Action(8)).await.unwrap();
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 2).await;
    let previous_id = demo.saved.pot.id.clone();
    let original_addresses = demo.saved.addresses.clone();
    demo.command(Command::Action(1)).await.unwrap();
    assert_eq!(demo.saved.pot.paid, [false; 4]);
    assert_eq!(demo.view().unwrap().background_pending_sats, 1000);
    assert_eq!(demo.view().unwrap().background_failed_sats, 0);
    assert_eq!(
        demo.saved.pending_matches[0]
            .pot
            .pending()
            .iter()
            .sum::<u64>(),
        1000
    );
    let original_prepared = serde_json::to_value(&demo.saved.pending_matches[0].prepared).unwrap();
    drop(demo);
    tokio::task::yield_now().await;

    let mut demo = Demo::open(dir.path()).await.unwrap();
    demo.command(Command::Addresses([
        "carol@example.test".into(),
        "dave@example.test".into(),
    ]))
    .await
    .unwrap();
    demo.command(Command::Action(2)).await.unwrap();
    demo.command(Command::Action(3)).await.unwrap();
    demo.reconcile().await.unwrap();
    demo.command(Command::Action(4)).await.unwrap();
    assert_eq!(demo.view().unwrap().phase, 3);
    assert_eq!(demo.view().unwrap().background_pending_sats, 1000);
    assert_eq!(demo.saved.pending_matches[0].addresses, original_addresses);
    assert_eq!(
        serde_json::to_value(&demo.saved.pending_matches[0].prepared).unwrap(),
        original_prepared
    );
    hits(&mut demo, 1);
    demo.command(Command::Action(8)).await.unwrap();
    demo.reconcile().await.unwrap();
    assert_eq!(demo.saved.pot.phase, Phase::Settled);
    assert_eq!(demo.saved.pot.earned.iter().sum::<u64>(), 1000);
    let attempts = demo.simulation.as_ref().unwrap().payment_attempts();
    assert_eq!(attempts, 2); // This game's damage and returns coalesce per recipient.
    assert_eq!(demo.saved.pending_matches.len(), 1);
    let hashes: Vec<_> = demo.saved.pending_matches[0]
        .prepared
        .values()
        .map(|p| p.payment_hash.clone())
        .collect();
    for hash in hashes {
        demo.simulation
            .as_ref()
            .unwrap()
            .set_payment_state(&hash, PaymentState::Succeeded { fee_sats: 0 });
    }
    demo.reconcile().await.unwrap();
    assert!(demo.saved.pending_matches.is_empty());
    assert_eq!(demo.view().unwrap().background_pending_sats, 0);
    assert_eq!(
        demo.simulation.as_ref().unwrap().payment_attempts(),
        attempts
    );
    let archived: Saved = serde_json::from_slice(
        &std::fs::read(
            dir.path()
                .join("matches")
                .join(format!("{previous_id}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(archived.pot.phase, Phase::Settled);
    assert_eq!(archived.pot.earned.iter().sum::<u64>(), 1000);
    assert_eq!(archived.receipts.len(), 2);
    assert_eq!(archived.addresses, original_addresses);
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
    let outbound = Arc::new(outbound_debug::DebugView::default());
    let task = tokio::spawn(run_worker(
        demo,
        receiver,
        shared.clone(),
        outbound.clone(),
        Default::default(),
    ));
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
        room_status: Default::default(),
        sender,
        shared: shared.clone(),
        outbound,
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

#[tokio::test(start_paused = true)]
async fn an_abandoned_unpaid_invoice_is_not_polled_every_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, _) = playing(dir.path(), false).await;
    let invoice = demo
        .wallet
        .create_invoice(500, "abandoned", 600)
        .await
        .unwrap();
    let key = format!("incoming-{}", invoice.payment_hash);
    demo.saved.retired_entries.push(RetiredEntry {
        incoming_hash: invoice.payment_hash,
        player: 0,
        address: "alice@example.test".into(),
        expected_sats: 500,
        received_sats: None,
        prepared: None,
        state: PayoutState::Created,
    });
    let count = || {
        std::fs::read_to_string(dir.path().join("payment-timings.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| row["stage"] == "incoming_lookup" && row["id"] == key)
            .count()
    };
    advance_io(&mut demo, 0).await;
    assert_eq!(count(), 1);
    advance_io(&mut demo, 29).await;
    assert_eq!(count(), 1);
    advance_io(&mut demo, 1).await;
    assert_eq!(count(), 2);
}

#[tokio::test(start_paused = true)]
async fn missed_timeout_releases_later_payments_to_the_same_address() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, wallet) = playing(dir.path(), false).await;
    demo.io.track_missed = true;
    demo.io.serialize_submissions = true;
    demo.command(Command::Credits(vec![Credit {
        sequence: 1,
        attacker: 0,
        victim: 1,
        damage_centi: 4000,
    }]))
    .await
    .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5000);
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 1).await;
    let id = demo.saved.pot.payouts[0].id.clone();
    let hash = demo.saved.prepared[&id].payment_hash.clone();
    tokio::time::advance(Duration::from_secs(16)).await;
    drive(&mut demo, |d| d.recovery(&id).is_some_and(|r| r.missed)).await;
    assert_eq!(demo.missed_summary().0, 40);
    demo.command(Command::Action(16)).await.unwrap();
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    demo.command(Command::Credits(vec![Credit {
        sequence: 2,
        attacker: 0,
        victim: 1,
        damage_centi: 2000,
    }]))
    .await
    .unwrap();
    demo.saved.pot.flush_due(now_ms() + 5000);
    drive(&mut demo, |d| d.saved.pot.earned[0] == 20).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 2);
    assert_eq!(demo.saved.prepared[&id].payment_hash, hash);
    assert_eq!(
        demo.saved
            .pot
            .payouts
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .state,
        PayoutState::InFlight
    );
    demo.command(Command::Action(8)).await.unwrap();
    for _ in 0..3 {
        demo.command(Command::Action(16)).await.unwrap();
    }
    assert!(
        !demo
            .saved
            .payout_groups
            .values()
            .any(|members| members.contains(&id))
    );
    assert_eq!(demo.saved.prepared[&id].payment_hash, hash);
}

#[tokio::test]
async fn missed_amounts_group_by_address_after_match_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (mut demo, _) = four_playing(
        dir.path(),
        [
            "shared@example.test",
            "bob@example.test",
            "shared@example.test",
            "dana@example.test",
        ],
    )
    .await;
    demo.wallet = demo.simulation.clone().unwrap();
    demo.io.track_missed = true;
    let held: Vec<_> = demo
        .saved
        .pot
        .payouts
        .iter()
        .filter(|p| p.player == 0 || p.player == 2)
        .map(|p| p.id.clone())
        .collect();
    struct UnavailableReceiver;
    #[async_trait::async_trait]
    impl AddressInvoices for UnavailableReceiver {
        async fn invoice(
            &self,
            address: &LightningAddress,
            sats: u64,
            id: &str,
        ) -> anyhow::Result<String> {
            if address.as_str() == "shared@example.test" {
                anyhow::bail!("private-provider-response");
            }
            FakeAddressInvoices.invoice(address, sats, id).await
        }
    }
    demo.provider = Arc::new(UnavailableReceiver);
    for _ in 0..100 {
        let _ = demo.tick().await;
        if held
            .iter()
            .all(|id| demo.recovery(id).is_some_and(|r| r.missed))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        held.iter()
            .all(|id| demo.recovery(id).is_some_and(|r| r.missed))
    );
    demo.provider = Arc::new(FakeAddressInvoices);
    demo.command(Command::Action(16)).await.unwrap();
    assert!(demo.saved.payout_groups.is_empty());
    drive(&mut demo, |d| {
        d.saved.pot.earned[1] == 10 && d.saved.pot.earned[3] == 10
    })
    .await;
    assert_eq!(demo.saved.pot.earned[0], 0);
    assert_eq!(demo.missed_summary().0, 20);
    demo.command(Command::Action(8)).await.unwrap();
    drive(&mut demo, |d| {
        d.saved
            .pot
            .payouts
            .iter()
            .all(|p| p.state.complete() || held.contains(&p.id))
    })
    .await;
    demo.pay_missed().unwrap();
    assert_eq!(demo.saved.payout_groups.len(), 1);
    let leader = demo.saved.payout_groups.keys().next().unwrap().clone();
    assert_eq!(demo.saved.group_sats(&leader), Some(20));
    assert_eq!(demo.missed_summary().1, 0); // Already requested amounts cannot be requested twice.
    let groups = demo.saved.payout_groups.clone();
    demo.pay_missed().unwrap();
    assert_eq!(demo.saved.payout_groups, groups);
    demo.saved.validate_payout_groups().unwrap();
    struct FailedBatch;
    #[async_trait::async_trait]
    impl AddressInvoices for FailedBatch {
        async fn invoice(
            &self,
            address: &LightningAddress,
            sats: u64,
            id: &str,
        ) -> anyhow::Result<String> {
            FakeAddressInvoices
                .invoice(address, sats, &format!("{id}-fail"))
                .await
        }
    }
    demo.provider = Arc::new(FailedBatch);
    drive(&mut demo, |d| {
        held.iter().all(|id| {
            d.saved
                .pot
                .payouts
                .iter()
                .any(|p| &p.id == id && p.state == PayoutState::Failed)
        })
    })
    .await;
    let failed_hash = demo.saved.prepared[&leader].payment_hash.clone();
    assert!(!demo.recovery(&leader).unwrap().manual_ready);
    demo.save().unwrap();
    drop(demo);
    let mut demo = Demo::open(dir.path()).await.unwrap();
    demo.io.track_missed = true;
    for _ in 0..5 {
        demo.tick().await.unwrap();
    }
    assert_eq!(demo.simulation.as_ref().unwrap().payment_attempts(), 0);
    demo.pay_missed().unwrap();
    let wallet = Arc::new(HeldWallet::new(demo.simulation.clone().unwrap(), true));
    demo.wallet = wallet.clone();
    drive(&mut demo, |_| wallet.calls.load(Ordering::SeqCst) == 1).await;
    let submitted_hash = demo.saved.prepared[&leader].payment_hash.clone();
    assert_ne!(submitted_hash, failed_hash);
    demo.saved.validate_payout_groups().unwrap();
    drop(demo);
    let mut demo = Demo::open(dir.path()).await.unwrap();
    demo.wallet = wallet.clone();
    demo.io.track_missed = true;
    demo.pay_missed().unwrap();
    assert_eq!(demo.saved.prepared[&leader].payment_hash, submitted_hash);
    wallet
        .inner
        .set_payment_state(&submitted_hash, PaymentState::Succeeded { fee_sats: 0 });
    drive(&mut demo, |d| d.saved.pot.phase == Phase::Settled).await;
    assert_eq!(wallet.calls.load(Ordering::SeqCst), 1);
    assert_eq!(demo.saved.pot.earned, [demo.saved.pot.entry_sats; 4]);
    let a = &demo.saved.prepared[&held[0]];
    let b = &demo.saved.prepared[&held[1]];
    assert_eq!(a.payment_hash, b.payment_hash);
    assert_eq!(
        demo.wallet.decode_invoice(&a.invoice).unwrap().amount_sats,
        Some(20)
    );
    assert_eq!(
        demo.saved
            .receipts
            .iter()
            .filter(|r| held.contains(&r.id))
            .map(|r| r.sats)
            .sum::<u64>(),
        20
    );
    assert_eq!(demo.missed_summary().0, 0);
    demo.saved.validate_payout_groups().unwrap();
    let receipts = demo.saved.receipts.len();
    demo.pay_missed().unwrap();
    for _ in 0..5 {
        demo.tick().await.unwrap();
    }
    assert_eq!(demo.saved.receipts.len(), receipts);
    assert!(
        !serde_json::to_string(&demo.saved.activity)
            .unwrap()
            .contains("private-provider-response")
    );
}
