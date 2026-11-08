use super::*;
use sats_arena_core::wallet::{
    AddressVerification, AddressVerifier, FakeAddressInvoices, FakeWallet, InvoiceState,
    LightningAddress, Wallet,
};
use sats_arena_protocol::netplay::StartManifest;
use std::time::{SystemTime, UNIX_EPOCH};

struct Verify;
#[async_trait::async_trait]
impl AddressVerifier for Verify {
    async fn verify(&self, address: &LightningAddress) -> anyhow::Result<AddressVerification> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Ok(AddressVerification {
            address: address.as_str().into(),
            min_msat: 1000,
            max_msat: 1_000_000_000,
            callback_host: "example.com".into(),
            checked_at_ms: now,
            test_invoice_msat: 1000,
            test_payment_hash: "1".repeat(64),
            invoice_expires_at_ms: now + 600_000,
            description_binding: "test".into(),
        })
    }
}
fn rules() -> HostRules {
    HostRules {
        entry_sats: 500,
        sats_per_percent: 1,
        batch_window_ms: 1000,
        surcharge_bps: 0,
    }
}
fn wallet(fake: Arc<FakeWallet>) -> HostWallet {
    HostWallet {
        wallet: fake,
        provider: Arc::new(FakeAddressInvoices),
        verifier: Arc::new(Verify),
        identity: "f".repeat(64),
        mode: "fake".into(),
    }
}
fn token(slot: usize) -> String {
    char::from(b'a' + slot as u8).to_string().repeat(64)
}
fn headers(slot: usize) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", token(slot)).parse().unwrap(),
    );
    h
}
fn manifest() -> StartManifest {
    StartManifest {
        game_number: 1,
        stage: 31,
        seed: 42,
        teams: false,
        kinds: [0, 0, 3, 3],
        team_ids: [255; 4],
        characters: [17, 17, 255, 255],
    }
}
fn credit(sequence: u64, amount: u64) -> DamageCredit {
    DamageCredit {
        sequence,
        frame: sequence as i32 * 10,
        attacker: 0,
        victim: 1,
        damage_centi: amount,
    }
}
fn report(id: &str, credits: Vec<DamageCredit>, end: Option<EndReport>) -> Command {
    Command::Report {
        report: MatchObservation {
            match_id: id.into(),
            manifest: manifest(),
            credits,
            end,
        },
    }
}

struct RepeatedRefund;
#[async_trait::async_trait]
impl sats_arena_core::wallet::AddressInvoices for RepeatedRefund {
    async fn invoice(&self, _: &LightningAddress, sats: u64, _: &str) -> anyhow::Result<String> {
        Ok(format!("lnfake:{sats}:same-recipient-invoice"))
    }
}

#[tokio::test]
async fn different_rooms_cannot_settle_debts_using_one_recipient_payment_proof_even_after_restart()
{
    for restart in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeWallet::default());
        let mut config = wallet(fake.clone());
        config.provider = Arc::new(RepeatedRefund);
        let mut payments = Payments::open(temp.path().into(), rules(), config.clone(), None)
            .await
            .ok()
            .unwrap();
        for index in 1..=2 {
            let id = index.to_string().repeat(32);
            payments
                .create(&id, &token(0), RoomConfig::default())
                .await
                .ok()
                .unwrap();
            payments
                .authenticated(
                    &id,
                    &headers(0),
                    Some(Command::SetAddress {
                        address: "player0@example.com".into(),
                    }),
                )
                .await
                .ok()
                .unwrap();
            pump(&payments, 20).await;
            fund(&payments, &fake, &id, &[0]).await;
            payments.abandon(&id).await.ok().unwrap();
            if index == 1 {
                settle(&payments, &id).await;
                assert_eq!(
                    payments.view(&id).await.ok().unwrap().players[0].earned_sats,
                    500
                );
                if restart {
                    drop(payments);
                    payments = Payments::open(temp.path().into(), rules(), config.clone(), None)
                        .await
                        .ok()
                        .unwrap();
                }
            } else {
                for _ in 0..100 {
                    payments.maintain().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let second = payments.view(&id).await.ok().unwrap();
                assert_eq!(second.players[0].earned_sats, 0);
                assert_eq!(second.players[0].pending_sats, 500);
                assert_eq!(second.phase, 6);
            }
        }
        assert_eq!(fake.payment_attempts(), 1);
    }
}

