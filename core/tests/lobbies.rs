use sats_arena_core::{
    Engine, Rules,
    protocol::{
        LobbyPhase, LobbyRequest, MatchMode, MatchReport, MatchStatus, PROTOCOL_VERSION,
        PlayerSlot, RegisterResponse, WithdrawalRequest,
    },
    wallet::{FakeWallet, InvoiceState, Wallet},
};
use std::{path::Path, sync::Arc};

async fn setup() -> (Engine, Arc<FakeWallet>, [RegisterResponse; 2], LobbyRequest) {
    let wallet = Arc::new(FakeWallet::default());
    let engine = Engine::open(Path::new(":memory:"), Rules::default(), wallet.clone()).unwrap();
    let players = [
        engine.register("Alice").await.unwrap(),
        engine.register("Bob").await.unwrap(),
    ];
    let request = LobbyRequest {
        id: "game-1".into(),
        mode: MatchMode::Local,
        budget_sats: 500,
        players: players
            .iter()
            .enumerate()
            .map(|(port, p)| PlayerSlot {
                port: port as u8,
                player_id: p.player_id.clone(),
            })
            .collect(),
    };
    engine
        .create_lobby(&players[0].token, request.clone())
        .await
        .unwrap();
    (engine, wallet, players, request)
}

async fn pay(engine: &Engine, wallet: &FakeWallet, token: &str, amount: u64) {
    let invoice = engine.create_deposit(token, amount).await.unwrap();
    wallet.set_invoice_state(
        &wallet.decode_invoice(&invoice.bolt11).unwrap().payment_hash,
        InvoiceState::Paid {
            amount_sats: amount,
        },
    );
    engine.deposit(token, &invoice.id).await.unwrap();
}

async fn fund_and_start(
    engine: &Engine,
    wallet: &FakeWallet,
    players: &[RegisterResponse; 2],
    id: &str,
) {
    for p in players {
        pay(engine, wallet, &p.token, 1000).await;
        engine.ready_lobby(&p.token, id).await.unwrap();
    }
    engine.start_lobby(&players[0].token, id).await.unwrap();
}

fn report(request: &LobbyRequest, port: u8, winner: u8) -> MatchReport {
    MatchReport {
        protocol_version: PROTOCOL_VERSION,
        match_id: request.id.clone(),
        mode: request.mode,
        reporter_port: port,
        players: request.players.clone(),
        hits: vec![],
        winner_port: Some(winner),
    }
}

