//! Fake-only local host. This has no network listener and cannot use an NWC key.
//! C++ only queues actions and polls a fixed-layout snapshot; all SQLite/payment
//! work runs on an owned background thread. See include/sats_arena_local.h.
use sats_arena_core::{
    Engine, Error, Result, Rules,
    protocol::{
        Deposit, DepositStatus, Lobby, LobbyPhase, LobbyRequest, MatchMode, MatchReport,
        PROTOCOL_VERSION, PlayerSlot, RegisterResponse,
    },
    wallet::{FakeWallet, InvoiceState, Wallet},
};
use serde::{Deserialize, Serialize};
use std::{
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
    pub padding: u32,
    pub revision: u64,
    pub budget_sats: u64,
    pub available_sats: [u64; 2],
    pub reserved_sats: [u64; 2],
    pub invoices: [[u8; 128]; 2],
    pub error: [u8; 256],
}

impl Default for LocalView {
    fn default() -> Self {
        Self {
            abi_version: 1,
            phase: 0,
            busy: 1,
            padding: 0,
            revision: 0,
            budget_sats: 500,
            available_sats: [0; 2],
            reserved_sats: [0; 2],
            invoices: [[0; 128]; 2],
            error: [0; 256],
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    players: [RegisterResponse; 2],
    lobby_id: String,
    deposits: [Option<Deposit>; 2],
}

struct Demo {
    engine: Engine,
    wallet: Arc<FakeWallet>,
    saved: Saved,
    path: PathBuf,
}

impl Demo {
    async fn open(folder: &Path) -> Result<Self> {
        std::fs::create_dir_all(folder).map_err(|e| Error::Internal(e.into()))?;
        let wallet = Arc::new(FakeWallet::default()); // Never auto-pay; the UI simulates each payment explicitly.
        let engine = Engine::open(
            &folder.join("fake.db"),
            Rules {
                sats_per_percent: 0,
                ..Rules::default()
            },
            wallet.clone(),
        )?;
        let path = folder.join("fake-players.json");
        let saved = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path).map_err(|e| Error::Internal(e.into()))?)?
        } else {
            Saved {
                players: [
                    engine.register("Player 1").await?,
                    engine.register("Player 2").await?,
                ],
                lobby_id: new_id(),
                deposits: [None, None],
            }
        };
        let mut demo = Self {
            engine,
            wallet,
            saved,
            path,
        };
        demo.save()?;
        demo.prepare().await?;
        Ok(demo)
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
        let temporary = self.path.with_extension("tmp");
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
        std::fs::rename(temporary, &self.path).map_err(|e| Error::Internal(e.into()))?;
        Ok(())
    }

    fn roster(&self) -> Vec<PlayerSlot> {
        self.saved
            .players
            .iter()
            .enumerate()
            .map(|(port, player)| PlayerSlot {
                port: port as u8,
                player_id: player.player_id.clone(),
            })
            .collect()
    }

    async fn lobby(&self) -> Result<Lobby> {
        self.engine
            .lobby(&self.saved.players[0].token, &self.saved.lobby_id)
            .await
    }

    async fn prepare(&mut self) -> Result<()> {
        let lobby = self
            .engine
            .create_lobby(
                &self.saved.players[0].token,
                LobbyRequest {
                    id: self.saved.lobby_id.clone(),
                    mode: MatchMode::Local,
                    players: self.roster(),
                    budget_sats: 500,
                },
            )
            .await?;
        if !matches!(lobby.phase, LobbyPhase::Funding | LobbyPhase::Ready) {
            return Ok(());
        }
        for i in 0..2 {
            let slot = &lobby.players[i];
            if slot.reserved_sats > 0 {
                continue;
            }
            if slot.available_sats >= lobby.budget_sats {
                self.engine
                    .ready_lobby(&self.saved.players[i].token, &self.saved.lobby_id)
                    .await?;
            } else if self.saved.deposits[i].is_none() {
                let invoice = self
                    .engine
                    .create_deposit(
                        &self.saved.players[i].token,
                        lobby.budget_sats - slot.available_sats,
                    )
                    .await?;
                self.saved.deposits[i] = Some(invoice);
                self.save()?;
            }
        }
        Ok(())
    }

    async fn action(&mut self, action: u32) -> Result<()> {
        match action {
            1 => {
                if !matches!(
                    self.lobby().await?.phase,
                    LobbyPhase::Settled | LobbyPhase::Cancelled
                ) {
                    return Err(Error::Rejected("finish or cancel this lobby first".into()));
                }
                self.saved.lobby_id = new_id();
                self.saved.deposits = [None, None];
                self.save()?;
                self.prepare().await?;
            }
            2 | 3 => {
                if self.lobby().await?.phase != LobbyPhase::Funding {
                    return Err(Error::Rejected("lobby is not awaiting payment".into()));
                }
                let i = (action - 2) as usize;
                if let Some(deposit) = &self.saved.deposits[i] {
                    let hash = self
                        .wallet
                        .decode_invoice(&deposit.bolt11)
                        .map_err(Error::Internal)?
                        .payment_hash;
                    self.wallet.set_invoice_state(
                        &hash,
                        InvoiceState::Paid {
                            amount_sats: deposit.amount_sats,
                        },
                    );
                    self.saved.deposits[i] = Some(
                        self.engine
                            .deposit(&self.saved.players[i].token, &deposit.id)
                            .await?,
                    );
                    self.save()?;
                }
                self.engine
                    .ready_lobby(&self.saved.players[i].token, &self.saved.lobby_id)
                    .await?;
            }
            4 => {
                self.engine
                    .start_lobby(&self.saved.players[0].token, &self.saved.lobby_id)
                    .await?;
            }
            5 => {
                self.engine
                    .cancel_lobby(&self.saved.players[0].token, &self.saved.lobby_id)
                    .await?;
            }
            6..=8 => {
                if self.lobby().await?.phase != LobbyPhase::Playing {
                    return Err(Error::Rejected("only a started session can finish".into()));
                }
                for i in 0..2 {
                    self.engine
                        .report_match(
                            &self.saved.players[i].token,
                            MatchReport {
                                protocol_version: PROTOCOL_VERSION,
                                match_id: self.saved.lobby_id.clone(),
                                mode: MatchMode::Local,
                                reporter_port: i as u8,
                                players: self.roster(),
                                hits: vec![],
                                winner_port: if action == 8 {
                                    None
                                } else {
                                    Some((action - 6) as u8)
                                },
                            },
                        )
                        .await?;
                }
            }
            _ => return Err(Error::Rejected("unknown local preview action".into())),
        }
        Ok(())
    }

    async fn view(&self) -> Result<LocalView> {
        let lobby = self.lobby().await?;
        let phase = match lobby.phase {
            LobbyPhase::Funding => 1,
            LobbyPhase::Ready => 2,
            LobbyPhase::Playing => 3,
            LobbyPhase::Settled => 4,
            LobbyPhase::Cancelled => 5,
            LobbyPhase::Disputed => 6,
        };
        let mut view = LocalView {
            phase,
            busy: 0,
            ..LocalView::default()
        };
        for (i, slot) in lobby.players.iter().enumerate() {
            view.available_sats[i] = slot.available_sats;
            view.reserved_sats[i] = slot.reserved_sats;
            if let Some(deposit) = &self.saved.deposits[i] {
                if deposit.status == DepositStatus::Pending {
                    text(&mut view.invoices[i], &deposit.bolt11);
                }
            }
        }
        Ok(view)
    }
}