struct RepeatedEntry(Arc<FakeWallet>);
#[async_trait::async_trait]
impl Wallet for RepeatedEntry {
    async fn create_invoice(
        &self,
        sats: u64,
        _: &str,
        _: u64,
    ) -> anyhow::Result<sats_arena_core::wallet::NewInvoice> {
        let bolt11 = format!("lnfake:{sats}:same-recipient-invoice");
        let payment_hash = self.0.decode_invoice(&bolt11)?.payment_hash;
        Ok(sats_arena_core::wallet::NewInvoice {
            bolt11,
            payment_hash,
        })
    }
    async fn invoice_state(&self, hash: &str) -> anyhow::Result<InvoiceState> {
        self.0.invoice_state(hash).await
    }
    fn decode_invoice(
        &self,
        invoice: &str,
    ) -> anyhow::Result<sats_arena_core::wallet::DecodedInvoice> {
        self.0.decode_invoice(invoice)
    }
    async fn pay_invoice(
        &self,
        invoice: &str,
        fees: u64,
    ) -> anyhow::Result<sats_arena_core::wallet::PaymentState> {
        self.0.pay_invoice(invoice, fees).await
    }
    async fn payment_state(
        &self,
        hash: &str,
    ) -> anyhow::Result<sats_arena_core::wallet::PaymentState> {
        self.0.payment_state(hash).await
    }
}

