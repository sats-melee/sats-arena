//! Optional read-only diagnostics. No wallet calls and no financial mutations.
use super::*;
use std::{
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime},
};

#[derive(Default)]
pub(super) struct DebugView {
    enabled: AtomicBool,
    text: Mutex<String>,
    updated: Mutex<Option<Instant>>,
}

impl DebugView {
    pub(super) fn refresh(&self, demo: &Demo, history: &mut History) {
        if !self.enabled.load(Ordering::Relaxed) {
            *history = History::default();
            return;
        }
        let mut updated = self.updated.lock().unwrap();
        if updated.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        let snapshot = history.render(demo);
        *self.text.lock().unwrap() = snapshot;
        *updated = Some(Instant::now());
    }
}

struct Archive {
    modified: Option<SystemTime>,
    bytes: u64,
    payments: Vec<Payment>,
}

#[derive(Default)]
pub(super) struct History {
    archives: BTreeMap<PathBuf, Archive>,
    scanned: Option<Instant>,
    unreadable: bool,
}

#[derive(Clone)]
struct Payment {
    id: String,
    player: usize,
    sats: u64,
    address: String,
    kind: &'static str,
    state: PayoutState,
    prepared: bool,
    confirmed_at_ms: Option<u64>,
    simulated: bool,
    collecting_until_ms: Option<u64>,
    replaced_by: Option<String>,
}

fn payments(saved: &Saved) -> Vec<Payment> {
    let mut rows = Vec::new();
    for payout in &saved.pot.payouts {
        let prepared = saved.prepared.get(&payout.id);
        rows.push(Payment {
            id: payout.id.clone(),
            player: payout.player,
            sats: payout.sats,
            // The invoice's bound receiver wins over any later address edits.
            address: prepared
                .map(|p| p.address.clone())
                .unwrap_or_else(|| saved.addresses[payout.player].clone()),
            kind: match payout.reason {
                Reason::Damage => "Damage",
                Reason::WinnerPot => "Winnings",
                Reason::Return => "Refund",
                Reason::Settlement => "Settlement",
            },
            state: payout.state,
            prepared: prepared.is_some(),
            confirmed_at_ms: prepared.and_then(|p| p.confirmed_at_ms),
            simulated: saved.simulated,
            collecting_until_ms: None,
            replaced_by: saved.pot.payout_replacements.get(&payout.id).cloned(),
        });
    }
    for entry in &saved.retired_entries {
        let Some(sats) = entry.received_sats.filter(|&sats| sats > 0) else {
            continue;
        };
        rows.push(Payment {
            id: format!("late-entry-{}", entry.incoming_hash),
            player: entry.player,
            sats,
            address: entry.address.clone(),
            kind: "Late entry refund",
            state: entry.state,
            prepared: entry.prepared.is_some(),
            confirmed_at_ms: entry.prepared.as_ref().and_then(|p| p.confirmed_at_ms),
            simulated: saved.simulated,
            collecting_until_ms: None,
            replaced_by: None,
        });
    }
    rows
}

