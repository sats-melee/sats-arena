//! Four-slot match funds with sequenced attacker/victim credits.
//!
//! Hosts persist each transition before submitting a payout. Credits must be in
//! finalized game order: that order determines who can steal the last sats from
//! an exhausted victim. No balance can be spent beyond its original entry.
use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    pot::{Outcome, Payout, PayoutState, Phase, Reason},
    rules::MAX_BALANCE_SATS,
};

pub const PLAYERS: usize = 4;
pub const HUMAN: u8 = 0;
pub const CPU: u8 = 1;
pub const CLOSED: u8 = 3;
pub const NO_TEAM: u8 = 255;

/// A finalized nominal-damage credit, ordered across the entire match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    pub sequence: u64,
    pub attacker: usize,
    pub victim: usize,
    pub damage_centi: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchPot {
    pub id: String,
    pub entry_sats: u64,
    pub sats_per_percent: u64,
    #[serde(default = "default_batch_window_ms")]
    pub batch_window_ms: u64,
    /// Extra invoice contribution retained by the host wallet for routing fees.
    #[serde(default)]
    pub surcharge_bps: u32,
    pub phase: Phase,
    pub paid: [bool; PLAYERS],
    pub remaining: [u64; PLAYERS],
    pub earned: [u64; PLAYERS],
    /// Cumulative received nominal damage; retained for diagnostic/legacy hosts.
    #[serde(default)]
    pub damage_centi: [u64; PLAYERS],
    pub payouts: Vec<Payout>,
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub unbatched: [u64; PLAYERS],
    #[serde(default)]
    pub batch_deadline_ms: [Option<u64>; PLAYERS],
    #[serde(default = "default_kinds")]
    pub kinds: [u8; PLAYERS],
    #[serde(default = "default_teams")]
    pub team_ids: [u8; PLAYERS],
    #[serde(default)]
    pub teams: bool,
    #[serde(default)]
    pub last_credit_sequence: u64,
    /// Fractional sats are carried independently for each attacker/victim pair.
    #[serde(default)]
    pub fractional_sats: [[u8; PLAYERS]; PLAYERS],
}

fn default_kinds() -> [u8; PLAYERS] {
    [HUMAN, HUMAN, CLOSED, CLOSED]
}
fn default_teams() -> [u8; PLAYERS] {
    [NO_TEAM; PLAYERS]
}

fn default_batch_window_ms() -> u64 {
    5_000
}

impl MatchPot {
    pub fn new(id: String, entry_sats: u64, sats_per_percent: u64) -> Result<Self> {
        if id.is_empty()
            || entry_sats == 0
            || entry_sats > MAX_BALANCE_SATS / PLAYERS as u64
            || sats_per_percent > MAX_BALANCE_SATS
        {
            return Err(Error::Rejected("invalid match payment settings".into()));
        }
        Ok(Self {
            id,
            entry_sats,
            sats_per_percent,
            batch_window_ms: default_batch_window_ms(),
            surcharge_bps: 0,
            phase: Phase::Funding,
            paid: [false; PLAYERS],
            remaining: [0; PLAYERS],
            earned: [0; PLAYERS],
            damage_centi: [0; PLAYERS],
            payouts: vec![],
            outcome: None,
            unbatched: [0; PLAYERS],
            batch_deadline_ms: [None; PLAYERS],
            kinds: default_kinds(),
            team_ids: default_teams(),
            teams: false,
            last_credit_sequence: 0,
            fractional_sats: [[0; PLAYERS]; PLAYERS],
        })
    }

    pub fn surcharge_sats(&self) -> u64 {
        (u128::from(self.entry_sats) * u128::from(self.surcharge_bps)).div_ceil(10_000) as u64
    }
    pub fn invoice_sats(&self) -> u64 {
        self.entry_sats + self.surcharge_sats()
    }
    pub fn collected_fee_sats(&self) -> u64 {
        self.paid.iter().filter(|&&paid| paid).count() as u64 * self.surcharge_sats()
    }
    pub fn set_surcharge_bps(&mut self, basis_points: u32) -> Result<()> {
        if self.phase != Phase::Funding || self.paid.iter().any(|&paid| paid) {
            return Err(Error::Rejected(
                "funded match payment settings are locked".into(),
            ));
        }
        let fee = (u128::from(self.entry_sats) * u128::from(basis_points)).div_ceil(10_000);
        if basis_points > 10_000
            || u128::from(self.entry_sats) + fee > u128::from(MAX_BALANCE_SATS / PLAYERS as u64)
        {
            return Err(Error::Rejected(
                "routing surcharge or total invoice is outside supported limits".into(),
            ));
        }
        self.surcharge_bps = basis_points;
        Ok(())
    }

