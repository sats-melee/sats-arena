//! NIP-47 wallet adapter. Connection probes do not create or pay invoices.
//! Standard NWC has no per-payment fee cap: fees must be controlled by the
//! wallet connection's policy, explicitly selected with NWC_WALLET_MANAGED_FEES.
use super::{DecodedInvoice, InvoiceState, NewInvoice, PaymentState, Wallet};
use ::nwc::NostrWalletConnect;
use ::nwc::prelude::{
    Keys, ListTransactionsRequest, LookupInvoiceRequest, LookupInvoiceResponse, MakeInvoiceRequest,
    Method, NostrWalletConnectUri, PayInvoiceRequest, TransactionState, TransactionType,
};
use anyhow::{anyhow, ensure};
use async_trait::async_trait;
use lightning_invoice::{Bolt11Invoice, Currency};
use sha2::{Digest, Sha256};
use std::{
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const NWC_WALLET_MANAGED_FEES: u64 = u64::MAX;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Intentionally not Debug: the SDK client owns a secret connection URI.
pub struct NwcWallet {
    client: NostrWalletConnect,
    identity: String,
    timeout: Duration,
    can_list_transactions: bool,
}

/// Fixed, credential-free explanations that the local UI may show directly.
#[derive(Debug)]
pub enum IncomingReceiptError {
    HistoryPermission,
    HistoryUnavailable,
    ReceiptNotFound,
}
impl std::fmt::Display for IncomingReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HistoryPermission => "This self-payment needs list_transactions permission on the host NWC connection to confirm its incoming receipt. Do not pay again.",
            Self::HistoryUnavailable => "Could not read the incoming receipt for this self-payment. Check list_transactions access on the host NWC connection; retrying. Do not pay again.",
            Self::ReceiptNotFound => "The wallet returned the outgoing side of this self-payment; its matching incoming receipt is not visible yet. Retrying. Do not pay again.",
        })
    }
}
impl std::error::Error for IncomingReceiptError {}
impl NwcWallet {
    /// Validate permissions and establish the encrypted NWC connection without
    /// creating invoices, sending funds, or disclosing wallet/provider errors.
    pub async fn connect(uri: &str) -> anyhow::Result<Self> {
        Self::connect_with_timeout(uri, REQUEST_TIMEOUT).await
    }
    async fn connect_with_timeout(uri: &str, timeout: Duration) -> anyhow::Result<Self> {
        let uri = NostrWalletConnectUri::parse(uri)
            .map_err(|_| anyhow!("invalid NWC connection string"))?;
        ensure!(!uri.relays.is_empty(), "NWC connection has no relays");
        let mut relays = Vec::new();
        for relay in &uri.relays {
            let url =
                reqwest::Url::parse(relay.as_str()).map_err(|_| anyhow!("invalid NWC relay"))?;
            let loopback = url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            ensure!(
                url.scheme() == "wss" || (url.scheme() == "ws" && loopback),
                "NWC relays must use secure WebSockets (local debug relays may use ws)"
            );
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "NWC relay URL must not contain credentials or query strings"
            );
            relays.push(relay.as_str().to_owned());
        }
        relays.sort();
        relays.dedup();
        let public_client = Keys::new(uri.secret.clone()).public_key();
        let mut hash = Sha256::new();
        hash.update(uri.public_key.to_hex());
        hash.update(public_client.to_hex());
        for relay in relays {
            hash.update([0]);
            hash.update(relay.as_bytes());
        }
        let identity = hex::encode(hash.finalize());
        let client = NostrWalletConnect::builder(uri).timeout(timeout).build();
        let info = match tokio::time::timeout(timeout, async { client.get_info().await }).await {
            Ok(Ok(info)) => info,
            failure => {
                client.shutdown().await;
                let timed_out = match failure {
                    Err(_) => true,
                    Ok(Err(error)) => error.kind() == ::nwc::error::ErrorKind::NoResponse,
                    _ => false,
                };
                return Err(if timed_out {
                    anyhow!(
                        "NWC wallet connection timed out: no reply to get_info. Check the wallet is online and unlocked, and the connection is still active."
                    )
                } else {
                    anyhow!(
                        "NWC wallet connection or authorization failed. Check the saved connection and wallet permissions."
                    )
                });
            }
        };
        for required in [
            Method::MakeInvoice,
            Method::LookupInvoice,
            Method::PayInvoice,
        ] {
            ensure!(
                info.methods.contains(&required),
                "NWC connection must permit make_invoice, lookup_invoice, and pay_invoice"
            );
        }
        if let Some(network) = info.network {
            ensure!(
                matches!(network.as_str(), "bitcoin" | "mainnet"),
                "NWC wallet must use Bitcoin mainnet"
            );
        }
        Ok(Self {
            client,
            identity,
            timeout,
            can_list_transactions: info.methods.contains(&Method::ListTransactions),
        })
    }
    /// Fingerprint of public connection identities and relay URLs, never the URI.
    pub fn identity(&self) -> String {
        self.identity.clone()
    }
    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
    async fn lookup(&self, payment_hash: &str) -> anyhow::Result<LookupInvoiceResponse> {
        let hash = normalized_hash(payment_hash)?;
        let response = tokio::time::timeout(self.timeout, async {
            self.client
                .lookup_invoice(LookupInvoiceRequest {
                    payment_hash: Some(hash.clone()),
                    invoice: None,
                })
                .await
        })
        .await
        .map_err(|_| anyhow!("NWC lookup timed out"))?
        .map_err(|_| anyhow!("NWC lookup unavailable"))?;
        checked_lookup(response, &hash)
    }

    async fn incoming_receipt(&self, payment_hash: &str) -> anyhow::Result<LookupInvoiceResponse> {
        if !self.can_list_transactions {
            return Err(IncomingReceiptError::HistoryPermission.into());
        }
        let hash = normalized_hash(payment_hash)?;
        // Alby can return the newer outgoing record for a payment to the same wallet.
        // Only a separate matching incoming receipt may authorize entry credit.
        for page in 0..4 {
            let request = ListTransactionsRequest {
                transaction_type: Some(TransactionType::Incoming),
                unpaid: Some(true),
                limit: Some(50),
                offset: Some(page * 50),
                ..Default::default()
            };
            let receipts = tokio::time::timeout(self.timeout, async {
                self.client.list_transactions(request).await
            })
            .await
            .map_err(|_| IncomingReceiptError::HistoryUnavailable)?
            .map_err(|_| IncomingReceiptError::HistoryUnavailable)?;
            let count = receipts.len();
            for receipt in receipts {
                if receipt.transaction_type == Some(TransactionType::Incoming)
                    && normalized_hash(&receipt.payment_hash).ok().as_deref() == Some(hash.as_str())
                {
                    return checked_lookup(receipt, &hash);
                }
            }
            if count < 50 {
                break;
            }
        }
        Err(IncomingReceiptError::ReceiptNotFound.into())
    }
}

