//! Real-mode incoming invoice lifecycle tests, using an injected wallet only.
//! No NWC connection, public receiving service, or real payment is used.
use super::*;

fn run(test: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}
async fn open(path: &Path, wallet: Arc<FakeWallet>) -> Demo {
    Demo::open_with(
        path,
        wallet,
        Arc::new(FakeAddressInvoices),
        None,
        Arc::new(TestVerifier),
    )
    .await
    .unwrap()
}
async fn reconcile_now(demo: &mut Demo) {
    // Drive the injected-wallet lifecycle without sleeping for network polling cadence.
    demo.next_invoice_poll = std::time::Instant::now();
    demo.next_retired_poll = std::time::Instant::now();
    demo.reconcile().await.unwrap();
}
async fn verified_room(demo: &mut Demo) {
    demo.command(Command::Address(0, "alice@example.test".into()))
        .await
        .unwrap();
    demo.command(Command::Address(1, "bob@example.test".into()))
        .await
        .unwrap();
    reconcile_now(demo).await;
    assert!(demo.saved.hashes[0..2].iter().all(|h| !h.is_empty()));
}
async fn until(demo: &mut Demo, ready: impl Fn(&Demo) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while !ready(demo) {
        reconcile_now(demo).await;
        assert!(
            std::time::Instant::now() < deadline,
            "invoice/refund did not reconcile"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
fn late_id(hash: &str) -> String {
    format!("late-entry-{hash}")
}

#[test]
fn real_entry_invoices_are_not_created_until_receivers_are_verified() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = open(dir.path(), Arc::new(FakeWallet::default())).await;
        assert!(demo.saved.invoices.iter().all(String::is_empty));
        demo.command(Command::Address(0, "alice@example.test".into()))
            .await
            .unwrap();
        reconcile_now(&mut demo).await;
        assert!(demo.saved.hashes.iter().all(String::is_empty));
        demo.command(Command::Address(1, "bob@example.test".into()))
            .await
            .unwrap();
        reconcile_now(&mut demo).await;
        assert!(demo.saved.hashes[..2].iter().all(|h| !h.is_empty()));
        assert!(demo.saved.hashes[2..].iter().all(String::is_empty));
        assert_eq!(
            &demo.saved.invoice_addresses[..2],
            &["alice@example.test", "bob@example.test"]
        );
    });
}

#[test]
fn address_change_retires_its_invoice_and_late_payment_returns_to_original_receiver() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(FakeWallet::default());
        let mut demo = open(dir.path(), wallet.clone()).await;
        verified_room(&mut demo).await;
        let old_hash = demo.saved.hashes[0].clone();
        // The wallet can settle before the UI observes it and retires the room.
        wallet.set_invoice_state(&old_hash, InvoiceState::Paid { amount_sats: 500 });
        demo.command(Command::Address(0, "carol@example.test".into()))
            .await
            .unwrap();
        reconcile_now(&mut demo).await;
        assert_ne!(demo.saved.hashes[0], old_hash);
        assert!(!demo.saved.hashes[0].is_empty());
        wallet.set_invoice_state(&old_hash, InvoiceState::Paid { amount_sats: 500 });
        until(&mut demo, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&old_hash))
        })
        .await;
        let receipt = demo
            .saved
            .receipts
            .iter()
            .find(|r| r.id == late_id(&old_hash))
            .unwrap();
        assert_eq!(receipt.address, "alice@example.test");
        assert_eq!(receipt.sats, 500);
        assert_eq!(receipt.detail, "Late entry return");
        assert_eq!(demo.saved.addresses[0], "carol@example.test");
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(demo.saved.pot.remaining, [0; 4]);
        let attempts = wallet.payment_attempts();
        reconcile_now(&mut demo).await;
        assert_eq!(wallet.payment_attempts(), attempts);
    });
}