#[tokio::test]
async fn an_incoming_receipt_cannot_fund_two_rooms_or_be_reused_for_an_outgoing_debt() {
    for restart in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeWallet::default());
        let mut config = wallet(fake.clone());
        config.wallet = Arc::new(RepeatedEntry(fake.clone()));
        config.provider = Arc::new(RepeatedRefund);
        let mut payments = Payments::open(temp.path().into(), rules(), config.clone(), None)
            .await
            .ok()
            .unwrap();
        let first = "1".repeat(32);
        payments
            .create(&first, &token(0), RoomConfig::default())
            .await
            .ok()
            .unwrap();
        payments
            .authenticated(
                &first,
                &headers(0),
                Some(Command::SetAddress {
                    address: "player0@example.com".into(),
                }),
            )
            .await
            .ok()
            .unwrap();
        pump(&payments, 20).await;
        fund(&payments, &fake, &first, &[0]).await;
        assert_eq!(
            payments.view(&first).await.ok().unwrap().players[0].paid_sats,
            500
        );
        let provider = payments.0.lock().await.wallet.provider.clone();
        assert!(
            provider
                .invoice(
                    &LightningAddress::parse("player0@example.com").unwrap(),
                    500,
                    "another-debt"
                )
                .await
                .is_err()
        );
        if restart {
            drop(provider);
            drop(payments);
            payments = Payments::open(temp.path().into(), rules(), config, None)
                .await
                .ok()
                .unwrap();
        }
        let second = "2".repeat(32);
        payments
            .create(&second, &token(0), RoomConfig::default())
            .await
            .ok()
            .unwrap();
        payments
            .authenticated(
                &second,
                &headers(0),
                Some(Command::SetAddress {
                    address: "player0@example.com".into(),
                }),
            )
            .await
            .ok()
            .unwrap();
        for _ in 0..40 {
            payments.maintain().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let view = payments.view(&second).await.ok().unwrap();
        assert!(view.players[0].invoice.is_empty());
        assert_eq!(view.players[0].paid_sats, 0);
        assert!(payments.admitted(&second).await.is_err());
        assert_eq!(fake.payment_attempts(), 0);
    }
}
pub(crate) async fn pump(payments: &Payments, rounds: usize) {
    for _ in 0..rounds {
        {
            let mut guard = payments.0.lock().await;
            for record in guard.records.values_mut() {
                record.host.tick().await.unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
pub(crate) async fn settle(payments: &Payments, id: &str) {
    for _ in 0..1200 {
        pump(payments, 1).await;
        if payments.view(id).await.ok().unwrap().phase == 4 {
            return;
        }
    }
    panic!("Payouts did not reach confirmed settlement");
}
pub(crate) async fn prepared() -> (tempfile::TempDir, Payments, Arc<FakeWallet>, String) {
    prepared_config(RoomConfig::default()).await
}
async fn prepared_config(
    config: RoomConfig,
) -> (tempfile::TempDir, Payments, Arc<FakeWallet>, String) {
    let temp = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeWallet::default());
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    let id = "1".repeat(32);
    assert!(
        payments
            .create(&id, &token(0), config.clone())
            .await
            .is_ok()
    );
    for slot in 1..config.player_count as usize {
        assert!(
            payments
                .add_member(&id, slot as u8, &token(slot))
                .await
                .is_ok()
        );
    }
    for slot in 0..config.player_count as usize {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(Command::SetAddress {
                        address: format!("player{slot}@example.com")
                    })
                )
                .await
                .is_ok()
        );
    }
    pump(&payments, 20).await;
    (temp, payments, fake, id)
}
pub(crate) async fn fund(payments: &Payments, fake: &FakeWallet, id: &str, slots: &[usize]) {
    let snapshot = payments.view(id).await.ok().unwrap();
    for slot in slots {
        assert!(!snapshot.players[*slot].invoice.is_empty());
        let hash = fake
            .decode_invoice(&snapshot.players[*slot].invoice)
            .unwrap()
            .payment_hash;
        fake.set_invoice_state(
            &hash,
            InvoiceState::Paid {
                amount_sats: snapshot.invoice_sats,
            },
        );
    }
    for _ in 0..600 {
        pump(payments, 1).await;
        let view = payments.view(id).await.ok().unwrap();
        if slots
            .iter()
            .all(|slot| view.players[*slot].paid_sats == view.invoice_sats)
        {
            return;
        }
    }
    panic!("Wallet receipts were not confirmed");
}

#[tokio::test]
async fn confirmed_entries_and_two_reports_gate_all_funds_and_retries_are_idempotent() {
    let (_temp, payments, fake, id) = prepared().await;
    assert!(payments.admitted(&id).await.is_err());
    fund(&payments, &fake, &id, &[0]).await;
    assert!(payments.admitted(&id).await.is_err());
    fund(&payments, &fake, &id, &[1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(&id, vec![credit(1, 10_000)], None))
            )
            .await
            .is_ok()
    );
    assert_eq!(payments.view(&id).await.ok().unwrap().agreed_sequence, 0);
    assert!(
        payments
            .authenticated(&id, &headers(1), Some(report(&id, vec![], None)))
            .await
            .is_ok()
    );
    assert_eq!(payments.view(&id).await.ok().unwrap().agreed_sequence, 0);
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(report(&id, vec![credit(1, 10_000)], None))
            )
            .await
            .is_ok()
    );
    for _ in 0..3 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(0),
                    Some(report(&id, vec![credit(1, 10_000)], None))
                )
                .await
                .is_ok()
        );
    }
    let snapshot = payments.view(&id).await.ok().unwrap();
    assert_eq!(snapshot.agreed_sequence, 1);
    assert_eq!(snapshot.players[0].pending_sats, 100);
    let end = EndReport {
        last_sequence: 1,
        winner: Some(0),
        complete: true,
    };
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(&id, vec![], Some(end.clone())))
            )
            .await
            .is_ok()
    );
    assert_eq!(payments.view(&id).await.ok().unwrap().phase, 3);
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(report(&id, vec![], Some(end.clone())))
            )
            .await
            .is_ok()
    );
    settle(&payments, &id).await;
    let settled = payments.view(&id).await.ok().unwrap();
    assert_eq!(settled.phase, 4);
    assert_eq!(settled.players[0].earned_sats, 1000);
    assert_eq!(settled.players[1].earned_sats, 0);
    let attempts = fake.payment_attempts();
    for slot in 0..2 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(report(&id, vec![], Some(end.clone())))
                )
                .await
                .is_ok()
        );
    }
    pump(&payments, 10).await;
    assert_eq!(fake.payment_attempts(), attempts);
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(&id, vec![credit(1, 11_000)], Some(end)))
            )
            .await
            .is_err()
    );
    assert!(!payments.view(&id).await.ok().unwrap().frozen);
}

