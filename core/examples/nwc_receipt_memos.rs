//! Read-only receiver-side memo checks. Credentials arrive on private stdin.
use nwc::{
    NostrWalletConnect,
    prelude::{LookupInvoiceRequest, NostrWalletConnectUri, TransactionState, TransactionType},
};
use serde::Deserialize;
use std::{
    io::{self, Read},
    time::Duration,
};

#[derive(Deserialize)]
struct Payment {
    payment_hash: String,
    memo: String,
}
#[derive(Deserialize)]
struct Config {
    wallet_uri: String,
    payments: Vec<Payment>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().take(32768).read_to_string(&mut input)?;
    let config: Config =
        serde_json::from_str(&input).map_err(|_| anyhow::anyhow!("Invalid input"))?;
    let uri = NostrWalletConnectUri::parse(&config.wallet_uri)
        .map_err(|_| anyhow::anyhow!("Invalid connection"))?;
    let client = NostrWalletConnect::builder(uri)
        .timeout(Duration::from_secs(12))
        .build();
    for payment in config.payments {
        let response = tokio::time::timeout(Duration::from_secs(15), async {
            client
                .lookup_invoice(LookupInvoiceRequest {
                    payment_hash: Some(payment.payment_hash.clone()),
                    invoice: None,
                })
                .await
        })
        .await
        .map_err(|_| anyhow::anyhow!("Receiver lookup timed out"))?
        .map_err(|_| anyhow::anyhow!("Receiver lookup failed"))?;
        let metadata_has_memo = !payment.memo.is_empty()
            && response
                .metadata
                .as_ref()
                .is_some_and(|m| m.to_string().contains(&payment.memo));
        let description_has_memo = !payment.memo.is_empty()
            && response
                .description
                .as_ref()
                .is_some_and(|d| d.contains(&payment.memo));
        println!(
            "{}",
            serde_json::json!({"payment_hash":payment.payment_hash,"memo":payment.memo,
            "incoming":response.transaction_type == Some(TransactionType::Incoming),
            "settled":response.state == Some(TransactionState::Settled) || response.settled_at.is_some(),
            "metadata_has_memo":metadata_has_memo,"description_has_memo":description_has_memo})
        );
    }
    client.shutdown().await;
    Ok(())
}
