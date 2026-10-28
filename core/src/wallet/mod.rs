//! The only part of the engine that touches Lightning. Each backend adapts one
//! way of reaching a node (NWC, LND, CLN, ...) to this interface.

mod address;
mod fake;
mod lnurl;
mod nwc;
pub use address::{
    AddressInvoices, LightningAddress, PreparedPayout, prepare_payout, prepare_payout_with_comment,
};
pub use lnurl::{AddressVerification, AddressVerifier, LnurlClient, VERIFICATION_TTL_MS};

use async_trait::async_trait;

pub use fake::{FakeAddressInvoices, FakeWallet};
pub use nwc::{IncomingReceiptError, NWC_WALLET_MANAGED_FEES, NwcWallet};

pub struct NewInvoice {
    pub bolt11: String,
    pub payment_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceState {
    Pending,
    /// The wallet, not the client, confirms a settled incoming payment.
    Paid {
        amount_sats: u64,
    },
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
    Succeeded {
        fee_sats: u64,
    },
    /// Only return this when the payment can never succeed later; the server
    /// refunds the player immediately.
    Failed {
        reason: String,
    },
}

#[async_trait]
pub trait Wallet: Send + Sync {
    /// Create an ordinary receiving invoice. No hold-invoice support is needed.
    async fn create_invoice(
        &self,
        amount_sats: u64,
        memo: &str,
        expiry_secs: u64,
    ) -> anyhow::Result<NewInvoice>;
    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState>;
    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice>;
    /// An `Err` means the outcome is unknown, not that the payment failed.
    async fn pay_invoice(&self, bolt11: &str, max_fee_sats: u64) -> anyhow::Result<PaymentState>;
    /// A missing payment or lost response is Pending/unknown, NOT proof of
    /// failure. Only Failed authorizes releasing a withdrawal's reserved funds.
    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState>;
}
