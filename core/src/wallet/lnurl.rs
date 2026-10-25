//! Read-only LUD-16/LUD-06 verification and invoice resolution. Never pays.
use super::{AddressInvoices, LightningAddress};
use anyhow::{Context, ensure};
use async_trait::async_trait;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef, Currency};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_BODY: usize = 256 * 1024;
pub const VERIFICATION_TTL_MS: u64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddressVerification {
    pub address: String,
    pub min_msat: u64,
    pub max_msat: u64,
    pub callback_host: String,
    pub checked_at_ms: u64,
    pub test_invoice_msat: u64,
    pub test_payment_hash: String,
    pub invoice_expires_at_ms: u64,
    pub description_binding: String,
}
impl AddressVerification {
    pub fn fresh(&self, now_ms: u64) -> bool {
        now_ms >= self.checked_at_ms
            && now_ms.saturating_sub(self.checked_at_ms) < VERIFICATION_TTL_MS
    }
    pub fn supports_match(&self, entry_sats: u64) -> bool {
        self.min_msat <= 1000 && self.max_msat >= entry_sats.saturating_mul(2000)
    }
}

#[async_trait]
pub trait AddressVerifier: Send + Sync {
    async fn verify(&self, address: &LightningAddress) -> anyhow::Result<AddressVerification>;
}

