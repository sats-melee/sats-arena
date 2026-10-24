//! Embedded/local host: no HTTP listener, server process, or real wallet.
use std::{path::Path, sync::Arc};

use sats_arena_core::{
    Engine, Rules,
    protocol::{Hit, MatchMode, MatchReport, PROTOCOL_VERSION, PlayerSlot, WithdrawalRequest},
    wallet::FakeWallet,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A real embedded host supplies a persistent path and a real wallet adapter
    // on its background runtime. auto_pay() is ONLY for this fake demo.
    let engine = Engine::open(
        Path::new(":memory:"),
        Rules::default(),
        Arc::new(FakeWallet::auto_pay()),
    )?;
    let alice = engine.register("Alice").await?;
    let bob = engine.register("Bob").await?;
    for player in [&alice, &bob] {
        let invoice = engine.create_deposit(&player.token, 1000).await?;
        // Normally show the BOLT11/QR, then poll. No credit until wallet lookup.
        let deposit = engine.deposit(&player.token, &invoice.id).await?;
        println!("Deposit: {:?}", deposit.status);
    }

    let report = MatchReport {
        protocol_version: PROTOCOL_VERSION,
        match_id: "local-demo-game-1".into(),
        mode: MatchMode::Local,
        reporter_port: 0,
        players: vec![
            PlayerSlot {
                port: 0,
                player_id: alice.player_id.clone(),
            },
            PlayerSlot {
                port: 1,
                player_id: bob.player_id.clone(),
            },
        ],
        hits: vec![Hit {
            frame: 100,
            victim_port: 1,
            attacker_port: Some(0),
            damage_centi: 5000,
        }],
        winner_port: Some(0),
    };
    engine
        .create_lobby(
            &alice.token,
            sats_arena_core::protocol::LobbyRequest {
                id: report.match_id.clone(),
                mode: report.mode,
                players: report.players.clone(),
                budget_sats: 500,
            },
        )
        .await?;
    engine.ready_lobby(&alice.token, &report.match_id).await?;
    engine.ready_lobby(&bob.token, &report.match_id).await?;
    engine.start_lobby(&alice.token, &report.match_id).await?;
    engine.report_match(&alice.token, report.clone()).await?;
    let mut other = report;
    other.reporter_port = 1;
    println!("Match: {:?}", engine.report_match(&bob.token, other).await?);

    let payout = engine
        .withdraw(
            &alice.token,
            WithdrawalRequest {
                bolt11: "lnfake:500:alice-demo-payout".into(),
                amount_sats: 500,
            },
        )
        .await?;
    println!("Withdrawal: {:?}", payout.status);
    println!(
        "Alice: {} sats",
        engine.account(&alice.token).await?.balance_sats
    );
    println!(
        "Bob: {} sats",
        engine.account(&bob.token).await?.balance_sats
    );
    engine.shutdown().await?;
    Ok(())
}