fn checked_lookup(
    response: LookupInvoiceResponse,
    hash: &str,
) -> anyhow::Result<LookupInvoiceResponse> {
    ensure!(
        normalized_hash(&response.payment_hash)? == hash,
        "NWC returned a different payment hash"
    );
    // Some wallets include an empty/placeholder preimage on unpaid invoices.
    // It is not a payment proof until the wallet reports settlement.
    if settled(&response) {
        if let Some(preimage) = &response.preimage {
            verify_preimage(preimage, hash)?;
        }
    }
    Ok(response)
}
fn normalized_hash(value: &str) -> anyhow::Result<String> {
    let bytes = hex::decode(value).map_err(|_| anyhow!("invalid payment hash"))?;
    ensure!(bytes.len() == 32, "invalid payment hash");
    Ok(hex::encode(bytes))
}
fn verify_preimage(preimage: &str, hash: &str) -> anyhow::Result<()> {
    let bytes =
        hex::decode(preimage).map_err(|_| anyhow!("NWC returned an invalid payment proof"))?;
    ensure!(
        bytes.len() == 32 && hex::encode(Sha256::digest(bytes)) == hash,
        "NWC returned an invalid payment proof"
    );
    Ok(())
}
fn parse_invoice(value: &str) -> anyhow::Result<Bolt11Invoice> {
    let invoice =
        Bolt11Invoice::from_str(value).map_err(|_| anyhow!("invalid Lightning invoice"))?;
    invoice
        .check_signature()
        .map_err(|_| anyhow!("invalid invoice signature"))?;
    ensure!(
        invoice.currency() == Currency::Bitcoin,
        "invoice must use Bitcoin mainnet"
    );
    ensure!(
        invoice
            .amount_milli_satoshis()
            .is_some_and(|a| a > 0 && a % 1000 == 0),
        "invoice must request a whole-satoshi amount"
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    ensure!(
        invoice.duration_since_epoch() <= now.saturating_add(Duration::from_secs(60)),
        "invoice timestamp is in the future"
    );
    ensure!(
        invoice.expires_at().is_some_and(|expiry| expiry > now),
        "invoice has expired"
    );
    Ok(invoice)
}
fn fee_sats(msats: u64) -> u64 {
    msats / 1000 + u64::from(!msats.is_multiple_of(1000))
}
fn settled(response: &LookupInvoiceResponse) -> bool {
    response.state == Some(TransactionState::Settled)
        || (response.state.is_none() && response.settled_at.is_some())
}
#[async_trait]
impl Wallet for NwcWallet {
    async fn create_invoice(
        &self,
        amount_sats: u64,
        memo: &str,
        expiry_secs: u64,
    ) -> anyhow::Result<NewInvoice> {
        let amount = amount_sats
            .checked_mul(1000)
            .filter(|a| *a > 0)
            .ok_or_else(|| anyhow!("invalid invoice amount"))?;
        let response = tokio::time::timeout(self.timeout, async {
            self.client
                .make_invoice(MakeInvoiceRequest {
                    amount,
                    description: Some(memo.to_owned()),
                    description_hash: None,
                    expiry: Some(expiry_secs),
                })
                .await
        })
        .await
        .map_err(|_| anyhow!("NWC invoice creation timed out"))?
        .map_err(|_| anyhow!("NWC invoice creation failed"))?;
        let parsed = parse_invoice(&response.invoice)?;
        ensure!(
            parsed.amount_milli_satoshis() == Some(amount),
            "NWC created an invoice for a different amount"
        );
        let hash = parsed.payment_hash().to_string();
        if let Some(reported) = response.payment_hash {
            ensure!(
                normalized_hash(&reported)? == hash,
                "NWC created an invoice with inconsistent hash"
            );
        }
        if let Some(reported) = response.amount {
            ensure!(
                reported == amount,
                "NWC returned an inconsistent invoice amount"
            );
        }
        Ok(NewInvoice {
            bolt11: response.invoice,
            payment_hash: hash,
        })
    }
    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState> {
        let mut response = self.lookup(payment_hash).await?;
        if response.transaction_type == Some(TransactionType::Outgoing) {
            response = self.incoming_receipt(payment_hash).await?;
        }
        ensure!(
            response.transaction_type == Some(TransactionType::Incoming),
            "NWC lookup did not identify an incoming invoice"
        );
        if settled(&response) {
            ensure!(
                response.amount > 0 && response.amount % 1000 == 0,
                "NWC incoming amount is not whole satoshis"
            );
            Ok(InvoiceState::Paid {
                amount_sats: response.amount / 1000,
            })
        } else if response.state == Some(TransactionState::Expired)
            || (response.state.is_none()
                && response.expires_at.is_some_and(|expiry| {
                    expiry.as_secs()
                        <= SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                }))
        {
            Ok(InvoiceState::Expired)
        } else {
            Ok(InvoiceState::Pending)
        }
    }
    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice> {
        let invoice = parse_invoice(bolt11)?;
        Ok(DecodedInvoice {
            amount_sats: invoice.amount_milli_satoshis().map(|a| a / 1000),
            payment_hash: invoice.payment_hash().to_string(),
        })
    }
    async fn pay_invoice(&self, bolt11: &str, max_fee_sats: u64) -> anyhow::Result<PaymentState> {
        ensure!(
            max_fee_sats == NWC_WALLET_MANAGED_FEES,
            "standard NWC cannot enforce a per-payment fee cap; select wallet-managed fees"
        );
        let invoice = match self.decode_invoice(bolt11) {
            Ok(invoice) => invoice,
            Err(_) => {
                return Ok(PaymentState::Failed {
                    reason: "Outgoing invoice is invalid or expired before submission".into(),
                });
            }
        };
        let response = tokio::time::timeout(self.timeout, async {
            self.client
                .pay_invoice(PayInvoiceRequest::new(bolt11))
                .await
        })
        .await;
        match response {
            Ok(Ok(paid)) => {
                // A success acknowledgement must contain the invoice's real preimage.
                if verify_preimage(&paid.preimage, &invoice.payment_hash).is_err() {
                    return Ok(PaymentState::Pending);
                }
                // NIP-47 permits missing fee data; the embedded host does not debit
                // fees from player pots. Zero means unreported when omitted.
                Ok(PaymentState::Succeeded {
                    fee_sats: fee_sats(paid.fees_paid.unwrap_or(0)),
                })
            }
            // Lost or rejected responses do not authorize releasing funds. The
            // durable host looks up the original payment hash and never resends.
            _ => Ok(PaymentState::Pending),
        }
    }
    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState> {
        // An unavailable lookup is an unknown outcome, but is not evidence
        // that the provider actually reports an in-flight transaction.
        let response = self.lookup(payment_hash).await?;
        ensure!(
            response.transaction_type == Some(TransactionType::Outgoing),
            "NWC lookup did not identify an outgoing payment"
        );
        if settled(&response) {
            Ok(PaymentState::Succeeded {
                fee_sats: fee_sats(response.fees_paid),
            })
        } else if response.state == Some(TransactionState::Failed) {
            Ok(PaymentState::Failed {
                reason: "NWC wallet confirmed the outgoing payment failed".into(),
            })
        } else {
            Ok(PaymentState::Pending)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        hashes::{Hash, sha256},
        secp256k1::{Secp256k1, SecretKey},
    };
    use futures_util::StreamExt;
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};
    use nostr_sdk::{local_relay::MockRelay, prelude::*};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        requests: Vec<String>,
        incoming_paid: bool,
        incoming_preimage: Option<String>,
        incoming_expired: bool,
        legacy_expiry: bool,
        self_payment: bool,
        history: Vec<Value>,
        history_error: bool,
        outgoing: String,
        wrong_proof: bool,
        omit_pay_response: bool,
        wrong_amount: bool,
    }
    struct Service {
        _relay: MockRelay,
        client: Client,
        task: tokio::task::JoinHandle<()>,
        uri: String,
        state: Arc<Mutex<State>>,
        incoming: String,
        outgoing: String,
    }
    fn fixture(sats: u64, preimage: [u8; 32], currency: Currency) -> String {
        let secret = SecretKey::from_slice(&[42; 32]).unwrap();
        InvoiceBuilder::new(currency)
            .amount_milli_satoshis(sats * 1000)
            .description("Arena NWC test".into())
            .payment_hash(sha256::Hash::hash(&preimage))
            .payment_secret(PaymentSecret([9; 32]))
            .duration_since_epoch(SystemTime::now().duration_since(UNIX_EPOCH).unwrap())
            .expiry_time(Duration::from_secs(3600))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|m| Secp256k1::new().sign_ecdsa_recoverable(m, &secret))
            .unwrap()
            .to_string()
    }
    impl Service {
        async fn start(cipher: Nip47Ciphers, methods: &[&str], network: &str) -> Self {
            let relay = MockRelay::run().await.unwrap();
            let relay_url = relay.url().await;
            let wallet_keys = Keys::generate();
            let client_keys = Keys::generate();
            let uri = NostrWalletConnectUri::new(
                wallet_keys.public_key(),
                vec![relay_url.clone()],
                client_keys.secret_key().clone(),
                None,
            )
            .to_string();
            let client = Client::new();
            client.add_relay(relay_url).and_connect().await.unwrap();
            let mut notifications = client.notifications();
            client
                .subscribe(
                    Filter::new()
                        .kind(Kind::WalletConnectRequest)
                        .author(client_keys.public_key())
                        .pubkey(wallet_keys.public_key()),
                )
                .await
                .unwrap();
            let mut info = EventBuilder::new(Kind::WalletConnectInfo, methods.join(" "));
            // No encryption tag deliberately exercises legacy NIP04 fallback.
            if cipher != Nip47Ciphers::NIP04 {
                info = info.tag(Nip47Tag::Encryption(cipher));
            }
            client
                .send_event(&info.finalize(&wallet_keys).unwrap())
                .await
                .unwrap();
            let state = Arc::new(Mutex::new(State::default()));
            let test_state = state.clone();
            let sender = client.clone();
            let methods: Vec<String> = methods.iter().map(|m| (*m).to_owned()).collect();
            let network = network.to_owned();
            let incoming = fixture(500, [7; 32], Currency::Bitcoin);
            let outgoing = fixture(8, [8; 32], Currency::Bitcoin);
            let in_hash = hex::encode(Sha256::digest([7; 32]));
            let out_hash = hex::encode(Sha256::digest([8; 32]));
            let task = tokio::spawn(async move {
                while let Some(notification) = notifications.next().await {
                    let ClientNotification::Event { event, .. } = notification else {
                        continue;
                    };
                    if event.kind != Kind::WalletConnectRequest {
                        continue;
                    }
                    let actual = event
                        .tags
                        .iter()
                        .find_map(|t| Nip47Tag::try_from(t).ok().map(|Nip47Tag::Encryption(c)| c))
                        .unwrap_or(Nip47Ciphers::NIP04);
                    assert_eq!(actual, cipher);
                    let decrypted = actual
                        .decrypt(
                            wallet_keys.secret_key(),
                            &client_keys.public_key(),
                            &event.content,
                        )
                        .unwrap();
                    let request: Value = serde_json::from_str(&decrypted).unwrap();
                    let method = request["method"].as_str().unwrap();
                    let response = {
                        let mut state = test_state.lock().unwrap();
                        state.requests.push(method.into());
                        let result = match method {
                            "get_info" => json!({"methods":methods,"network":network}),
                            "make_invoice" => {
                                let amount = request["params"]["amount"].as_u64().unwrap();
                                let sats = amount / 1000 + u64::from(state.wrong_amount);
                                json!({"invoice":fixture(sats,[7;32],Currency::Bitcoin),"payment_hash":in_hash,"amount":sats*1000})
                            }
                            "lookup_invoice" => {
                                let hash = request["params"]["payment_hash"].as_str().unwrap();
                                if hash == in_hash {
                                    json!({"type":if state.self_payment {"outgoing"} else {"incoming"},"state":if state.incoming_paid {Some("settled")} else if state.incoming_expired {Some("expired")} else if state.legacy_expiry {None} else {Some("pending")},"expires_at":if state.legacy_expiry {Some(1)} else {None},"payment_hash":in_hash,"preimage":state.incoming_preimage,"amount":500000,"fees_paid":0,"created_at":1,"settled_at":if state.incoming_paid {Some(2)} else {None}})
                                } else if hash == out_hash {
                                    json!({"type":"outgoing","state":if state.outgoing.is_empty() {"pending"} else {&state.outgoing},"payment_hash":out_hash,"preimage":if state.outgoing=="settled" {Some(hex::encode([8;32]))} else {None},"amount":8000,"fees_paid":1500,"created_at":1,"settled_at":if state.outgoing=="settled" {Some(2)} else {None}})
                                } else {
                                    json!({"_lookup_error":true})
                                }
                            }
                            "list_transactions" => {
                                assert_eq!(request["params"]["type"], "incoming");
                                assert_eq!(request["params"]["unpaid"], true);
                                assert_eq!(request["params"]["limit"], 50);
                                let offset = request["params"]["offset"].as_u64().unwrap() as usize;
                                if state.history_error {
                                    json!({"_lookup_error":true})
                                } else {
                                    json!({"transactions":state.history.iter().skip(offset).take(50).collect::<Vec<_>>()})
                                }
                            }
                            "pay_invoice" => {
                                if state.omit_pay_response {
                                    continue;
                                }
                                json!({"preimage":hex::encode(if state.wrong_proof {[9;32]} else {[8;32]}),"fees_paid":1500})
                            }
                            _ => panic!("unexpected method"),
                        };
                        if result["_lookup_error"].as_bool() == Some(true) {
                            json!({"result_type":method,"error":{"code":"NOT_FOUND","message":"unknown nostr+walletconnect://secret-must-not-leak"},"result":null})
                        } else {
                            json!({"result_type":method,"error":null,"result":result})
                        }
                    };
                    let encrypted = actual
                        .encrypt(
                            wallet_keys.secret_key(),
                            &client_keys.public_key(),
                            &response.to_string(),
                        )
                        .unwrap();
                    let response = EventBuilder::new(Kind::WalletConnectResponse, encrypted)
                        .tag(Tag::public_key(client_keys.public_key()))
                        .tag(Tag::event(event.id))
                        .finalize(&wallet_keys)
                        .unwrap();
                    sender.send_event(&response).await.unwrap();
                }
            });
            Self {
                _relay: relay,
                client,
                task,
                uri,
                state,
                incoming,
                outgoing,
            }
        }
        async fn stop(self) {
            self.client.shutdown().await;
            self.task.abort();
        }
    }
    const METHODS: &[&str] = &["get_info", "make_invoice", "lookup_invoice", "pay_invoice"];

    fn receipt() -> Value {
        json!({"type":"incoming","state":"settled","payment_hash":hex::encode(Sha256::digest([7;32])),"preimage":hex::encode([7;32]),"amount":500000,"fees_paid":0,"created_at":1,"settled_at":2})
    }

    async fn self_payment_service() -> Service {
        let mut methods = METHODS.to_vec();
        methods.push("list_transactions");
        let service = Service::start(Nip47Ciphers::NIP44V2, &methods, "bitcoin").await;
        {
            let mut state = service.state.lock().unwrap();
            state.self_payment = true;
            state.incoming_paid = true;
            state.incoming_preimage = Some(hex::encode([7; 32]));
        }
        service
    }

    #[tokio::test]
    async fn self_payment_requires_a_matching_incoming_receipt_and_paginates() {
        let service = self_payment_service().await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        let mut unrelated = receipt();
        unrelated["payment_hash"] = json!("aa".repeat(32));
        let mut outgoing = receipt();
        outgoing["type"] = json!("outgoing");
        {
            let mut state = service.state.lock().unwrap();
            state.history = vec![unrelated; 50];
            state.history.push(outgoing);
        }
        let error = wallet.invoice_state(&hash).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(IncomingReceiptError::ReceiptNotFound)
        ));
        service.state.lock().unwrap().history.push(receipt());
        assert_eq!(
            wallet.invoice_state(&hash).await.unwrap(),
            InvoiceState::Paid { amount_sats: 500 }
        );
        assert_eq!(
            service.state.lock().unwrap().requests,
            [
                "get_info",
                "lookup_invoice",
                "list_transactions",
                "list_transactions",
                "lookup_invoice",
                "list_transactions",
                "list_transactions"
            ]
        );
        wallet.shutdown().await;
        service.stop().await;
    }

    #[tokio::test]
    async fn self_payment_history_enforces_settlement_proof_and_amount() {
        let service = self_payment_service().await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        let mut pending = receipt();
        pending["state"] = json!("pending");
        pending["settled_at"] = Value::Null;
        pending["preimage"] = json!("");
        service.state.lock().unwrap().history = vec![pending];
        assert_eq!(
            wallet.invoice_state(&hash).await.unwrap(),
            InvoiceState::Pending
        );
        let mut invalid = receipt();
        invalid["preimage"] = json!("00".repeat(32));
        service.state.lock().unwrap().history = vec![invalid];
        assert!(wallet.invoice_state(&hash).await.is_err());
        for amount in [0, 500001] {
            let mut invalid = receipt();
            invalid["amount"] = json!(amount);
            service.state.lock().unwrap().history = vec![invalid];
            assert!(wallet.invoice_state(&hash).await.is_err());
        }
        service.state.lock().unwrap().history_error = true;
        let error = wallet.invoice_state(&hash).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(IncomingReceiptError::HistoryUnavailable)
        ));
        assert!(!error.to_string().contains("secret-must-not-leak"));
        wallet.shutdown().await;
        service.stop().await;
    }

    #[tokio::test]
    async fn self_payment_missing_permission_has_an_actionable_error() {
        let service = Service::start(Nip47Ciphers::NIP44V2, METHODS, "bitcoin").await;
        service.state.lock().unwrap().self_payment = true;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        let error = wallet.invoice_state(&hash).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(IncomingReceiptError::HistoryPermission)
        ));
        assert!(error.to_string().contains("list_transactions permission"));
        assert_eq!(
            service.state.lock().unwrap().requests,
            ["get_info", "lookup_invoice"]
        );
        wallet.shutdown().await;
        service.stop().await;
    }

    #[tokio::test]
    async fn self_payment_history_search_is_bounded() {
        let service = self_payment_service().await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        let mut unrelated = receipt();
        unrelated["payment_hash"] = json!("aa".repeat(32));
        service.state.lock().unwrap().history = vec![unrelated; 250];
        assert!(wallet.invoice_state(&hash).await.is_err());
        assert_eq!(
            service
                .state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|m| *m == "list_transactions")
                .count(),
            4
        );
        wallet.shutdown().await;
        service.stop().await;
    }

    #[tokio::test]
    async fn pending_invoice_placeholder_proofs_do_not_block_confirmation() {
        let service = Service::start(Nip47Ciphers::NIP44V2, METHODS, "bitcoin").await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        for proof in [String::new(), "00".repeat(32)] {
            service.state.lock().unwrap().incoming_preimage = Some(proof);
            assert_eq!(
                wallet.invoice_state(&hash).await.unwrap(),
                InvoiceState::Pending
            );
        }
        service.state.lock().unwrap().incoming_paid = true;
        // A placeholder must never authorize credit once the wallet claims settlement.
        assert!(wallet.invoice_state(&hash).await.is_err());
        service.state.lock().unwrap().incoming_preimage = Some(hex::encode([7; 32]));
        assert_eq!(
            wallet.invoice_state(&hash).await.unwrap(),
            InvoiceState::Paid { amount_sats: 500 }
        );
        wallet.shutdown().await;
        service.stop().await;
    }

    #[tokio::test]
    async fn encrypted_relay_exercises_entry_confirmation_payout_and_lookup() {
        let service = Service::start(Nip47Ciphers::NIP44V2, METHODS, "bitcoin").await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        assert_eq!(service.state.lock().unwrap().requests, vec!["get_info"]);
        assert_eq!(wallet.identity().len(), 64);
        assert!(!wallet.identity().contains("secret"));
        let invoice = wallet.create_invoice(500, "P1 entry", 600).await.unwrap();
        assert_eq!(
            wallet.decode_invoice(&invoice.bolt11).unwrap().amount_sats,
            Some(500)
        );
        assert_eq!(
            wallet.invoice_state(&invoice.payment_hash).await.unwrap(),
            InvoiceState::Pending
        );
        service.state.lock().unwrap().incoming_paid = true;
        assert_eq!(
            wallet.invoice_state(&invoice.payment_hash).await.unwrap(),
            InvoiceState::Paid { amount_sats: 500 }
        );
        assert!(wallet.pay_invoice(&service.outgoing, 10).await.is_err());
        assert!(
            !service
                .state
                .lock()
                .unwrap()
                .requests
                .contains(&"pay_invoice".into())
        );
        assert_eq!(
            wallet
                .pay_invoice(&service.outgoing, NWC_WALLET_MANAGED_FEES)
                .await
                .unwrap(),
            PaymentState::Succeeded { fee_sats: 2 }
        );
        let hash = wallet
            .decode_invoice(&service.outgoing)
            .unwrap()
            .payment_hash;
        service.state.lock().unwrap().outgoing = "settled".into();
        assert_eq!(
            wallet.payment_state(&hash).await.unwrap(),
            PaymentState::Succeeded { fee_sats: 2 }
        );
        assert!(wallet.payment_state(&"aa".repeat(32)).await.is_err());
        assert_eq!(
            service
                .state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|m| m.as_str() == "pay_invoice")
                .count(),
            1
        );
        wallet.shutdown().await;
        service.stop().await;
    }
    #[tokio::test]
    async fn legacy_encryption_failure_and_expiry_are_handled_without_resending() {
        let service = Service::start(Nip47Ciphers::NIP04, METHODS, "bitcoin").await;
        let wallet = NwcWallet::connect(&service.uri).await.unwrap();
        let hash = wallet
            .decode_invoice(&service.incoming)
            .unwrap()
            .payment_hash;
        service.state.lock().unwrap().incoming_expired = true;
        assert_eq!(
            wallet.invoice_state(&hash).await.unwrap(),
            InvoiceState::Expired
        );
        {
            let mut state = service.state.lock().unwrap();
            state.incoming_expired = false;
            state.legacy_expiry = true;
        }
        assert_eq!(
            wallet.invoice_state(&hash).await.unwrap(),
            InvoiceState::Expired
        );
        service.state.lock().unwrap().wrong_proof = true;
        assert_eq!(
            wallet
                .pay_invoice(&service.outgoing, NWC_WALLET_MANAGED_FEES)
                .await
                .unwrap(),
            PaymentState::Pending
        );
        let hash = wallet
            .decode_invoice(&service.outgoing)
            .unwrap()
            .payment_hash;
        service.state.lock().unwrap().outgoing = "failed".into();
        assert!(matches!(
            wallet.payment_state(&hash).await.unwrap(),
            PaymentState::Failed { .. }
        ));
        service.state.lock().unwrap().wrong_amount = true;
        assert!(wallet.create_invoice(500, "entry", 600).await.is_err());
        assert!(
            wallet
                .decode_invoice(&fixture(1, [1; 32], Currency::BitcoinTestnet))
                .is_err()
        );
        wallet.shutdown().await;
        service.stop().await;
    }
    #[tokio::test]
    async fn lost_payment_response_is_looked_up_without_repeating_payment() {
        let service = Service::start(Nip47Ciphers::NIP44V2, METHODS, "bitcoin").await;
        let wallet = NwcWallet::connect_with_timeout(&service.uri, Duration::from_secs(1))
            .await
            .unwrap();
        service.state.lock().unwrap().omit_pay_response = true;
        assert_eq!(
            wallet
                .pay_invoice(&service.outgoing, NWC_WALLET_MANAGED_FEES)
                .await
                .unwrap(),
            PaymentState::Pending
        );
        let hash = wallet
            .decode_invoice(&service.outgoing)
            .unwrap()
            .payment_hash;
        service.state.lock().unwrap().outgoing = "settled".into();
        assert_eq!(
            wallet.payment_state(&hash).await.unwrap(),
            PaymentState::Succeeded { fee_sats: 2 }
        );
        assert_eq!(
            service
                .state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|m| m.as_str() == "pay_invoice")
                .count(),
            1
        );
        wallet.shutdown().await;
        service.stop().await;
    }
    #[tokio::test]
    async fn capability_and_network_checks_do_not_make_payments_and_errors_are_redacted() {
        let service = Service::start(
            Nip47Ciphers::NIP44V2,
            &["get_info", "pay_invoice"],
            "bitcoin",
        )
        .await;
        let error = NwcWallet::connect(&service.uri)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains(&service.uri));
        assert_eq!(service.state.lock().unwrap().requests, vec!["get_info"]);
        service.stop().await;
        let service = Service::start(Nip47Ciphers::NIP44V2, METHODS, "testnet").await;
        assert!(NwcWallet::connect(&service.uri).await.is_err());
        assert_eq!(service.state.lock().unwrap().requests, vec!["get_info"]);
        service.stop().await;
        let uri = "nostr+walletconnect://bad?secret=DO_NOT_DISCLOSE";
        let error = NwcWallet::connect(uri).await.err().unwrap().to_string();
        assert!(!error.contains("DO_NOT_DISCLOSE"));
    }
}