#[tokio::test]
async fn start_requires_two_confirmed_payments_and_explicit_budget_reservations() {
    let (engine, wallet, players, request) = setup().await;
    assert!(
        engine
            .start_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .ready_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .report_match(&players[0].token, report(&request, 0, 0))
            .await
            .is_err()
    );
    pay(&engine, &wallet, &players[0].token, 500).await;
    let one = engine
        .ready_lobby(&players[0].token, &request.id)
        .await
        .unwrap();
    assert_eq!(one.phase, LobbyPhase::Funding);
    assert_eq!(one.players[0].reserved_sats, 500);
    assert!(
        engine
            .start_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    // Repeated readiness doesn't reserve twice.
    assert_eq!(
        engine
            .ready_lobby(&players[0].token, &request.id)
            .await
            .unwrap(),
        one
    );
    let invoice = engine.create_deposit(&players[1].token, 500).await.unwrap();
    engine
        .deposit(&players[1].token, &invoice.id)
        .await
        .unwrap(); // Still unpaid.
    assert!(
        engine
            .ready_lobby(&players[1].token, &request.id)
            .await
            .is_err()
    );
    pay(&engine, &wallet, &players[1].token, 500).await;
    assert_eq!(
        engine
            .ready_lobby(&players[1].token, &request.id)
            .await
            .unwrap()
            .phase,
        LobbyPhase::Ready
    );
    assert_eq!(
        engine
            .start_lobby(&players[0].token, &request.id)
            .await
            .unwrap()
            .phase,
        LobbyPhase::Playing
    );
    assert!(
        engine
            .start_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .cancel_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn reserved_match_funds_cannot_be_withdrawn_and_cancel_refunds_once() {
    let (engine, wallet, players, request) = setup().await;
    pay(&engine, &wallet, &players[0].token, 500).await;
    engine
        .ready_lobby(&players[0].token, &request.id)
        .await
        .unwrap();
    assert_eq!(
        engine
            .account(&players[0].token)
            .await
            .unwrap()
            .balance_sats,
        0
    );
    assert!(
        engine
            .withdraw(
                &players[0].token,
                WithdrawalRequest {
                    amount_sats: 100,
                    bolt11: "lnfake:100:reserved".into()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(wallet.payment_attempts(), 0);
    engine
        .cancel_lobby(&players[1].token, &request.id)
        .await
        .unwrap();
    engine
        .cancel_lobby(&players[1].token, &request.id)
        .await
        .unwrap();
    assert_eq!(
        engine
            .account(&players[0].token)
            .await
            .unwrap()
            .balance_sats,
        500
    );
    assert_eq!(
        engine
            .account(&players[1].token)
            .await
            .unwrap()
            .balance_sats,
        0
    );
    assert!(
        engine
            .start_lobby(&players[0].token, &request.id)
            .await
            .is_err()
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn settlement_releases_remaining_budget_without_touching_unreserved_balance() {
    let (engine, wallet, players, request) = setup().await;
    fund_and_start(&engine, &wallet, &players, &request.id).await;
    assert_eq!(
        engine
            .account(&players[0].token)
            .await
            .unwrap()
            .balance_sats,
        500
    );
    engine
        .report_match(&players[0].token, report(&request, 0, 0))
        .await
        .unwrap();
    assert!(matches!(
        engine
            .report_match(&players[1].token, report(&request, 1, 0))
            .await
            .unwrap(),
        MatchStatus::Settled { .. }
    ));
    engine
        .report_match(&players[1].token, report(&request, 1, 0))
        .await
        .unwrap();
    let lobby = engine.lobby(&players[0].token, &request.id).await.unwrap();
    assert_eq!(lobby.phase, LobbyPhase::Settled);
    assert!(lobby.players.iter().all(|p| p.reserved_sats == 0));
    assert_eq!(
        engine
            .account(&players[0].token)
            .await
            .unwrap()
            .balance_sats,
        1100
    );
    assert_eq!(
        engine
            .account(&players[1].token)
            .await
            .unwrap()
            .balance_sats,
        900
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn outsiders_cannot_access_or_admit_a_lobby_and_creates_are_retry_safe() {
    let (engine, _, players, request) = setup().await;
    let outsider = engine.register("Other").await.unwrap();
    assert!(engine.lobby(&outsider.token, &request.id).await.is_err());
    assert!(
        engine
            .ready_lobby(&outsider.token, &request.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .cancel_lobby(&outsider.token, &request.id)
            .await
            .is_err()
    );
    assert_eq!(
        engine
            .create_lobby(&players[0].token, request.clone())
            .await
            .unwrap()
            .phase,
        LobbyPhase::Funding
    );
    let mut changed = request;
    changed.budget_sats = 1000;
    assert!(
        engine
            .create_lobby(&players[0].token, changed)
            .await
            .is_err()
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn disputed_games_keep_funds_locked_instead_of_refunding_a_sore_loser() {
    let (engine, wallet, players, request) = setup().await;
    fund_and_start(&engine, &wallet, &players, &request.id).await;
    engine
        .report_match(&players[0].token, report(&request, 0, 0))
        .await
        .unwrap();
    assert_eq!(
        engine
            .report_match(&players[1].token, report(&request, 1, 1))
            .await
            .unwrap(),
        MatchStatus::Disputed
    );
    let lobby = engine.lobby(&players[0].token, &request.id).await.unwrap();
    assert_eq!(lobby.phase, LobbyPhase::Disputed);
    assert!(
        lobby
            .players
            .iter()
            .all(|p| p.reserved_sats == 500 && p.available_sats == 500)
    );
    assert!(
        engine
            .cancel_lobby(&players[1].token, &request.id)
            .await
            .is_err()
    );
    engine.shutdown().await.unwrap();
}