    pub fn set_batch_window_ms(&mut self, milliseconds: u64) -> Result<()> {
        if !(1_000..=120_000).contains(&milliseconds) || milliseconds % 1000 != 0 {
            return Err(Error::Rejected(
                "damage payout delay must be 1–120 whole seconds".into(),
            ));
        }
        if self.phase != Phase::Funding || self.paid.iter().any(|&paid| paid) {
            return Err(Error::Rejected(
                "funded match payment settings are locked".into(),
            ));
        }
        self.batch_window_ms = milliseconds;
        Ok(())
    }

    /// Only an unfunded lobby may change roster. Funded changes must first take
    /// the no-contest return path, then use a new match and fresh entry invoices.
    pub fn configure(
        &mut self,
        teams: bool,
        kinds: [u8; PLAYERS],
        team_ids: [u8; PLAYERS],
    ) -> Result<()> {
        if self.phase != Phase::Funding || self.paid.iter().any(|&paid| paid) {
            return Err(Error::Rejected("funded match roster is locked".into()));
        }
        let mut participants = 0;
        let mut sides = 0u8;
        for (&kind, &team) in kinds.iter().zip(&team_ids) {
            if !matches!(kind, HUMAN | CPU | CLOSED) {
                return Err(Error::Rejected("invalid player kind".into()));
            }
            if kind != CLOSED {
                participants += 1;
                if teams {
                    if team > 2 {
                        return Err(Error::Rejected("invalid team ID".into()));
                    }
                    sides |= 1 << team;
                } else if team != NO_TEAM {
                    return Err(Error::Rejected("FFA slots must have no team".into()));
                }
            } else if team != NO_TEAM {
                return Err(Error::Rejected("closed slots must have no team".into()));
            }
        }
        if !(2..=PLAYERS).contains(&participants) || (teams && sides.count_ones() < 2) {
            return Err(Error::Rejected(
                "select at least two opposing players".into(),
            ));
        }
        self.teams = teams;
        self.kinds = kinds;
        self.team_ids = team_ids;
        self.update_funding_phase();
        Ok(())
    }