#[tokio::test]
async fn agreed_self_damage_advances_sequence_without_moving_funds() {
    let (_temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    let mut observation = credit(1, 10_000);
    observation.victim = observation.attacker;
    for slot in 0..2 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(report(&id, vec![observation.clone()], None))
                )
                .await
                .is_ok()
        );
    }
    let snapshot = payments.view(&id).await.ok().unwrap();
    assert_eq!(snapshot.agreed_sequence, 1);
    assert_eq!(snapshot.players[0].remaining_sats, 500);
    assert_eq!(snapshot.players[1].remaining_sats, 500);
    assert_eq!(snapshot.players[0].pending_sats, 0);
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn disagreement_freezes_durably_and_offline_review_can_refund() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(&id, vec![credit(1, 10_000)], None))
            )
            .await
            .is_ok()
    );
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(report(&id, vec![credit(1, 20_000)], None))
            )
            .await
            .is_ok()
    );
    assert!(payments.view(&id).await.ok().unwrap().frozen);
    pump(&payments, 20).await;
    assert_eq!(fake.payment_attempts(), 0);
    drop(payments);
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    assert!(
        payments
            .authenticated(&id, &headers(0), None)
            .await
            .ok()
            .unwrap()
            .frozen
    );
    assert!(
        payments
            .refund_after_review(
                &id,
                "Friends reviewed the disagreement; return entries".into()
            )
            .await
            .is_ok()
    );
    settle(&payments, &id).await;
    let snapshot = payments.view(&id).await.ok().unwrap();
    assert_eq!(snapshot.phase, 4);
    assert_eq!(snapshot.players[0].earned_sats, 500);
    assert_eq!(snapshot.players[1].earned_sats, 500);
}

#[tokio::test]
async fn unstarted_cancellation_refunds_and_tokens_survive_room_restart_without_secrets() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.abandon(&id).await.is_ok());
    settle(&payments, &id).await;
    assert_eq!(payments.view(&id).await.ok().unwrap().phase, 4);
    let disk = std::fs::read_to_string(temp.path().join(&id).join("observations.json")).unwrap();
    assert!(!disk.contains(&token(0)) && !disk.contains(&token(1)));
    drop(payments);
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake), None)
        .await
        .unwrap();
    assert_eq!(
        payments
            .authenticated(&id, &headers(1), None)
            .await
            .ok()
            .unwrap()
            .phase,
        4
    );
    let mut outsider = HeaderMap::new();
    outsider.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", "c".repeat(64)).parse().unwrap(),
    );
    assert!(payments.authenticated(&id, &outsider, None).await.is_err());
}

#[tokio::test]
async fn gaps_and_incomplete_or_changed_results_cannot_authorize_payouts() {
    let (_temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(&id, vec![credit(2, 1000)], None))
            )
            .await
            .is_err()
    );
    assert_eq!(
        payments.view(&id).await.ok().unwrap().own_next_sequence,
        [1, 1, 1, 1]
    );
    let end = EndReport {
        last_sequence: 15,
        winner: None,
        complete: false,
    };
    assert!(
        payments
            .authenticated(&id, &headers(0), Some(report(&id, vec![], Some(end))))
            .await
            .is_ok()
    );
    assert!(payments.view(&id).await.ok().unwrap().frozen);
    pump(&payments, 20).await;
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn one_complete_end_cannot_omit_the_other_players_later_credit() {
    let (_temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(report(
                    &id,
                    vec![],
                    Some(EndReport {
                        last_sequence: 0,
                        winner: Some(0),
                        complete: true
                    })
                ))
            )
            .await
            .is_ok()
    );
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(report(&id, vec![credit(1, 1000)], None))
            )
            .await
            .is_ok()
    );
    assert!(payments.view(&id).await.ok().unwrap().frozen);
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn saved_close_intent_replays_cancellation_after_crash() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    {
        let mut coordinator = payments.0.lock().await;
        let record = coordinator.records.get_mut(&id).unwrap();
        record.saved.closed = true;
        persist(&record.path, &record.saved).unwrap();
        // Process dies after recording cancellation, before the host transition.
    }
    drop(payments);
    // A crash between mkdir and initial journal must not block recovery of existing funds.
    std::fs::create_dir(temp.path().join("2".repeat(32))).unwrap();
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    settle(&payments, &id).await;
    let view = payments.view(&id).await.ok().unwrap();
    assert_eq!(view.players[0].earned_sats, 500);
    assert_eq!(view.players[1].earned_sats, 500);
    assert_eq!(fake.payment_attempts(), 2);
}

