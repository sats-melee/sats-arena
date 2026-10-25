//! Host integration checks: shared pot, verification, wallet, persistence and logs.
use super::*;

fn run(test: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}
fn roster(teams: bool, kinds: [u8; 4], team_ids: [u8; 4]) -> Roster {
    Roster {
        teams,
        kinds,
        team_ids,
        characters: [1, 2, 3, 4],
    }
}
fn ffa() -> Roster {
    roster(false, [0; 4], [255; 4])
}
fn teams() -> Roster {
    roster(true, [0; 4], [0, 1, 0, 1])
}
async fn verify(demo: &mut Demo, player: usize) {
    demo.command(Command::Address(
        player,
        format!("player{}@example.test", player + 1),
    ))
    .await
    .unwrap();
    assert_eq!(demo.view().unwrap().verification_state[player], 2);
}
async fn verify_humans(demo: &mut Demo) {
    for player in 0..4 {
        if demo.saved.pot.kinds[player] == 0 {
            verify(demo, player).await;
        }
    }
}
fn fund_action(player: usize) -> u32 {
    [2, 3, 9, 10][player]
}
async fn fund_humans(demo: &mut Demo) {
    for player in 0..4 {
        if demo.saved.pot.kinds[player] == 0 {
            demo.command(Command::Action(fund_action(player)))
                .await
                .unwrap();
            demo.reconcile().await.unwrap();
        }
    }
    assert_eq!(demo.saved.pot.phase, Phase::Ready);
}
fn conserved(demo: &Demo) {
    let p = &demo.saved.pot;
    assert_eq!(
        p.remaining.iter().sum::<u64>()
            + p.earned.iter().sum::<u64>()
            + p.pending().iter().sum::<u64>(),
        p.paid.iter().filter(|&&paid| paid).count() as u64 * p.entry_sats
    );
}
fn log(demo: &Demo) -> String {
    String::from_utf8_lossy(&demo.view().unwrap().payment_log)
        .trim_end_matches('\0')
        .into()
}

#[test]
fn four_humans_all_require_current_verification_and_fresh_funding() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        for player in 0..3 {
            verify(&mut demo, player).await;
        }
        assert!(demo.command(Command::Action(2)).await.is_err());
        assert!(demo.command(Command::Action(4)).await.is_err());
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        verify(&mut demo, 3).await;
        for player in 0..4 {
            assert!(demo.command(Command::Action(4)).await.is_err());
            demo.command(Command::Action(fund_action(player)))
                .await
                .unwrap();
            demo.reconcile().await.unwrap();
            conserved(&demo);
        }
        assert_eq!(demo.view().unwrap().paid_sats, [500; 4]);
        demo.command(Command::Action(4)).await.unwrap();
        assert_eq!(demo.view().unwrap().phase, 3);
        assert_eq!(demo.view().unwrap().roster_configured, 1);
    });
}

#[test]
fn expired_or_incompatible_fourth_receiver_blocks_a_fully_paid_lobby() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        fund_humans(&mut demo).await;
        demo.saved.verification[3]
            .report
            .as_mut()
            .unwrap()
            .checked_at_ms = now_ms() - sats_arena_core::wallet::VERIFICATION_TTL_MS;
        assert_eq!(demo.view().unwrap().verification_state[3], 4);
        assert!(demo.command(Command::Action(4)).await.is_err());
        let report = demo.saved.verification[3].report.as_mut().unwrap();
        report.checked_at_ms = now_ms();
        report.max_msat = 1_000_000; // Covers two 500-sat entries, not the four-player pot.
        assert_eq!(demo.view().unwrap().verification_state[3], 5);
        assert!(demo.command(Command::Action(4)).await.is_err());
        assert_eq!(demo.saved.pot.phase, Phase::Ready);
        verify(&mut demo, 3).await; // Refresh the same locked, already-funded recipient.
        assert_eq!(demo.saved.pot.paid, [true; 4]);
        demo.command(Command::Action(4)).await.unwrap();
        conserved(&demo);
    });
}