    /// The wallet has confirmed this current-match invoice's exact entry amount.
    pub fn confirm_entry(&mut self, player: usize, sats: u64) -> Result<()> {
        if player >= PLAYERS || self.kinds[player] != HUMAN || sats != self.invoice_sats() {
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
        self.remaining[player] = self.entry_sats;
        self.update_funding_phase();
        Ok(())
    }

    fn update_funding_phase(&mut self) {
        if self
            .kinds
            .iter()
            .enumerate()
            .all(|(i, &kind)| kind != HUMAN || self.paid[i])
        {
            self.phase = Phase::Ready;
        }
    }

    pub fn start(&mut self) -> Result<()> {
        if self.phase != Phase::Ready
            || self
                .kinds
                .iter()
                .enumerate()
                .any(|(i, &kind)| kind == HUMAN && !self.paid[i])
        {
            return Err(Error::Rejected(
                "all human entry invoices must be paid".into(),
            ));
        }
        self.phase = Phase::Playing;
        Ok(())
    }

    /// Apply a complete ordered batch atomically. Replayed IDs are harmless;
    /// any missing ID fails before deadlines, pots or counters are changed.
    pub fn credit_batch_at(&mut self, credits: &[Credit], now_ms: u64) -> Result<()> {
        if self.phase != Phase::Playing {
            return Err(Error::Rejected(
                "damage payouts require a playing match".into(),
            ));
        }
        let mut last = self.last_credit_sequence;
        for credit in credits {
            if credit.sequence == 0
                || credit.attacker >= PLAYERS
                || credit.victim >= PLAYERS
                || self.kinds[credit.attacker] == CLOSED
                || self.kinds[credit.victim] == CLOSED
            {
                return Err(Error::Rejected("invalid finalized damage credit".into()));
            }
            if credit.sequence <= last {
                continue;
            }
            if last.checked_add(1) != Some(credit.sequence) {
                return Err(Error::Rejected(
                    "finalized damage credit sequence has a gap".into(),
                ));
            }
            last = credit.sequence;
        }
        self.flush_due(now_ms);
        for credit in credits {
            if credit.sequence <= self.last_credit_sequence {
                continue;
            }
            self.last_credit_sequence = credit.sequence;
            let attacker = credit.attacker;
            let victim = credit.victim;
            self.damage_centi[victim] =
                self.damage_centi[victim].saturating_add(credit.damage_centi);
            if attacker == victim
                || self.kinds[attacker] != HUMAN
                || self.kinds[victim] != HUMAN
                || (self.teams && self.team_ids[attacker] == self.team_ids[victim])
            {
                continue;
            }
            let units = u128::from(credit.damage_centi) * u128::from(self.sats_per_percent)
                + u128::from(self.fractional_sats[attacker][victim]);
            self.fractional_sats[attacker][victim] = (units % 100) as u8;
            let sats = (units / 100).min(u128::from(self.remaining[victim])) as u64;
            self.remaining[victim] -= sats;
            if sats > 0 {
                self.unbatched[attacker] += sats;
                self.batch_deadline_ms[attacker]
                    .get_or_insert(now_ms.saturating_add(self.batch_window_ms));
            }
        }
        Ok(())
    }

    /// Compatibility only for a two-human P1/P2 FFA host with cumulative victim
    /// counters. Expanded matches must supply explicit sequenced attribution.
    pub fn credit_at(&mut self, totals: [u64; 2], now_ms: u64) -> Result<()> {
        if self.teams || self.kinds != default_kinds() {
            return Err(Error::Rejected(
                "expanded matches require attributed credits".into(),
            ));
        }
        let mut credits = Vec::new();
        for (victim, total) in totals.into_iter().enumerate() {
            if total > self.damage_centi[victim] {
                let sequence = self
                    .last_credit_sequence
                    .checked_add(credits.len() as u64 + 1)
                    .ok_or_else(|| Error::Rejected("damage sequence exhausted".into()))?;
                credits.push(Credit {
                    sequence,
                    attacker: 1 - victim,
                    victim,
                    damage_centi: total - self.damage_centi[victim],
                });
            }
        }
        self.credit_batch_at(&credits, now_ms)
    }

    pub fn flush_due(&mut self, now_ms: u64) {
        for player in 0..PLAYERS {
            if self.batch_deadline_ms[player].is_some_and(|deadline| now_ms >= deadline) {
                let sats = std::mem::take(&mut self.unbatched[player]);
                self.batch_deadline_ms[player] = None;
                self.payout(player, sats, Reason::Damage);
            }
        }
    }

    fn payout(&mut self, player: usize, sats: u64, reason: Reason) {
        if sats > 0 {
            self.payouts.push(Payout {
                id: format!("{}-{}", self.id, self.payouts.len()),
                player,
                sats,
                reason,
                state: PayoutState::Created,
            });
        }
    }

    fn same_side(&self, a: usize, b: usize) -> bool {
        if self.teams {
            self.team_ids[a] == self.team_ids[b]
        } else {
            a == b
        }
    }

    fn canonical_winner(&self, winner: usize) -> Result<usize> {
        if winner >= PLAYERS || self.kinds[winner] == CLOSED {
            return Err(Error::Rejected("invalid winner".into()));
        }
        Ok((0..PLAYERS)
            .find(|&i| self.kinds[i] != CLOSED && self.same_side(i, winner))
            .unwrap())
    }

    /// A winner identifies a side, not just the surviving player. Winning humans
    /// recover their own unspent funds, and split all losing-side funds equally.
    /// CPU-only victories and no-contests return each owner's unspent entry.
    pub fn finish(&mut self, winner: Option<usize>) -> Result<()> {
        let winner = winner.map(|w| self.canonical_winner(w)).transpose()?;
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
        self.batch_deadline_ms = [None; PLAYERS];
        let winners: Vec<usize> = (0..PLAYERS)
            .filter(|&i| self.kinds[i] == HUMAN && winner.is_some_and(|w| self.same_side(i, w)))
            .collect();
        if winners.is_empty() {
            for player in 0..PLAYERS {
                final_sats[player] += remaining[player];
            }
        } else {
            let mut opponents = 0;
            for player in 0..PLAYERS {
                if winners.contains(&player) {
                    final_sats[player] += remaining[player];
                } else {
                    opponents += remaining[player];
                }
            }
            let count = winners.len() as u64;
            for (index, player) in winners.into_iter().enumerate() {
                // Ascending controller port receives any indivisible remainder.
                final_sats[player] +=
                    opponents / count + u64::from((index as u64) < opponents % count);
            }
        }
        for (player, sats) in final_sats.into_iter().enumerate() {
            self.payout(player, sats, Reason::Settlement);
        }
        self.phase = Phase::Settling;
        self.update_phase();
        Ok(())
    }

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

    pub fn pending(&self) -> [u64; PLAYERS] {
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

    fn pot(teams: bool, kinds: [u8; 4], team_ids: [u8; 4]) -> MatchPot {
        let mut pot = MatchPot::new("arena".into(), 500, 10).unwrap();
        pot.configure(teams, kinds, team_ids).unwrap();
        pot
    }
    fn ffa() -> MatchPot {
        pot(false, [HUMAN; 4], [NO_TEAM; 4])
    }
    fn teams() -> MatchPot {
        pot(true, [HUMAN; 4], [0, 1, 0, 1])
    }
    fn fund(pot: &mut MatchPot) {
        for player in 0..PLAYERS {
            if pot.kinds[player] == HUMAN {
                assert!(pot.start().is_err());
                pot.confirm_entry(player, pot.entry_sats).unwrap();
                conserved(pot);
            }
        }
        assert_eq!(pot.phase, Phase::Ready);
        pot.start().unwrap();
    }
    fn conserved(pot: &MatchPot) {
        assert_eq!(
            pot.remaining.iter().sum::<u64>()
                + pot.earned.iter().sum::<u64>()
                + pot.pending().iter().sum::<u64>(),
            pot.paid.iter().filter(|&&paid| paid).count() as u64 * pot.entry_sats
        );
        assert!(
            pot.remaining
                .iter()
                .all(|&remaining| remaining <= pot.entry_sats)
        );
        assert!(pot.payouts.iter().all(|p| pot.kinds[p.player] == HUMAN));
        for player in 0..PLAYERS {
            if pot.kinds[player] != HUMAN {
                assert_eq!(
                    (
                        pot.paid[player],
                        pot.remaining[player],
                        pot.pending()[player],
                        pot.earned[player]
                    ),
                    (false, 0, 0, 0)
                );
            }
        }
    }
    fn credit(pot: &mut MatchPot, attacker: usize, victim: usize, damage_centi: u64, now: u64) {
        let sequence = pot.last_credit_sequence + 1;
        pot.credit_batch_at(
            &[Credit {
                sequence,
                attacker,
                victim,
                damage_centi,
            }],
            now,
        )
        .unwrap();
        conserved(pot);
    }
    fn pay(pot: &mut MatchPot) {
        pot.flush_due(u64::MAX);
        for index in 0..pot.payouts.len() {
            if pot.payouts[index].state == PayoutState::Created {
                pot.begin_payout(index).unwrap();
                pot.confirm_payout(index).unwrap();
                pot.confirm_payout(index).unwrap();
                conserved(pot);
            }
        }
    }

    #[test]
    fn four_way_ffa_steals_from_exact_victim_and_settles_all_losing_pots() {
        let mut p = ffa();
        fund(&mut p);
        credit(&mut p, 0, 2, 800, 1_000);
        credit(&mut p, 3, 1, 1200, 2_000);
        assert_eq!(p.remaining, [500, 380, 420, 500]);
        assert_eq!(p.pending(), [80, 0, 0, 120]);
        p.finish(Some(2)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [80, 0, 1800, 120]);
        assert_eq!(p.phase, Phase::Settled);
        conserved(&p);
    }

    #[test]
    fn team_winners_recover_own_pots_and_share_only_opponents_remaining_funds() {
        let mut p = teams();
        fund(&mut p);
        credit(&mut p, 0, 1, 800, 0);
        credit(&mut p, 3, 2, 1200, 0);
        assert_eq!(p.remaining, [500, 420, 380, 500]);
        p.finish(Some(2)).unwrap();
        p.finish(Some(0)).unwrap(); // Eliminated teammates are still on the winning side.
        assert!(p.finish(Some(1)).is_err());
        pay(&mut p);
        assert_eq!(p.earned, [1040, 0, 840, 120]);
        assert_eq!(p.outcome, Some(Outcome::Winner(0)));
        conserved(&p);
    }

    #[test]
    fn uneven_teams_split_indivisible_sats_in_controller_order() {
        let mut p = pot(true, [HUMAN; 4], [0, 0, 0, 1]);
        fund(&mut p);
        p.finish(Some(1)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [667, 667, 666, 0]);
        conserved(&p);
    }

    #[test]
    fn three_sides_and_non_contiguous_ports_are_supported() {
        let mut p = pot(true, [HUMAN; 4], [2, 0, 1, 2]);
        fund(&mut p);
        p.finish(Some(3)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [1000, 0, 0, 1000]);
        conserved(&p);
        let mut p = pot(false, [CLOSED, HUMAN, CLOSED, HUMAN], [NO_TEAM; 4]);
        fund(&mut p);
        credit(&mut p, 3, 1, 100, 0);
        p.finish(Some(1)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [0, 990, 0, 10]);
    }

    #[test]
    fn cpu_friendly_fire_self_damage_never_transfer_money() {
        let mut p = pot(true, [HUMAN, HUMAN, CPU, HUMAN], [0, 0, 1, 1]);
        fund(&mut p);
        assert!(p.confirm_entry(2, 500).is_err());
        for (a, v) in [(0, 1), (0, 0), (2, 0), (0, 2), (3, 2)] {
            credit(&mut p, a, v, 1000, 0);
        }
        assert_eq!(p.remaining, [500, 500, 0, 500]);
        assert_eq!(p.pending(), [0; 4]);
        // The CPU may represent its winning side; only its human teammate is paid.
        p.finish(Some(2)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [0, 0, 0, 1500]);
        conserved(&p);
    }

    #[test]
    fn cpu_only_win_returns_unspent_contributions_and_keeps_damage_earnings() {
        let mut p = pot(false, [HUMAN, CPU, HUMAN, CLOSED], [NO_TEAM; 4]);
        fund(&mut p);
        credit(&mut p, 0, 2, 100, 0);
        p.flush_due(5_000);
        pay(&mut p);
        assert_eq!(p.earned, [10, 0, 0, 0]);
        p.finish(Some(1)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [510, 0, 490, 0]);
        conserved(&p);
        let mut all_cpus = pot(true, [CPU; 4], [0, 1, 0, 1]);
        assert_eq!(all_cpus.phase, Phase::Ready);
        all_cpus.start().unwrap();
        all_cpus.finish(Some(2)).unwrap();
        assert_eq!(all_cpus.phase, Phase::Settled);
        conserved(&all_cpus);
    }

    #[test]
    fn no_contest_also_returns_a_partly_funded_lobby_exactly_once() {
        let mut p = ffa();
        p.confirm_entry(0, 500).unwrap();
        p.confirm_entry(2, 500).unwrap();
        assert!(p.configure(true, [HUMAN; 4], [0, 1, 0, 1]).is_err());
        assert!(p.start().is_err());
        p.finish(None).unwrap();
        p.finish(None).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [500, 0, 500, 0]);
        assert_eq!(p.payouts.len(), 2);
        conserved(&p);
    }

    #[test]
    fn sequence_gaps_and_invalid_records_leave_entire_state_unchanged() {
        let mut p = ffa();
        fund(&mut p);
        let initial = serde_json::to_value(&p).unwrap();
        let good = Credit {
            sequence: 1,
            attacker: 0,
            victim: 1,
            damage_centi: 500,
        };
        let gap = Credit {
            sequence: 3,
            ..good
        };
        assert!(p.credit_batch_at(&[good, gap], 99_000).is_err());
        assert_eq!(serde_json::to_value(&p).unwrap(), initial);
        let invalid = Credit {
            sequence: 2,
            attacker: 4,
            ..good
        };
        assert!(p.credit_batch_at(&[good, invalid], 99_000).is_err());
        assert_eq!(serde_json::to_value(&p).unwrap(), initial);
        p.credit_batch_at(&[good, good], 0).unwrap();
        let credited = serde_json::to_value(&p).unwrap();
        p.credit_batch_at(&[good], 1).unwrap();
        assert_eq!(serde_json::to_value(&p).unwrap(), credited);
        assert_eq!(p.pending(), [50, 0, 0, 0]);
    }

    #[test]
    fn first_hit_deadlines_are_independent_and_survive_restart() {
        let mut p = ffa();
        fund(&mut p);
        credit(&mut p, 0, 1, 200, 1_000);
        credit(&mut p, 2, 3, 100, 2_000);
        credit(&mut p, 0, 3, 300, 4_000);
        assert_eq!(p.batch_deadline_ms, [Some(6_000), None, Some(7_000), None]);
        let mut p: MatchPot = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        p.flush_due(5_999);
        assert!(p.payouts.is_empty());
        p.flush_due(6_000);
        assert_eq!(p.payouts.len(), 1);
        assert_eq!((p.payouts[0].player, p.payouts[0].sats), (0, 50));
        p.begin_payout(0).unwrap();
        let mut p: MatchPot = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert!(p.begin_payout(0).is_err());
        p.confirm_payout(0).unwrap();
        p.flush_due(7_000);
        assert_eq!(p.payouts.len(), 2);
        assert_ne!(p.payouts[0].id, p.payouts[1].id);
        conserved(&p);
    }

    #[test]
    fn tiny_hits_carry_fraction_per_pair_and_original_order_caps_victim_funds() {
        let mut p = ffa();
        fund(&mut p);
        credit(&mut p, 0, 3, 5, 0); // half sat
        credit(&mut p, 1, 3, 5, 0); // independent half sat
        assert_eq!(p.pending(), [0; 4]);
        credit(&mut p, 0, 3, 5, 0);
        assert_eq!(p.pending(), [1, 0, 0, 0]);
        credit(&mut p, 2, 3, 4990, 0); // drains remaining 499 before P2's next hit
        credit(&mut p, 1, 3, 100_000, 0);
        assert_eq!(p.pending(), [1, 0, 499, 0]);
        assert_eq!(p.remaining[3], 0);
        p.finish(None).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [501, 500, 999, 0]);
        conserved(&p);
    }