#[test]
fn partial_paid_roster_change_keeps_unpaid_old_invoice_refundable_after_new_match() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(FakeWallet::default());
        let mut demo = open(dir.path(), wallet.clone()).await;
        verified_room(&mut demo).await;
        let old_hash = demo.saved.hashes[1].clone();
        let old_match = demo.saved.pot.id.clone();
        wallet.set_invoice_state(
            &demo.saved.hashes[0],
            InvoiceState::Paid { amount_sats: 500 },
        );
        reconcile_now(&mut demo).await;
        assert_eq!(demo.saved.pot.paid, [true, false, false, false]);
        demo.command(Command::Roster(Roster {
            teams: true,
            kinds: [0, 0, 3, 3],
            team_ids: [0, 1, 255, 255],
            characters: [1, 2, 255, 255],
        }))
        .await
        .unwrap();
        until(&mut demo, |d| d.saved.pot.id != old_match).await;
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        wallet.set_invoice_state(&old_hash, InvoiceState::Paid { amount_sats: 500 });
        until(&mut demo, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&old_hash))
        })
        .await;
        let receipt = demo
            .saved
            .receipts
            .iter()
            .find(|r| r.id == late_id(&old_hash))
            .unwrap();
        assert_eq!(receipt.player, 1);
        assert_eq!(receipt.address, "bob@example.test");
        assert_eq!(demo.saved.pot.remaining, [0; 4]);
        assert_eq!(wallet.payment_attempts(), 2); // Known P1 return and late P2 return.
    });
}

#[test]
fn expired_invoice_replacement_keeps_old_hash_monitored_for_late_settlement() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(FakeWallet::default());
        let mut demo = open(dir.path(), wallet.clone()).await;
        verified_room(&mut demo).await;
        let hash = demo.saved.hashes[0].clone();
        wallet.set_invoice_state(&hash, InvoiceState::Expired);
        until(&mut demo, |d| {
            !d.saved.hashes[0].is_empty() && d.saved.hashes[0] != hash
        })
        .await;
        assert!(
            demo.saved
                .retired_entries
                .iter()
                .any(|e| e.incoming_hash == hash)
        );
        wallet.set_invoice_state(&hash, InvoiceState::Paid { amount_sats: 500 });
        until(&mut demo, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&hash))
        })
        .await;
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(
            demo.saved
                .receipts
                .iter()
                .find(|r| r.id == late_id(&hash))
                .unwrap()
                .address,
            "alice@example.test"
        );
    });
}

#[test]
fn wrong_amount_is_accounted_as_a_return_without_funding_the_match() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(FakeWallet::default());
        let mut demo = open(dir.path(), wallet.clone()).await;
        verified_room(&mut demo).await;
        let hash = demo.saved.hashes[0].clone();
        wallet.set_invoice_state(&hash, InvoiceState::Paid { amount_sats: 501 });
        until(&mut demo, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&hash))
        })
        .await;
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(
            demo.saved
                .receipts
                .iter()
                .find(|r| r.id == late_id(&hash))
                .unwrap()
                .sats,
            501
        );
        assert_eq!(wallet.payment_attempts(), 1);
    });
}

#[test]
fn recovered_late_return_is_looked_up_and_never_submitted_again() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(FakeWallet::default());
        let mut demo = open(dir.path(), wallet.clone()).await;
        verified_room(&mut demo).await;
        let hash = demo.saved.hashes[0].clone();
        demo.command(Command::Address(0, "carol@example.test".into()))
            .await
            .unwrap();
        let address = LightningAddress::parse("alice@example.test").unwrap();
        let prepared = prepare_payout(
            demo.wallet.as_ref(),
            demo.provider.as_ref(),
            &address,
            500,
            &late_id(&hash),
        )
        .await
        .unwrap();
        let outgoing_hash = prepared.payment_hash.clone();
        let entry = demo
            .saved
            .retired_entries
            .iter_mut()
            .find(|e| e.incoming_hash == hash)
            .unwrap();
        entry.received_sats = Some(500);
        entry.prepared = Some(prepared);
        entry.state = PayoutState::InFlight;
        demo.save().unwrap();
        drop(demo);
        let mut recovered = open(dir.path(), wallet.clone()).await;
        recovered.reconcile().await.unwrap();
        assert_eq!(wallet.payment_attempts(), 0);
        assert!(
            recovered
                .saved
                .receipts
                .iter()
                .all(|r| r.id != late_id(&hash))
        );
        wallet.set_payment_state(&outgoing_hash, PaymentState::Succeeded { fee_sats: 0 });
        until(&mut recovered, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&hash))
        })
        .await;
        assert_eq!(wallet.payment_attempts(), 0);
        assert_eq!(
            recovered
                .saved
                .receipts
                .iter()
                .filter(|r| r.id == late_id(&hash))
                .count(),
            1
        );
        let receipt = recovered
            .saved
            .receipts
            .iter()
            .find(|r| r.id == late_id(&hash))
            .unwrap();
        assert_eq!(receipt.address, "alice@example.test");
        assert_eq!(recovered.saved.pot.paid, [false; 4]);
    });
}

