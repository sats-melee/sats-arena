//! A wallet with no real sats, for development. Every deposit invoice counts
//! as paid as soon as it is created. To test withdrawals, submit an invoice
//! like `lnfake:500:anything`; put `fail` in it to simulate a failed payment.

use std::{collections::HashMap, sync::Mutex};

use anyhow::{Context, bail};
use async_trait::async_trait;
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::{DecodedInvoice, InvoiceState, NewInvoice, PaymentState, Wallet};

#[derive(Default)]
pub struct FakeWallet {
    invoices: Mutex<HashMap<String, InvoiceState>>,
    payments: Mutex<HashMap<String, PaymentState>>,
}

#[async_trait]
impl Wallet for FakeWallet {
    async fn create_invoice(&self, amount_sats: u64, _memo: &str, _expiry_secs: u64) -> anyhow::Result<NewInvoice> {
        let mut nonce = [0u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        let bolt11 = format!("lnfake:{amount_sats}:{}", hex::encode(nonce));
        let payment_hash = hash(&bolt11);
        self.invoices.lock().unwrap().insert(payment_hash.clone(), InvoiceState::Paid);
        Ok(NewInvoice { bolt11, payment_hash })
    }

    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState> {
        // Invoices from before a restart are gone, like an expired invoice.
        Ok(self.invoices.lock().unwrap().get(payment_hash).copied().unwrap_or(InvoiceState::Expired))
    }

    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice> {
        let mut parts = bolt11.splitn(3, ':');
        let (Some("lnfake"), Some(amount), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("the fake wallet only understands invoices like lnfake:<sats>:<anything>");
        };
        let amount_sats = amount.parse().context("invalid amount in fake invoice")?;
        Ok(DecodedInvoice { amount_sats: Some(amount_sats), payment_hash: hash(bolt11) })
    }

    async fn pay_invoice(&self, bolt11: &str, _max_fee_sats: u64) -> anyhow::Result<PaymentState> {
        let state = if bolt11.contains("fail") {
            PaymentState::Failed { reason: "fake wallet: simulated failure".into() }
        } else {
            PaymentState::Succeeded { fee_sats: 0 }
        };
        self.payments.lock().unwrap().insert(hash(bolt11), state.clone());
        Ok(state)
    }

    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState> {
        Ok(self.payments.lock().unwrap().get(payment_hash).cloned().unwrap_or(PaymentState::Failed {
            reason: "fake wallet: payment not found".into(),
        }))
    }
}

fn hash(bolt11: &str) -> String {
    hex::encode(Sha256::digest(bolt11.as_bytes()))
}
