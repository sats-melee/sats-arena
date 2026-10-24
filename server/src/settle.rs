//! Turns an agreed match report into balance transfers. Pure; no I/O.

use std::collections::{BTreeMap, HashMap};

use sats_arena_protocol::{MatchReport, PROTOCOL_VERSION, Transfer, TransferReason};

use crate::config::Rules;

pub const MAX_HITS: usize = 20_000;
const MAX_ID_LEN: usize = 128;

/// Checks a report is internally consistent before anything is stored.
pub fn validate(report: &MatchReport) -> Result<(), String> {
    if report.protocol_version != PROTOCOL_VERSION {
        return Err(format!("unsupported protocol version {}", report.protocol_version));
    }
    if report.match_id.is_empty() || report.match_id.len() > MAX_ID_LEN {
        return Err("invalid match_id".into());
    }
    if !(2..=4).contains(&report.players.len()) {
        return Err("a match needs 2 to 4 players".into());
    }
    let mut ports = [false; 4];
    for (i, slot) in report.players.iter().enumerate() {
        if slot.port > 3 || std::mem::replace(&mut ports[slot.port as usize], true) {
            return Err(format!("invalid or repeated port {}", slot.port));
        }
        if report.players[..i].iter().any(|p| p.player_id == slot.player_id) {
            return Err("the same player can't be on two ports".into());
        }
    }
    let known = |port: u8| port < 4 && ports[port as usize];
    if !known(report.reporter_port) {
        return Err("reporter_port isn't in the match".into());
    }
    if report.winner_port.is_some_and(|w| !known(w)) {
        return Err("winner_port isn't in the match".into());
    }
    if report.hits.len() > MAX_HITS {
        return Err("too many hits".into());
    }
    for hit in &report.hits {
        if !known(hit.victim_port) || hit.attacker_port.is_some_and(|a| !known(a) || a == hit.victim_port) {
            return Err(format!("hit on frame {} has invalid ports", hit.frame));
        }
    }
    Ok(())
}

/// Computes transfers for a validated report. Nobody can pay more than they
/// have; transfers are capped at the payer's balance, in a fixed order.
pub fn compute(report: &MatchReport, rules: &Rules, balances: &HashMap<String, u64>) -> Vec<Transfer> {
    let id_of = |port: u8| -> &str {
        &report.players.iter().find(|p| p.port == port).expect("validated port").player_id
    };
    let mut remaining = balances.clone();
    let mut transfers = Vec::new();
    let mut pay = |from: &str, to: &str, owed: u64, reason| {
        let balance = remaining.entry(from.to_string()).or_default();
        let sats = owed.min(*balance);
        if sats > 0 {
            *balance -= sats;
            *remaining.entry(to.to_string()).or_default() += sats;
            transfers.push(Transfer { from_player_id: from.into(), to_player_id: to.into(), sats, reason });
        }
    };

    // Sum damage per (victim, attacker) before rounding so small hits still count.
    let mut damage: BTreeMap<(u8, u8), u64> = BTreeMap::new();
    for hit in &report.hits {
        if let Some(attacker) = hit.attacker_port {
            *damage.entry((hit.victim_port, attacker)).or_default() += u64::from(hit.damage_centi);
        }
    }
    for ((victim, attacker), centi) in damage {
        let owed = centi.saturating_mul(rules.sats_per_percent) / 100;
        pay(id_of(victim), id_of(attacker), owed, TransferReason::Damage);
    }

    if let Some(winner) = report.winner_port {
        let mut losers: Vec<_> = report.players.iter().filter(|p| p.port != winner).collect();
        losers.sort_by_key(|p| p.port);
        for loser in losers {
            pay(&loser.player_id, id_of(winner), rules.win_sats, TransferReason::Win);
        }
    }
    transfers
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_arena_protocol::{Hit, MatchMode, PlayerSlot};

    fn rules() -> Rules {
        Rules { sats_per_percent: 2, win_sats: 100, max_deposit_sats: 1_000_000, invoice_expiry_secs: 600 }
    }

    fn report(hits: Vec<Hit>, winner_port: Option<u8>) -> MatchReport {
        MatchReport {
            protocol_version: PROTOCOL_VERSION,
            match_id: "m".into(),
            mode: MatchMode::Local,
            reporter_port: 0,
            players: vec![
                PlayerSlot { port: 0, player_id: "a".into() },
                PlayerSlot { port: 1, player_id: "b".into() },
            ],
            hits,
            winner_port,
        }
    }

    fn hit(victim_port: u8, attacker_port: Option<u8>, damage_centi: u32) -> Hit {
        Hit { frame: 0, victim_port, attacker_port, damage_centi }
    }

    fn balances(a: u64, b: u64) -> HashMap<String, u64> {
        HashMap::from([("a".into(), a), ("b".into(), b)])
    }

    #[test]
    fn damage_is_summed_before_rounding() {
        // Ten 0.5% hits = 5% = 10 sats, even though each hit alone rounds to 1.
        let hits = (0..10).map(|_| hit(1, Some(0), 50)).collect();
        let t = compute(&report(hits, None), &rules(), &balances(0, 1000));
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].from_player_id.as_str(), t[0].sats), ("b", 10));
    }

    #[test]
    fn unattributed_damage_is_free() {
        let t = compute(&report(vec![hit(1, None, 5000)], None), &rules(), &balances(1000, 1000));
        assert!(t.is_empty());
    }

    #[test]
    fn payments_are_capped_at_balance() {
        let t = compute(&report(vec![hit(1, Some(0), 10_000)], Some(0)), &rules(), &balances(0, 150));
        // b owes 200 for damage but only has 150, then has nothing left for the win.
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].sats, 150);
    }

    #[test]
    fn winnings_received_can_be_paid_back_out() {
        // a takes 50% from b (owes 100), then wins and b pays 100 more.
        let t = compute(&report(vec![hit(0, Some(1), 5000)], Some(1)), &rules(), &balances(150, 0));
        let paid: u64 = t.iter().map(|t| t.sats).sum();
        assert_eq!(paid, 150);
    }

    #[test]
    fn validation_rejects_bad_reports() {
        assert!(validate(&report(vec![], Some(0))).is_ok());
        assert!(validate(&report(vec![hit(1, Some(1), 100)], None)).is_err());
        assert!(validate(&report(vec![], Some(3))).is_err());
        let mut r = report(vec![], None);
        r.players[1].player_id = "a".into();
        assert!(validate(&r).is_err());
    }
}