    #[test]
    fn invalid_lobbies_never_replace_an_existing_roster() {
        let mut p = ffa();
        let initial = serde_json::to_value(&p).unwrap();
        for (mode, kinds, ids) in [
            (true, [HUMAN; 4], [0; 4]),
            (true, [HUMAN; 4], [0, 1, 2, 3]),
            (false, [HUMAN; 4], [0; 4]),
            (false, [HUMAN, CLOSED, CLOSED, CLOSED], [NO_TEAM; 4]),
            (false, [HUMAN, HUMAN, 2, CLOSED], [NO_TEAM; 4]),
        ] {
            assert!(p.configure(mode, kinds, ids).is_err());
            assert_eq!(serde_json::to_value(&p).unwrap(), initial);
        }
    }

    #[test]
    fn legacy_two_player_counters_remain_idempotent() {
        let mut p = MatchPot::new("legacy".into(), 500, 10).unwrap();
        fund(&mut p);
        p.credit_at([0, 800], 0).unwrap();
        p.credit_at([0, 800], 0).unwrap();
        p.credit_at([0, 700], 0).unwrap();
        assert_eq!(p.pending(), [80, 0, 0, 0]);
        p.finish(Some(1)).unwrap();
        pay(&mut p);
        assert_eq!(p.earned, [80, 920, 0, 0]);
        conserved(&p);
    }

