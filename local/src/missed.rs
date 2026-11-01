//! A missed transfer retains its own debt; unrelated live payments keep running.
use super::*;

impl Saved {
    pub(super) fn validate_payout_groups(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for (leader, members) in &self.payout_groups {
            let head = self
                .pot
                .payouts
                .iter()
                .find(|p| &p.id == leader)
                .ok_or(Error::NotFound)?;
            if !members.contains(leader)
                || self.pot.outcome.is_none()
                || self.group_sats(leader).is_none()
            {
                return Err(Error::Rejected("Invalid missed payment group".into()));
            }
            for id in members {
                let part = self
                    .pot
                    .payouts
                    .iter()
                    .find(|p| &p.id == id)
                    .ok_or(Error::NotFound)?;
                if !seen.insert(id)
                    || self.addresses[part.player] != self.addresses[head.player]
                    || part.state != head.state
                    || self
                        .prepared
                        .get(id)
                        .map(|p| (&p.payment_hash, &p.invoice, &p.address))
                        != self
                            .prepared
                            .get(leader)
                            .map(|p| (&p.payment_hash, &p.invoice, &p.address))
                {
                    return Err(Error::Rejected("Inconsistent missed payment group".into()));
                }
            }
        }
        Ok(())
    }
    pub(super) fn group_leader(&self, id: &str) -> Option<&str> {
        self.payout_groups
            .iter()
            .find(|(_, members)| members.iter().any(|m| m == id))
            .map(|(leader, _)| leader.as_str())
    }
    pub(super) fn group_sats(&self, id: &str) -> Option<u64> {
        let members = self.payout_groups.get(id)?;
        self.pot
            .payouts
            .iter()
            .filter(|p| members.contains(&p.id))
            .try_fold(0u64, |sum, p| sum.checked_add(p.sats))
    }
    pub(super) fn held_payouts(&self) -> BTreeSet<String> {
        self.pot
            .payouts
            .iter()
            .filter(|p| {
                self.recovery.get(&p.id).is_some_and(|r| r.missed)
                    || self.group_leader(&p.id).is_some()
            })
            .map(|p| p.id.clone())
            .collect()
    }
    fn safe_missed(&self, p: &Payout) -> bool {
        let leader = self.group_leader(&p.id).unwrap_or(&p.id);
        let Some(r) = self.recovery.get(leader).filter(|r| r.missed) else {
            return false;
        };
        !r.manual_ready
            && (p.state == PayoutState::Created
                || (p.state == PayoutState::Failed
                    && self
                        .prepared
                        .get(leader)
                        .is_some_and(|invoice| r.confirmed_failed(invoice))))
    }
}

impl Demo {
    pub(super) fn mark_missed(&mut self, id: &str) -> Result<()> {
        if !self.io.track_missed {
            return Ok(());
        }
        if let Some(r) = self.recovery(id).filter(|r| r.missed) {
            if !r.manual_ready {
                return Ok(());
            }
            self.recovery_mut(id)?.manual_ready = false;
            return self.save();
        }
        let (sats, address, _) = self.payment_details(id).ok_or(Error::NotFound)?;
        let recovery = self.recovery_mut(id)?;
        recovery.missed = true;
        recovery.manual_ready = false;
        recovery.retry_at_ms = None;
        self.event(format!("MISSED: {sats} sats to {address}. Other payouts continue; settle this amount after the match. Submitted payments keep their original hash."));
        self.save()
    }

