//! No real sats. By default invoices start unpaid; tests mark them paid using
//! `set_invoice_state`. The server preview opts into `auto_pay()` explicitly.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::{DecodedInvoice, InvoiceState, NewInvoice, PaymentState, Wallet};

#[derive(Default)]
pub struct FakeWallet {
    invoices: Mutex<HashMap<String, InvoiceState>>,
    payments: Mutex<HashMap<String, PaymentState>>,
    auto_pay: bool,
    attempts: AtomicUsize,
}

impl FakeWallet {
    pub fn auto_pay() -> Self {
        Self {
            auto_pay: true,
            ..Self::default()
        }
    }

    pub fn set_invoice_state(&self, payment_hash: &str, state: InvoiceState) {
        self.invoices
            .lock()
            .unwrap()
            .insert(payment_hash.into(), state);
    }

    pub fn set_payment_state(&self, payment_hash: &str, state: PaymentState) {
        self.payments
            .lock()
            .unwrap()
            .insert(payment_hash.into(), state);
    }

    pub fn payment_attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Wallet for FakeWallet {
    async fn create_invoice(
        &self,
        amount_sats: u64,
        _memo: &str,
        _expiry_secs: u64,
    ) -> anyhow::Result<NewInvoice> {
        let mut nonce = [0u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        let bolt11 = format!("lnfake:{amount_sats}:{}", hex::encode(nonce));
        let payment_hash = hash(&bolt11);
        let state = if self.auto_pay {
            InvoiceState::Paid { amount_sats }
        } else {
            InvoiceState::Pending
        };
        self.invoices
            .lock()
            .unwrap()
            .insert(payment_hash.clone(), state);
        Ok(NewInvoice {
            bolt11,
            payment_hash,
        })
    }

    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState> {
        Ok(self
            .invoices
            .lock()
            .unwrap()
            .get(payment_hash)
            .copied()
            .unwrap_or(InvoiceState::Pending))
    }

    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice> {
        let mut parts = bolt11.splitn(3, ':');
        let (Some("lnfake"), Some(amount), Some(_)) = (parts.next(), parts.next(), parts.next())
        else {
            bail!("the fake wallet only understands invoices like lnfake:<sats>:<anything>");
        };
        let amount_sats = amount.parse().context("invalid amount in fake invoice")?;
        Ok(DecodedInvoice {
            amount_sats: Some(amount_sats),
            payment_hash: hash(bolt11),
        })
    }

    async fn pay_invoice(&self, bolt11: &str, _max_fee_sats: u64) -> anyhow::Result<PaymentState> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let state = if bolt11.contains("fail") {
            PaymentState::Failed {
                reason: "fake wallet: simulated failure".into(),
            }
        } else if bolt11.contains("pending") || bolt11.contains("unknown") {
            PaymentState::Pending
        } else {
            PaymentState::Succeeded { fee_sats: 0 }
        };
        self.payments
            .lock()
            .unwrap()
            .insert(hash(bolt11), state.clone());
        if bolt11.contains("unknown") {
            bail!("simulated lost response");
        }
        Ok(state)
    }

    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState> {
        Ok(self
            .payments
            .lock()
            .unwrap()
            .get(payment_hash)
            .cloned()
            .unwrap_or(PaymentState::Pending))
    }
}

fn hash(bolt11: &str) -> String {
    hex::encode(Sha256::digest(bolt11.as_bytes()))
}