fn new_id() -> String {
    format!("local-{:032x}", rand::random::<u128>())
}
fn text<const N: usize>(buffer: &mut [u8; N], value: &str) {
    buffer.fill(0);
    let bytes = value.as_bytes();
    let length = bytes.len().min(N - 1);
    buffer[..length].copy_from_slice(&bytes[..length]);
}

pub struct LocalHandle {
    sender: SyncSender<u32>,
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
                                .block_on(demo.action(action))
                                .err()
                                .map(|e| e.to_string())
                                .unwrap_or_default();
                            true
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                    };
                    if let Ok(mut view) = runtime.block_on(demo.view()) {
                        let mut previous = target.lock().unwrap();
                        // A command can arrive while an idle snapshot is being
                        // built. Only its processed acknowledgement may clear
                        // busy; otherwise the UI could mistake stale Ready for
                        // the response to its Start request.
                        if !processed_action && previous.phase != 0 {
                            view.busy = previous.busy;
                        }
                        view.revision = previous.revision + 1;
                        text(&mut view.error, &message);
                        *previous = view;
                    }
                }
                let _ = runtime.block_on(demo.engine.shutdown());
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
        if handle.sender.try_send(action).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Handle(*mut LocalHandle);
    impl Handle {
        fn open(path: &Path) -> Self {
            let path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            let handle = Self(unsafe { arena_local_create(path.as_ptr()) });
            assert!(!handle.0.is_null());
            handle.wait(|v| v.phase != 0 && v.busy == 0);
            handle
        }
        fn wait(&self, ready: impl Fn(&LocalView) -> bool) -> LocalView {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let mut view = LocalView::default();
                if unsafe { arena_local_snapshot(self.0, &mut view) } && ready(&view) {
                    assert_ne!(view.phase, 7);
                    return view;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "local worker failed to respond"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn action(&self, action: u32) -> LocalView {
            self.wait(|v| v.busy == 0);
            assert!(unsafe { arena_local_action(self.0, action) });
            self.wait(|v| v.busy == 0)
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                arena_local_destroy(self.0);
            }
        }
    }

    #[test]
    fn c_abi_full_funding_start_settlement_and_restart_flow() {
        assert_eq!(std::mem::size_of::<LocalView>(), 576);
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        assert_eq!(host.action(4).phase, 1); // Start is denied before payments.
        let one = host.action(2);
        assert_eq!(one.phase, 1);
        assert_eq!(one.reserved_sats, [500, 0]);
        assert_eq!(host.action(4).phase, 1);
        assert_eq!(host.action(3).phase, 2);
        assert_eq!(host.action(4).phase, 3);
        assert!(host.action(4).error[0] != 0); // Authorization cannot be reused.
        let settled = host.action(6);
        assert_eq!(settled.phase, 4);
        assert_eq!(settled.available_sats, [600, 400]);
        assert_eq!(settled.reserved_sats, [0, 0]);
        drop(host);
        let recovered = Handle::open(dir.path());
        assert_eq!(recovered.wait(|v| v.busy == 0).available_sats, [600, 400]);
        assert_eq!(recovered.action(1).phase, 1); // P1 reuses balance; P2 needs only 100.
    }

    #[test]
    fn ready_reservations_survive_restart_and_cancel_releases_them_once() {
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        host.action(2);
        assert_eq!(host.action(3).phase, 2);
        drop(host);
        let recovered = Handle::open(dir.path());
        assert_eq!(recovered.wait(|v| v.busy == 0).reserved_sats, [500, 500]);
        let cancelled = recovered.action(5);
        assert_eq!(cancelled.phase, 5);
        assert_eq!(cancelled.available_sats, [500, 500]);
        assert_eq!(recovered.action(5).available_sats, [500, 500]);
    }

    #[test]
    fn invalid_ffi_inputs_are_rejected_without_a_panic() {
        unsafe {
            assert!(arena_local_create(std::ptr::null()).is_null());
            assert!(!arena_local_action(std::ptr::null(), 4));
            assert!(!arena_local_snapshot(
                std::ptr::null(),
                std::ptr::null_mut()
            ));
            arena_local_destroy(std::ptr::null_mut());
        }
    }

    #[test]
    fn queued_actions_are_not_acknowledged_by_idle_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        host.action(2);
        assert_eq!(host.action(3).phase, 2);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(101));
            assert_eq!(host.action(4).phase, 3);
            assert_eq!(host.action(8).phase, 4);
            assert_eq!(host.action(1).phase, 2);
        }
    }
}