    pub(super) fn missed_summary(&self) -> (u64, u64, String) {
        let mut total = 0;
        let mut payable = 0;
        let mut rows = Vec::new();
        for owner in std::iter::once(&self.saved).chain(self.saved.pending_matches.iter()) {
            for p in &owner.pot.payouts {
                if p.state.complete() || !owner.recovery.get(&p.id).is_some_and(|r| r.missed) {
                    continue;
                }
                total += p.sats;
                let safe = owner.safe_missed(p);
                let finished = owner.pot.outcome.is_some();
                if safe && finished {
                    payable += p.sats;
                }
                let status = if p.state == PayoutState::InFlight {
                    "Awaiting wallet result; original send checked, excluded from new payouts"
                } else if !finished {
                    "Held until match end"
                } else if owner
                    .recovery
                    .get(owner.group_leader(&p.id).unwrap_or(&p.id))
                    .is_some_and(|r| r.manual_ready)
                {
                    "Manual payout requested"
                } else if safe {
                    "Ready for Pay missed payments"
                } else {
                    "Original failure must be verified before retrying"
                };
                rows.push(format!(
                    "P{} • {} sats → {}\n{} • {}\n",
                    p.player + 1,
                    p.sats,
                    owner.addresses[p.player],
                    p.id,
                    status
                ));
            }
        }
        for entry in &self.saved.retired_entries {
            let id = format!("late-entry-{}", entry.incoming_hash);
            let Some(r) = self.saved.recovery.get(&id).filter(|r| r.missed) else {
                continue;
            };
            if entry.state.complete() {
                continue;
            }
            let Some(sats) = entry.received_sats else {
                continue;
            };
            total += sats;
            if !r.manual_ready
                && (entry.state == PayoutState::Created
                    || (entry.state == PayoutState::Failed
                        && entry
                            .prepared
                            .as_ref()
                            .is_some_and(|p| r.confirmed_failed(p))))
            {
                payable += sats;
            }
            rows.push(format!(
                "Late P{} entry • {sats} sats → {}\n{}\n",
                entry.player + 1,
                entry.address,
                r.description()
            ));
        }
        let mut text = format!(
            "Missed: {total} sats • Safe to request after match: {payable} sats\nUnknown sends remain reserved and are never paid twice.\n\n"
        );
        for row in rows.iter().take(22) {
            text.push_str(row);
            text.push('\n');
        }
        if rows.len() > 22 {
            text.push_str("More records remain in Outbound payments.\n");
        }
        (total, payable, text)
    }

    pub(super) fn pay_missed(&mut self) -> Result<()> {
        let mut requested = Vec::new();
        let candidates: Vec<_> = std::iter::once(&self.saved)
            .chain(self.saved.pending_matches.iter())
            .filter(|s| s.pot.outcome.is_some())
            .flat_map(|s| {
                s.pot
                    .payouts
                    .iter()
                    .filter(|p| s.safe_missed(p))
                    .map(|p| p.id.clone())
            })
            .collect();
        self.io.cancel_preparations(&candidates);
        let mut collect = |owner: &mut Saved| {
            if owner.pot.outcome.is_none() {
                return;
            }
            let mut destinations: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for p in &owner.pot.payouts {
                if !owner.safe_missed(p) {
                    continue;
                }
                if let Some(leader) = owner.group_leader(&p.id) {
                    if leader == p.id {
                        requested.push((
                            leader.to_owned(),
                            owner.addresses[p.player].clone(),
                            owner.group_sats(leader).unwrap(),
                        ));
                    }
                } else {
                    destinations
                        .entry(owner.addresses[p.player].clone())
                        .or_default()
                        .push(p.id.clone());
                }
            }
            for (address, members) in destinations {
                let leader = members[0].clone();
                let sats = owner
                    .pot
                    .payouts
                    .iter()
                    .filter(|p| members.contains(&p.id))
                    .map(|p| p.sats)
                    .sum();
                // Only never-submitted or wallet-proven-failed amounts reach here.
                for p in owner
                    .pot
                    .payouts
                    .iter_mut()
                    .filter(|p| members.contains(&p.id))
                {
                    p.state = PayoutState::Created;
                    owner.prepared.remove(&p.id);
                }
                owner.payout_groups.insert(leader.clone(), members);
                requested.push((leader, address, sats));
            }
        };
        collect(&mut self.saved);
        for owner in &mut self.saved.pending_matches {
            collect(owner);
        }
        for entry in &self.saved.retired_entries {
            let id = format!("late-entry-{}", entry.incoming_hash);
            if entry.state.complete() {
                continue;
            }
            let Some(r) = self.saved.recovery.get(&id).filter(|r| r.missed) else {
                continue;
            };
            if !r.manual_ready
                && (entry.state == PayoutState::Created
                    || (entry.state == PayoutState::Failed
                        && entry
                            .prepared
                            .as_ref()
                            .is_some_and(|p| r.confirmed_failed(p))))
            {
                if let Some(sats) = entry.received_sats {
                    requested.push((id, entry.address.clone(), sats));
                }
            }
        }
        for (id, address, sats) in &requested {
            let r = self.recovery_mut(id)?;
            r.manual_ready = true;
            r.limit = r.limit.max(r.failed.len() + 1);
            r.retry_at_ms = Some(now_ms());
            self.io.clear_retry(id);
            self.event(format!(
                "Manual payout requested: {sats} sats → {address}. Unknown original sends excluded."
            ));
        }
        if requested.is_empty() {
            self.event("No safe missed amounts to send. Finish the match or wait for original payment results.".into());
        }
        self.save()
    }
}
