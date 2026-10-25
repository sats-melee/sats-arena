//! Fake-only prepaid match host. Two fresh invoices per match; outgoing
//! micropayments during play, remaining pot to the winner. No reusable balances.
use sats_arena_core::{
    Error, Result,
    pot::{MatchPot, PayoutState, Phase},
    wallet::{
        AddressInvoices, FakeAddressInvoices, FakeWallet, InvoiceState, LightningAddress,
        PaymentState, PreparedPayout, Wallet, prepare_payout,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, c_char},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::Duration,
};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LocalView {
    pub abi_version: u32,
    pub phase: u32,
    pub busy: u32,
    pub simulated: u32,
    pub revision: u64,
    pub entry_sats: u64,
    pub sats_per_percent: u64,
    pub paid_sats: [u64; 2],
    pub remaining_sats: [u64; 2],
    pub earned_sats: [u64; 2],
    pub pending_sats: [u64; 2],
    pub invoices: [[u8; 2048]; 2],
    pub error: [u8; 256],
    pub addresses: [[u8; 128]; 2],
    pub batch_remaining_ms: [u64; 2],
    pub payment_log: [u8; 4096],
}
impl Default for LocalView {
    fn default() -> Self {
        Self {
            abi_version: 3,
            phase: 0,
            busy: 1,
            simulated: 1,
            revision: 0,
            entry_sats: 500,
            sats_per_percent: 10,
            paid_sats: [0; 2],
            remaining_sats: [0; 2],
            earned_sats: [0; 2],
            pending_sats: [0; 2],
            invoices: [[0; 2048]; 2],
            error: [0; 256],
            addresses: [[0; 128]; 2],
            batch_remaining_ms: [0; 2],
            payment_log: [0; 4096],
        }
    }
}

enum Command {
    Action(u32),
    Credit([u64; 2]),
    Addresses([String; 2]),
}

