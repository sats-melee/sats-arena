//! Shared, single-owner payment runtime for embedded and server-hosted matches.
//! Call `tick` on a background Tokio runtime, including after rooms expire.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRules {
    pub entry_sats: u64,
    pub sats_per_percent: u64,
    pub batch_window_ms: u64,
    pub surcharge_bps: u32,
}

#[derive(Clone)]
pub struct HostWallet {
    pub wallet: Arc<dyn Wallet>,
    pub provider: Arc<dyn AddressInvoices>,
    pub verifier: Arc<dyn AddressVerifier>,
    /// Public wallet fingerprint; never the NWC credential.
    pub identity: String,
    pub mode: String,
}

pub(super) struct HostedOpen {
    pub pot: MatchPot,
    pub identity: String,
    pub mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub match_id: String,
    pub phase: Phase,
    pub frozen: bool,
    pub mode: String,
    pub paid: [bool; 4],
    pub verified: [bool; 4],
    pub verification_state: [u32; 4],
    pub verification_message: [String; 4],
    pub addresses: [String; 4],
    pub invoices: [String; 4],
    pub remaining_sats: [u64; 4],
    pub earned_sats: [u64; 4],
    pub pending_sats: [u64; 4],
    pub last_credit_sequence: u64,
    pub entry_sats: u64,
    pub invoice_sats: u64,
    pub sats_per_percent: u64,
    pub batch_window_ms: u64,
    pub surcharge_bps: u32,
    pub teams: bool,
    pub kinds: [u8; 4],
    pub team_ids: [u8; 4],
    pub characters: [u8; 4],
    pub batch_remaining_ms: [u64; 4],
    pub damage_centi: [[u64; 4]; 4],
    pub payment_log: String,
    pub error: Option<String>,
}

/// Reuses native persistence, verification, payout scheduling and recovery.
/// The caller authenticates clients and supplies only mutually agreed events.
pub struct MatchHost {
    runtime: Demo,
    tick_failed: bool,
}

impl MatchHost {
    /// Rebuild the server wallet's invoice-reuse guard before resuming any host.
    pub fn used_payment_hashes(&self) -> &BTreeSet<String> {
        &self.runtime.saved.used_payment_hashes
    }

    pub fn used_entry_hashes(&self) -> &BTreeSet<String> {
        &self.runtime.saved.used_entry_hashes
    }