impl History {
    fn scan(&mut self, folder: &Path) {
        if self
            .scanned
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.scanned = Some(Instant::now());
        self.unreadable = false;
        let entries = match std::fs::read_dir(folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                self.unreadable = true;
                return;
            }
        };
        for entry in entries {
            let loaded = (|| -> anyhow::Result<()> {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    return Ok(());
                }
                let metadata = entry.metadata()?;
                let modified = metadata.modified().ok();
                if self.archives.get(&path).is_some_and(|archive| {
                    archive.modified == modified && archive.bytes == metadata.len()
                }) {
                    return Ok(());
                }
                let saved: Saved = serde_json::from_slice(&std::fs::read(&path)?)?;
                anyhow::ensure!(
                    saved.pot.payouts.iter().all(|p| p.player < 4)
                        && saved.retired_entries.iter().all(|p| p.player < 4),
                    "invalid archived player"
                );
                self.archives.insert(
                    path,
                    Archive {
                        modified,
                        bytes: metadata.len(),
                        payments: payments(&saved),
                    },
                );
                Ok(())
            })();
            if loaded.is_err() {
                self.unreadable = true;
            }
        }
    }

    fn render(&mut self, demo: &Demo) -> String {
        self.scan(&demo.path.parent().unwrap().join("matches"));
        let mut rows = BTreeMap::new();
        // Archives may contain earlier copies of recovering matches. The active
        // ledger always overrides those copies, keyed by immutable payment ID.
        for archive in self.archives.values() {
            for row in &archive.payments {
                rows.insert(row.id.clone(), row.clone());
            }
        }
        for saved in demo
            .saved
            .pending_matches
            .iter()
            .chain(std::iter::once(&demo.saved))
        {
            for row in payments(saved) {
                rows.insert(row.id.clone(), row);
            }
        }
        for player in 0..4 {
            let sats = demo.saved.pot.unbatched[player];
            if sats > 0 {
                let id = format!("{}-open-P{}", demo.saved.pot.id, player + 1);
                rows.insert(
                    id.clone(),
                    Payment {
                        id,
                        player,
                        sats,
                        address: demo.saved.addresses[player].clone(),
                        kind: "Damage batch",
                        state: PayoutState::Created,
                        prepared: false,
                        confirmed_at_ms: None,
                        simulated: demo.saved.simulated,
                        collecting_until_ms: demo.saved.pot.batch_deadline_ms[player],
                        replaced_by: None,
                    },
                );
            }
        }
        let count = |state| rows.values().filter(|row| row.state == state).count();
        let mut text = format!(
            "{} outbound payments / open batches\n{} queued | {} sending or awaiting confirmation | {} paid | {} failed\n",
            rows.len(),
            count(PayoutState::Created),
            count(PayoutState::InFlight),
            count(PayoutState::Paid),
            count(PayoutState::Failed),
        );
        if count(PayoutState::Superseded) > 0 {
            let _ = writeln!(
                text,
                "{} consolidated records; their amounts are included in replacements",
                count(PayoutState::Superseded)
            );
        }
        if self.unreadable {
            text.push_str("Some archived payments could not be read. History may be incomplete.\n");
        }
        if rows.is_empty() {
            text.push_str("\nNo outbound payments yet.\n");
        }
        let mut rows: Vec<_> = rows.into_values().collect();
        rows.sort_by_key(|row| {
            (
                match row.state {
                    PayoutState::Failed => 0,
                    PayoutState::Created => 1,
                    PayoutState::InFlight => 2,
                    PayoutState::Paid => 3,
                    PayoutState::Superseded => 4,
                },
                row.id.clone(),
            )
        });
        for row in rows {
            let state = match row.state {
                PayoutState::Created => "QUEUED",
                PayoutState::InFlight => "IN FLIGHT",
                PayoutState::Paid => "PAID",
                PayoutState::Failed => "FAILED",
                PayoutState::Superseded => "CONSOLIDATED",
            };
            let simulated = if row.simulated { " / SIMULATED" } else { "" };
            let _ = writeln!(
                text,
                "\n[{state}] P{} / {} sats / {}{simulated}",
                row.player + 1,
                row.sats,
                row.kind
            );
            let _ = writeln!(text, "To: {}\nID: {}", row.address, row.id);
            if let Some(replacement) = row.replaced_by {
                let _ = writeln!(text, "Replaced by: {replacement}");
            }
            if let Some(deadline) = row.collecting_until_ms {
                let _ = writeln!(
                    text,
                    "Collecting batch; due in {}s",
                    deadline.saturating_sub(now_ms()).div_ceil(1000)
                );
            } else {
                let _ = writeln!(
                    text,
                    "{}",
                    demo.io.outbound_status(&row.id, row.state, row.prepared)
                );
            }
            if let Some(at) = row.confirmed_at_ms {
                let seconds = at / 1000;
                let _ = writeln!(
                    text,
                    "Confirmed {:02}:{:02}:{:02} UTC (Unix ms {at})",
                    seconds / 3600 % 24,
                    seconds / 60 % 60,
                    seconds % 60
                );
            }
        }
        text
    }
}