#[derive(Serialize, Deserialize)]
struct Saved {
    schema: u32,
    pot: MatchPot,
    invoices: [String; 2],
    hashes: [String; 2],
    #[serde(default)]
    addresses: [String; 2],
    #[serde(default)]
    prepared: BTreeMap<String, PreparedPayout>,
    #[serde(default)]
    used_payment_hashes: BTreeSet<String>,
    #[serde(default)]
    receipts: Vec<Receipt>,
    #[serde(default = "default_fee_limit")]
    max_fee_sats: u64,
    #[serde(default = "default_simulated")]
    simulated: bool,
}
#[derive(Serialize, Deserialize, Clone)]
struct Receipt {
    id: String,
    at_ms: u64,
    player: usize,
    address: String,
    sats: u64,
    detail: String,
}
struct Demo {
    wallet: Arc<dyn Wallet>,
    provider: Arc<dyn AddressInvoices>,
    simulation: Option<Arc<FakeWallet>>,
    saved: Saved,
    path: PathBuf,
}
impl Demo {
    async fn fresh(wallet: &dyn Wallet, simulated: bool) -> Result<Saved> {
        let setting = |name: &str, default: u64| -> Result<u64> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse()
                    .map_err(|_| Error::Rejected(format!("invalid {name}"))),
                Err(_) => Ok(default),
            }
        };
        let pot = MatchPot::new(
            format!("match-{:032x}", rand::random::<u128>()),
            setting("SATS_ARENA_ENTRY_SATS", 500)?,
            setting("SATS_ARENA_SATS_PER_PERCENT", 10)?,
        )?;
        let mut invoices = [String::new(), String::new()];
        let mut hashes = [String::new(), String::new()];
        for i in 0..2 {
            let invoice = wallet
                .create_invoice(pot.entry_sats, &format!("{} P{}", pot.id, i + 1), 600)
                .await?;
            invoices[i] = invoice.bolt11;
            hashes[i] = invoice.payment_hash;
        }
        Ok(Saved {
            schema: 2,
            pot,
            invoices,
            hashes,
            addresses: [String::new(), String::new()],
            prepared: BTreeMap::new(),
            used_payment_hashes: BTreeSet::new(),
            receipts: vec![],
            simulated,
            max_fee_sats: setting("SATS_ARENA_MAX_PAYOUT_FEE_SATS", default_fee_limit())?,
        })
    }
    async fn open(folder: &Path) -> Result<Self> {
        std::fs::create_dir_all(folder).map_err(|e| Error::Internal(e.into()))?;
        let fake = Arc::new(FakeWallet::default());
        Self::open_with(
            folder,
            fake.clone(),
            Arc::new(FakeAddressInvoices),
            Some(fake),
        )
        .await
    }
    // No NWC/LND-specific behavior below this boundary.
    async fn open_with(
        folder: &Path,
        wallet: Arc<dyn Wallet>,
        provider: Arc<dyn AddressInvoices>,
        simulation: Option<Arc<FakeWallet>>,
    ) -> Result<Self> {
        let path = folder.join("prepaid-match.json");
        let mut saved: Saved = if path.exists() {
            let bytes = std::fs::read(&path).map_err(|e| Error::Internal(e.into()))?;
            let mut json: serde_json::Value = serde_json::from_slice(&bytes)?;
            // Recover the first preview's nested-Option outcome representation.
            if json["pot"]["outcome"].is_number() {
                json["pot"]["outcome"] = serde_json::json!({"Winner": json["pot"]["outcome"]});
            }
            serde_json::from_value(json)?
        } else {
            Self::fresh(wallet.as_ref(), simulation.is_some()).await?
        };
        if saved.simulated != simulation.is_some() {
            return Err(Error::Rejected(
                "payment state belongs to a different wallet mode".into(),
            ));
        }
        if !(1..=2).contains(&saved.schema) {
            return Err(Error::Rejected("unsupported prepaid match state".into()));
        }
        if saved.schema == 1 && saved.pot.paid.iter().any(|paid| *paid) {
            if simulation.is_none() {
                return Err(Error::Rejected(
                    "legacy fake state cannot use a real wallet".into(),
                ));
            }
            saved.addresses = ["player1@example.test".into(), "player2@example.test".into()];
            for payout in &saved.pot.payouts {
                if payout.state != PayoutState::Created {
                    let invoice = format!(
                        "lnfake:{}:player-{}-{}",
                        payout.sats,
                        payout.player + 1,
                        payout.id
                    );
                    let payment_hash = wallet.decode_invoice(&invoice)?.payment_hash;
                    saved.prepared.insert(
                        payout.id.clone(),
                        PreparedPayout {
                            address: saved.addresses[payout.player].clone(),
                            invoice,
                            payment_hash,
                            confirmed_at_ms: None,
                        },
                    );
                }
            }
        }
        saved
            .used_payment_hashes
            .extend(saved.prepared.values().map(|p| p.payment_hash.clone()));
        saved.schema = 2;
        let demo = Self {
            wallet,
            provider,
            simulation,
            saved,
            path,
        };
        demo.save()?;
        Ok(demo)
    }
    fn save(&self) -> Result<()> {
        self.persist(&self.path)
    }
    fn persist(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let temporary = path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|e| Error::Internal(e.into()))?;
        file.write_all(&serde_json::to_vec(&self.saved)?)
            .map_err(|e| Error::Internal(e.into()))?;
        file.sync_all().map_err(|e| Error::Internal(e.into()))?;
        std::fs::rename(temporary, path).map_err(|e| Error::Internal(e.into()))?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| Error::Internal(e.into()))?;
        }
        Ok(())
    }
    async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Credit(totals) => self.saved.pot.credit_at(totals, now_ms())?,
            Command::Addresses(addresses) => {
                if self.saved.pot.phase != Phase::Funding
                    || self.saved.pot.paid.iter().any(|paid| *paid)
                {
                    return Err(Error::Rejected(
                        "payout addresses are locked after entry payment".into(),
                    ));
                }
                let first = parse_address(&addresses[0])?;
                let second = parse_address(&addresses[1])?;
                self.saved.addresses = [first.as_str().into(), second.as_str().into()];
            }
            Command::Action(action) => match action {
                1 => {
                    if self.saved.pot.phase != Phase::Settled {
                        return Err(Error::Rejected(
                            "previous match payouts are not complete".into(),
                        ));
                    }
                    let folder = self.path.parent().unwrap().join("matches");
                    std::fs::create_dir_all(&folder).map_err(|e| Error::Internal(e.into()))?;
                    let archive = folder.join(format!("{}.json", self.saved.pot.id));
                    self.persist(&archive)?;
                    let mut next =
                        Self::fresh(self.wallet.as_ref(), self.simulation.is_some()).await?;
                    next.addresses = self.saved.addresses.clone();
                    next.receipts = self.saved.receipts.clone();
                    next.used_payment_hashes = self.saved.used_payment_hashes.clone();
                    self.saved = next;
                }
                2 | 3 => {
                    if self.saved.pot.phase != Phase::Funding {
                        return Err(Error::Rejected(
                            "match is not awaiting entry payment".into(),
                        ));
                    }
                    let player = (action - 2) as usize;
                    self.require_addresses()?;
                    self.simulation
                        .as_ref()
                        .ok_or_else(|| Error::Rejected("simulation is disabled".into()))?
                        .set_invoice_state(
                            &self.saved.hashes[player],
                            InvoiceState::Paid {
                                amount_sats: self.saved.pot.entry_sats,
                            },
                        );
                }
                4 => {
                    self.require_addresses()?;
                    self.saved.pot.start()?;
                }
                5 | 8 => self.saved.pot.finish(None)?,
                6 | 7 => self.saved.pot.finish(Some((action - 6) as usize))?,
                _ => return Err(Error::Rejected("unknown prepaid match action".into())),
            },
        }
        self.save()?;
        Ok(())
    }
    fn require_addresses(&self) -> Result<()> {
        for address in &self.saved.addresses {
            parse_address(address)?;
        }
        Ok(())
    }
    async fn reconcile(&mut self) -> Result<()> {
        let previous_count = self.saved.pot.payouts.len();
        self.saved.pot.flush_due(now_ms());
        if previous_count != self.saved.pot.payouts.len() {
            self.save()?;
        }
        for player in 0..2 {
            if !self.saved.pot.paid[player] && self.saved.pot.phase == Phase::Funding {
                match self
                    .wallet
                    .invoice_state(&self.saved.hashes[player])
                    .await?
                {
                    InvoiceState::Paid { amount_sats } => {
                        self.require_addresses()?;
                        self.saved.pot.confirm_entry(player, amount_sats)?;
                        self.save()?;
                    }
                    InvoiceState::Pending => (),
                    InvoiceState::Expired => {
                        let invoice = self
                            .wallet
                            .create_invoice(self.saved.pot.entry_sats, &self.saved.pot.id, 600)
                            .await?;
                        self.saved.invoices[player] = invoice.bolt11;
                        self.saved.hashes[player] = invoice.payment_hash;
                        self.save()?;
                    }
                }
            }
        }
        for index in 0..self.saved.pot.payouts.len() {
            let payout = self.saved.pot.payouts[index].clone();
            if payout.state == PayoutState::Paid {
                continue;
            }
            if payout.state == PayoutState::Failed {
                return Err(Error::Rejected("payout failed; needs attention".into()));
            }
            if !self.saved.prepared.contains_key(&payout.id) {
                if payout.state == PayoutState::InFlight {
                    return Err(Error::Rejected(
                        "missing persisted invoice for pending payout".into(),
                    ));
                }
                let address = parse_address(&self.saved.addresses[payout.player])?;
                let prepared = tokio::time::timeout(
                    Duration::from_secs(10),
                    prepare_payout(
                        self.wallet.as_ref(),
                        self.provider.as_ref(),
                        &address,
                        payout.sats,
                        &payout.id,
                    ),
                )
                .await
                .map_err(|_| Error::WalletUnavailable)??;
                // Never resolve this ID again once its invoice is persisted.
                if self
                    .saved
                    .used_payment_hashes
                    .contains(&prepared.payment_hash)
                {
                    return Err(Error::Rejected("recipient reused a payment hash".into()));
                }
                self.saved
                    .used_payment_hashes
                    .insert(prepared.payment_hash.clone());
                self.saved.prepared.insert(payout.id.clone(), prepared);
                self.save()?;
            }
            let prepared = self.saved.prepared[&payout.id].clone();
            let state = match payout.state {
                PayoutState::Created => {
                    self.saved.pot.begin_payout(index)?;
                    self.save()?;
                    match tokio::time::timeout(
                        Duration::from_secs(10),
                        self.wallet
                            .pay_invoice(&prepared.invoice, self.saved.max_fee_sats),
                    )
                    .await
                    {
                        Ok(Ok(state)) => state,
                        _ => PaymentState::Pending,
                    }
                }
                PayoutState::InFlight => tokio::time::timeout(
                    Duration::from_secs(10),
                    self.wallet.payment_state(&prepared.payment_hash),
                )
                .await
                .map_err(|_| Error::WalletUnavailable)??,
                _ => unreachable!(),
            };
            match state {
                PaymentState::Succeeded { .. } => {
                    self.saved.pot.confirm_payout(index)?;
                    let at_ms = now_ms();
                    self.saved
                        .prepared
                        .get_mut(&payout.id)
                        .unwrap()
                        .confirmed_at_ms = Some(at_ms);
                    self.saved.receipts.push(Receipt {
                        id: payout.id.clone(),
                        at_ms,
                        player: payout.player,
                        address: prepared.address,
                        sats: payout.sats,
                        detail: format!("{:?}", payout.reason),
                    });
                    if self.saved.receipts.len() > 100 {
                        self.saved.receipts.remove(0);
                    }
                    self.save()?;
                }
                PaymentState::Pending => (),
                PaymentState::Failed { reason } => {
                    self.saved.pot.payouts[index].state = PayoutState::Failed;
                    self.save()?;
                    return Err(Error::Rejected(reason));
                }
            }
        }
        Ok(())
    }
    fn view(&self) -> Result<LocalView> {
        let pot = &self.saved.pot;
        let mut view = LocalView {
            phase: match pot.phase {
                Phase::Funding => 1,
                Phase::Ready => 2,
                Phase::Playing => 3,
                Phase::Settling => 6,
                Phase::Settled => 4,
            },
            busy: 0,
            simulated: u32::from(self.simulation.is_some()),
            entry_sats: pot.entry_sats,
            sats_per_percent: pot.sats_per_percent,
            paid_sats: pot.paid.map(|paid| if paid { pot.entry_sats } else { 0 }),
            remaining_sats: pot.remaining,
            earned_sats: pot.earned,
            pending_sats: pot.pending(),
            ..LocalView::default()
        };
        for i in 0..2 {
            text(&mut view.invoices[i], &self.saved.invoices[i]);
            text(&mut view.addresses[i], &self.saved.addresses[i]);
            view.batch_remaining_ms[i] = pot.batch_deadline_ms[i]
                .map(|d| d.saturating_sub(now_ms()))
                .unwrap_or(0);
        }
        let mut log = String::new();
        for receipt in self.saved.receipts.iter().rev().take(12).rev() {
            let seconds = receipt.at_ms / 1000;
            log.push_str(&format!(
                "{:02}:{:02}:{:02}Z {}PAID {} sats -> P{} {} ({})\n",
                (seconds / 3600) % 24,
                (seconds / 60) % 60,
                seconds % 60,
                if self.simulation.is_some() {
                    "SIMULATED "
                } else {
                    ""
                },
                receipt.sats,
                receipt.player + 1,
                receipt.address,
                receipt.detail
            ));
        }
        text(&mut view.payment_log, &log);
        Ok(view)
    }
}
fn parse_address(input: &str) -> Result<LightningAddress> {
    sats_arena_core::wallet::LightningAddress::parse(input)
        .map_err(|error| Error::Rejected(error.to_string()))
}
fn default_simulated() -> bool {
    true
}
fn default_fee_limit() -> u64 {
    10
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn text<const N: usize>(buffer: &mut [u8; N], value: &str) {
    buffer.fill(0);
    let mut length = value.len().min(N - 1);
    while !value.is_char_boundary(length) {
        length -= 1;
    }
    buffer[..length].copy_from_slice(&value.as_bytes()[..length]);
}

pub struct LocalHandle {
    sender: SyncSender<Command>,
    shared: Arc<Mutex<LocalView>>,
    worker: Option<JoinHandle<()>>,
}

/// # Safety
/// `folder` is a valid NUL-terminated UTF-8 path, alive for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_create(folder: *const c_char) -> *mut LocalHandle {
    ffi_guard(|| {
        if folder.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(folder) = (unsafe { CStr::from_ptr(folder) }).to_str() else {
            return std::ptr::null_mut();
        };
        let folder = PathBuf::from(folder);
        let shared = Arc::new(Mutex::new(LocalView::default()));
        let target = shared.clone();
        let (sender, receiver) = mpsc::sync_channel(16);
        let worker = std::thread::Builder::new()
            .name("arena-local-fake".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(value) => value,
                    Err(error) => {
                        let mut view = target.lock().unwrap();
                        view.phase = 7;
                        view.busy = 0;
                        text(&mut view.error, &error.to_string());
                        return;
                    }
                };
                let mut demo = match runtime.block_on(Demo::open(&folder)) {
                    Ok(value) => value,
                    Err(error) => {
                        let mut view = target.lock().unwrap();
                        view.phase = 7;
                        view.busy = 0;
                        text(&mut view.error, &error.to_string());
                        return;
                    }
                };
                let mut message = String::new();
                loop {
                    let processed_action = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(action) => {
                            message = runtime
                                .block_on(demo.command(action))
                                .err()
                                .map(|e| e.to_string())
                                .unwrap_or_default();
                            true
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                    };
                    let payment_message = runtime
                        .block_on(demo.reconcile())
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default();
                    if let Ok(mut view) = demo.view() {
                        let mut previous = target.lock().unwrap();
                        // A command can arrive while an idle snapshot is being
                        // built. Only its processed acknowledgement may clear
                        // busy; otherwise the UI could mistake stale Ready for
                        // the response to its Start request.
                        if !processed_action && previous.phase != 0 {
                            view.busy = previous.busy;
                        }
                        view.revision = previous.revision + 1;
                        text(
                            &mut view.error,
                            if payment_message.is_empty() {
                                &message
                            } else {
                                &payment_message
                            },
                        );
                        *previous = view;
                    }
                }
            });
        let Ok(worker) = worker else {
            return std::ptr::null_mut();
        };
        Box::into_raw(Box::new(LocalHandle {
            sender,
            shared,
            worker: Some(worker),
        }))
    })
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `handle` is a live handle returned by create, and `out` points to writable LocalView storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_snapshot(
    handle: *const LocalHandle,
    out: *mut LocalView,
) -> bool {
    ffi_guard(|| {
        if handle.is_null() || out.is_null() {
            return false;
        }
        let handle = unsafe { &*handle };
        let Ok(view) = handle.shared.try_lock() else {
            return false;
        };
        unsafe {
            out.write(*view);
        }
        true
    })
    .unwrap_or(false)
}