#[test]
fn four_player_ffa_sequences_pay_exact_recipients_and_log_winner() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        fund_humans(&mut demo).await;
        demo.command(Command::Action(4)).await.unwrap();
        let credits = vec![
            Credit {
                sequence: 1,
                attacker: 0,
                victim: 2,
                damage_centi: 800,
            },
            Credit {
                sequence: 2,
                attacker: 3,
                victim: 1,
                damage_centi: 1200,
            },
        ];
        demo.command(Command::Credits(credits.clone()))
            .await
            .unwrap();
        demo.command(Command::Credits(credits)).await.unwrap();
        assert_eq!(demo.view().unwrap().pending_sats, [8, 0, 0, 12]);
        assert_eq!(demo.view().unwrap().remaining_sats, [500, 488, 492, 500]);
        for deadline in &mut demo.saved.pot.batch_deadline_ms {
            if deadline.is_some() {
                *deadline = Some(0);
            }
        }
        demo.reconcile().await.unwrap();
        assert_eq!(demo.view().unwrap().earned_sats, [8, 0, 0, 12]);
        assert_eq!(demo.saved.pot.phase, Phase::Playing);
        assert_eq!(demo.saved.receipts.len(), 2);
        assert!(log(&demo).contains("Player 1 received 8 sats • damage → player1@example.test"));
        assert!(log(&demo).contains("Player 4 received 12 sats • damage → player4@example.test"));
        demo.command(Command::Action(11)).await.unwrap(); // P3
        demo.reconcile().await.unwrap();
        assert_eq!(demo.view().unwrap().earned_sats, [8, 0, 1980, 12]);
        assert_eq!(demo.saved.pot.phase, Phase::Settled);
        assert!(log(&demo).contains("Player 3 wins"));
        assert!(
            log(&demo).contains("Player 3 received 1980 sats • settlement → player3@example.test")
        );
        conserved(&demo);
    });
}

#[test]
fn team_match_excludes_cpu_and_friendly_fire_and_logs_the_winning_team() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(roster(true, [0, 0, 0, 1], [0, 1, 0, 1])))
            .await
            .unwrap();
        verify_humans(&mut demo).await;
        assert!(demo.command(Command::Action(10)).await.is_err());
        fund_humans(&mut demo).await;
        assert!(demo.saved.addresses[3].is_empty());
        demo.command(Command::Action(4)).await.unwrap();
        demo.command(Command::Credits(vec![
            Credit {
                sequence: 1,
                attacker: 0,
                victim: 1,
                damage_centi: 800,
            },
            Credit {
                sequence: 2,
                attacker: 0,
                victim: 2,
                damage_centi: 1000,
            },
            Credit {
                sequence: 3,
                attacker: 3,
                victim: 0,
                damage_centi: 1000,
            },
            Credit {
                sequence: 4,
                attacker: 0,
                victim: 3,
                damage_centi: 1000,
            },
        ]))
        .await
        .unwrap();
        assert_eq!(demo.saved.pot.remaining, [500, 492, 500, 0]);
        assert_eq!(demo.saved.pot.pending(), [8, 0, 0, 0]);
        demo.command(Command::Action(12)).await.unwrap(); // CPU represents winning Blue side.
        demo.command(Command::Action(7)).await.unwrap(); // Same Blue result from a human representative.
        demo.reconcile().await.unwrap();
        assert_eq!(
            demo.saved
                .events
                .iter()
                .filter(|event| event.contains("Blue Team wins"))
                .count(),
            1
        );
        assert_eq!(demo.view().unwrap().earned_sats, [8, 1492, 0, 0]);
        assert!(log(&demo).contains("Blue Team wins"));
        assert!(demo.saved.receipts.iter().all(|r| r.player != 3));
        conserved(&demo);
    });
}

#[test]
fn funded_roster_change_returns_entries_before_issuing_new_match_invoices() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        demo.command(Command::Action(2)).await.unwrap();
        demo.reconcile().await.unwrap();
        let old_id = demo.saved.pot.id.clone();
        let old_invoices = demo.saved.invoices.clone();
        demo.command(Command::Roster(teams())).await.unwrap();
        assert_eq!(demo.saved.pot.phase, Phase::Settling);
        assert_eq!(demo.saved.pot.id, old_id);
        assert_eq!(demo.saved.next_roster, Some(teams()));
        assert!(demo.command(Command::Action(4)).await.is_err());
        demo.reconcile().await.unwrap();
        assert_eq!(demo.saved.roster, Some(teams()));
        assert_eq!(demo.saved.next_roster, None);
        assert_ne!(demo.saved.pot.id, old_id);
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(demo.saved.pot.remaining, [0; 4]);
        assert_eq!(demo.saved.pot.phase, Phase::Funding);
        assert!(
            demo.saved
                .invoices
                .iter()
                .zip(old_invoices)
                .all(|(a, b)| *a != b)
        );
        assert_eq!(demo.saved.receipts.len(), 1);
        assert_eq!(
            (demo.saved.receipts[0].player, demo.saved.receipts[0].sats),
            (0, 500)
        );
        assert!(log(&demo).contains("Roster changed"));
        let archive = dir.path().join("matches").join(format!("{old_id}.json"));
        let previous: Saved = serde_json::from_slice(&std::fs::read(archive).unwrap()).unwrap();
        assert_eq!(previous.pot.phase, Phase::Settled);
        assert_eq!(previous.pot.earned, [500, 0, 0, 0]);
        conserved(&demo);
    });
}