#[derive(Clone, Default)]
pub struct LnurlClient;
#[derive(Deserialize, Clone)]
struct PayRequest {
    tag: String,
    callback: String,
    metadata: String,
    #[serde(rename = "minSendable")]
    min_msat: u64,
    #[serde(rename = "maxSendable")]
    max_msat: u64,
    #[serde(default, rename = "payerData")]
    payer_data: serde_json::Value,
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let b = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && b[0] != 0
                && b[0] < 224
                && !(b[0] == 100 && (64..128).contains(&b[1]))
                && !(b[0] == 198 && (b[1] == 18 || b[1] == 19))
                && !ip.is_documentation()
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = ip.segments();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}
fn checked_url(value: &str) -> anyhow::Result<Url> {
    let url = Url::parse(value).context("invalid receiving-service URL")?;
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "receiving service must use HTTPS without credentials on port 443"
    );
    let host = url.host_str().context("receiving service has no host")?;
    ensure!(
        host != "localhost" && !host.ends_with(".localhost") && !host.ends_with(".local"),
        "receiving service must use a public host"
    );
    if let Ok(ip) = host.trim_matches(['[', ']']).parse() {
        ensure!(
            public_ip(ip),
            "receiving service must use public IP addresses"
        );
    }
    Ok(url)
}
fn parse_request(value: serde_json::Value) -> anyhow::Result<PayRequest> {
    service_error(&value)?;
    let request: PayRequest =
        serde_json::from_value(value).context("invalid LNURL-pay response")?;
    ensure!(
        request.tag == "payRequest",
        "address does not support LNURL-pay"
    );
    ensure!(
        request.min_msat > 0 && request.max_msat >= request.min_msat,
        "receiving service advertised invalid payment limits"
    );
    checked_url(&request.callback)?;
    let metadata: Vec<Vec<serde_json::Value>> = serde_json::from_str(&request.metadata)
        .context("receiving service advertised invalid metadata")?;
    ensure!(
        metadata
            .iter()
            .all(|row| row.first().is_some_and(|v| v.is_string()))
            && metadata
                .iter()
                .filter(
                    |row| row.first().and_then(|v| v.as_str()) == Some("text/plain")
                        && row
                            .get(1)
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                )
                .count()
                == 1,
        "receiving metadata needs one text/plain description"
    );
    if let Some(fields) = request.payer_data.as_object() {
        ensure!(
            !fields
                .values()
                .any(|v| v.get("mandatory").and_then(|v| v.as_bool()) == Some(true)),
            "receiving service requires additional payer information"
        );
    }
    Ok(request)
}
fn service_error(value: &serde_json::Value) -> anyhow::Result<()> {
    if value.get("status").and_then(|v| v.as_str()) == Some("ERROR") {
        anyhow::bail!(
            "receiving service rejected request: {}",
            value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
        );
    }
    Ok(())
}
fn validate_invoice(
    invoice: &str,
    request: &PayRequest,
    msat: u64,
    now_ms: u64,
) -> anyhow::Result<(String, u64)> {
    let invoice: Bolt11Invoice = invoice
        .parse()
        .context("receiving service returned invalid BOLT11")?;
    invoice
        .check_signature()
        .context("invoice signature is invalid")?;
    ensure!(
        invoice.currency() == Currency::Bitcoin,
        "receiving invoice is not on Bitcoin mainnet"
    );
    ensure!(
        invoice.amount_milli_satoshis() == Some(msat),
        "receiving invoice has the wrong amount"
    );
    let expected = hex::encode(Sha256::digest(request.metadata.as_bytes()));
    match invoice.description() {
        Bolt11InvoiceDescriptionRef::Hash(hash) => ensure!(
            hash.0.to_string() == expected,
            "receiving invoice metadata hash does not match"
        ),
        Bolt11InvoiceDescriptionRef::Direct(description) => {
            let metadata: Vec<Vec<serde_json::Value>> = serde_json::from_str(&request.metadata)?;
            let text = metadata
                .iter()
                .find(|row| row.first().and_then(|v| v.as_str()) == Some("text/plain"))
                .and_then(|row| row.get(1))
                .and_then(|v| v.as_str())
                .context("missing metadata description")?;
            ensure!(
                description.to_string() == text,
                "signed invoice description does not match receiving metadata"
            );
        }
    }
    let expiry_ms = invoice
        .expires_at()
        .context("invalid invoice expiry")?
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    ensure!(
        expiry_ms > now_ms.saturating_add(30_000),
        "receiving invoice expires too soon"
    );
    ensure!(
        invoice.duration_since_epoch().as_millis() <= u128::from(now_ms.saturating_add(60_000)),
        "receiving invoice timestamp is in the future"
    );
    Ok((invoice.payment_hash().to_string(), expiry_ms))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl LnurlClient {
    async fn get_json(&self, mut url: Url) -> anyhow::Result<serde_json::Value> {
        for redirects in 0..=3 {
            let host = url.host_str().context("missing host")?;
            // Pin validated DNS answers for this request. Redirects are not followed.
            let addresses: Vec<SocketAddr> =
                tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
                    .await
                    .context("receiving-service DNS timed out")??
                    .collect();
            ensure!(
                !addresses.is_empty() && addresses.iter().all(|s| public_ip(s.ip())),
                "receiving service did not resolve to public addresses"
            );
            let client = Client::builder()
                .no_proxy()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .user_agent("SatsArena/0.1 LNURL verification")
                .resolve_to_addrs(host, &addresses)
                .build()?;
            let mut response = client
                .get(url.clone())
                .send()
                .await
                .context("receiving service is unreachable")?;
            if response.status().is_redirection() {
                ensure!(redirects < 3, "receiving service redirected too many times");
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("receiving redirect has no Location")?
                    .to_str()?;
                url = checked_url(url.join(location)?.as_str())?;
                continue;
            }
            ensure!(
                response.status().is_success(),
                "receiving service returned HTTP {}",
                response.status()
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= MAX_BODY as u64),
                "receiving response is too large"
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= MAX_BODY,
                    "receiving response is too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            return serde_json::from_slice(&bytes).context("receiving service did not return JSON");
        }
        anyhow::bail!("receiving service redirected too many times")
    }
    async fn discover(&self, address: &LightningAddress) -> anyhow::Result<PayRequest> {
        let (user, domain) = address
            .as_str()
            .split_once('@')
            .context("invalid address")?;
        let mut url = checked_url(&format!("https://{domain}/"))?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid address URL"))?
            .extend([".well-known", "lnurlp", user]);
        parse_request(self.get_json(url).await?)
    }
    async fn request_invoice(
        &self,
        request: &PayRequest,
        msat: u64,
    ) -> anyhow::Result<(String, String, u64)> {
        ensure!(
            (request.min_msat..=request.max_msat).contains(&msat),
            "amount is outside receiving limits"
        );
        let mut url = checked_url(&request.callback)?;
        ensure!(
            !url.query_pairs().any(|(key, _)| key == "amount"),
            "callback already contains an amount"
        );
        url.query_pairs_mut()
            .append_pair("amount", &msat.to_string());
        let value = self.get_json(url).await?;
        service_error(&value)?;
        let invoice = value
            .get("pr")
            .and_then(|v| v.as_str())
            .context("receiving service returned no invoice")?;
        let (hash, expiry) = validate_invoice(invoice, request, msat, now_ms())?;
        Ok((invoice.into(), hash, expiry))
    }
}
#[async_trait]
impl AddressVerifier for LnurlClient {
    async fn verify(&self, address: &LightningAddress) -> anyhow::Result<AddressVerification> {
        let request = self.discover(address).await?;
        let test_msat = request
            .min_msat
            .div_ceil(1000)
            .max(1)
            .checked_mul(1000)
            .context("receiving minimum is too large")?;
        let (invoice, hash, expiry) = self.request_invoice(&request, test_msat).await?;
        let invoice: Bolt11Invoice = invoice.parse()?;
        Ok(AddressVerification {
            address: address.as_str().into(),
            min_msat: request.min_msat,
            max_msat: request.max_msat,
            callback_host: checked_url(&request.callback)?.host_str().unwrap().into(),
            checked_at_ms: now_ms(),
            test_invoice_msat: test_msat,
            test_payment_hash: hash,
            invoice_expires_at_ms: expiry,
            description_binding: match invoice.description() {
                Bolt11InvoiceDescriptionRef::Hash(_) => "metadata_hash".into(),
                Bolt11InvoiceDescriptionRef::Direct(_) => "signed_text_description".into(),
            },
        })
    }
}
#[async_trait]
impl AddressInvoices for LnurlClient {
    async fn invoice(
        &self,
        address: &LightningAddress,
        sats: u64,
        _: &str,
    ) -> anyhow::Result<String> {
        let request = self.discover(address).await?;
        let msat = sats
            .checked_mul(1000)
            .context("payment amount is too large")?;
        Ok(self.request_invoice(&request, msat).await?.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        hashes::{Hash, sha256},
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};

    fn request() -> PayRequest {
        parse_request(serde_json::json!({
            "tag":"payRequest", "callback":"https://receiver.example/pay?user=alice",
            "metadata":"[[\"text/plain\",\"Alice payments\"],[\"text/identifier\",\"alice@receiver.example\"]]",
            "minSendable":1000, "maxSendable":10_000_000,
        })).unwrap()
    }
    fn invoice(
        request: &PayRequest,
        amount: u64,
        at_ms: u64,
        direct: Option<&str>,
        currency: Currency,
    ) -> String {
        let builder = InvoiceBuilder::new(currency).amount_milli_satoshis(amount);
        let builder = match direct {
            Some(text) => builder.description(text.into()),
            None => builder.description_hash(sha256::Hash::hash(request.metadata.as_bytes())),
        };
        let secret = SecretKey::from_slice(&[42; 32]).unwrap();
        builder
            .payment_hash(sha256::Hash::hash(b"fixture-payment"))
            .payment_secret(PaymentSecret([9; 32]))
            .duration_since_epoch(Duration::from_millis(at_ms))
            .expiry_time(Duration::from_secs(3600))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|message| Secp256k1::new().sign_ecdsa_recoverable(message, &secret))
            .unwrap()
            .to_string()
    }
    #[test]
    fn signed_mainnet_invoice_is_bound_to_amount_description_and_expiry() {
        let request = request();
        let now = 1_800_000_000_000;
        let hash_invoice = invoice(&request, 1000, now, None, Currency::Bitcoin);
        assert!(validate_invoice(&hash_invoice, &request, 1000, now).is_ok());
        assert!(validate_invoice(&hash_invoice, &request, 2000, now).is_err());
        assert!(validate_invoice(&hash_invoice, &request, 1000, now + 3_600_000).is_err());
        assert!(validate_invoice(&hash_invoice, &request, 1000, now - 120_000).is_err());
        let mut changed = request.clone();
        changed.metadata.push(' ');
        assert!(validate_invoice(&hash_invoice, &changed, 1000, now).is_err());
        let direct = invoice(
            &request,
            1000,
            now,
            Some("Alice payments"),
            Currency::Bitcoin,
        );
        assert!(validate_invoice(&direct, &request, 1000, now).is_ok());
        let wrong = invoice(&request, 1000, now, Some("Someone else"), Currency::Bitcoin);
        assert!(validate_invoice(&wrong, &request, 1000, now).is_err());
        let testnet = invoice(&request, 1000, now, None, Currency::BitcoinTestnet);
        assert!(validate_invoice(&testnet, &request, 1000, now).is_err());
        assert!(validate_invoice("not-an-invoice", &request, 1000, now).is_err());
    }
    #[test]
    fn bad_endpoints_limits_metadata_and_required_payer_data_are_rejected() {
        for url in [
            "http://receiver.example/pay",
            "https://user:pass@receiver.example/pay",
            "https://receiver.example:444/pay",
            "https://127.0.0.1/pay",
            "https://[::1]/pay",
            "https://localhost/pay",
            "https://server.local/pay",
            "https://receiver.example/pay#fragment",
        ] {
            assert!(checked_url(url).is_err(), "{url}");
        }
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        let valid = serde_json::json!({
            "tag":"payRequest", "callback":"https://receiver.example/pay",
            "metadata":"[[\"text/plain\",\"Alice\"]]", "minSendable":1000, "maxSendable":10000
        });
        for (key, value) in [
            ("tag", serde_json::json!("withdrawRequest")),
            ("minSendable", serde_json::json!(0)),
            ("maxSendable", serde_json::json!(500)),
            ("metadata", serde_json::json!("[]")),
            ("callback", serde_json::json!("http://receiver.example/pay")),
            ("payerData", serde_json::json!({"email":{"mandatory":true}})),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(parse_request(bad).is_err(), "{key}");
        }
        assert!(
            parse_request(serde_json::json!({"status":"ERROR","reason":"Unknown user"})).is_err()
        );
    }
    #[test]
    fn verification_freshness_and_match_compatibility_are_separate_from_ownership() {
        let mut verified = AddressVerification {
            address: "alice@receiver.example".into(),
            min_msat: 1000,
            max_msat: 1_000_000,
            callback_host: "receiver.example".into(),
            checked_at_ms: 1000,
            test_invoice_msat: 1000,
            test_payment_hash: "fixture".into(),
            invoice_expires_at_ms: 3_601_000,
            description_binding: "metadata_hash".into(),
        };
        assert!(verified.fresh(1000) && verified.supports_match(500));
        assert!(!verified.fresh(1000 + VERIFICATION_TTL_MS));
        assert!(!verified.fresh(999));
        verified.min_msat = 2000;
        assert!(!verified.supports_match(500));
        verified.min_msat = 1000;
        verified.max_msat = 999_999;
        assert!(!verified.supports_match(500));
    }
}
