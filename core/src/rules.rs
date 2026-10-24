use serde::{Deserialize, Serialize};

use crate::{Error, Result};

// A conservative accounting bound (21 million BTC), comfortably inside i64.
pub const MAX_BALANCE_SATS: u64 = 2_100_000_000_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    pub sats_per_percent: u64,
    pub win_sats: u64,
    pub max_deposit_sats: u64,
    #[serde(default = "default_invoice_expiry")]
    pub invoice_expiry_secs: u64,
}

fn default_invoice_expiry() -> u64 {
    600
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            sats_per_percent: 1,
            win_sats: 100,
            max_deposit_sats: 100_000,
            invoice_expiry_secs: 600,
        }
    }
}

impl Rules {
    pub fn validate(&self) -> Result<()> {
        if self.max_deposit_sats == 0
            || self.max_deposit_sats > MAX_BALANCE_SATS
            || self.win_sats > MAX_BALANCE_SATS
            || self.sats_per_percent > MAX_BALANCE_SATS
        {
            return Err(Error::Rejected(
                "payment amounts are outside the supported range".into(),
            ));
        }
        if !(1..=86_400).contains(&self.invoice_expiry_secs) {
            return Err(Error::Rejected(
                "invoice expiry must be 1-86400 seconds".into(),
            ));
        }
        Ok(())
    }
}
