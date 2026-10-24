//! The only part of the server that touches Lightning. Each backend adapts one
//! way of reaching a node (NWC, LND, CLN, ...) to this interface.

mod fake;

use async_trait::async_trait;

pub use fake::FakeWallet;

pub struct NewInvoice {
    pub bolt11: String,
    pub payment_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceState {
    Pending,
    Paid,
    Expired,
}

pub struct DecodedInvoice {
    /// `None` for an amountless invoice, which the server refuses to pay.
    pub amount_sats: Option<u64>,
    pub payment_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentState {
    /// In flight, or the backend can't tell yet. Funds stay reserved.
    Pending,
    Succeeded { fee_sats: u64 },
    /// Only return this when the payment can never succeed later; the server
    /// refunds the player immediately.
    Failed { reason: String },
}

#[async_trait]
pub trait Wallet: Send + Sync {
    async fn create_invoice(&self, amount_sats: u64, memo: &str, expiry_secs: u64) -> anyhow::Result<NewInvoice>;
    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState>;
    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice>;
    /// An `Err` means the outcome is unknown, not that the payment failed.
    async fn pay_invoice(&self, bolt11: &str, max_fee_sats: u64) -> anyhow::Result<PaymentState>;
    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState>;
}
