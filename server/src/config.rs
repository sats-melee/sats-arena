use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub database: PathBuf,
    /// Anyone who knows this can create an account. Share it with your friends.
    pub invite_code: String,
    pub rules: Rules,
    pub wallet: WalletConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    /// Paid by the victim to the attacker for every 1% of damage dealt.
    pub sats_per_percent: u64,
    /// Paid by every loser to the winner at the end of a game.
    pub win_sats: u64,
    pub max_deposit_sats: u64,
    #[serde(default = "default_invoice_expiry")]
    pub invoice_expiry_secs: u64,
}

fn default_invoice_expiry() -> u64 {
    600
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum WalletConfig {
    /// In-memory wallet with no real sats. Deposits are paid instantly.
    Fake,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.invite_code.trim().is_empty() || config.invite_code == "change-me" {
            bail!("set invite_code in {} to something only your friends know", path.display());
        }
        if config.rules.max_deposit_sats == 0 {
            bail!("rules.max_deposit_sats must be greater than 0");
        }
        Ok(config)
    }
}
