mod api;
mod config;

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use sats_arena_core::{
    Engine,
    wallet::{FakeWallet, Wallet},
};
use tracing_subscriber::EnvFilter;

use crate::{api::AppState, config::WalletConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "sats-arena.toml".into());
    let config = config::Config::load(&path)?;

    let wallet: Arc<dyn Wallet> = match config.wallet {
        WalletConfig::Fake => {
            tracing::warn!("using the fake wallet: no real sats are involved");
            Arc::new(FakeWallet::auto_pay())
        }
        WalletConfig::Nwc { .. } => {
            anyhow::bail!("the nwc wallet backend isn't implemented yet; use backend = \"fake\"")
        }
    };

    let engine = Engine::open(&config.database, config.rules, wallet)?;
    let state = Arc::new(AppState {
        engine: engine.clone(),
        invite_code: config.invite_code,
    });

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listening on {}", config.listen))?;
    tracing::info!("Sats Arena server listening on http://{}", config.listen);
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    engine.shutdown().await?;
    Ok(())
}
