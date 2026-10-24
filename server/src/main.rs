mod api;
mod config;
mod ledger;
mod reconcile;
mod settle;
mod wallet;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use crate::{api::AppState, config::WalletConfig, wallet::Wallet};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(|| "sats-arena.toml".into());
    let config = config::Config::load(&path)?;

    let wallet: Box<dyn Wallet> = match config.wallet {
        WalletConfig::Fake => {
            tracing::warn!("using the fake wallet: no real sats are involved");
            Box::new(wallet::FakeWallet::default())
        }
    };

    let state = Arc::new(AppState {
        ledger: Mutex::new(ledger::Ledger::open(&config.database)?),
        wallet,
        rules: config.rules,
        invite_code: config.invite_code,
    });
    tokio::spawn(reconcile::run(state.clone()));

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listening on {}", config.listen))?;
    tracing::info!("Sats Arena server listening on http://{}", config.listen);
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