/// # Safety
/// `handle` is live and is not being destroyed. Actions are documented in the header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_action(handle: *const LocalHandle, action: u32) -> bool {
    ffi_guard(|| {
        if handle.is_null() || !(1..=8).contains(&action) {
            return false;
        }
        let handle = unsafe { &*handle };
        let Ok(mut view) = handle.shared.try_lock() else {
            return false;
        };
        if view.busy != 0 {
            return false;
        }
        if handle.sender.try_send(Command::Action(action)).is_err() {
            return false;
        }
        view.busy = 1;
        true
    })
    .unwrap_or(false)
}

/// # Safety
/// Own the live handle exclusively and destroy it exactly once, after polling stops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_destroy(handle: *mut LocalHandle) {
    if handle.is_null() {
        return;
    }
    let _ = ffi_guard(|| {
        let LocalHandle {
            sender,
            shared: _,
            mut worker,
        } = *unsafe { Box::from_raw(handle) };
        drop(sender);
        if let Some(worker) = worker.take() {
            let _ = worker.join();
        }
    });
}

// A panicking call must never unwind into C++. The worker's JoinHandle is
// accessed only by exclusive destruction; normal calls only use the channel
// and the mutex-protected snapshot.
fn ffi_guard<T>(action: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).ok()
}

/// # Safety
/// Handle is live. Counters are cumulative finalized attributed damage for the
/// current admitted match, in hundredths of a percent. Never pass speculative data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_credit(
    handle: *const LocalHandle,
    p1_damage_centi: u64,
    p2_damage_centi: u64,
) -> bool {
    ffi_guard(|| {
        if handle.is_null() {
            return false;
        }
        let handle = unsafe { &*handle };
        let Ok(mut view) = handle.shared.try_lock() else {
            return false;
        };
        if view.busy != 0 || view.phase != 3 {
            return false;
        }
        if handle
            .sender
            .try_send(Command::Credit([p1_damage_centi, p2_damage_centi]))
            .is_err()
        {
            return false;
        }
        view.busy = 1;
        true
    })
    .unwrap_or(false)
}