    #[test]
    fn conservation_holds_through_many_multi_player_hits_and_replayed_batches() {
        for team_ids in [[0, 1, 0, 1], [0, 1, 2, 0], [0, 0, 0, 1]] {
            let mut p = pot(true, [HUMAN; 4], team_ids);
            fund(&mut p);
            for n in 1..=300 {
                let c = Credit {
                    sequence: n,
                    attacker: (n as usize * 7) % 4,
                    victim: ((n as usize * 11) / 3) % 4,
                    damage_centi: n * 7 % 173,
                };
                p.credit_batch_at(&[c], n * 100).unwrap();
                p.credit_batch_at(&[c], n * 100).unwrap();
                if n % 37 == 0 {
                    pay(&mut p);
                }
                conserved(&p);
            }
            p.finish(Some(2)).unwrap();
            pay(&mut p);
            assert_eq!(p.phase, Phase::Settled);
            conserved(&p);
        }
    }
    #[test]
    fn surcharge_is_added_to_invoice_and_never_stolen_or_settled() {
        for teams_mode in [false, true] {
            let mut p = MatchPot::new("fees".into(), 401, 1).unwrap();
            p.set_surcharge_bps(250).unwrap(); // fractional fee rounds up once per invoice
            assert_eq!(p.surcharge_sats(), 11);
            assert_eq!(p.invoice_sats(), 412);
            p.configure(
                teams_mode,
                [HUMAN; 4],
                if teams_mode {
                    [0, 1, 0, 1]
                } else {
                    [NO_TEAM; 4]
                },
            )
            .unwrap();
            assert!(p.confirm_entry(0, 401).is_err());
            for player in 0..4 {
                p.confirm_entry(player, 412).unwrap();
            }
            assert_eq!(p.remaining, [401; 4]);
            assert_eq!(p.collected_fee_sats(), 44);
            assert!(p.set_surcharge_bps(0).is_err());
            assert!(p.set_batch_window_ms(2_000).is_err());
            p.start().unwrap();
            p.finish(Some(0)).unwrap();
            assert_eq!(p.payouts.iter().map(|p| p.sats).sum::<u64>(), 1604);
        }
    }
    #[test]
    fn configurable_window_is_fixed_from_first_hit_and_persisted() {
        let mut p = MatchPot::new("delay".into(), 400, 1).unwrap();
        assert!(p.set_batch_window_ms(0).is_err());
        assert!(p.set_batch_window_ms(1500).is_err());
        assert!(p.set_batch_window_ms(121_000).is_err());
        p.set_batch_window_ms(2_000).unwrap();
        for i in 0..2 {
            p.confirm_entry(i, 400).unwrap();
        }
        p.start().unwrap();
        p.credit_batch_at(
            &[Credit {
                sequence: 1,
                attacker: 0,
                victim: 1,
                damage_centi: 1000,
            }],
            1_000,
        )
        .unwrap();
        p.credit_batch_at(
            &[Credit {
                sequence: 2,
                attacker: 0,
                victim: 1,
                damage_centi: 500,
            }],
            2_500,
        )
        .unwrap();
        assert_eq!(p.batch_deadline_ms[0], Some(3_000));
        let mut recovered: MatchPot =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        recovered.flush_due(2_999);
        assert!(recovered.payouts.is_empty());
        recovered.flush_due(3_000);
        assert_eq!(recovered.payouts[0].sats, 15);
        let mut old = serde_json::to_value(&p).unwrap();
        old.as_object_mut().unwrap().remove("batch_window_ms");
        old.as_object_mut().unwrap().remove("surcharge_bps");
        let old: MatchPot = serde_json::from_value(old).unwrap();
        assert_eq!(old.batch_window_ms, 5_000);
        assert_eq!(old.surcharge_bps, 0);
    }
    #[test]
    fn cancelled_match_returns_playable_pot_and_retains_disclosed_contribution() {
        let mut p = MatchPot::new("cancel".into(), 400, 1).unwrap();
        p.set_surcharge_bps(200).unwrap();
        p.confirm_entry(0, 408).unwrap();
        p.finish(None).unwrap();
        assert_eq!(p.payouts.iter().map(|p| p.sats).sum::<u64>(), 400);
        assert_eq!(p.collected_fee_sats(), 8);
        let mut huge = MatchPot::new("limit".into(), MAX_BALANCE_SATS / 4, 1).unwrap();
        assert!(huge.set_surcharge_bps(1).is_err());
        assert!(p.set_surcharge_bps(10_001).is_err());
    }
}
