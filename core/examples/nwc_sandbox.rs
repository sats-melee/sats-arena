//! Opt-in real NWC sandbox round trip. Credentials arrive on stdin, never argv.
use sats_arena_core::wallet::{InvoiceState, NWC_WALLET_MANAGED_FEES, NwcWallet, Wallet};
use serde::Deserialize;
use std::io::{self, Read};

#[derive(Deserialize)]
struct Config {
    arena_uri: String,
    payer_uri: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().take(32768).read_to_string(&mut input)?;
    let config: Config = serde_json::from_str(&input)?;
    let arena = NwcWallet::connect(&config.arena_uri).await?;
    let payer = NwcWallet::connect(&config.payer_uri).await?;
    let invoice = arena
        .create_invoice(1, "Arena sandbox NWC check", 600)
        .await?;
    let payment = payer
        .pay_invoice(&invoice.bolt11, NWC_WALLET_MANAGED_FEES)
        .await?;
    for _ in 0..30 {
        if arena.invoice_state(&invoice.payment_hash).await?
            == (InvoiceState::Paid { amount_sats: 1 })
        {
            println!(
                "Sandbox NWC: invoice created, payment {payment:?}, incoming receipt confirmed"
            );
            arena.shutdown().await;
            payer.shutdown().await;
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    anyhow::bail!("Sandbox receipt was not confirmed")
}
