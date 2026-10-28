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
    /// Never submitted; its funds are included in another payout instead.
    Superseded,
}

impl PayoutState {
    pub fn complete(self) -> bool {
        matches!(self, Self::Paid | Self::Superseded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    Damage,
    WinnerPot,
    Return,
    Settlement,
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
    #[serde(default)]
    pub unbatched: [u64; 2],
    #[serde(default)]
    pub batch_deadline_ms: [Option<u64>; 2],
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
            unbatched: [0; 2],
            batch_deadline_ms: [None; 2],
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
    /// Clock is injected; durable hosts use Unix milliseconds so deadlines survive restart.
    /// A later hit never extends the first hit's five-second deadline.
    pub fn credit_at(&mut self, totals: [u64; 2], now_ms: u64) -> Result<()> {
        if self.phase != Phase::Playing {
            return Err(Error::Rejected(
                "damage payouts require a playing match".into(),
            ));
        }
        self.flush_due(now_ms);
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
            if paid != 0 {
                let recipient = 1 - victim;
                self.unbatched[recipient] += paid;
                self.batch_deadline_ms[recipient].get_or_insert(now_ms.saturating_add(5_000));
            }
        }
        Ok(())
    }

    /// Freeze due batches. New damage will accumulate under a new immutable payout ID.
    pub fn flush_due(&mut self, now_ms: u64) {
        for player in 0..2 {
            if self.batch_deadline_ms[player].is_some_and(|deadline| now_ms >= deadline) {
                let sats = std::mem::take(&mut self.unbatched[player]);
                self.batch_deadline_ms[player] = None;
                self.payout(player, sats, Reason::Damage);
            }
        }
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
        let mut final_sats = std::mem::take(&mut self.unbatched);
        self.batch_deadline_ms = [None; 2];
        if let Some(player) = winner {
            final_sats[player] += remaining[0] + remaining[1];
        } else {
            for player in 0..2 {
                final_sats[player] += remaining[player];
            }
        }
        for (player, sats) in final_sats.into_iter().enumerate() {
            self.payout(player, sats, Reason::Settlement);
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
        let mut pending = self.unbatched;
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
        pot.flush_due(u64::MAX);
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
    fn five_second_window_groups_hits_without_extending_deadline() {
        let mut p = pot();
        fund(&mut p);
        p.credit_at([0, 200], 1_000).unwrap();
        p.credit_at([0, 500], 4_999).unwrap();
        assert_eq!(p.unbatched, [50, 0]);
        assert_eq!(p.batch_deadline_ms, [Some(6_000), None]);
        p.flush_due(5_999);
        assert!(p.payouts.is_empty());
        p.credit_at([0, 800], 6_000).unwrap();
        assert_eq!(p.payouts[0].sats, 50);
        assert_eq!(p.unbatched, [30, 0]);
        assert_eq!(p.batch_deadline_ms[0], Some(11_000));
        p.flush_due(11_000);
        assert_eq!(p.payouts[1].sats, 30);
        conserved(&p);
    }

    #[test]
    fn independent_recipient_windows_and_end_flush_without_minimum_amount() {
        let mut p = pot();
        fund(&mut p);
        p.credit_at([0, 10], 100).unwrap(); // P1 earns only one sat.
        p.credit_at([100, 10], 200).unwrap();
        assert_eq!(p.batch_deadline_ms, [Some(5_100), Some(5_200)]);
        p.finish(Some(0)).unwrap();
        // Winner receives unbatched damage + remainder in one settlement payment.
        assert_eq!(p.payouts.len(), 2);
        assert_eq!(p.payouts[0].sats, 990);
        assert_eq!(p.payouts[1].sats, 10);
        assert_eq!(p.batch_deadline_ms, [None; 2]);
        pay(&mut p);
        conserved(&p);
    }

    #[test]
    fn open_window_recovers_with_original_deadline_and_no_duplicate_batch() {
        let mut p = pot();
        fund(&mut p);
        p.credit_at([0, 800], 1_000).unwrap();
        let mut recovered: MatchPot =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        recovered.flush_due(5_999);
        assert!(recovered.payouts.is_empty());
        recovered.flush_due(6_000);
        recovered.flush_due(9_000);
        assert_eq!(recovered.payouts.len(), 1);
        assert_eq!(recovered.payouts[0].sats, 80);
        conserved(&recovered);
    }

    #[test]
    fn live_damage_then_winner_receives_all_leftovers() {
        let mut p = pot();
        fund(&mut p);
        p.credit_at([0, 800], 0).unwrap();
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
            p.credit_at(totals, 0).unwrap();
        }
        assert!(p.payouts.is_empty());
        assert_eq!(p.unbatched, [80, 0]);
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
        p.credit_at([0, 9000], 0).unwrap();
        pay(&mut p);
        p.credit_at([9000, 99999], 0).unwrap();
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
        p.credit_at([0, 5], 0).unwrap();
        assert!(p.payouts.is_empty());
        p.credit_at([0, 10], 0).unwrap();
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
        p.credit_at([0, 800], 0).unwrap();
        p.flush_due(5_000);
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