#[test]
fn unresolved_return_does_not_enable_or_refund_a_new_roster_twice() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        demo.command(Command::Action(2)).await.unwrap();
        demo.reconcile().await.unwrap();
        let old_id = demo.saved.pot.id.clone();
        demo.command(Command::Roster(teams())).await.unwrap();
        let payout = demo.saved.pot.payouts[0].clone();
        let address = parse_address(&demo.saved.addresses[0]).unwrap();
        let prepared = prepare_payout(
            demo.wallet.as_ref(),
            demo.provider.as_ref(),
            &address,
            payout.sats,
            &payout.id,
        )
        .await
        .unwrap();
        let payment_hash = prepared.payment_hash.clone();
        demo.saved.prepared.insert(payout.id, prepared);
        demo.saved.pot.begin_payout(0).unwrap();
        demo.save().unwrap();
        drop(demo);
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.reconcile().await.unwrap(); // Fake wallet cannot know this interrupted payment's result.
        assert_eq!(demo.saved.pot.id, old_id);
        assert_eq!(demo.saved.pot.phase, Phase::Settling);
        assert_eq!(demo.saved.next_roster, Some(teams()));
        assert_eq!(demo.simulation.as_ref().unwrap().payment_attempts(), 0);
        assert!(demo.command(Command::Action(4)).await.is_err());
        let changed = roster(true, [0; 4], [0, 0, 1, 1]);
        demo.command(Command::Roster(changed.clone()))
            .await
            .unwrap();
        assert_eq!(demo.saved.pot.id, old_id);
        demo.simulation
            .as_ref()
            .unwrap()
            .set_payment_state(&payment_hash, PaymentState::Succeeded { fee_sats: 0 });
        demo.reconcile().await.unwrap();
        assert_ne!(demo.saved.pot.id, old_id);
        assert_eq!(demo.saved.roster, Some(changed));
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(demo.saved.receipts.len(), 1);
        assert_eq!(demo.simulation.as_ref().unwrap().payment_attempts(), 0);
        conserved(&demo);
    });
}

#[test]
fn restart_and_no_contest_keep_credited_earnings_without_replaying_transfers() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        fund_humans(&mut demo).await;
        demo.command(Command::Action(4)).await.unwrap();
        let credits = vec![Credit {
            sequence: 1,
            attacker: 3,
            victim: 0,
            damage_centi: 1000,
        }];
        demo.command(Command::Credits(credits.clone()))
            .await
            .unwrap();
        drop(demo);
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Credits(credits)).await.unwrap();
        assert_eq!(demo.view().unwrap().last_credit_sequence, 1);
        assert_eq!(demo.saved.pot.pending(), [0, 0, 0, 10]);
        demo.command(Command::Action(8)).await.unwrap();
        demo.reconcile().await.unwrap();
        assert_eq!(demo.view().unwrap().earned_sats, [490, 500, 500, 510]);
        let receipts = demo.saved.receipts.len();
        demo.command(Command::Action(8)).await.unwrap();
        demo.reconcile().await.unwrap();
        assert_eq!(demo.saved.receipts.len(), receipts);
        conserved(&demo);
        demo.command(Command::Action(1)).await.unwrap();
        assert_eq!(demo.saved.roster, Some(ffa()));
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(demo.saved.pot.last_credit_sequence, 0);
        assert_eq!(demo.saved.pot.remaining, [0; 4]);
    });
}