/// # Safety
/// Both address pointers are valid NUL-terminated UTF-8 strings. Handle is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_addresses(
    handle: *const LocalHandle,
    first: *const c_char,
    second: *const c_char,
) -> bool {
    ffi_guard(|| {
        if handle.is_null() || first.is_null() || second.is_null() {
            return false;
        }
        let (Ok(first), Ok(second)) = (
            unsafe { CStr::from_ptr(first) }.to_str(),
            unsafe { CStr::from_ptr(second) }.to_str(),
        ) else {
            return false;
        };
        let handle = unsafe { &*handle };
        let Ok(mut view) = handle.shared.try_lock() else {
            return false;
        };
        if view.busy != 0 {
            return false;
        }
        if handle
            .sender
            .try_send(Command::Addresses([first.into(), second.into()]))
            .is_err()
        {
            return false;
        }
        view.busy = 1;
        true
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Handle(*mut LocalHandle);
    impl Handle {
        fn open(path: &Path) -> Self {
            let folder = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            let handle = Self(unsafe { arena_local_create(folder.as_ptr()) });
            assert!(!handle.0.is_null());
            handle.wait(|v| v.phase != 0 && v.busy == 0);
            handle
        }
        fn wait(&self, ready: impl Fn(&LocalView) -> bool) -> LocalView {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                let mut view = LocalView::default();
                if unsafe { arena_local_snapshot(self.0, &mut view) } && ready(&view) {
                    assert_ne!(view.phase, 7, "{}", String::from_utf8_lossy(&view.error));
                    return view;
                }
                assert!(std::time::Instant::now() < deadline, "worker timeout");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn action(&self, action: u32) -> LocalView {
            self.wait(|v| v.busy == 0);
            assert!(unsafe { arena_local_action(self.0, action) });
            self.wait(|v| v.busy == 0)
        }
        fn credit(&self, totals: [u64; 2]) -> LocalView {
            self.wait(|v| v.busy == 0);
            assert!(unsafe { arena_local_credit(self.0, totals[0], totals[1]) });
            self.wait(|v| v.busy == 0)
        }
        fn fund(&self) {
            self.wait(|v| v.busy == 0);
            let first = std::ffi::CString::new("alice@example.test").unwrap();
            let second = std::ffi::CString::new("bob@example.test").unwrap();
            assert!(unsafe { arena_local_addresses(self.0, first.as_ptr(), second.as_ptr()) });
            self.wait(|v| v.busy == 0);
            assert_eq!(self.action(4).phase, 1);
            assert_eq!(self.action(2).paid_sats, [500, 0]);
            assert_eq!(self.action(4).phase, 1);
            assert_eq!(self.action(3).phase, 2);
            assert_eq!(self.action(4).phase, 3);
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { arena_local_destroy(self.0) }
        }
    }

    #[test]
    fn two_fresh_invoices_live_outgoing_payout_then_winner_pot_and_new_invoices() {
        assert_eq!(std::mem::size_of::<LocalView>(), 8824);
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        let initial = host.wait(|v| v.busy == 0);
        assert!(initial.invoices.iter().all(|v| v[0] != 0));
        assert_ne!(initial.invoices[0], initial.invoices[1]);
        host.fund();
        let live = host.credit([0, 800]);
        assert_eq!(live.earned_sats, [0, 0]);
        assert_eq!(live.pending_sats, [80, 0]);
        assert!(live.batch_remaining_ms[0] > 4_000);
        let paid = host.wait(|v| v.earned_sats == [80, 0]);
        assert!(String::from_utf8_lossy(&paid.payment_log).contains("alice@example.test"));
        assert_eq!(live.remaining_sats, [500, 420]);
        assert_eq!(live.phase, 3);
        assert_eq!(host.credit([0, 800]).earned_sats, [80, 0]);
        let end = host.action(7); // P2 wins: all 920 remaining sats go to P2.
        assert_eq!(end.earned_sats, [80, 920]);
        assert_eq!(end.phase, 4);
        assert_eq!(host.action(7).earned_sats, [80, 920]); // Duplicate end is harmless.
        let next = host.action(1);
        assert_eq!(next.phase, 1);
        assert_eq!(next.paid_sats, [0, 0]);
        assert_eq!(next.remaining_sats, [0, 0]);
        assert_eq!(next.earned_sats, [0, 0]);
        assert_ne!(next.invoices[0], initial.invoices[0]);
        assert_ne!(next.invoices[1], initial.invoices[1]);
        assert_eq!(host.action(4).phase, 1);
    }

    #[test]
    fn restart_keeps_paid_entries_and_payout_ids_without_reusing_winnings() {
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        host.fund();
        host.credit([0, 800]);
        drop(host);
        let host = Handle::open(dir.path());
        assert_eq!(host.credit([0, 800]).pending_sats, [80, 0]);
        assert_eq!(host.action(8).earned_sats, [580, 420]); // Return unspent contributions.
        drop(host);
        let host = Handle::open(dir.path());
        assert_eq!(host.action(1).paid_sats, [0, 0]);
    }

    #[test]
    fn recovered_unknown_outgoing_payment_is_looked_up_not_sent_twice() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut demo = Demo::open(dir.path()).await.unwrap();
            demo.saved.pot.confirm_entry(0, 500).unwrap();
            demo.saved.pot.confirm_entry(1, 500).unwrap();
            demo.saved.pot.start().unwrap();
            demo.saved.addresses = ["alice@example.test".into(), "bob@example.test".into()];
            demo.saved.pot.credit_at([0, 800], 0).unwrap();
            demo.saved.pot.flush_due(5_000);
            let payout = &demo.saved.pot.payouts[0];
            let address = parse_address(&demo.saved.addresses[0]).unwrap();
            let prepared = prepare_payout(
                demo.wallet.as_ref(),
                demo.provider.as_ref(),
                &address,
                payout.sats,
                &payout.id,
            )
            .await
            .unwrap();
            demo.saved.prepared.insert(payout.id.clone(), prepared);
            demo.saved.pot.begin_payout(0).unwrap();
            demo.save().unwrap(); // Crash before a conclusive outgoing acknowledgement.
            drop(demo);
            let mut recovered = Demo::open(dir.path()).await.unwrap();
            recovered.reconcile().await.unwrap();
            assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 0);
            assert_eq!(recovered.view().unwrap().pending_sats, [80, 0]);
            assert_eq!(recovered.view().unwrap().earned_sats, [0, 0]);
            recovered.command(Command::Action(7)).await.unwrap();
            recovered.reconcile().await.unwrap();
            assert_eq!(recovered.saved.pot.phase, Phase::Settling);
            assert_eq!(recovered.saved.pot.earned, [0, 920]);
            assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 1); // Only the new winner payout.
            assert!(recovered.command(Command::Action(1)).await.is_err());
        });
    }

    #[test]
    fn recipients_are_validated_persisted_and_locked_before_any_entry_is_paid() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut demo = Demo::open(dir.path()).await.unwrap();
            assert!(demo.command(Command::Action(2)).await.is_err());
            assert!(
                demo.command(Command::Addresses([
                    "bad".into(),
                    "bob@example.test".into()
                ]))
                .await
                .is_err()
            );
            demo.command(Command::Addresses([
                "alice@EXAMPLE.test".into(),
                "bob@example.test".into(),
            ]))
            .await
            .unwrap();
            assert_eq!(demo.saved.addresses[0], "alice@example.test");
            demo.command(Command::Action(2)).await.unwrap();
            demo.reconcile().await.unwrap();
            assert!(
                demo.command(Command::Addresses([
                    "eve@example.test".into(),
                    "bob@example.test".into()
                ]))
                .await
                .is_err()
            );
            drop(demo);
            let recovered = Demo::open(dir.path()).await.unwrap();
            assert_eq!(recovered.saved.addresses[0], "alice@example.test");
            assert_eq!(recovered.saved.pot.paid, [true, false]);
        });
    }

    #[test]
    fn match_end_coalesces_open_window_with_winners_remaining_pot() {
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        host.fund();
        let pending = host.credit([0, 800]);
        assert_eq!(pending.earned_sats, [0, 0]);
        let end = host.action(6);
        assert_eq!(end.earned_sats, [1000, 0]);
        let saved: Saved =
            serde_json::from_slice(&std::fs::read(dir.path().join("prepaid-match.json")).unwrap())
                .unwrap();
        assert_eq!(saved.pot.payouts.len(), 1);
        assert_eq!(saved.receipts.len(), 1);
        assert_eq!(saved.receipts[0].address, "alice@example.test");
        assert_eq!(saved.receipts[0].sats, 1000);
    }

    #[test]
    fn null_ffi_inputs_fail_without_unwinding() {
        unsafe {
            assert!(arena_local_create(std::ptr::null()).is_null());
            assert!(!arena_local_addresses(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null()
            ));
            assert!(!arena_local_credit(std::ptr::null(), 0, 800));
            assert!(!arena_local_snapshot(
                std::ptr::null(),
                std::ptr::null_mut()
            ));
            arena_local_destroy(std::ptr::null_mut());
        }
    }
}
