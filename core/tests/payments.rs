use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use sats_arena_core::{
    Engine, Error, MAX_BALANCE_SATS, Rules,
    protocol::{
        DepositStatus, Hit, MatchMode, MatchReport, MatchStatus, PROTOCOL_VERSION, PlayerSlot,
        RegisterResponse, WithdrawalRequest, WithdrawalStatus,
    },
    wallet::{DecodedInvoice, FakeWallet, InvoiceState, NewInvoice, PaymentState, Wallet},
};
use tokio::sync::Notify;

fn engine(wallet: Arc<dyn Wallet>) -> Engine {
    Engine::open(Path::new(":memory:"), Rules::default(), wallet).unwrap()
}

async fn fund(engine: &Engine, token: &str, amount: u64) {
    let deposit = engine.create_deposit(token, amount).await.unwrap();
    assert_eq!(
        engine.deposit(token, &deposit.id).await.unwrap().status,
        DepositStatus::Credited
    );
}

fn withdrawal(amount: u64, label: &str) -> WithdrawalRequest {
    WithdrawalRequest {
        bolt11: format!("lnfake:{amount}:{label}"),
        amount_sats: amount,
    }
}

#[tokio::test]
async fn only_wallet_confirmed_paid_invoices_credit_money_once() {
    let wallet = Arc::new(FakeWallet::default());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    let deposit = engine.create_deposit(&player.token, 1000).await.unwrap();
    assert_eq!(deposit.status, DepositStatus::Pending);
    assert_eq!(
        engine
            .deposit(&player.token, &deposit.id)
            .await
            .unwrap()
            .status,
        DepositStatus::Pending
    );
    assert_eq!(engine.account(&player.token).await.unwrap().balance_sats, 0);

    let hash = wallet.decode_invoice(&deposit.bolt11).unwrap().payment_hash;
    wallet.set_invoice_state(&hash, InvoiceState::Paid { amount_sats: 1000 });
    let (a, b) = tokio::join!(
        engine.deposit(&player.token, &deposit.id),
        engine.deposit(&player.token, &deposit.id)
    );
    assert_eq!(a.unwrap().status, DepositStatus::Credited);
    assert_eq!(b.unwrap().status, DepositStatus::Credited);
    engine.reconcile().await.unwrap();
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        1000
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn expiry_and_incorrect_paid_amounts_never_create_spendable_funds() {
    let wallet = Arc::new(FakeWallet::default());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    let expired = engine.create_deposit(&player.token, 1000).await.unwrap();
    wallet.set_invoice_state(
        &wallet.decode_invoice(&expired.bolt11).unwrap().payment_hash,
        InvoiceState::Expired,
    );
    assert_eq!(
        engine
            .deposit(&player.token, &expired.id)
            .await
            .unwrap()
            .status,
        DepositStatus::Expired
    );
    let short = engine.create_deposit(&player.token, 1000).await.unwrap();
    wallet.set_invoice_state(
        &wallet.decode_invoice(&short.bolt11).unwrap().payment_hash,
        InvoiceState::Paid { amount_sats: 999 },
    );
    assert!(engine.deposit(&player.token, &short.id).await.is_err());
    assert_eq!(engine.account(&player.token).await.unwrap().balance_sats, 0);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn account_and_deposit_access_require_the_correct_arena_token() {
    let engine = engine(Arc::new(FakeWallet::default()));
    let alice = engine.register("Alice").await.unwrap();
    let bob = engine.register("Bob").await.unwrap();
    let deposit = engine.create_deposit(&alice.token, 1000).await.unwrap();
    assert!(matches!(
        engine.account("bad-token").await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        engine.deposit(&bob.token, &deposit.id).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        engine.create_deposit("bad-token", 1000).await,
        Err(Error::Unauthorized)
    ));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn payout_retries_do_not_send_or_debit_twice() {
    let wallet = Arc::new(FakeWallet::auto_pay());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    let request = withdrawal(500, "success");
    let (a, b) = tokio::join!(
        engine.withdraw(&player.token, request.clone()),
        engine.withdraw(&player.token, request)
    );
    let a = a.unwrap();
    assert_eq!(a, b.unwrap());
    assert_eq!(a.status, WithdrawalStatus::Completed);
    assert_eq!(wallet.payment_attempts(), 1);
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        500
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn definite_failures_refund_only_once() {
    let wallet = Arc::new(FakeWallet::auto_pay());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    let request = withdrawal(500, "fail");
    assert_eq!(
        engine
            .withdraw(&player.token, request.clone())
            .await
            .unwrap()
            .status,
        WithdrawalStatus::Failed
    );
    engine.withdraw(&player.token, request).await.unwrap();
    engine.reconcile().await.unwrap();
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        1000
    );
    assert_eq!(wallet.payment_attempts(), 1);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_payouts_survive_restarts_without_refund_or_resend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let wallet = Arc::new(FakeWallet::auto_pay());
    let engine = Engine::open(&path, Rules::default(), wallet.clone()).unwrap();
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    let request = withdrawal(500, "unknown");
    let hash = wallet.decode_invoice(&request.bolt11).unwrap().payment_hash;
    let payout = engine
        .withdraw(&player.token, request.clone())
        .await
        .unwrap();
    assert_eq!(payout.status, WithdrawalStatus::Pending);
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        490
    );
    engine.shutdown().await.unwrap();

    // This backend has no payment history at all. "Not found" is not failure.
    let recovered_wallet = Arc::new(FakeWallet::default());
    let recovered = Engine::open(&path, Rules::default(), recovered_wallet.clone()).unwrap();
    recovered.reconcile().await.unwrap();
    assert_eq!(
        recovered.withdraw(&player.token, request).await.unwrap(),
        payout
    );
    assert_eq!(
        recovered.account(&player.token).await.unwrap().balance_sats,
        490
    );
    assert_eq!(recovered_wallet.payment_attempts(), 0);

    recovered_wallet.set_payment_state(&hash, PaymentState::Succeeded { fee_sats: 3 });
    recovered.reconcile().await.unwrap();
    recovered.reconcile().await.unwrap();
    assert_eq!(
        recovered
            .withdrawal(&player.token, &payout.id)
            .await
            .unwrap()
            .status,
        WithdrawalStatus::Completed
    );
    assert_eq!(
        recovered.account(&player.token).await.unwrap().balance_sats,
        497
    );
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_amounts_and_insufficient_fee_reserve_never_call_pay() {
    let wallet = Arc::new(FakeWallet::auto_pay());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    for amount in [0, 1001, u64::MAX] {
        assert!(
            engine
                .withdraw(&player.token, withdrawal(amount, "bad"))
                .await
                .is_err()
        );
    }
    assert!(
        engine
            .withdraw(&player.token, withdrawal(1000, "no-room-for-fees"))
            .await
            .is_err()
    );
    assert!(
        engine
            .create_deposit(&player.token, u64::MAX)
            .await
            .is_err()
    );
    assert_eq!(wallet.payment_attempts(), 0);
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        1000
    );
    engine.shutdown().await.unwrap();
}

fn report(players: &[RegisterResponse; 2], mode: MatchMode) -> MatchReport {
    MatchReport {
        protocol_version: PROTOCOL_VERSION,
        match_id: "one-game-not-just-a-matchmaking-session".into(),
        mode,
        reporter_port: 0,
        players: vec![
            PlayerSlot {
                port: 0,
                player_id: players[0].player_id.clone(),
            },
            PlayerSlot {
                port: 1,
                player_id: players[1].player_id.clone(),
            },
        ],
        hits: vec![Hit {
            frame: 100,
            victim_port: 1,
            attacker_port: Some(0),
            damage_centi: 5000,
        }],
        winner_port: Some(0),
    }
}

async fn admit(engine: &Engine, players: &[RegisterResponse; 2], report: &MatchReport) {
    engine
        .create_lobby(
            &players[0].token,
            sats_arena_core::protocol::LobbyRequest {
                id: report.match_id.clone(),
                mode: report.mode,
                players: report.players.clone(),
                budget_sats: 1000,
            },
        )
        .await
        .unwrap();
    for player in players {
        engine
            .ready_lobby(&player.token, &report.match_id)
            .await
            .unwrap();
    }
    engine
        .start_lobby(&players[0].token, &report.match_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn local_and_netplay_use_identical_settlement_and_never_move_lightning_per_hit() {
    for mode in [MatchMode::Local, MatchMode::Netplay] {
        let wallet = Arc::new(FakeWallet::auto_pay());
        let engine = engine(wallet.clone());
        let players = [
            engine.register("Alice").await.unwrap(),
            engine.register("Bob").await.unwrap(),
        ];
        for player in &players {
            fund(&engine, &player.token, 1000).await;
        }
        let report = report(&players, mode);
        admit(&engine, &players, &report).await;
        assert_eq!(
            engine
                .report_match(&players[0].token, report.clone())
                .await
                .unwrap(),
            MatchStatus::WaitingForOpponent
        );
        let mut other = report;
        other.reporter_port = 1;
        let settled = engine
            .report_match(&players[1].token, other.clone())
            .await
            .unwrap();
        assert!(matches!(settled, MatchStatus::Settled { .. }));
        engine.report_match(&players[1].token, other).await.unwrap();
        assert_eq!(
            engine
                .account(&players[0].token)
                .await
                .unwrap()
                .balance_sats,
            1150
        );
        assert_eq!(
            engine
                .account(&players[1].token)
                .await
                .unwrap()
                .balance_sats,
            850
        );
        assert_eq!(wallet.payment_attempts(), 0);
        engine.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn balances_never_overflow_the_integer_accounting_range() {
    let wallet = Arc::new(FakeWallet::auto_pay());
    let rules = Rules {
        max_deposit_sats: MAX_BALANCE_SATS,
        ..Rules::default()
    };
    let engine = Engine::open(Path::new(":memory:"), rules, wallet).unwrap();
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, MAX_BALANCE_SATS).await;
    let extra = engine.create_deposit(&player.token, 1).await.unwrap();
    assert!(engine.deposit(&player.token, &extra.id).await.is_err());
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        MAX_BALANCE_SATS
    );
    engine.shutdown().await.unwrap();
}

struct GatedWallet {
    fake: FakeWallet,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl Wallet for GatedWallet {
    async fn create_invoice(
        &self,
        amount: u64,
        memo: &str,
        expiry: u64,
    ) -> anyhow::Result<NewInvoice> {
        self.fake.create_invoice(amount, memo, expiry).await
    }
    async fn invoice_state(&self, hash: &str) -> anyhow::Result<InvoiceState> {
        self.fake.invoice_state(hash).await
    }
    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice> {
        self.fake.decode_invoice(bolt11)
    }
    async fn pay_invoice(&self, bolt11: &str, fee: u64) -> anyhow::Result<PaymentState> {
        self.entered.notify_one();
        self.release.notified().await;
        self.fake.pay_invoice(bolt11, fee).await
    }
    async fn payment_state(&self, hash: &str) -> anyhow::Result<PaymentState> {
        self.fake.payment_state(hash).await
    }
}

#[tokio::test]
async fn dropping_a_caller_does_not_cancel_an_accepted_withdrawal() {
    let wallet = Arc::new(GatedWallet {
        fake: FakeWallet::auto_pay(),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    let copy = engine.clone();
    let token = player.token.clone();
    let request = withdrawal(500, "survives-disconnect");
    let request_copy = request.clone();
    let task = tokio::spawn(async move { copy.withdraw(&token, request_copy).await });
    wallet.entered.notified().await;
    task.abort();
    let _ = task.await;
    wallet.release.notify_one();
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        500
    );
    assert_eq!(
        engine
            .withdraw(&player.token, request)
            .await
            .unwrap()
            .status,
        WithdrawalStatus::Completed
    );
    assert_eq!(wallet.fake.payment_attempts(), 1);
    engine.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn periodic_reconciliation_also_runs_in_embedded_mode() {
    let wallet = Arc::new(FakeWallet::default());
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    let deposit = engine.create_deposit(&player.token, 1000).await.unwrap();
    wallet.set_invoice_state(
        &wallet.decode_invoice(&deposit.bolt11).unwrap().payment_hash,
        InvoiceState::Paid { amount_sats: 1000 },
    );
    tokio::time::advance(std::time::Duration::from_secs(16)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        1000
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_games_keep_their_original_rules_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let wallet = Arc::new(FakeWallet::auto_pay());
    let engine = Engine::open(&path, Rules::default(), wallet.clone()).unwrap();
    let players = [
        engine.register("Alice").await.unwrap(),
        engine.register("Bob").await.unwrap(),
    ];
    for player in &players {
        fund(&engine, &player.token, 1000).await;
    }
    let report = report(&players, MatchMode::Netplay);
    admit(&engine, &players, &report).await;
    engine
        .report_match(&players[0].token, report.clone())
        .await
        .unwrap();
    engine.shutdown().await.unwrap();
    let changed_rules = Rules {
        win_sats: 900,
        ..Rules::default()
    };
    let recovered = Engine::open(&path, changed_rules, wallet).unwrap();
    let mut other = report;
    other.reporter_port = 1;
    recovered
        .report_match(&players[1].token, other)
        .await
        .unwrap();
    assert_eq!(
        recovered
            .account(&players[0].token)
            .await
            .unwrap()
            .balance_sats,
        1150
    );
    assert_eq!(
        recovered
            .account(&players[1].token)
            .await
            .unwrap()
            .balance_sats,
        850
    );
    recovered.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn wallet_timeouts_never_release_reserved_funds() {
    let wallet = Arc::new(GatedWallet {
        fake: FakeWallet::auto_pay(),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let engine = engine(wallet.clone());
    let player = engine.register("Alice").await.unwrap();
    fund(&engine, &player.token, 1000).await;
    let copy = engine.clone();
    let token = player.token.clone();
    let task = tokio::spawn(async move { copy.withdraw(&token, withdrawal(500, "timeout")).await });
    wallet.entered.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    assert_eq!(
        task.await.unwrap().unwrap().status,
        WithdrawalStatus::Pending
    );
    engine.reconcile().await.unwrap();
    assert_eq!(
        engine.account(&player.token).await.unwrap().balance_sats,
        490
    );
    engine.shutdown().await.unwrap();
}
