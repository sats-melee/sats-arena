use serde::{Deserialize, Serialize};

/// Allowlisted descriptions only; provider error text can contain credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletIssue {
    Timeout,
    Unavailable,
    NotFound,
    Expired,
    InvalidProof,
    OutgoingReceiptMissing,
    RateLimited,
    InsufficientBalance,
    QuotaExceeded,
    Restricted,
    Unauthorized,
    NotImplemented,
    ReportedFailure,
    Failed,
    InvalidInvoice,
    ReceivingInvoice,
}

impl WalletIssue {
    pub fn message(self) -> &'static str {
        match self {
            Self::Timeout => "Wallet response timed out; payment outcome is unknown",
            Self::Unavailable => "Wallet status is unavailable; payment outcome is unknown",
            Self::NotFound => "Wallet cannot find the original payment; reconciliation needed",
            Self::Expired => "Wallet reports an expired outgoing record; reconciliation needed",
            Self::InvalidProof => "Wallet returned an invalid payment proof; reconciliation needed",
            Self::OutgoingReceiptMissing => {
                "Wallet returned an incoming invoice without an outgoing payment result; reconciliation needed"
            }
            Self::RateLimited => "Wallet rejected the request because commands arrived too quickly",
            Self::InsufficientBalance => {
                "Wallet rejected the request because its available balance is insufficient"
            }
            Self::QuotaExceeded => {
                "Wallet rejected the request because the connection spending limit was reached"
            }
            Self::Restricted => "Wallet rejected the request because payment permission is missing",
            Self::Unauthorized => {
                "Wallet rejected the request because the connection is not authorized"
            }
            Self::NotImplemented => "Wallet does not support this payment request",
            Self::ReportedFailure => {
                "Wallet reported a payment error; checking the original payment before retrying"
            }
            Self::Failed => "Wallet confirmed that the outgoing payment failed",
            Self::InvalidInvoice => "Receiving invoice was invalid or expired before submission",
            Self::ReceivingInvoice => {
                "Receiving address did not return a valid invoice; no new payment was sent"
            }
        }
    }

    pub fn rejected_before_payment(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::InsufficientBalance
                | Self::QuotaExceeded
                | Self::Restricted
                | Self::Unauthorized
                | Self::NotImplemented
                | Self::InvalidInvoice
        )
    }

    pub fn automatic_retry(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Failed | Self::InvalidInvoice
        )
    }

    pub fn failure_reason(reason: &str) -> Self {
        [
            Self::RateLimited,
            Self::InsufficientBalance,
            Self::QuotaExceeded,
            Self::Restricted,
            Self::Unauthorized,
            Self::NotImplemented,
            Self::InvalidInvoice,
        ]
        .into_iter()
        .find(|issue| issue.message() == reason)
        .unwrap_or(Self::Failed)
    }

    pub(super) fn from_nwc(error: &::nwc::error::Error) -> Self {
        use ::nwc::error::ErrorKind;
        if error.kind() == ErrorKind::NoResponse {
            return Self::Timeout;
        }
        if error.kind() != ErrorKind::Protocol {
            return Self::Unavailable;
        }
        let message = error.to_string();
        [
            ("RateLimited", Self::RateLimited),
            ("InsufficientBalance", Self::InsufficientBalance),
            ("QuotaExceeded", Self::QuotaExceeded),
            ("Restricted", Self::Restricted),
            ("Unauthorized", Self::Unauthorized),
            ("NotImplemented", Self::NotImplemented),
            ("NotFound", Self::NotFound),
            ("PaymentFailed", Self::ReportedFailure),
        ]
        .into_iter()
        .find_map(|(code, issue)| message.ends_with(&format!("[{code}]")).then_some(issue))
        .unwrap_or(Self::Unavailable)
    }
}

impl std::fmt::Display for WalletIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for WalletIssue {}