    pub async fn open(
        folder: &Path,
        match_id: &str,
        rules: HostRules,
        wallet: HostWallet,
    ) -> Result<Self> {
        if match_id.len() != 32 || !match_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Rejected("Invalid hosted match ID".into()));
        }
        if wallet.identity.len() != 64
            || !wallet.identity.bytes().all(|c| c.is_ascii_hexdigit())
            || !matches!(wallet.mode.as_str(), "nwc" | "sandbox" | "fake")
        {
            return Err(Error::Rejected(
                "Invalid host wallet identity or mode".into(),
            ));
        }
        let mut pot = MatchPot::new(match_id.into(), rules.entry_sats, rules.sats_per_percent)?;
        pot.set_batch_window_ms(rules.batch_window_ms)?;
        pot.set_surcharge_bps(rules.surcharge_bps)?;
        // This loader validates stored identity/terms while holding the state lock,
        // before a save, migration or wallet operation can change the old state.
        let mut runtime = Demo::open_with_match(
            folder,
            wallet.wallet,
            wallet.provider,
            None,
            wallet.verifier,
            Some(HostedOpen {
                pot,
                identity: wallet.identity,
                mode: wallet.mode,
            }),
        )
        .await?;
        runtime.io.serialize_submissions = true;
        runtime.io.track_missed = true;
        Ok(Self {
            runtime,
            tick_failed: false,
        })
    }

    fn active(&self) -> Result<()> {
        if self.runtime.saved.frozen {
            return Err(Error::Rejected("Match is frozen for review".into()));
        }
        Ok(())
    }

    async fn command(&mut self, command: Command) -> Result<()> {
        let result = self.runtime.command(command).await;
        if matches!(&result, Err(Error::Internal(_))) {
            // A failed durable transition must not later authorize an outgoing send.
            self.runtime.saved.frozen = true;
            self.tick_failed = true;
        }
        result
    }

    pub async fn configure_roster(
        &mut self,
        teams: bool,
        kinds: [u8; 4],
        team_ids: [u8; 4],
        characters: [u8; 4],
    ) -> Result<()> {
        self.active()?;
        if kinds.iter().zip(characters).any(|(&kind, character)| {
            (kind < 2 && character > 32 && character != 255) || (kind == 3 && character != 255)
        }) {
            return Err(Error::Rejected("Invalid hosted character selection".into()));
        }
        // Shared hosts never rotate the caller's financial match ID on a roster edit.
        let mut pot = self.runtime.saved.pot.clone();
        pot.configure(teams, kinds, team_ids)?;
        self.runtime.saved.pot = pot;
        for player in 0..4 {
            if !self.runtime.saved.pot.participates(player) && !self.runtime.saved.pot.paid[player]
            {
                self.runtime.retire_entry(player, None);
            }
        }
        self.runtime.saved.roster = Some(Roster {
            teams,
            kinds,
            team_ids,
            characters,
        });
        let result = self.runtime.save();
        if result.is_err() {
            self.runtime.saved.frozen = true;
        }
        result
    }

    pub async fn set_address(&mut self, slot: usize, address: String) -> Result<()> {
        self.active()?;
        self.command(Command::Address(slot, address)).await
    }

    pub fn snapshot(&self) -> HostSnapshot {
        let pot = &self.runtime.saved.pot;
        let now = now_ms();
        let view = self.runtime.view().unwrap_or_default();
        HostSnapshot {
            match_id: pot.id.clone(),
            phase: pot.phase,
            frozen: self.runtime.saved.frozen,
            mode: self
                .runtime
                .saved
                .host_mode
                .clone()
                .unwrap_or_else(|| "nwc".into()),
            paid: pot.paid,
            verified: std::array::from_fn(|i| self.runtime.require_address(i).is_ok()),
            verification_state: view.verification_state,
            verification_message: view.verification_message.map(|bytes| {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            }),
            addresses: self.runtime.saved.addresses.clone(),
            invoices: self.runtime.saved.invoices.clone(),
            remaining_sats: pot.remaining,
            earned_sats: pot.earned,
            pending_sats: pot.pending(),
            last_credit_sequence: pot.last_credit_sequence,
            entry_sats: pot.entry_sats,
            invoice_sats: pot.invoice_sats(),
            sats_per_percent: pot.sats_per_percent,
            batch_window_ms: pot.batch_window_ms,
            surcharge_bps: pot.surcharge_bps,
            teams: pot.teams,
            kinds: pot.kinds,
            team_ids: pot.team_ids,
            characters: self
                .runtime
                .saved
                .roster
                .as_ref()
                .map(|r| r.characters)
                .unwrap_or([255; 4]),
            batch_remaining_ms: pot
                .batch_deadline_ms
                .map(|d| d.unwrap_or(now).saturating_sub(now)),
            damage_centi: pot.damage_by_player_centi,
            payment_log: {
                let end = view
                    .payment_log
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(view.payment_log.len());
                String::from_utf8_lossy(&view.payment_log[..end]).into_owned()
            },
            error: self.runtime.payment_notice().or_else(|| {
                self.tick_failed.then(|| {
                    "Wallet operations need attention; pending funds remain reserved".into()
                })
            }),
        }
    }

    pub async fn tick(&mut self) -> Result<()> {
        let result = self.runtime.tick().await;
        self.tick_failed = result.is_err();
        if matches!(&result, Err(Error::Internal(_))) {
            self.runtime.saved.frozen = true;
        }
        result
    }

    pub async fn start(&mut self) -> Result<()> {
        self.active()?;
        if self.runtime.saved.pot.phase == Phase::Playing {
            return Ok(());
        }
        if self.runtime.saved.roster.as_ref().is_none_or(|roster| {
            roster
                .kinds
                .iter()
                .zip(roster.characters)
                .any(|(&kind, character)| kind < 2 && character > 32)
        }) {
            return Err(Error::Rejected(
                "Agree character selections before starting".into(),
            ));
        }
        self.command(Command::Action(4)).await
    }

    pub async fn credits(&mut self, credits: Vec<Credit>) -> Result<()> {
        self.active()?;
        self.command(Command::Credits(credits)).await
    }

    pub async fn finish(&mut self, outcome: Outcome, complete: bool) -> Result<()> {
        self.active()?;
        if !complete && outcome != Outcome::NoContest {
            return Err(Error::Rejected(
                "Incomplete gameplay cannot authorize a winner payout".into(),
            ));
        }
        let action = match outcome {
            Outcome::NoContest => {
                if complete {
                    5
                } else {
                    13
                }
            }
            Outcome::Winner(0) => 6,
            Outcome::Winner(1) => 7,
            Outcome::Winner(2) => 11,
            Outcome::Winner(3) => 12,
            Outcome::Winner(_) => return Err(Error::Rejected("Invalid winner slot".into())),
        };
        if matches!(self.runtime.saved.pot.phase, Phase::Funding | Phase::Ready) {
            self.runtime.retire_entries();
        }
        self.command(Command::Action(action)).await
    }

    pub async fn cancel(&mut self) -> Result<()> {
        self.active()?;
        if self.runtime.saved.pot.outcome == Some(Outcome::NoContest) {
            return Ok(());
        }
        if !matches!(self.runtime.saved.pot.phase, Phase::Funding | Phase::Ready) {
            return Err(Error::Rejected(
                "Only an unstarted match can be cancelled".into(),
            ));
        }
        self.finish(Outcome::NoContest, true).await
    }

    /// Persist the stop before returning; only already-InFlight sends can reconcile.
    pub fn freeze(&mut self) -> Result<()> {
        self.runtime.saved.frozen = true;
        self.runtime.save()
    }

    /// Offline operator recovery only: preserve agreed earnings and refund the remainder.
    pub async fn refund_after_review(&mut self) -> Result<()> {
        if !self.runtime.saved.frozen {
            // The host journal may commit before its caller records completed review.
            if self.runtime.saved.pot.outcome == Some(Outcome::NoContest)
                && matches!(
                    self.runtime.saved.pot.phase,
                    Phase::Settling | Phase::Settled
                )
            {
                return Ok(());
            }
            return Err(Error::Rejected(
                "Only a frozen match can be reviewed".into(),
            ));
        }
        if matches!(self.runtime.saved.pot.outcome, Some(Outcome::Winner(_))) {
            return Err(Error::Rejected(
                "Cannot replace an agreed winner settlement".into(),
            ));
        }
        self.runtime.retire_entries();
        self.runtime.saved.frozen = false;
        let result = self.command(Command::Action(13)).await;
        if result.is_err() {
            self.runtime.saved.frozen = true;
        }
        result
    }
}

#[cfg(test)]
#[path = "match_host_tests.rs"]
mod tests;
