//! One outgoing NWC command at a time across every room using the server wallet.
use std::{collections::BTreeMap, sync::Arc};

use sats_arena_core::wallet::{
    AddressInvoices, DecodedInvoice, InvoiceState, LightningAddress, NewInvoice, PaymentState,
    Wallet,
};
use tokio::sync::Mutex;

pub(crate) struct PaymentWallet {
    inner: Arc<dyn Wallet>,
    invoices: Arc<UniqueInvoices>,
    submissions: Mutex<()>,
}

/// Every room sharing a wallet must bind each recipient invoice to one debt.
pub(crate) struct UniqueInvoices {
    inner: Arc<dyn AddressInvoices>,
    wallet: Arc<dyn Wallet>,
    hashes: std::sync::Mutex<BTreeMap<String, String>>,
}

impl UniqueInvoices {
    pub(crate) fn new(inner: Arc<dyn AddressInvoices>, wallet: Arc<dyn Wallet>) -> Self {
        Self {
            inner,
            wallet,
            hashes: Default::default(),
        }
    }

    pub(crate) fn restore(
        &self,
        hashes: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<()> {
        let mut used = self.hashes.lock().unwrap();
        for hash in hashes {
            anyhow::ensure!(
                !used.contains_key(hash),
                "Recipient invoice reused across stored matches; review payment state"
            );
            // Persisted hashes remain spent even if their latest preparation was replaced.
            used.insert(hash.clone(), String::new());
        }
        Ok(())
    }

    fn reserve(&self, invoice: &str, sats: u64, id: &str) -> anyhow::Result<()> {
        let decoded = self.wallet.decode_invoice(invoice)?;
        anyhow::ensure!(
            decoded.amount_sats == Some(sats) && !decoded.payment_hash.is_empty(),
            "Invalid recipient invoice"
        );
        self.reserve_hash(&decoded.payment_hash, id)
    }

    fn reserve_hash(&self, hash: &str, id: &str) -> anyhow::Result<()> {
        let mut used = self.hashes.lock().unwrap();
        if let Some(owner) = used.get(hash) {
            anyhow::ensure!(
                owner == id,
                "Recipient reused a payment hash across matches"
            );
        } else {
            used.insert(hash.to_owned(), id.to_owned());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AddressInvoices for UniqueInvoices {
    async fn invoice(
        &self,
        address: &LightningAddress,
        sats: u64,
        id: &str,
    ) -> anyhow::Result<String> {
        self.invoice_with_comment(address, sats, id, "").await
    }

    async fn invoice_with_comment(
        &self,
        address: &LightningAddress,
        sats: u64,
        id: &str,
        comment: &str,
    ) -> anyhow::Result<String> {
        let invoice = self
            .inner
            .invoice_with_comment(address, sats, id, comment)
            .await?;
        self.reserve(&invoice, sats, &format!("payout:{id}"))?;
        Ok(invoice)
    }
}

impl PaymentWallet {
    pub(crate) fn new(inner: Arc<dyn Wallet>, invoices: Arc<UniqueInvoices>) -> Self {
        Self {
            inner,
            invoices,
            submissions: Mutex::new(()),
        }
    }
}

#[async_trait::async_trait]
impl Wallet for PaymentWallet {
    async fn create_invoice(
        &self,
        amount_sats: u64,
        memo: &str,
        expiry_secs: u64,
    ) -> anyhow::Result<NewInvoice> {
        let invoice = self
            .inner
            .create_invoice(amount_sats, memo, expiry_secs)
            .await?;
        let decoded = self.inner.decode_invoice(&invoice.bolt11)?;
        anyhow::ensure!(
            decoded.amount_sats == Some(amount_sats)
                && !decoded.payment_hash.is_empty()
                && decoded.payment_hash == invoice.payment_hash,
            "Invalid entry invoice"
        );
        self.invoices
            .reserve_hash(&invoice.payment_hash, &format!("entry:{memo}"))?;
        Ok(invoice)
    }

    async fn invoice_state(&self, payment_hash: &str) -> anyhow::Result<InvoiceState> {
        self.inner.invoice_state(payment_hash).await
    }

    fn decode_invoice(&self, bolt11: &str) -> anyhow::Result<DecodedInvoice> {
        self.inner.decode_invoice(bolt11)
    }

    async fn pay_invoice(&self, bolt11: &str, max_fee_sats: u64) -> anyhow::Result<PaymentState> {
        // Cancellation while queued sends nothing. The host retains its original
        // hash as uncertain and reconciles it by lookup, without resubmission.
        let _submission = self.submissions.lock().await;
        self.inner.pay_invoice(bolt11, max_fee_sats).await
    }

    async fn payment_state(&self, payment_hash: &str) -> anyhow::Result<PaymentState> {
        self.inner.payment_state(payment_hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_arena_core::wallet::{FakeAddressInvoices, FakeWallet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        sync::Semaphore,
        time::{Duration, timeout},
    };

    struct RepeatedInvoice;
    #[async_trait::async_trait]
    impl AddressInvoices for RepeatedInvoice {
        async fn invoice(&self, _: &LightningAddress, _: u64, _: &str) -> anyhow::Result<String> {
            Ok("lnfake:500:repeated".into())
        }
    }

    #[tokio::test]
    async fn concurrent_preparations_cannot_share_an_invoice_and_own_retries_are_safe() {
        let invoices = Arc::new(UniqueInvoices::new(
            Arc::new(RepeatedInvoice),
            Arc::new(FakeWallet::default()),
        ));
        let address = LightningAddress::parse("alice@example.com").unwrap();
        assert!(
            invoices
                .invoice(&address, 100, "wrong-amount")
                .await
                .is_err()
        );
        let mut attempts = Vec::new();
        for i in 0..8 {
            let invoices = invoices.clone();
            let address = address.clone();
            attempts.push(tokio::spawn(async move {
                (
                    i,
                    invoices
                        .invoice(&address, 500, &format!("debt-{i}"))
                        .await
                        .is_ok(),
                )
            }));
        }
        let mut accepted = Vec::new();
        for attempt in attempts {
            let (id, ok) = attempt.await.unwrap();
            if ok {
                accepted.push(id);
            }
        }
        assert_eq!(accepted.len(), 1);
        assert!(
            invoices
                .invoice(&address, 500, &format!("debt-{}", accepted[0]))
                .await
                .is_ok()
        );
    }

    struct ControlledWallet {
        fake: FakeWallet,
        active: AtomicUsize,
        maximum: AtomicUsize,
        attempts: AtomicUsize,
        started: Semaphore,
        release: Semaphore,
    }
    impl Default for ControlledWallet {
        fn default() -> Self {
            Self {
                fake: FakeWallet::default(),
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
                attempts: AtomicUsize::new(0),
                started: Semaphore::new(0),
                release: Semaphore::new(0),
            }
        }
    }
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl Wallet for ControlledWallet {
        async fn create_invoice(
            &self,
            sats: u64,
            memo: &str,
            expiry: u64,
        ) -> anyhow::Result<NewInvoice> {
            self.fake.create_invoice(sats, memo, expiry).await
        }
        async fn invoice_state(&self, hash: &str) -> anyhow::Result<InvoiceState> {
            self.fake.invoice_state(hash).await
        }
        fn decode_invoice(&self, invoice: &str) -> anyhow::Result<DecodedInvoice> {
            self.fake.decode_invoice(invoice)
        }
        async fn pay_invoice(&self, _invoice: &str, _fees: u64) -> anyhow::Result<PaymentState> {
            let count = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _active = Active(&self.active);
            self.maximum.fetch_max(count, Ordering::SeqCst);
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(PaymentState::Succeeded { fee_sats: 0 })
        }
        async fn payment_state(&self, hash: &str) -> anyhow::Result<PaymentState> {
            self.fake.payment_state(hash).await
        }
    }

    #[tokio::test]
    async fn rooms_share_one_send_lane_while_invoice_and_payment_lookups_continue() {
        let inner = Arc::new(ControlledWallet::default());
        let invoices = Arc::new(UniqueInvoices::new(
            Arc::new(FakeAddressInvoices),
            inner.clone(),
        ));
        let wallet = Arc::new(PaymentWallet::new(inner.clone(), invoices));
        let mut sends = Vec::new();
        for _ in 0..8 {
            let wallet = wallet.clone();
            sends.push(tokio::spawn(async move {
                wallet.pay_invoice("invoice", 0).await
            }));
        }
        for _ in 0..8 {
            timeout(Duration::from_secs(1), inner.started.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
            assert_eq!(inner.active.load(Ordering::SeqCst), 1);
            let invoice = timeout(
                Duration::from_millis(100),
                wallet.create_invoice(1, "test", 60),
            )
            .await
            .unwrap()
            .unwrap();
            let hash = wallet.decode_invoice(&invoice.bolt11).unwrap().payment_hash;
            assert_eq!(
                timeout(Duration::from_millis(100), wallet.invoice_state(&hash))
                    .await
                    .unwrap()
                    .unwrap(),
                InvoiceState::Pending
            );
            assert_eq!(
                timeout(Duration::from_millis(100), wallet.payment_state(&hash))
                    .await
                    .unwrap()
                    .unwrap(),
                PaymentState::Pending
            );
            inner.release.add_permits(1);
        }
        for send in sends {
            assert_eq!(
                send.await.unwrap().unwrap(),
                PaymentState::Succeeded { fee_sats: 0 }
            );
        }
        assert_eq!(inner.maximum.load(Ordering::SeqCst), 1);
        assert_eq!(inner.attempts.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn timed_out_queued_send_is_never_dispatched_and_its_hash_can_be_looked_up() {
        let inner = Arc::new(ControlledWallet::default());
        let invoices = Arc::new(UniqueInvoices::new(
            Arc::new(FakeAddressInvoices),
            inner.clone(),
        ));
        let wallet = Arc::new(PaymentWallet::new(inner.clone(), invoices));
        let first = wallet.clone();
        let first = tokio::spawn(async move { first.pay_invoice("first", 0).await });
        inner.started.acquire().await.unwrap().forget();
        assert!(
            timeout(Duration::from_millis(1), wallet.pay_invoice("queued", 0))
                .await
                .is_err()
        );
        assert_eq!(inner.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            wallet.payment_state("original-queued-hash").await.unwrap(),
            PaymentState::Pending
        );
        inner.release.add_permits(1);
        first.await.unwrap().unwrap();
        tokio::task::yield_now().await;
        assert_eq!(inner.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(inner.active.load(Ordering::SeqCst), 0);
    }
}