#[test]
fn same_receiver_refresh_preserves_issued_invoice_and_binding() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = open(dir.path(), Arc::new(FakeWallet::default())).await;
        verified_room(&mut demo).await;
        let hashes = demo.saved.hashes.clone();
        demo.command(Command::Address(0, "alice@EXAMPLE.test".into()))
            .await
            .unwrap();
        reconcile_now(&mut demo).await;
        assert_eq!(demo.saved.hashes, hashes);
        assert!(demo.saved.retired_entries.is_empty());
        assert_eq!(demo.saved.invoice_addresses[0], "alice@example.test");
    });
}

struct LookupUnavailable {
    inner: Arc<FakeWallet>,
    unavailable: Mutex<BTreeSet<String>>,
}
#[async_trait::async_trait]
impl Wallet for LookupUnavailable {
    async fn create_invoice(
        &self,
        amount: u64,
        memo: &str,
        expiry: u64,
    ) -> anyhow::Result<sats_arena_core::wallet::NewInvoice> {
        self.inner.create_invoice(amount, memo, expiry).await
    }
    async fn invoice_state(&self, hash: &str) -> anyhow::Result<InvoiceState> {
        if self.unavailable.lock().unwrap().contains(hash) {
            anyhow::bail!("lookup unavailable")
        }
        self.inner.invoice_state(hash).await
    }
    fn decode_invoice(
        &self,
        invoice: &str,
    ) -> anyhow::Result<sats_arena_core::wallet::DecodedInvoice> {
        self.inner.decode_invoice(invoice)
    }
    async fn pay_invoice(&self, invoice: &str, fee: u64) -> anyhow::Result<PaymentState> {
        self.inner.pay_invoice(invoice, fee).await
    }
    async fn payment_state(&self, hash: &str) -> anyhow::Result<PaymentState> {
        self.inner.payment_state(hash).await
    }
}

#[test]
fn unknown_retired_invoice_stays_monitored_without_blocking_other_returns_or_current_entries() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeWallet::default());
        let wallet = Arc::new(LookupUnavailable {
            inner: fake.clone(),
            unavailable: Mutex::new(BTreeSet::new()),
        });
        let mut demo = Demo::open_with(
            dir.path(),
            wallet.clone(),
            Arc::new(FakeAddressInvoices),
            None,
            Arc::new(TestVerifier),
        )
        .await
        .unwrap();
        verified_room(&mut demo).await;
        let old = demo.saved.hashes.clone();
        demo.command(Command::Address(0, "carol@example.test".into()))
            .await
            .unwrap();
        reconcile_now(&mut demo).await;
        wallet.unavailable.lock().unwrap().insert(old[0].clone());
        fake.set_invoice_state(&old[1], InvoiceState::Paid { amount_sats: 500 });
        fake.set_invoice_state(
            &demo.saved.hashes[0],
            InvoiceState::Paid { amount_sats: 500 },
        );
        until(&mut demo, |d| {
            d.saved.receipts.iter().any(|r| r.id == late_id(&old[1]))
        })
        .await;
        assert!(demo.saved.pot.paid[0]);
        assert_eq!(demo.saved.pot.remaining, [500, 0, 0, 0]);
        let unknown = demo
            .saved
            .retired_entries
            .iter()
            .find(|e| e.incoming_hash == old[0])
            .unwrap();
        assert_eq!(unknown.received_sats, None);
        assert_eq!(unknown.state, PayoutState::Created);
        assert_eq!(fake.payment_attempts(), 1);
    });
}