/// # Safety
/// Handle is live and is not being destroyed. This only enables a read-only view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_outbound_debug(
    handle: *const LocalHandle,
    enabled: bool,
) -> bool {
    ffi_guard(|| {
        let Some(handle) = (unsafe { handle.as_ref() }) else {
            return false;
        };
        handle.outbound.enabled.store(enabled, Ordering::Relaxed);
        true
    })
    .unwrap_or(false)
}

/// # Safety
/// Handle is live; a non-null out points to size writable bytes. An undersized
/// buffer is never partially filled, so UTF-8 and the full history stay intact.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_outbound_snapshot(
    handle: *const LocalHandle,
    out: *mut c_char,
    size: u32,
) -> u32 {
    ffi_guard(|| {
        let Some(handle) = (unsafe { handle.as_ref() }) else {
            return 0;
        };
        if !handle.outbound.enabled.load(Ordering::Relaxed) {
            return 0;
        }
        let Ok(text) = handle.outbound.text.try_lock() else {
            return 0;
        };
        if text.is_empty() {
            return 0;
        }
        let Ok(required) = u32::try_from(text.len() + 1) else {
            return 0;
        };
        if !out.is_null() && size >= required {
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), text.len());
                out.add(text.len()).write(0);
            }
        }
        required
    })
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_history_includes_open_queued_recovering_failed_and_archived_payments_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut demo = Demo::open(dir.path()).await.unwrap();
        demo.saved.addresses[0] = "later@example.test".into();
        demo.saved.pot.payouts = (0..120)
            .map(|i| Payout {
                id: format!("current-payout-{i:03}"),
                player: 0,
                sats: 1,
                reason: Reason::Damage,
                state: PayoutState::Created,
            })
            .collect();
        demo.saved.prepared.insert(
            "current-payout-000".into(),
            PreparedPayout {
                address: "locked@example.test".into(),
                invoice: "invoice-must-not-appear".into(),
                payment_hash: "hash-must-not-appear".into(),
                confirmed_at_ms: None,
                memo: String::new(),
            },
        );
        demo.saved.pot.unbatched[0] = 7;
        demo.saved.pot.batch_deadline_ms[0] = Some(now_ms() + 5000);
        let mut archived = demo.saved.clone();
        archived.pot.unbatched = [0; 4];
        archived.pot.payouts = vec![Payout {
            id: "archived-payout".into(),
            player: 1,
            sats: 23,
            reason: Reason::Settlement,
            state: PayoutState::Paid,
        }];
        archived.prepared.clear();
        let folder = demo.path.parent().unwrap().join("matches");
        std::fs::create_dir_all(&folder).unwrap();
        Demo::persist_saved(&archived, &folder.join("old.json")).unwrap();
        let mut recovering = archived.clone();
        recovering.pot.payouts[0].id = "recovering-payout".into();
        recovering.pot.payouts[0].state = PayoutState::InFlight;
        // A stale archive for this same payout must never duplicate/override it.
        archived.pot.payouts[0].id = "recovering-payout".into();
        Demo::persist_saved(&archived, &folder.join("recovering.json")).unwrap();
        demo.saved.pending_matches.push(recovering);
        demo.saved.retired_entries.push(RetiredEntry {
            incoming_hash: "late-hash".into(),
            player: 1,
            address: "refund@example.test".into(),
            expected_sats: 10,
            received_sats: Some(10),
            prepared: None,
            state: PayoutState::Failed,
        });
        demo.save().unwrap();
        let before = serde_json::to_value(&demo.saved).unwrap();
        let on_disk = std::fs::read(&demo.path).unwrap();
        let text = History::default().render(&demo);
        assert!(text.starts_with("124 outbound payments / open batches\n121 queued | 1 sending or awaiting confirmation | 1 paid | 1 failed\n"));
        assert!(text.len() > 4096);
        assert!(text.contains("ID: current-payout-119\n"));
        assert!(text.contains("[IN FLIGHT] P2 / 23 sats"));
        assert_eq!(text.matches("ID: recovering-payout\n").count(), 1);
        assert!(text.contains("[PAID] P2 / 23 sats"));
        assert!(text.contains("[FAILED] P2 / 10 sats / Late entry refund"));
        assert!(text.contains("To: locked@example.test\nID: current-payout-000\nQueued to send"));
        assert!(text.contains("Collecting batch; due in"));
        assert!(!text.contains("invoice-must-not-appear"));
        assert!(!text.contains("hash-must-not-appear"));
        assert_eq!(serde_json::to_value(&demo.saved).unwrap(), before);
        assert_eq!(std::fs::read(&demo.path).unwrap(), on_disk);
    }

    #[tokio::test]
    async fn diagnostics_are_disabled_by_default_and_report_incomplete_archives() {
        let dir = tempfile::tempdir().unwrap();
        let demo = Demo::open(dir.path()).await.unwrap();
        let folder = demo.path.parent().unwrap().join("matches");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("unreadable.json"), b"invalid archive").unwrap();
        let mut invalid = demo.saved.clone();
        invalid.pot.payouts.push(Payout {
            id: "invalid-player".into(),
            player: 4,
            sats: 1,
            reason: Reason::Damage,
            state: PayoutState::Created,
        });
        Demo::persist_saved(&invalid, &folder.join("invalid-player.json")).unwrap();
        let debug = DebugView::default();
        let mut history = History::default();
        debug.refresh(&demo, &mut history);
        assert!(debug.text.lock().unwrap().is_empty());
        assert!(history.scanned.is_none());
        debug.enabled.store(true, Ordering::Relaxed);
        debug.refresh(&demo, &mut history);
        let text = debug.text.lock().unwrap();
        assert!(text.contains("History may be incomplete"));
        assert!(text.contains("No outbound payments yet"));
    }

    #[test]
    fn ffi_delivers_complete_utf8_and_never_partially_writes_a_short_buffer() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let handle = LocalHandle {
            sender,
            shared: Arc::new(Mutex::new(LocalView::default())),
            outbound: Arc::new(DebugView::default()),
            worker: None,
        };
        let text = "Payment → 100 sats\n".repeat(1000);
        *handle.outbound.text.lock().unwrap() = text.clone();
        unsafe {
            assert_eq!(
                arena_local_outbound_snapshot(&handle, std::ptr::null_mut(), 0),
                0
            );
            assert!(arena_local_outbound_debug(&handle, true));
            let required = arena_local_outbound_snapshot(&handle, std::ptr::null_mut(), 0);
            assert_eq!(required as usize, text.len() + 1);
            let mut short = [b'!'; 8];
            assert_eq!(
                arena_local_outbound_snapshot(&handle, short.as_mut_ptr().cast(), 8),
                required
            );
            assert_eq!(short, [b'!'; 8]);
            let mut exact = vec![b'!'; required as usize];
            assert_eq!(
                arena_local_outbound_snapshot(&handle, exact.as_mut_ptr().cast(), required),
                required
            );
            assert_eq!(
                CStr::from_ptr(exact.as_ptr().cast()).to_str().unwrap(),
                text
            );
            assert!(arena_local_outbound_debug(&handle, false));
            assert_eq!(
                arena_local_outbound_snapshot(&handle, exact.as_mut_ptr().cast(), required),
                0
            );
            assert!(!arena_local_outbound_debug(std::ptr::null(), true));
            assert_eq!(
                arena_local_outbound_snapshot(std::ptr::null(), std::ptr::null_mut(), 0),
                0
            );
        }
        assert_eq!(handle.shared.lock().unwrap().busy, 1);
    }
}