#[tokio::test]
async fn creation_and_operator_review_intents_recover_across_crash_boundaries() {
    let (temp, payments, fake, id) = prepared().await;
    let initial = payments
        .0
        .lock()
        .await
        .records
        .get(&id)
        .unwrap()
        .saved
        .clone();
    let orphan = tempfile::tempdir().unwrap();
    let folder = orphan.path().join(&id);
    std::fs::create_dir(&folder).unwrap();
    persist(&folder.join("observations.json"), &initial).unwrap();
    let recovered = Payments::open(orphan.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    assert_eq!(recovered.view(&id).await.ok().unwrap().phase, 4);
    drop(recovered);
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    {
        let mut coordinator = payments.0.lock().await;
        let record = coordinator.records.get_mut(&id).unwrap();
        record
            .freeze("Explicit interruption test")
            .unwrap_or_else(|_| panic!("Could not freeze"));
        record.saved.closed = true;
        record.saved.review_reason =
            Some("Operator checked both clients and authorized refunds".into());
        persist(&record.path, &record.saved).unwrap();
    }
    drop(payments);
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    settle(&payments, &id).await;
    assert!(!payments.view(&id).await.ok().unwrap().frozen);
}

#[tokio::test]
async fn one_surviving_client_cannot_keep_the_other_members_financial_lease_alive() {
    for admitted in [false, true] {
        let (_temp, payments, fake, id) = prepared().await;
        fund(&payments, &fake, &id, &[0, 1]).await;
        if admitted {
            assert!(payments.admitted(&id).await.is_ok());
        }
        {
            let mut coordinator = payments.0.lock().await;
            coordinator.records.get_mut(&id).unwrap().seen[1] = Some(Instant::now() - LEASE);
        }
        assert!(payments.authenticated(&id, &headers(0), None).await.is_ok());
        payments.maintain().await;
        if admitted {
            assert!(payments.view(&id).await.ok().unwrap().frozen);
            assert_eq!(fake.payment_attempts(), 0);
        } else {
            settle(&payments, &id).await;
            assert_eq!(
                payments.view(&id).await.ok().unwrap().players[0].earned_sats,
                500
            );
            assert_eq!(
                payments.view(&id).await.ok().unwrap().players[1].earned_sats,
                500
            );
        }
    }
}

#[tokio::test]
async fn paid_room_http_routes_share_the_financial_id_and_survive_room_deletion() {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    async fn request(
        app: &Router,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    let (_temp, payments, fake, _) = prepared().await;
    let app = crate::rooms::router_with_payments("frontend/dist".into(), Some(payments.clone()));
    let player = |port| json!({"name":"Friends", "build":"test", "rules":"test", "endpoint":format!("127.0.0.1:{port}")});
    let (status, host) = request(&app, "POST", "/netplay/rooms", "", player(45001)).await;
    assert_eq!(status, StatusCode::OK);
    let path = format!("/netplay/rooms/{}", host["room"]["code"].as_str().unwrap());
    let id = host["room"]["payment"]["match_id"].as_str().unwrap();
    let financial = format!("/netplay/matches/{id}");
    let host_token = host["token"].as_str().unwrap();
    let (status, guest) = request(&app, "POST", &format!("{path}/join"), "", player(45002)).await;
    assert_eq!(status, StatusCode::OK);
    let guest_token = guest["token"].as_str().unwrap();
    assert_eq!(
        request(&app, "POST", &format!("{path}/join"), "", player(45003))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&app, "GET", &financial, &token(0), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for (slot, bearer) in [host_token, guest_token].iter().enumerate() {
        assert_eq!(
            request(
                &app,
                "POST",
                &financial,
                bearer,
                json!({"type":"set_address", "address":format!("http{slot}@example.com")})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &path,
                bearer,
                json!({"type":"ready", "ready":true})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(&app, "POST", &path, host_token, json!({"type":"start"}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    pump(&payments, 20).await;
    fund(&payments, &fake, id, &[0, 1]).await;
    let (status, started) = request(&app, "POST", &path, host_token, json!({"type":"start"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(started["match_id"], id);
    assert_eq!(started["payment"]["match_id"], id);
    assert_eq!(
        request(&app, "POST", &path, guest_token, json!({"type":"leave"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", &path, host_token, Value::Null).await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "POST", &path, host_token, json!({"type":"close"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", &path, host_token, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, retained) = request(&app, "GET", &financial, host_token, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retained["frozen"], true);
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn journaled_reports_replay_once_and_incomplete_crash_intent_freezes_on_restart() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    {
        let mut guard = payments.0.lock().await;
        let record = guard.records.get_mut(&id).unwrap();
        for observation in &mut record.saved.observations[..2] {
            observation.manifest = Some(manifest());
            observation.credits = vec![credit(1, 10_000)];
        }
        persist(&record.path, &record.saved).unwrap();
        // Process dies after report WAL, before the shared host applies credits.
        assert_eq!(record.host.snapshot().last_credit_sequence, 0);
    }
    drop(payments);
    for _ in 0..2 {
        let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
            .await
            .unwrap();
        let view = payments.view(&id).await.ok().unwrap();
        assert_eq!(view.agreed_sequence, 1);
        assert_eq!(view.players[0].pending_sats, 100);
        assert_eq!(view.players[1].remaining_sats, 400);
        drop(payments);
    }
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    {
        let mut guard = payments.0.lock().await;
        let record = guard.records.get_mut(&id).unwrap();
        record.saved.observations[0].end = Some(EndReport {
            last_sequence: 2,
            winner: None,
            complete: false,
        });
        persist(&record.path, &record.saved).unwrap();
        // Process dies before persisting freeze. One incomplete end alone is sufficient.
    }
    drop(payments);
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    assert!(payments.view(&id).await.ok().unwrap().frozen);
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn completed_host_refund_recovers_before_review_journal_acknowledgement() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    {
        let mut guard = payments.0.lock().await;
        let record = guard.records.get_mut(&id).unwrap();
        assert!(record.freeze("Operator review test").is_ok());
        record.saved.closed = true;
        record.saved.review_reason = Some("Both friends confirmed the interrupted game".into());
        persist(&record.path, &record.saved).unwrap();
        record.host.refund_after_review().await.unwrap();
        assert!(!record.host.snapshot().frozen);
        assert!(record.saved.frozen);
        // Crash after host NoContest save, before acknowledging it in observations.json.
    }
    drop(payments);
    let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
        .await
        .unwrap();
    settle(&payments, &id).await;
    let view = payments.view(&id).await.ok().unwrap();
    assert!(!view.frozen);
    assert_eq!(view.players[0].earned_sats, 500);
    assert_eq!(view.players[1].earned_sats, 500);
    assert_eq!(fake.payment_attempts(), 2);
}

fn configured_report(
    id: &str,
    config: &RoomConfig,
    credits: Vec<DamageCredit>,
    end: Option<EndReport>,
) -> Command {
    let mut manifest = manifest();
    manifest.teams = config.teams;
    manifest.kinds = config.kinds();
    manifest.team_ids = config.team_ids;
    manifest.characters = std::array::from_fn(|i| {
        if i < config.player_count as usize {
            17
        } else {
            255
        }
    });
    Command::Report {
        report: MatchObservation {
            match_id: id.into(),
            manifest,
            credits,
            end,
        },
    }
}

#[tokio::test]
async fn four_player_ffa_requires_every_entry_and_every_report() {
    let config = RoomConfig {
        player_count: 4,
        teams: false,
        team_ids: [255; 4],
    };
    let (_temp, payments, fake, id) = prepared_config(config.clone()).await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_err());
    fund(&payments, &fake, &id, &[2, 3]).await;
    assert!(payments.admitted(&id).await.is_ok());
    let hit = DamageCredit {
        victim: 3,
        ..credit(1, 2500)
    };
    for slot in 0..4 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(configured_report(&id, &config, vec![hit.clone()], None))
                )
                .await
                .is_ok()
        );
        assert_eq!(
            payments.view(&id).await.ok().unwrap().agreed_sequence,
            u64::from(slot == 3)
        );
    }
    for slot in 0..4 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(configured_report(
                        &id,
                        &config,
                        vec![],
                        Some(EndReport {
                            last_sequence: 1,
                            winner: Some(2),
                            complete: true
                        })
                    ))
                )
                .await
                .is_ok()
        );
        if slot < 3 {
            assert_eq!(payments.view(&id).await.ok().unwrap().phase, 3);
        }
    }
    settle(&payments, &id).await;
    let view = payments.view(&id).await.ok().unwrap();
    assert_eq!(view.players[0].earned_sats, 25);
    assert_eq!(view.players[2].earned_sats, 1975);
    assert_eq!(
        view.players.iter().map(|p| p.earned_sats).sum::<u64>(),
        2000
    );
}

#[tokio::test]
async fn teams_ignore_friendly_damage_and_agree_on_the_winning_side() {
    for count in [3, 4] {
        let config = RoomConfig {
            player_count: count,
            teams: true,
            team_ids: if count == 3 {
                [0, 1, 0, 255]
            } else {
                [0, 1, 0, 1]
            },
        };
        let (temp, payments, fake, id) = prepared_config(config.clone()).await;
        let slots: Vec<_> = (0..count as usize).collect();
        fund(&payments, &fake, &id, &slots).await;
        assert!(payments.admitted(&id).await.is_ok());
        let credits = vec![
            DamageCredit {
                victim: 2,
                ..credit(1, 5000)
            },
            DamageCredit {
                sequence: 2,
                frame: 20,
                attacker: 2,
                victim: 1,
                damage_centi: 10000,
            },
        ];
        for slot in 0..count as usize {
            assert!(
                payments
                    .authenticated(
                        &id,
                        &headers(slot),
                        Some(configured_report(&id, &config, credits.clone(), None))
                    )
                    .await
                    .is_ok()
            );
        }
        let view = payments.view(&id).await.ok().unwrap();
        assert_eq!(view.agreed_sequence, 2);
        assert_eq!(view.players[2].remaining_sats, 500);
        assert_eq!(view.players[2].pending_sats, 100);
        // Restart keeps all four member histories and the immutable team contract.
        drop(payments);
        let payments = Payments::open(temp.path().into(), rules(), wallet(fake.clone()), None)
            .await
            .unwrap();
        for slot in 0..count as usize {
            let winner = if slot % 2 == 0 { 0 } else { 2 };
            assert!(
                payments
                    .authenticated(
                        &id,
                        &headers(slot),
                        Some(configured_report(
                            &id,
                            &config,
                            vec![],
                            Some(EndReport {
                                last_sequence: 2,
                                winner: Some(winner),
                                complete: true
                            })
                        ))
                    )
                    .await
                    .is_ok()
            );
        }
        settle(&payments, &id).await;
        let view = payments.view(&id).await.ok().unwrap();
        assert_eq!(
            view.players[0].earned_sats,
            if count == 3 { 700 } else { 950 }
        );
        assert_eq!(
            view.players[2].earned_sats,
            if count == 3 { 800 } else { 1050 }
        );
        assert_eq!(
            view.players.iter().map(|p| p.earned_sats).sum::<u64>(),
            u64::from(count) * 500
        );
    }
}

#[tokio::test]
async fn legacy_two_player_journals_migrate_without_losing_auth_or_funds() {
    let (temp, payments, fake, id) = prepared().await;
    fund(&payments, &fake, &id, &[0, 1]).await;
    assert!(payments.admitted(&id).await.is_ok());
    {
        let guard = payments.0.lock().await;
        let record = guard.records.get(&id).unwrap();
        let mut legacy = serde_json::to_value(&record.saved).unwrap();
        legacy["version"] = 1.into();
        legacy["sandbox_addresses"] =
            serde_json::json!(["player0@example.com", "player1@example.com"]);
        legacy.as_object_mut().unwrap().remove("config");
        for key in ["observations", "token_hashes"] {
            legacy[key].as_array_mut().unwrap().truncate(2);
        }
        std::fs::write(&record.path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    }
    drop(payments);
    let payments = Payments::open(
        temp.path().into(),
        rules(),
        wallet(fake.clone()),
        Some(
            (0..4)
                .map(|i| format!("newplayer{i}@example.com"))
                .collect(),
        ),
    )
    .await
    .unwrap();
    let view = payments
        .authenticated(&id, &headers(1), None)
        .await
        .ok()
        .unwrap();
    assert_eq!(view.kinds, [0, 0, 3, 3]);
    assert_eq!(view.players[1].paid_sats, 500);
    assert_eq!(view.own_next_sequence, [1; 4]);
    let migrated: serde_json::Value = serde_json::from_slice(
        &std::fs::read(temp.path().join(&id).join("observations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(migrated["version"], 2);
    assert_eq!(migrated["token_hashes"].as_array().unwrap().len(), 4);
    assert!(
        payments
            .authenticated(&id, &headers(2), None)
            .await
            .is_err()
    );
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(Command::SetAddress {
                    address: "player1@example.com".into()
                })
            )
            .await
            .is_ok()
    );
    assert!(
        payments
            .authenticated(
                &id,
                &headers(1),
                Some(Command::SetAddress {
                    address: "newplayer1@example.com".into()
                })
            )
            .await
            .is_err()
    );
    let fresh = payments
        .create(
            &"3".repeat(32),
            &token(0),
            RoomConfig {
                player_count: 4,
                teams: false,
                team_ids: [255; 4],
            },
        )
        .await
        .ok()
        .unwrap();
    assert_eq!(fresh.kinds, [0; 4]);
    assert_eq!(fake.payment_attempts(), 0);
}

#[tokio::test]
async fn fourth_player_disagreement_freezes_before_any_credit_is_applied() {
    let config = RoomConfig {
        player_count: 4,
        teams: false,
        team_ids: [255; 4],
    };
    let (_temp, payments, fake, id) = prepared_config(config.clone()).await;
    fund(&payments, &fake, &id, &[0, 1, 2, 3]).await;
    assert!(payments.admitted(&id).await.is_ok());
    for slot in 0..4 {
        let hit = DamageCredit {
            attacker: 2,
            victim: 3,
            damage_centi: if slot == 3 { 2000 } else { 1000 },
            ..credit(1, 1000)
        };
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(configured_report(&id, &config, vec![hit], None))
                )
                .await
                .is_ok()
        );
    }
    let view = payments.view(&id).await.ok().unwrap();
    assert!(view.frozen);
    assert_eq!(view.agreed_sequence, 0);
    assert_eq!(fake.payment_attempts(), 0);
    assert!(payments.add_member(&id, 4, &"e".repeat(64)).await.is_err());
}

#[tokio::test]
async fn completed_member_can_leave_while_other_three_reports_arrive_and_settle_once() {
    let config = RoomConfig {
        player_count: 4,
        teams: false,
        team_ids: [255; 4],
    };
    let (_temp, payments, fake, id) = prepared_config(config.clone()).await;
    fund(&payments, &fake, &id, &[0, 1, 2, 3]).await;
    assert!(payments.admitted(&id).await.is_ok());
    let hit = DamageCredit {
        victim: 3,
        ..credit(1, 1000)
    };
    let end = EndReport {
        last_sequence: 1,
        winner: Some(1),
        complete: true,
    };
    assert!(
        payments
            .authenticated(
                &id,
                &headers(0),
                Some(configured_report(
                    &id,
                    &config,
                    vec![hit.clone()],
                    Some(end.clone())
                ))
            )
            .await
            .is_ok()
    );
    for _ in 0..2 {
        let view = payments
            .authenticated(&id, &headers(0), Some(Command::Leave))
            .await
            .ok()
            .unwrap();
        assert!(!view.frozen);
        assert_eq!(view.agreed_sequence, 0);
    }
    for slot in 1..4 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(configured_report(
                        &id,
                        &config,
                        vec![hit.clone()],
                        Some(end.clone())
                    ))
                )
                .await
                .is_ok()
        );
    }
    settle(&payments, &id).await;
    let view = payments.view(&id).await.ok().unwrap();
    assert_eq!(view.agreed_sequence, 1);
    assert_eq!(view.players[0].earned_sats, 10);
    assert_eq!(view.players[1].earned_sats, 1990);
    assert!(!view.frozen);
    assert_eq!(fake.payment_attempts(), 2);
    for slot in 0..4 {
        assert!(
            payments
                .authenticated(
                    &id,
                    &headers(slot),
                    Some(configured_report(&id, &config, vec![], Some(end.clone())))
                )
                .await
                .is_ok()
        );
        assert!(
            payments
                .authenticated(&id, &headers(slot), Some(Command::Leave))
                .await
                .is_ok()
        );
    }
    pump(&payments, 20).await;
    assert_eq!(fake.payment_attempts(), 2);
}

#[tokio::test]
async fn leaving_without_complete_gameplay_still_freezes_and_completed_grace_still_expires() {
    let config = RoomConfig {
        player_count: 4,
        teams: false,
        team_ids: [255; 4],
    };
    for completion in [None, Some(false), Some(true)] {
        let (_temp, payments, fake, id) = prepared_config(config.clone()).await;
        fund(&payments, &fake, &id, &[0, 1, 2, 3]).await;
        assert!(payments.admitted(&id).await.is_ok());
        if let Some(complete) = completion {
            assert!(
                payments
                    .authenticated(
                        &id,
                        &headers(0),
                        Some(configured_report(
                            &id,
                            &config,
                            vec![],
                            Some(EndReport {
                                last_sequence: 0,
                                winner: None,
                                complete
                            })
                        ))
                    )
                    .await
                    .is_ok()
            );
        }
        let view = payments
            .authenticated(&id, &headers(0), Some(Command::Leave))
            .await
            .ok()
            .unwrap();
        assert_eq!(view.frozen, completion != Some(true));
        if completion == Some(true) {
            let mut guard = payments.0.lock().await;
            guard.records.get_mut(&id).unwrap().seen[0] = Some(Instant::now() - LEASE);
            drop(guard);
            payments.maintain().await;
            assert!(payments.view(&id).await.ok().unwrap().frozen);
        }
        assert_eq!(fake.payment_attempts(), 0);
    }
}
