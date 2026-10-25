//! Prepaid per-match funds. No reusable balances or budget reservations.
//! Durable hosts persist every transition before issuing an outgoing payment.
use serde::{Deserialize, Serialize};

use crate::{Error, Result, rules::MAX_BALANCE_SATS};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Funding,
    Ready,
    Playing,
    Settling,
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayoutState {
    Created,
    InFlight,
    Paid,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    Damage,
    WinnerPot,
    Return,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    pub id: String,
    pub player: usize,
    pub sats: u64,
    pub reason: Reason,
    pub state: PayoutState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Winner(usize),
    NoContest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchPot {
    pub id: String,
    pub entry_sats: u64,
    pub sats_per_percent: u64,
    pub phase: Phase,
    pub paid: [bool; 2],
    pub remaining: [u64; 2],
    pub earned: [u64; 2],
    /// Cumulative finalized damage received, in hundredths of a percent.
    pub damage_centi: [u64; 2],
    pub payouts: Vec<Payout>,
    pub outcome: Option<Outcome>,
}

impl MatchPot {
    pub fn new(id: String, entry_sats: u64, sats_per_percent: u64) -> Result<Self> {
        if id.is_empty()
            || entry_sats == 0
            || entry_sats > MAX_BALANCE_SATS / 2
            || sats_per_percent > MAX_BALANCE_SATS
        {
            return Err(Error::Rejected("invalid match payment settings".into()));
        }
        Ok(Self {
            id,
            entry_sats,
            sats_per_percent,
            phase: Phase::Funding,
            paid: [false; 2],
            remaining: [0; 2],
            earned: [0; 2],
            damage_centi: [0; 2],
            payouts: vec![],
            outcome: None,
        })
    }

    /// Called only after the wallet confirms the correct current-match invoice.
    pub fn confirm_entry(&mut self, player: usize, sats: u64) -> Result<()> {
        if player > 1 || sats != self.entry_sats {
            return Err(Error::Rejected("incorrect entry payment".into()));
        }
        if self.paid[player] {
            return Ok(());
        }
        if self.phase != Phase::Funding {
            return Err(Error::Rejected(
                "match is not awaiting entry payments".into(),
            ));
        }
        self.paid[player] = true;
        self.remaining[player] = sats;
        if self.paid == [true; 2] {
            self.phase = Phase::Ready;
        }
        Ok(())
    }

    pub fn start(&mut self) -> Result<()> {
        if self.phase != Phase::Ready {
            return Err(Error::Rejected(
                "both fresh entry invoices must be paid".into(),
            ));
        }
        self.phase = Phase::Playing;
        Ok(())
    }

    /// Cumulative counters make retries and missed UI polls safe. A stale total
    /// cannot undo or duplicate an already planned payout.
    pub fn credit(&mut self, totals: [u64; 2]) -> Result<()> {
        if self.phase != Phase::Playing {
            return Err(Error::Rejected(
                "damage payouts require a playing match".into(),
            ));
        }
        for (victim, &total) in totals.iter().enumerate() {
            if total <= self.damage_centi[victim] {
                continue;
            }
            let sats_for = |centi: u64| -> u64 {
                ((u128::from(centi) * u128::from(self.sats_per_percent)) / 100)
                    .min(u128::from(self.entry_sats)) as u64
            };
            let owed = sats_for(total) - sats_for(self.damage_centi[victim]);
            let paid = owed.min(self.remaining[victim]);
            self.damage_centi[victim] = total;
            self.remaining[victim] -= paid;
            self.payout(1 - victim, paid, Reason::Damage);
        }
        Ok(())
    }

    fn payout(&mut self, player: usize, sats: u64, reason: Reason) {
        if sats != 0 {
            self.payouts.push(Payout {
                id: format!("{}-{}", self.id, self.payouts.len()),
                player,
                sats,
                reason,
                state: PayoutState::Created,
            });
        }
    }

    /// None returns unspent contributions, keeping already-earned damage payouts.
    pub fn finish(&mut self, winner: Option<usize>) -> Result<()> {
        if winner.is_some_and(|player| player > 1) {
            return Err(Error::Rejected("invalid winner".into()));
        }
        let outcome = winner.map(Outcome::Winner).unwrap_or(Outcome::NoContest);
        if let Some(previous) = self.outcome {
            return if previous == outcome {
                Ok(())
            } else {
                Err(Error::Rejected("conflicting match result".into()))
            };
        }
        if self.phase != Phase::Playing
            && !(winner.is_none() && matches!(self.phase, Phase::Funding | Phase::Ready))
        {
            return Err(Error::Rejected("match cannot finish in this phase".into()));
        }
        self.outcome = Some(outcome);
        let remaining = std::mem::take(&mut self.remaining);
        if let Some(player) = winner {
            self.payout(player, remaining[0] + remaining[1], Reason::WinnerPot);
        } else {
            for (player, sats) in remaining.into_iter().enumerate() {
                self.payout(player, sats, Reason::Return);
            }
        }
        self.phase = Phase::Settling;
        self.update_phase();
        Ok(())
    }

    /// Persist this InFlight transition BEFORE calling the wallet. A recovered
    /// InFlight payment is looked up, never automatically sent a second time.
    pub fn begin_payout(&mut self, index: usize) -> Result<()> {
        let payout = self.payouts.get_mut(index).ok_or(Error::NotFound)?;
        if payout.state != PayoutState::Created {
            return Err(Error::Rejected("payout has already been submitted".into()));
        }
        payout.state = PayoutState::InFlight;
        Ok(())
    }

    pub fn confirm_payout(&mut self, index: usize) -> Result<()> {
        let payout = self.payouts.get_mut(index).ok_or(Error::NotFound)?;
        if payout.state == PayoutState::Paid {
            return Ok(());
        }
        if payout.state != PayoutState::InFlight {
            return Err(Error::Rejected("payout has not been submitted".into()));
        }
        payout.state = PayoutState::Paid;
        self.earned[payout.player] += payout.sats;
        self.update_phase();
        Ok(())
    }

    pub fn pending(&self) -> [u64; 2] {
        let mut pending = [0; 2];
        for payout in &self.payouts {
            if payout.state != PayoutState::Paid {
                pending[payout.player] += payout.sats;
            }
        }
        pending
    }

    fn update_phase(&mut self) {
        if self.phase == Phase::Settling
            && self.payouts.iter().all(|p| p.state == PayoutState::Paid)
        {
            self.phase = Phase::Settled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pot() -> MatchPot {
        MatchPot::new("game".into(), 500, 10).unwrap()
    }
    fn fund(pot: &mut MatchPot) {
        assert!(pot.start().is_err());
        pot.confirm_entry(0, 500).unwrap();
        assert!(pot.start().is_err());
        pot.confirm_entry(1, 500).unwrap();
        pot.start().unwrap();
    }
    fn pay(pot: &mut MatchPot) {
        for index in 0..pot.payouts.len() {
            if pot.payouts[index].state == PayoutState::Created {
                pot.begin_payout(index).unwrap();
                pot.confirm_payout(index).unwrap();
            }
        }
    }
    fn conserved(pot: &MatchPot) {
        assert_eq!(
            pot.remaining.iter().sum::<u64>()
                + pot.earned.iter().sum::<u64>()
                + pot.pending().iter().sum::<u64>(),
            pot.paid.iter().filter(|&&p| p).count() as u64 * pot.entry_sats
        );
    }

    #[test]
    fn live_damage_then_winner_receives_all_leftovers() {
        let mut p = pot();
        fund(&mut p);
        p.credit([0, 800]).unwrap();
        assert_eq!(p.remaining, [500, 420]);
        assert_eq!(p.pending(), [80, 0]);
        conserved(&p);
        pay(&mut p);
        assert_eq!(p.earned, [80, 0]); // Confirmed while the game is still playing.
        assert_eq!(p.phase, Phase::Playing);
        p.finish(Some(1)).unwrap();
        assert_eq!(p.phase, Phase::Settling);
        pay(&mut p);
        assert_eq!(p.earned, [80, 920]);
        assert_eq!(p.phase, Phase::Settled);
        conserved(&p);
        let next = MatchPot::new("next".into(), 500, 10).unwrap();
        assert_eq!(next.paid, [false; 2]); // Winnings never fund another match.
    }

    #[test]
    fn duplicate_counters_results_and_receipts_never_pay_twice() {
        let mut p = pot();
        fund(&mut p);
        for totals in [[0, 800], [0, 800], [0, 700]] {
            p.credit(totals).unwrap();
        }
        assert_eq!(p.payouts.len(), 1);
        pay(&mut p);
        p.confirm_payout(0).unwrap();
        assert_eq!(p.earned, [80, 0]);
        p.finish(Some(0)).unwrap();
        p.finish(Some(0)).unwrap();
        assert!(p.finish(Some(1)).is_err());
        pay(&mut p);
        assert_eq!(p.earned, [1000, 0]);
        conserved(&p);
    }

    #[test]
    fn funds_are_capped_and_winnings_cannot_be_spent_again() {
        let mut p = pot();
        fund(&mut p);
        p.credit([0, 9000]).unwrap();
        pay(&mut p);
        p.credit([9000, 99999]).unwrap();
        pay(&mut p);
        assert_eq!(p.remaining, [0, 0]);
        assert_eq!(p.earned, [500, 500]);
        p.finish(Some(0)).unwrap();
        assert_eq!(p.earned, [500, 500]);
        conserved(&p);
    }

    #[test]
    fn tiny_hits_accumulate_and_no_contest_returns_unspent_funds() {
        let mut p = pot();
        fund(&mut p);
        p.credit([0, 5]).unwrap();
        assert!(p.payouts.is_empty());
        p.credit([0, 10]).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [1, 0]);
        p.finish(None).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [501, 499]);
        let mut recovered: MatchPot =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(recovered.outcome, Some(Outcome::NoContest));
        recovered.finish(None).unwrap();
        assert!(recovered.finish(Some(0)).is_err());
        conserved(&p);
    }

    #[test]
    fn durable_inflight_payments_cannot_be_submitted_again() {
        let mut p = pot();
        fund(&mut p);
        p.credit([0, 800]).unwrap();
        p.begin_payout(0).unwrap();
        let mut recovered: MatchPot =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert!(recovered.begin_payout(0).is_err());
        assert_eq!(recovered.pending(), [80, 0]);
        recovered.confirm_payout(0).unwrap();
        recovered.confirm_payout(0).unwrap();
        assert_eq!(recovered.earned, [80, 0]);
        conserved(&recovered);
    }
}
