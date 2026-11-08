use super::*;
use crate::TestVerifier;

const ID: &str = "0123456789abcdef0123456789abcdef";
fn rules() -> HostRules {
    HostRules {
        entry_sats: 100,
        sats_per_percent: 1,
        batch_window_ms: 1000,
        surcharge_bps: 0,
    }
}
fn config(wallet: Arc<FakeWallet>) -> HostWallet {
    HostWallet {
        wallet,
        provider: Arc::new(FakeAddressInvoices),
        verifier: Arc::new(TestVerifier),
        identity: "a".repeat(64),
        mode: "fake".into(),
    }
}
async fn pump(host: &mut MatchHost, ready: impl Fn(&HostSnapshot) -> bool) {
    for _ in 0..1000 {
        host.tick().await.unwrap();
        if ready(&host.snapshot()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("host did not reach expected state");
}
async fn invoices(host: &mut MatchHost) {
    host.set_address(0, "alice@example.test".into())
        .await
        .unwrap();
    host.set_address(1, "bob@example.test".into())
        .await
        .unwrap();
    pump(host, |v| {
        !v.invoices[0].is_empty() && !v.invoices[1].is_empty()
    })
    .await;
}
async fn fund(host: &mut MatchHost, wallet: &FakeWallet, slot: usize) {
    let invoice = &host.snapshot().invoices[slot];
    let hash = wallet.decode_invoice(invoice).unwrap().payment_hash;
    wallet.set_invoice_state(&hash, InvoiceState::Paid { amount_sats: 100 });
    pump(host, |v| v.paid[slot]).await;
}
async fn funded(path: &Path, wallet: Arc<FakeWallet>) -> MatchHost {
    let mut host = MatchHost::open(path, ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    host.configure_roster(false, [0, 0, 3, 3], [255; 4], [0, 17, 255, 255])
        .await
        .unwrap();
    invoices(&mut host).await;
    fund(&mut host, &wallet, 0).await;
    fund(&mut host, &wallet, 1).await;
    host
}

#[tokio::test(start_paused = true)]
async fn shared_host_requires_receipts_and_selection_then_reuses_existing_payout_rules() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let mut host = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    assert!(host.snapshot().invoices.iter().all(String::is_empty));
    host.configure_roster(false, [0, 0, 3, 3], [255; 4], [255; 4])
        .await
        .unwrap();
    invoices(&mut host).await;
    assert_eq!(host.snapshot().match_id, ID);
    assert_eq!(host.snapshot().paid, [false; 4]);
    assert!(host.start().await.is_err());
    host.configure_roster(false, [0, 0, 3, 3], [255; 4], [0, 17, 255, 255])
        .await
        .unwrap();
    fund(&mut host, &wallet, 0).await;
    assert!(host.start().await.is_err());
    fund(&mut host, &wallet, 1).await;
    // Replay external character IDs include Roy/Ganondorf beyond the CSS icon range.
    host.configure_roster(false, [0, 0, 3, 3], [255; 4], [26, 25, 255, 255])
        .await
        .unwrap();
    assert_eq!(host.snapshot().paid, [true, true, false, false]);
    assert_eq!(host.snapshot().characters, [26, 25, 255, 255]);
    host.start().await.unwrap();
    let credit = Credit {
        sequence: 1,
        attacker: 0,
        victim: 1,
        damage_centi: 1000,
    };
    host.credits(vec![credit]).await.unwrap();
    host.credits(vec![credit]).await.unwrap();
    assert_eq!(host.snapshot().remaining_sats, [100, 90, 0, 0]);
    assert!(
        host.credits(vec![Credit {
            sequence: 3,
            ..credit
        }])
        .await
        .is_err()
    );
    assert_eq!(host.snapshot().last_credit_sequence, 1);
    assert!(host.finish(Outcome::Winner(0), false).await.is_err());
    host.finish(Outcome::Winner(0), true).await.unwrap();
    assert!(host.refund_after_review().await.is_err());
    pump(&mut host, |v| v.phase == Phase::Settled).await;
    assert_eq!(host.snapshot().earned_sats, [200, 0, 0, 0]);
    assert_eq!(wallet.payment_attempts(), 1);
}

#[tokio::test(start_paused = true)]
async fn reopen_rejects_wrong_terms_wallet_and_concurrent_owner_without_mutating_ledger() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let host = funded(folder.path(), wallet.clone()).await;
    let path = folder.path().join("prepaid-match.json");
    let before = std::fs::read(&path).unwrap();
    assert!(
        MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
            .await
            .is_err()
    );
    assert_eq!(before, std::fs::read(&path).unwrap());
    drop(host);
    let changed = HostRules {
        entry_sats: 101,
        ..rules()
    };
    assert!(
        MatchHost::open(folder.path(), ID, changed, config(wallet.clone()))
            .await
            .is_err()
    );
    assert_eq!(before, std::fs::read(&path).unwrap());
    let mut wrong_wallet = config(wallet.clone());
    wrong_wallet.identity = "b".repeat(64);
    assert!(
        MatchHost::open(folder.path(), ID, rules(), wrong_wallet)
            .await
            .is_err()
    );
    assert_eq!(before, std::fs::read(&path).unwrap());
    let mut wrong_mode = config(wallet.clone());
    wrong_mode.mode = "sandbox".into();
    assert!(
        MatchHost::open(folder.path(), ID, rules(), wrong_mode)
            .await
            .is_err()
    );
    assert_eq!(before, std::fs::read(&path).unwrap());
    let reopened = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    assert_eq!(reopened.snapshot().remaining_sats, [100, 100, 0, 0]);
    assert_eq!(wallet.payment_attempts(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelling_partial_funding_refunds_receipts_and_late_entries_after_restart() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let mut host = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    invoices(&mut host).await;
    let late = wallet
        .decode_invoice(&host.snapshot().invoices[1])
        .unwrap()
        .payment_hash;
    fund(&mut host, &wallet, 0).await;
    host.cancel().await.unwrap();
    pump(&mut host, |v| v.phase == Phase::Settled).await;
    assert_eq!(host.snapshot().earned_sats[0], 100);
    drop(host);
    wallet.set_invoice_state(&late, InvoiceState::Paid { amount_sats: 100 });
    let mut host = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    for _ in 0..1000 {
        host.tick().await.unwrap();
        if host
            .runtime
            .saved
            .retired_entries
            .iter()
            .any(|e| e.incoming_hash == late && e.state == PayoutState::Paid)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        host.runtime
            .saved
            .retired_entries
            .iter()
            .any(|e| e.incoming_hash == late && e.state == PayoutState::Paid)
    );
    assert_eq!(wallet.payment_attempts(), 2);
    drop(host);
    let mut host = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    for _ in 0..10 {
        host.tick().await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(wallet.payment_attempts(), 2);
}

struct UnknownInvoices;
#[async_trait::async_trait]
impl AddressInvoices for UnknownInvoices {
    async fn invoice(
        &self,
        _address: &LightningAddress,
        sats: u64,
        id: &str,
    ) -> anyhow::Result<String> {
        Ok(format!("lnfake:{sats}:unknown-{id}"))
    }
}
#[tokio::test(start_paused = true)]
async fn frozen_recovery_looks_up_original_uncertain_hash_without_new_submissions() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let mut cfg = config(wallet.clone());
    cfg.provider = Arc::new(UnknownInvoices);
    let mut host = MatchHost::open(folder.path(), ID, rules(), cfg.clone())
        .await
        .unwrap();
    host.configure_roster(false, [0, 0, 3, 3], [255; 4], [0, 17, 255, 255])
        .await
        .unwrap();
    invoices(&mut host).await;
    fund(&mut host, &wallet, 0).await;
    fund(&mut host, &wallet, 1).await;
    host.start().await.unwrap();
    host.finish(Outcome::NoContest, true).await.unwrap();
    for _ in 0..1000 {
        host.tick().await.unwrap();
        if wallet.payment_attempts() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    host.freeze().unwrap();
    assert!(host.credits(vec![]).await.is_err());
    let submitted = host
        .runtime
        .saved
        .pot
        .payouts
        .iter()
        .find(|p| p.state == PayoutState::InFlight)
        .unwrap();
    let slot = submitted.player;
    let hash = host.runtime.saved.prepared[&submitted.id]
        .payment_hash
        .clone();
    assert_eq!(wallet.payment_attempts(), 1);
    drop(host);
    wallet.set_payment_state(&hash, PaymentState::Succeeded { fee_sats: 0 });
    let mut host = MatchHost::open(folder.path(), ID, rules(), cfg)
        .await
        .unwrap();
    assert!(host.snapshot().frozen);
    pump(&mut host, |v| v.earned_sats[slot] == 100).await;
    assert_eq!(wallet.payment_attempts(), 1);
    assert_eq!(
        host.runtime
            .saved
            .prepared
            .values()
            .filter(|p| p.payment_hash == hash)
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn frozen_unsubmitted_payouts_wait_for_atomic_refund_only_review() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let mut host = funded(folder.path(), wallet.clone()).await;
    host.start().await.unwrap();
    host.credits(vec![Credit {
        sequence: 1,
        attacker: 0,
        victim: 1,
        damage_centi: 1000,
    }])
    .await
    .unwrap();
    host.freeze().unwrap();
    for _ in 0..10 {
        host.tick().await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(wallet.payment_attempts(), 0);
    assert!(host.finish(Outcome::Winner(0), true).await.is_err());
    host.refund_after_review().await.unwrap();
    assert!(!host.snapshot().frozen);
    assert_eq!(host.runtime.saved.pot.outcome, Some(Outcome::NoContest));
    pump(&mut host, |v| v.phase == Phase::Settled).await;
    assert_eq!(host.snapshot().earned_sats, [110, 90, 0, 0]);
    drop(host);
    let reopened = MatchHost::open(folder.path(), ID, rules(), config(wallet))
        .await
        .unwrap();
    assert!(!reopened.snapshot().frozen);
    assert_eq!(reopened.runtime.saved.pot.outcome, Some(Outcome::NoContest));
}

#[tokio::test(start_paused = true)]
async fn operator_review_replays_after_host_commit_without_reallocating_refunds() {
    let folder = tempfile::tempdir().unwrap();
    let wallet = Arc::new(FakeWallet::default());
    let mut host = funded(folder.path(), wallet.clone()).await;
    assert!(host.refund_after_review().await.is_err());
    host.start().await.unwrap();
    host.freeze().unwrap();
    host.refund_after_review().await.unwrap();
    let payouts = host.runtime.saved.pot.payouts.clone();
    // The coordinator's review-intent journal still says frozen when the process stops.
    drop(host);
    let mut host = MatchHost::open(folder.path(), ID, rules(), config(wallet.clone()))
        .await
        .unwrap();
    host.refund_after_review().await.unwrap();
    assert_eq!(host.runtime.saved.pot.payouts, payouts);
    assert_eq!(wallet.payment_attempts(), 0);
    pump(&mut host, |v| v.phase == Phase::Settled).await;
    host.refund_after_review().await.unwrap();
    assert_eq!(host.snapshot().earned_sats, [100, 100, 0, 0]);
    assert_eq!(wallet.payment_attempts(), 2);
}
