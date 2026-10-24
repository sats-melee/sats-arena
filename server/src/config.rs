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

/// Lets a launcher pass the connection string without writing it to disk.
pub const NWC_URI_ENV_VAR: &str = "SATS_ARENA_NWC_URI";

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum WalletConfig {
    /// In-memory wallet with no real sats. Deposits are paid instantly.
    Fake,
    /// Nostr Wallet Connect. `nwc_uri` may be left out and set through
    /// `SATS_ARENA_NWC_URI` instead.
    Nwc {
        #[serde(default)]
        nwc_uri: Option<String>,
    },
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.invite_code.trim().is_empty() || config.invite_code == "change-me" {
            bail!("set invite_code in {} to something only your friends know", path.display());
        }
        if let WalletConfig::Nwc { nwc_uri } = &mut config.wallet {
            if let Some(uri) = std::env::var(NWC_URI_ENV_VAR).ok().filter(|v| !v.trim().is_empty()) {
                *nwc_uri = Some(uri);
            }
            if nwc_uri.as_deref().is_none_or(|uri| !uri.starts_with("nostr+walletconnect://")) {
                bail!("the nwc wallet needs a nostr+walletconnect:// URI in nwc_uri or {NWC_URI_ENV_VAR}");
            }
        }
        if config.rules.max_deposit_sats == 0 {
            bail!("rules.max_deposit_sats must be greater than 0");
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config: Config = toml::from_str(include_str!("../../sats-arena.example.toml")).unwrap();
        assert!(matches!(config.wallet, WalletConfig::Fake));
    }

    #[test]
    fn nwc_uri_is_optional_in_the_file() {
        let wallet: WalletConfig = toml::from_str("backend = \"nwc\"").unwrap();
        assert!(matches!(wallet, WalletConfig::Nwc { nwc_uri: None }));
        assert!(toml::from_str::<WalletConfig>("backend = \"nwc\"\nunknown = 1").is_err());
    }
}