#[test]
fn confirmed_entry_is_accounted_and_refunded_when_another_receivers_check_expires() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.command(Command::Roster(ffa())).await.unwrap();
        verify_humans(&mut demo).await;
        let old_id = demo.saved.pot.id.clone();
        let payment_hash = demo.saved.hashes[0].clone();
        demo.simulation
            .as_ref()
            .unwrap()
            .set_invoice_state(&payment_hash, InvoiceState::Paid { amount_sats: 500 });
        // The wallet receipt has arrived, but another receiver expires before
        // the host observes it. Receipt accounting must not depend on admission.
        demo.saved.verification[3]
            .report
            .as_mut()
            .unwrap()
            .checked_at_ms = now_ms() - sats_arena_core::wallet::VERIFICATION_TTL_MS;
        demo.reconcile().await.unwrap();
        assert_eq!(demo.saved.pot.paid, [true, false, false, false]);
        assert_eq!(demo.saved.pot.remaining, [500, 0, 0, 0]);
        assert!(demo.command(Command::Action(4)).await.is_err());
        conserved(&demo);
        demo.command(Command::Roster(teams())).await.unwrap();
        demo.reconcile().await.unwrap();
        assert_ne!(demo.saved.pot.id, old_id);
        assert_eq!(demo.saved.pot.paid, [false; 4]);
        assert_eq!(demo.saved.receipts.len(), 1);
        assert_eq!(
            (demo.saved.receipts[0].player, demo.saved.receipts[0].sats),
            (0, 500)
        );
        assert_eq!(demo.view().unwrap().verification_state[3], 4);
        let archive = dir.path().join("matches").join(format!("{old_id}.json"));
        let settled: Saved = serde_json::from_slice(&std::fs::read(archive).unwrap()).unwrap();
        assert_eq!(settled.pot.phase, Phase::Settled);
        assert_eq!(settled.pot.earned, [500, 0, 0, 0]);
        conserved(&demo);
    });
}

#[test]
fn schema_two_migration_preserves_two_player_funds_payout_ids_and_fractional_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut original = Demo::open(dir.path()).await.unwrap();
        verify_humans(&mut original).await;
        // Serialize the actual old two-slot pot type, rather than a four-slot
        // state with its schema number changed. P2 already has a planned payout;
        // P1 has earned half a sat that only the old cumulative counter records.
        let old_id = original.saved.pot.id.clone();
        let mut old = sats_arena_core::pot::MatchPot::new(old_id.clone(), 500, 10).unwrap();
        old.confirm_entry(0, 500).unwrap();
        old.confirm_entry(1, 500).unwrap();
        old.start().unwrap();
        old.credit_at([100, 5], now_ms().saturating_sub(6_000))
            .unwrap();
        old.flush_due(now_ms());
        assert_eq!(old.damage_centi, [100, 5]);
        assert_eq!(old.unbatched, [0, 0]);
        assert_eq!(old.payouts.len(), 1);
        assert_eq!((old.payouts[0].player, old.payouts[0].sats), (1, 10));
        let old_payout_id = old.payouts[0].id.clone();
        let old_invoices = original.saved.invoices[..2].to_vec();
        let old_hashes = original.saved.hashes[..2].to_vec();
        let mut json = serde_json::to_value(&original.saved).unwrap();
        json["schema"] = serde_json::json!(2);
        json["pot"] = serde_json::to_value(old).unwrap();
        for key in ["invoices", "hashes", "addresses", "verification"] {
            json[key].as_array_mut().unwrap().truncate(2);
        }
        for key in ["roster", "next_roster", "events"] {
            json.as_object_mut().unwrap().remove(key);
        }
        assert!(json["pot"].get("kinds").is_none());
        assert!(json["pot"].get("fractional_sats").is_none());
        std::fs::write(
            dir.path().join("prepaid-match.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        drop(original);
        let mut migrated = Demo::open(dir.path()).await.unwrap();
        assert_eq!(migrated.saved.schema, 3);
        assert_eq!(migrated.saved.pot.id, old_id);
        assert_eq!(migrated.saved.pot.kinds, [0, 0, 3, 3]);
        assert_eq!(migrated.saved.pot.paid, [true, true, false, false]);
        assert_eq!(migrated.saved.pot.remaining, [490, 500, 0, 0]);
        assert_eq!(migrated.saved.pot.payouts[0].id, old_payout_id);
        assert_eq!(&migrated.saved.invoices[..2], old_invoices.as_slice());
        assert_eq!(&migrated.saved.hashes[..2], old_hashes.as_slice());
        assert!(migrated.saved.invoices[2..].iter().all(String::is_empty));
        assert_eq!(migrated.saved.pot.fractional_sats[0][1], 50);
        migrated.command(Command::Credit([100, 10])).await.unwrap();
        assert_eq!(migrated.saved.pot.unbatched, [1, 0, 0, 0]);
        assert_eq!(migrated.saved.pot.remaining, [490, 499, 0, 0]);
        migrated.reconcile().await.unwrap();
        assert_eq!(migrated.saved.pot.earned, [0, 10, 0, 0]);
        assert_eq!(migrated.saved.pot.payouts[0].id, old_payout_id);
        assert_eq!(migrated.saved.receipts.len(), 1);
        assert_eq!(migrated.saved.receipts[0].id, old_payout_id);
        conserved(&migrated);
        migrated.command(Command::Action(6)).await.unwrap();
        migrated.reconcile().await.unwrap();
        assert_eq!(migrated.saved.pot.earned, [990, 10, 0, 0]);
        assert_eq!(migrated.saved.receipts.len(), 2);
        assert_eq!(migrated.simulation.as_ref().unwrap().payment_attempts(), 2);
        assert_ne!(
            migrated.saved.pot.payouts[0].id,
            migrated.saved.pot.payouts[1].id
        );
        assert_eq!(migrated.saved.pot.phase, Phase::Settled);
        conserved(&migrated);
    });
}

