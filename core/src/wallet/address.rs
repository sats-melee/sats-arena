//! Receiving-address resolution is independent of the funding wallet.
//! Real providers implement LUD-16/LUD-06 and return a BOLT11 invoice;
//! NWC/LND/etc. adapters implement Wallet and validate/pay that invoice.
use super::Wallet;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightningAddress(String);

impl LightningAddress {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let value = input.trim();
        let (name, domain) = value
            .split_once('@')
            .ok_or_else(|| anyhow::anyhow!("use name@wallet.example"))?;
        anyhow::ensure!(
            value.len() <= 127 && !name.is_empty() && name.len() <= 64,
            "invalid Lightning Address length"
        );
        anyhow::ensure!(
            name.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c)),
            "invalid Lightning Address username"
        );
        anyhow::ensure!(
            domain.contains('.')
                && domain.split('.').all(|part| !part.is_empty()
                    && part.len() <= 63
                    && !part.starts_with('-')
                    && !part.ends_with('-')
                    && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')),
            "invalid Lightning Address domain"
        );
        Ok(Self(format!("{name}@{}", domain.to_ascii_lowercase())))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[async_trait]
pub trait AddressInvoices: Send + Sync {
    /// Request an exact-amount receiving invoice. Never makes an outgoing payment.
    async fn invoice(
        &self,
        address: &LightningAddress,
        sats: u64,
        payout_id: &str,
    ) -> anyhow::Result<String>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedPayout {
    pub address: String,
    pub invoice: String,
    pub payment_hash: String,
    pub confirmed_at_ms: Option<u64>,
}

/// Validate through the wallet adapter, including exact amount. Persist the
/// returned invoice/hash before submitting: recovery must reuse this invoice.
pub async fn prepare_payout(
    wallet: &dyn Wallet,
    provider: &dyn AddressInvoices,
    address: &LightningAddress,
    sats: u64,
    payout_id: &str,
) -> anyhow::Result<PreparedPayout> {
    anyhow::ensure!(sats != 0, "cannot pay a zero-sat invoice");
    let invoice = provider.invoice(address, sats, payout_id).await?;
    let decoded = wallet.decode_invoice(&invoice)?;
    anyhow::ensure!(
        decoded.amount_sats == Some(sats),
        "recipient invoice has the wrong amount"
    );
    anyhow::ensure!(
        !decoded.payment_hash.is_empty(),
        "recipient invoice has no payment hash"
    );
    Ok(PreparedPayout {
        address: address.as_str().into(),
        invoice,
        payment_hash: decoded.payment_hash,
        confirmed_at_ms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{FakeAddressInvoices, FakeWallet};
    #[test]
    fn validate_and_normalize_recipient() {
        assert_eq!(
            LightningAddress::parse(" alice@EXAMPLE.com ")
                .unwrap()
                .as_str(),
            "alice@example.com"
        );
        for value in [
            "",
            "alice",
            "@example.com",
            "a@localhost",
            "a@b.com/path",
            "a@b.com@c.com",
            "a\n@b.com",
        ] {
            assert!(LightningAddress::parse(value).is_err(), "{value}");
        }
    }
    #[tokio::test]
    async fn provider_and_wallet_are_separate_and_exact_amount_is_checked() {
        let wallet = FakeWallet::default();
        let address = LightningAddress::parse("alice@example.com").unwrap();
        let prepared = prepare_payout(&wallet, &FakeAddressInvoices, &address, 42, "batch-1")
            .await
            .unwrap();
        assert_eq!(
            wallet
                .decode_invoice(&prepared.invoice)
                .unwrap()
                .amount_sats,
            Some(42)
        );
        assert_eq!(wallet.payment_attempts(), 0); // Resolving never pays.
        struct WrongAmount;
        #[async_trait]
        impl AddressInvoices for WrongAmount {
            async fn invoice(
                &self,
                _: &LightningAddress,
                _: u64,
                _: &str,
            ) -> anyhow::Result<String> {
                Ok("lnfake:999:wrong".into())
            }
        }
        assert!(
            prepare_payout(&wallet, &WrongAmount, &address, 42, "batch-1")
                .await
                .is_err()
        );
        assert_eq!(wallet.payment_attempts(), 0);
    }
}