#[test]
fn activity_is_chronological_within_session_and_restart_starts_a_new_log() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        verify_humans(&mut demo).await;
        fund_humans(&mut demo).await;
        demo.command(Command::Action(4)).await.unwrap();
        demo.command(Command::Credits(vec![Credit {
            sequence: 1,
            attacker: 0,
            victim: 1,
            damage_centi: 800,
        }]))
        .await
        .unwrap();
        demo.saved.pot.batch_deadline_ms[0] = Some(0);
        demo.reconcile().await.unwrap();
        demo.command(Command::Action(7)).await.unwrap();
        demo.reconcile().await.unwrap();
        demo.command(Command::Action(1)).await.unwrap();
        fund_humans(&mut demo).await;
        let lines = log(&demo);
        let first_invoice = lines.find("Player 1 Invoice Paid — 500 sats").unwrap();
        let damage = lines.find("Player 1 received 8 sats • damage").unwrap();
        let winner = lines.find("Player 2 wins").unwrap();
        let settlement = lines
            .find("Player 2 received 992 sats • settlement")
            .unwrap();
        let next_invoice = lines.rfind("Player 1 Invoice Paid — 500 sats").unwrap();
        assert!(
            first_invoice < damage
                && damage < winner
                && winner < settlement
                && settlement < next_invoice
        );
        assert!(lines.lines().all(|line| line.as_bytes()[8] == b'Z'));
        assert!(!lines.contains("Entry P"));
        let receipt_count = demo.saved.receipts.len();
        let payout_ids = demo.saved.used_payment_hashes.clone();
        let paid = demo.saved.pot.paid;
        let invoices = demo.saved.invoices.clone();
        drop(demo);
        let mut recovered = Demo::open(dir.path()).await.unwrap();
        assert!(log(&recovered).is_empty());
        assert!(recovered.saved.activity.is_empty());
        assert!(recovered.saved.events.is_empty());
        assert_eq!(recovered.saved.receipts.len(), receipt_count);
        assert_eq!(recovered.saved.used_payment_hashes, payout_ids);
        assert_eq!(recovered.saved.pot.paid, paid);
        assert_eq!(recovered.saved.invoices, invoices);
        recovered.reconcile().await.unwrap();
        assert!(log(&recovered).is_empty());
        assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 0);
        recovered.command(Command::Action(4)).await.unwrap();
        recovered.command(Command::Action(8)).await.unwrap();
        recovered.reconcile().await.unwrap();
        let new_log = log(&recovered);
        assert!(new_log.contains("No contest"));
        assert!(!new_log.contains("Invoice Paid"));
        assert!(!new_log.contains("damage"));
        assert_eq!(recovered.saved.receipts.len(), receipt_count + 2);
        conserved(&recovered);
    });
}

#[test]
fn one_sat_default_updates_unfunded_lobbies_but_preserves_paid_agreements() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        assert_eq!(demo.saved.pot.sats_per_percent, 1);
        demo.saved.pot.sats_per_percent = 10;
        demo.save().unwrap();
        drop(demo);
        let mut demo = Demo::open(dir.path()).await.unwrap();
        assert_eq!(demo.saved.pot.sats_per_percent, 1);
        demo.saved.pot.sats_per_percent = 10;
        demo.saved.pot.confirm_entry(0, 500).unwrap();
        demo.save().unwrap();
        drop(demo);
        let demo = Demo::open(dir.path()).await.unwrap();
        assert_eq!(demo.saved.pot.sats_per_percent, 10);
    });
}

#[test]
fn one_process_owns_a_financial_state_folder_until_it_closes() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let first = Demo::open(dir.path()).await.unwrap();
        assert!(Demo::open(dir.path()).await.is_err());
        drop(first);
        assert!(Demo::open(dir.path()).await.is_ok());
    });
}
