//! Wallet-neutral local host for up to four funded humans and CPU participants.
//! Simulated wallet, live receiving verification, ordered damage and team settlement.
mod config;
mod wallet_check;

#[cfg(test)]
use sats_arena_core::wallet::prepare_payout;

use sats_arena_core::{
    Error, Result,
    arena_pot::{Credit, MatchPot},
    pot::{Outcome, Payout, PayoutState, Phase, Reason},
    wallet::{
        AddressInvoices, AddressVerification, AddressVerifier, FakeAddressInvoices, FakeWallet,
        InvoiceState, LightningAddress, PaymentState, PreparedPayout, Wallet,
        prepare_payout_with_comment,
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
    pub paid_sats: [u64; 4],
    pub remaining_sats: [u64; 4],
    pub earned_sats: [u64; 4],
    pub pending_sats: [u64; 4],
    pub invoices: [[u8; 2048]; 4],
    pub error: [u8; 256],
    pub addresses: [[u8; 128]; 4],
    pub batch_remaining_ms: [u64; 4],
    pub payment_log: [u8; 4096],
    pub verification_state: [u32; 4],
    pub min_sendable_msat: [u64; 4],
    pub max_sendable_msat: [u64; 4],
    pub verification_message: [[u8; 256]; 4],
    pub verification_checked_ms: [u64; 4],
    pub teams: u32,
    pub kinds: [u32; 4],
    pub team_ids: [u32; 4],
    pub characters: [u32; 4],
    pub roster_configured: u32,
    pub last_credit_sequence: u64,
    pub sandbox: u32,
}
impl Default for LocalView {
    fn default() -> Self {
        Self {
            abi_version: 6,
            phase: 0,
            busy: 1,
            simulated: 1,
            revision: 0,
            entry_sats: 500,
            sats_per_percent: 1,
            paid_sats: [0; 4],
            remaining_sats: [0; 4],
            earned_sats: [0; 4],
            pending_sats: [0; 4],
            invoices: [[0; 2048]; 4],
            error: [0; 256],
            addresses: [[0; 128]; 4],
            batch_remaining_ms: [0; 4],
            payment_log: [0; 4096],
            verification_state: [0; 4],
            min_sendable_msat: [0; 4],
            max_sendable_msat: [0; 4],
            verification_message: [[0; 256]; 4],
            verification_checked_ms: [0; 4],
            teams: 0,
            kinds: [0, 0, 3, 3],
            team_ids: [255; 4],
            characters: [255; 4],
            roster_configured: 0,
            last_credit_sequence: 0,
            sandbox: 0,
        }
    }
}

enum Command {
    Action(u32),
    Credit([u64; 2]),
    Addresses([String; 2]),
    Address(usize, String),
    Roster(Roster),
    Credits(Vec<Credit>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Roster {
    teams: bool,
    kinds: [u8; 4],
    team_ids: [u8; 4],
    characters: [u8; 4],
}
#[derive(Serialize, Deserialize)]
struct Saved {
    schema: u32,
    #[serde(default)]
    wallet_identity: Option<String>,
    #[serde(default)]
    invoice_addresses: [String; 4],
    #[serde(default)]
    retired_entries: Vec<RetiredEntry>,
    #[serde(default)]
    roster: Option<Roster>,
    #[serde(default)]
    next_roster: Option<Roster>,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    activity: Vec<Activity>,
    #[serde(default)]
    log_version: u32,
    pot: MatchPot,
    invoices: [String; 4],
    hashes: [String; 4],
    #[serde(default)]
    addresses: [String; 4],
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
    #[serde(default)]
    verification: [AddressCheck; 4],
}
#[derive(Serialize, Deserialize, Clone)]
struct RetiredEntry {
    incoming_hash: String,
    player: usize,
    address: String,
    expected_sats: u64,
    received_sats: Option<u64>,
    prepared: Option<PreparedPayout>,
    state: PayoutState,
}
#[derive(Serialize, Deserialize, Clone)]
struct Activity {
    at_ms: u64,
    message: String,
}
fn receipt_message(receipt: &Receipt, simulated: bool) -> String {
    let detail = if receipt.memo.is_empty() {
        &receipt.detail
    } else {
        receipt
            .memo
            .strip_prefix("Sats Melee — ")
            .unwrap_or(&receipt.memo)
    };
    format!(
        "{}Player {} received {} sats • {} → {}",
        if simulated { "SIMULATED " } else { "" },
        receipt.player + 1,
        receipt.sats,
        detail.to_lowercase(),
        receipt.address
    )
}

fn payout_memo(pot: &MatchPot, payout: &Payout) -> String {
    let victory = |player: usize| {
        if pot.teams {
            let team = match pot.team_ids[player] {
                0 => "Red",
                1 => "Blue",
                2 => "Green",
                _ => "Winning",
            };
            format!("{team} team victory")
        } else {
            "Match won".into()
        }
    };
    let reason = match payout.reason {
        Reason::Damage => "Damage payout".into(),
        Reason::Return => "Entry refunded".into(),
        Reason::WinnerPot => victory(payout.player),
        Reason::Settlement => match pot.outcome {
            Some(Outcome::NoContest) => "Entry refunded".into(),
            Some(Outcome::Winner(winner)) => {
                let same_side = |player: usize| {
                    if pot.teams {
                        pot.team_ids[player] == pot.team_ids[winner]
                    } else {
                        player == winner
                    }
                };
                let human_winners =
                    (0..4).any(|player| pot.kinds[player] == 0 && same_side(player));
                if !human_winners {
                    "Entry refunded".into()
                } else if same_side(payout.player) {
                    victory(payout.player)
                } else {
                    "Damage payout".into()
                }
            }
            None => "Match settlement".into(),
        },
    };
    format!("Sats Melee — {reason}")
}
#[derive(Serialize, Deserialize, Clone, Default)]
struct AddressCheck {
    report: Option<AddressVerification>,
    error: String,
}
#[derive(Serialize, Deserialize, Clone)]
struct Receipt {
    id: String,
    at_ms: u64,
    player: usize,
    address: String,
    sats: u64,
    detail: String,
    #[serde(default)]
    memo: String,
}
struct Demo {
    wallet: Arc<dyn Wallet>,
    provider: Arc<dyn AddressInvoices>,
    verifier: Arc<dyn AddressVerifier>,
    simulation: Option<Arc<FakeWallet>>,
    sandbox_players: Option<[config::SandboxPlayer; 4]>,
    saved: Saved,
    path: PathBuf,
    retired_cursor: usize,
    next_invoice_poll: std::time::Instant,
    next_retired_poll: std::time::Instant,
    _state_lock: std::fs::File,
}
impl Demo {
    async fn fresh(wallet: &dyn Wallet, simulated: bool) -> Result<Saved> {
        Self::fresh_for(wallet, simulated, None).await
    }
    async fn fresh_for(
        wallet: &dyn Wallet,
        simulated: bool,
        roster: Option<&Roster>,
    ) -> Result<Saved> {
        let setting = |name: &str, default: u64| -> Result<u64> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse()
                    .map_err(|_| Error::Rejected(format!("invalid {name}"))),
                Err(_) => Ok(default),
            }
        };
        let mut pot = MatchPot::new(
            format!("match-{:032x}", rand::random::<u128>()),
            setting("SATS_ARENA_ENTRY_SATS", 500)?,
            setting("SATS_ARENA_SATS_PER_PERCENT", 1)?,
        )?;
        if let Some(roster) = roster {
            pot.configure(roster.teams, roster.kinds, roster.team_ids)?;
        }
        let mut invoices = std::array::from_fn(|_| String::new());
        let mut hashes = std::array::from_fn(|_| String::new());
        for i in 0..4 {
            if !simulated || pot.kinds[i] != 0 {
                continue;
            }
            let invoice = wallet
                .create_invoice(pot.entry_sats, &format!("{} P{}", pot.id, i + 1), 600)
                .await?;
            invoices[i] = invoice.bolt11;
            hashes[i] = invoice.payment_hash;
        }
        Ok(Saved {
            schema: 3,
            wallet_identity: None,
            invoice_addresses: Default::default(),
            retired_entries: vec![],
            roster: None,
            next_roster: None,
            events: vec![],
            activity: vec![],
            log_version: 1,
            pot,
            invoices,
            hashes,
            addresses: std::array::from_fn(|_| String::new()),
            prepared: BTreeMap::new(),
            used_payment_hashes: BTreeSet::new(),
            receipts: vec![],
            simulated,
            verification: Default::default(),
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
            default_verifier(),
        )
        .await
    }
    async fn open_configured(
        fake_folder: &Path,
        live_folder: &Path,
        config: config::WalletConfig,
    ) -> Result<Self> {
        let (uri, sandbox_players) = match config {
            config::WalletConfig::Fake => return Self::open(fake_folder).await,
            config::WalletConfig::Nwc(uri) => (uri, None),
            config::WalletConfig::Sandbox(uri, players) => (uri, Some(players)),
        };
        {
            use sats_arena_core::wallet::{LnurlClient, NWC_WALLET_MANAGED_FEES, NwcWallet};
            if std::env::var("SATS_ARENA_MAX_PAYOUT_FEE_SATS")
                .ok()
                .is_some_and(|v| v.parse::<u64>().ok() != Some(NWC_WALLET_MANAGED_FEES))
            {
                return Err(Error::Rejected("NWC fees are controlled by the wallet connection; remove the unsupported app fee cap".into()));
            }
            let wallet =
                tokio::time::timeout(Duration::from_secs(30), NwcWallet::connect(uri.expose()))
                    .await
                    .map_err(|_| {
                        Error::Rejected(
                            "NWC connection timed out; check the launcher wallet and relay".into(),
                        )
                    })?
                    .map_err(|error| Error::Rejected(format!("NWC wallet: {error}")))?;
            let identity = wallet.identity();
            if identity.is_empty() || !identity.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Rejected("Invalid NWC wallet identity".into()));
            }
            let root = if sandbox_players.is_some() {
                live_folder
                    .parent()
                    .unwrap_or(live_folder)
                    .join("SatsArenaSandbox")
            } else {
                live_folder.to_path_buf()
            };
            let folder = root.join(&identity);
            std::fs::create_dir_all(&folder).map_err(|e| Error::Internal(e.into()))?;
            let mut demo = Self::open_with(
                &folder,
                Arc::new(wallet),
                Arc::new(LnurlClient),
                None,
                default_verifier(),
            )
            .await?;
            if demo
                .saved
                .wallet_identity
                .as_ref()
                .is_some_and(|old| old != &identity)
            {
                return Err(Error::Rejected(
                    "Stored match belongs to a different wallet connection".into(),
                ));
            }
            if demo.saved.wallet_identity.is_none()
                && (demo.saved.pot.paid.iter().any(|p| *p) || !demo.saved.prepared.is_empty())
            {
                return Err(Error::Rejected(
                    "Funded live state is missing its wallet identity".into(),
                ));
            }
            if let Some(players) = &sandbox_players {
                for (i, player) in players.iter().enumerate() {
                    if demo.saved.addresses[i].is_empty() {
                        demo.saved.addresses[i] = player.address.clone();
                    } else if demo.saved.addresses[i] != player.address {
                        return Err(Error::Rejected(
                            "Sandbox fixture changed; preserve the previous wallet state".into(),
                        ));
                    }
                }
            }
            demo.sandbox_players = sandbox_players;
            demo.saved.wallet_identity = Some(identity);
            demo.saved.max_fee_sats = NWC_WALLET_MANAGED_FEES;
            demo.save()?;
            Ok(demo)
        }
    }
    // No NWC/LND-specific behavior below this boundary.
    async fn open_with(
        folder: &Path,
        wallet: Arc<dyn Wallet>,
        provider: Arc<dyn AddressInvoices>,
        simulation: Option<Arc<FakeWallet>>,
        verifier: Arc<dyn AddressVerifier>,
    ) -> Result<Self> {
        std::fs::create_dir_all(folder).map_err(|e| Error::Internal(e.into()))?;
        let mut lock_options = std::fs::OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let state_lock = lock_options
            .open(folder.join(".arena.lock"))
            .map_err(|e| Error::Internal(e.into()))?;
        fs2::FileExt::try_lock_exclusive(&state_lock).map_err(|_| {
            Error::Rejected("This Arena wallet state is already open in another process".into())
        })?;
        let path = folder.join("prepaid-match.json");
        let mut saved: Saved = if path.exists() {
            let bytes = std::fs::read(&path).map_err(|e| Error::Internal(e.into()))?;
            let mut json: serde_json::Value = serde_json::from_slice(&bytes)?;
            // Recover the first preview's nested-Option outcome representation.
            if json["pot"]["outcome"].is_number() {
                json["pot"]["outcome"] = serde_json::json!({"Winner": json["pot"]["outcome"]});
            }
            if json["schema"].as_u64().is_some_and(|v| v < 3) {
                let mut fractional = [[0u8; 4]; 4];
                let rate = json["pot"]["sats_per_percent"].as_u64().unwrap_or(0);
                for victim in 0..2 {
                    let damage = json["pot"]["damage_centi"][victim].as_u64().unwrap_or(0);
                    fractional[1 - victim][victim] =
                        ((u128::from(damage) * u128::from(rate)) % 100) as u8;
                }
                json["pot"]["fractional_sats"] = serde_json::to_value(fractional)?;

                for (key, fill) in [
                    ("invoices", serde_json::json!("")),
                    ("hashes", serde_json::json!("")),
                    ("addresses", serde_json::json!("")),
                    (
                        "verification",
                        serde_json::json!({"report":null,"error":""}),
                    ),
                ] {
                    if let Some(values) = json[key].as_array_mut() {
                        while values.len() < 4 {
                            values.push(fill.clone());
                        }
                    }
                }
                for (key, fill) in [
                    ("paid", serde_json::json!(false)),
                    ("remaining", serde_json::json!(0)),
                    ("earned", serde_json::json!(0)),
                    ("damage_centi", serde_json::json!(0)),
                    ("unbatched", serde_json::json!(0)),
                    ("batch_deadline_ms", serde_json::Value::Null),
                ] {
                    if let Some(values) = json["pot"][key].as_array_mut() {
                        while values.len() < 4 {
                            values.push(fill.clone());
                        }
                    }
                }
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
        if !(1..=3).contains(&saved.schema) {
            return Err(Error::Rejected("unsupported prepaid match state".into()));
        }
        if saved.schema == 1 && saved.pot.paid.iter().any(|paid| *paid) {
            if simulation.is_none() {
                return Err(Error::Rejected(
                    "legacy fake state cannot use a real wallet".into(),
                ));
            }
            saved.addresses = [
                "player1@example.test".into(),
                "player2@example.test".into(),
                String::new(),
                String::new(),
            ];
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
                            memo: String::new(),
                        },
                    );
                }
            }
        }
        saved
            .used_payment_hashes
            .extend(saved.prepared.values().map(|p| p.payment_hash.clone()));
        // Activity is a per-application-session display. Financial receipts,
        // prepared invoices and payout states remain durable for recovery.
        saved.activity.clear();
        saved.events.clear();
        saved.log_version = 1;
        saved.schema = 3;
        let mut demo = Self {
            wallet,
            provider,
            verifier,
            simulation,
            sandbox_players: None,
            saved,
            path,
            retired_cursor: 0,
            next_invoice_poll: std::time::Instant::now(),
            next_retired_poll: std::time::Instant::now(),
            _state_lock: state_lock,
        };
        if demo.saved.pot.phase == Phase::Funding && !demo.saved.pot.paid.iter().any(|p| *p) {
            let setting = |name: &str, default: u64| -> Result<u64> {
                match std::env::var(name) {
                    Ok(value) => value
                        .parse()
                        .map_err(|_| Error::Rejected(format!("invalid {name}"))),
                    Err(_) => Ok(default),
                }
            };
            let entry = setting("SATS_ARENA_ENTRY_SATS", demo.saved.pot.entry_sats)?;
            let rate = setting("SATS_ARENA_SATS_PER_PERCENT", 1)?;
            demo.update_unfunded_settings(entry, rate).await?;
        }
        demo.save()?;
        Ok(demo)
    }
    async fn update_unfunded_settings(&mut self, entry: u64, rate: u64) -> Result<()> {
        // Never rewrite an agreement after even one player's entry was paid.
        if self.saved.pot.phase != Phase::Funding || self.saved.pot.paid.iter().any(|p| *p) {
            return Ok(());
        }
        MatchPot::new("settings".into(), entry, rate)?;
        if self.saved.pot.entry_sats == entry && self.saved.pot.sats_per_percent == rate {
            return Ok(());
        }
        // Retire at the old amount, so a late payment is returned to its original
        // recipient and cannot accidentally fund the changed match.
        self.retire_entries();
        self.save()?;
        self.saved.pot.entry_sats = entry;
        self.saved.pot.sats_per_percent = rate;
        if self.simulation.is_some() {
            for player in 0..4 {
                if self.saved.pot.kinds[player] == 0 {
                    let invoice = self
                        .wallet
                        .create_invoice(
                            entry,
                            &format!("{} P{}", self.saved.pot.id, player + 1),
                            600,
                        )
                        .await
                        .map_err(Error::Internal)?;
                    self.saved.invoices[player] = invoice.bolt11;
                    self.saved.hashes[player] = invoice.payment_hash;
                }
            }
        }
        self.save()
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
    fn record_activity(&mut self, at_ms: u64, message: String) {
        self.saved.activity.push(Activity { at_ms, message });
        if self.saved.activity.len() > 100 {
            self.saved.activity.remove(0);
        }
    }
    fn event(&mut self, message: String) {
        self.record_activity(now_ms(), message.clone());
        self.saved.events.push(message);
        if self.saved.events.len() > 30 {
            self.saved.events.remove(0);
        }
    }
    fn supports_report(&self, report: &AddressVerification) -> bool {
        let humans = self.saved.pot.kinds.iter().filter(|&&k| k == 0).count() as u64;
        report.min_msat <= 1000
            && report.max_msat
                >= self
                    .saved
                    .pot
                    .entry_sats
                    .saturating_mul(humans)
                    .saturating_mul(1000)
    }
    async fn next_match(&mut self, roster: Option<Roster>) -> Result<()> {
        self.retire_entries();
        self.save()?;
        let folder = self.path.parent().unwrap().join("matches");
        std::fs::create_dir_all(&folder).map_err(|e| Error::Internal(e.into()))?;
        self.persist(&folder.join(format!("{}.json", self.saved.pot.id)))?;
        let mut next = Self::fresh_for(
            self.wallet.as_ref(),
            self.simulation.is_some(),
            roster.as_ref(),
        )
        .await?;
        next.retired_entries = self.saved.retired_entries.clone();
        next.roster = roster;
        next.addresses = self.saved.addresses.clone();
        next.verification = self.saved.verification.clone();
        next.receipts = self.saved.receipts.clone();
        next.events = self.saved.events.clone();
        next.activity = self.saved.activity.clone();
        next.max_fee_sats = self.saved.max_fee_sats;
        next.wallet_identity = self.saved.wallet_identity.clone();
        next.used_payment_hashes = self.saved.used_payment_hashes.clone();
        self.saved = next;
        self.save()
    }
    async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Credits(credits) => self.saved.pot.credit_batch_at(&credits, now_ms())?,
            Command::Roster(roster) => {
                let mut validate = MatchPot::new(
                    "validate".into(),
                    self.saved.pot.entry_sats,
                    self.saved.pot.sats_per_percent,
                )?;
                validate.configure(roster.teams, roster.kinds, roster.team_ids)?;
                if roster
                    .kinds
                    .iter()
                    .zip(roster.characters)
                    .any(|(&kind, character)| kind < 2 && character >= 25)
                {
                    return Err(Error::Rejected("finish character selection first".into()));
                }
                if self.saved.roster.as_ref() == Some(&roster) {
                    return Ok(());
                }
                if self.saved.pot.phase == Phase::Playing {
                    return Err(Error::Rejected(
                        "finish the active match before changing its roster".into(),
                    ));
                }
                if self.saved.pot.phase == Phase::Settling {
                    self.saved.next_roster = Some(roster);
                } else if self.saved.pot.paid.iter().any(|paid| *paid)
                    && self.saved.pot.phase != Phase::Settled
                {
                    self.saved.next_roster = Some(roster);
                    self.saved.pot.finish(None)?;
                    self.event("Roster changed • returning paid entries".into());
                } else {
                    self.next_match(Some(roster)).await?;
                }
            }
            Command::Credit(totals) => self.saved.pot.credit_at(totals, now_ms())?,
            Command::Address(player, value) => {
                if player > 3 {
                    return Err(Error::Rejected("invalid player".into()));
                }
                let address = parse_address(&value)?;
                if self
                    .sandbox_players
                    .as_ref()
                    .is_some_and(|players| address.as_str() != players[player].address)
                {
                    return Err(Error::Rejected(
                        "Sandbox uses the prefilled player test addresses".into(),
                    ));
                }
                if !matches!(self.saved.pot.phase, Phase::Funding | Phase::Ready)
                    || (self.saved.pot.paid.iter().any(|paid| *paid)
                        && address.as_str() != self.saved.addresses[player])
                {
                    return Err(Error::Rejected(
                        "payout addresses are locked after entry payment".into(),
                    ));
                }
                if self.simulation.is_none() && address.as_str() != self.saved.addresses[player] {
                    self.retire_entries();
                }
                self.saved.addresses[player] = address.as_str().into();
                self.saved.verification[player] = AddressCheck::default();
                self.save()?;
                self.saved.verification[player] = match tokio::time::timeout(
                    Duration::from_secs(20),
                    self.verifier.verify(&address),
                )
                .await
                {
                    Ok(Ok(report)) => AddressCheck {
                        report: Some(report),
                        error: String::new(),
                    },
                    Ok(Err(error)) => AddressCheck {
                        report: None,
                        error: format!("{error:#}"),
                    },
                    Err(_) => AddressCheck {
                        report: None,
                        error: "Address verification timed out; try again".into(),
                    },
                };
            }
            Command::Addresses(addresses) => {
                let first = parse_address(&addresses[0])?;
                let second = parse_address(&addresses[1])?;
                let canonical = [first.as_str().into(), second.as_str().into()];
                if !matches!(self.saved.pot.phase, Phase::Funding | Phase::Ready)
                    || (self.saved.pot.paid.iter().any(|paid| *paid)
                        && canonical.as_slice() != &self.saved.addresses[..2])
                {
                    return Err(Error::Rejected(
                        "payout addresses are locked after entry payment".into(),
                    ));
                }
                if self.simulation.is_none() && canonical.as_slice() != &self.saved.addresses[..2] {
                    self.retire_entries();
                }
                self.saved.addresses[..2].clone_from_slice(&canonical);
                self.saved.verification[0] = AddressCheck::default();
                self.saved.verification[1] = AddressCheck::default();
                self.save()?; // Never leave old verification attached to edited addresses.
                let verifier = self.verifier.clone();
                let check = |address: LightningAddress| {
                    let verifier = verifier.clone();
                    async move {
                        match tokio::time::timeout(
                            Duration::from_secs(20),
                            verifier.verify(&address),
                        )
                        .await
                        {
                            Ok(Ok(report)) => AddressCheck {
                                report: Some(report),
                                error: String::new(),
                            },
                            Ok(Err(error)) => AddressCheck {
                                report: None,
                                error: format!("{error:#}"),
                            },
                            Err(_) => AddressCheck {
                                report: None,
                                error: "Address verification timed out; try again".into(),
                            },
                        }
                    }
                };
                let (first, second) = tokio::join!(check(first), check(second));
                self.saved.verification[0] = first;
                self.saved.verification[1] = second;
            }
            Command::Action(action) => match action {
                1 => {
                    if self.saved.pot.phase != Phase::Settled {
                        return Err(Error::Rejected(
                            "previous match payouts are not complete".into(),
                        ));
                    }
                    let roster = self.saved.roster.clone();
                    self.next_match(roster).await?;
                }
                2 | 3 | 9 | 10 => {
                    if self.saved.pot.phase != Phase::Funding {
                        return Err(Error::Rejected(
                            "match is not awaiting entry payment".into(),
                        ));
                    }
                    let player = if action < 4 {
                        (action - 2) as usize
                    } else {
                        (action - 7) as usize
                    };
                    self.require_addresses()?;
                    if self.saved.pot.kinds[player] != 0 {
                        return Err(Error::Rejected("only humans fund entries".into()));
                    }
                    if let Some(players) = &self.sandbox_players {
                        use sats_arena_core::wallet::{NWC_WALLET_MANAGED_FEES, NwcWallet};
                        let invoice = &self.saved.invoices[player];
                        if invoice.is_empty() {
                            return Err(Error::Rejected("Wait for the entry invoice".into()));
                        }
                        let payer = NwcWallet::connect(players[player].uri.expose())
                            .await
                            .map_err(Error::Internal)?;
                        let payment = payer.pay_invoice(invoice, NWC_WALLET_MANAGED_FEES).await;
                        payer.shutdown().await;
                        match payment.map_err(Error::Internal)? {
                            PaymentState::Failed { reason } => return Err(Error::Rejected(reason)),
                            _ => self.event(format!(
                                "Sandbox Player {} • entry payment sent",
                                player + 1
                            )),
                        }
                        // Admission only follows the arena wallet's normal incoming receipt lookup.
                    } else {
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
                }
                4 => {
                    self.require_addresses()?;
                    self.saved.pot.start()?;
                }
                5 | 8 => {
                    let previous = self.saved.pot.outcome;
                    self.saved.pot.finish(None)?;
                    if previous.is_none() {
                        self.event("No contest • unspent entries returned".into());
                    }
                }
                6 | 7 | 11 | 12 => {
                    let player = if action < 8 {
                        (action - 6) as usize
                    } else {
                        (action - 9) as usize
                    };
                    let previous = self.saved.pot.outcome;
                    self.saved.pot.finish(Some(player))?;
                    if previous.is_none() {
                        let name = if self.saved.pot.teams {
                            format!(
                                "{} Team",
                                ["Red", "Blue", "Green"][self.saved.pot.team_ids[player] as usize]
                            )
                        } else {
                            format!("Player {}", player + 1)
                        };
                        self.event(format!("{name} wins • settlement"));
                    }
                }
                _ => return Err(Error::Rejected("unknown prepaid match action".into())),
            },
        }
        self.save()?;
        Ok(())
    }
    fn require_addresses(&self) -> Result<()> {
        if self
            .saved
            .retired_entries
            .iter()
            .any(|entry| entry.received_sats.is_some() && entry.state != PayoutState::Paid)
        {
            return Err(Error::Rejected(
                "A previous entry return is still pending".into(),
            ));
        }
        for (player, address) in self.saved.addresses.iter().enumerate() {
            if self.saved.pot.kinds[player] != 0 {
                continue;
            }
            parse_address(address)?;
            let report = self.saved.verification[player]
                .report
                .as_ref()
                .ok_or_else(|| {
                    Error::Rejected(format!(
                        "Verify Player {} receiving address first",
                        player + 1
                    ))
                })?;
            if report.address != *address
                || !report.fresh(now_ms())
                || !self.supports_report(report)
            {
                return Err(Error::Rejected(format!(
                    "Player {} needs current, compatible address verification",
                    player + 1
                )));
            }
        }
        Ok(())
    }
    fn retire_entry(&mut self, player: usize, received_sats: Option<u64>) {
        let hash = std::mem::take(&mut self.saved.hashes[player]);
        self.saved.invoices[player].clear();
        let address = std::mem::take(&mut self.saved.invoice_addresses[player]);
        if hash.is_empty() {
            return;
        }
        if let Some(previous) = self
            .saved
            .retired_entries
            .iter_mut()
            .find(|e| e.incoming_hash == hash)
        {
            if received_sats.is_some() {
                previous.received_sats = received_sats;
            }
            return;
        }
        self.saved.retired_entries.push(RetiredEntry {
            incoming_hash: hash,
            player,
            address: if address.is_empty() {
                self.saved.addresses[player].clone()
            } else {
                address
            },
            expected_sats: self.saved.pot.entry_sats,
            received_sats,
            prepared: None,
            state: PayoutState::Created,
        });
    }
    fn retire_entries(&mut self) {
        for player in 0..4 {
            if !self.saved.pot.paid[player] {
                self.retire_entry(player, None);
            }
        }
    }
    async fn ensure_entry_invoices(&mut self) -> Result<()> {
        if self.simulation.is_some()
            || self.saved.pot.phase != Phase::Funding
            || self.require_addresses().is_err()
        {
            return Ok(());
        }
        for player in 0..4 {
            if self.saved.pot.kinds[player] != 0
                || self.saved.pot.paid[player]
                || !self.saved.hashes[player].is_empty()
            {
                continue;
            }
            let invoice = tokio::time::timeout(
                Duration::from_secs(15),
                self.wallet.create_invoice(
                    self.saved.pot.entry_sats,
                    &format!("{} Player {}", self.saved.pot.id, player + 1),
                    600,
                ),
            )
            .await
            .map_err(|_| Error::WalletUnavailable)??;
            self.saved.invoices[player] = invoice.bolt11;
            self.saved.hashes[player] = invoice.payment_hash;
            self.saved.invoice_addresses[player] = self.saved.addresses[player].clone();
            self.save()?;
        }
        Ok(())
    }
    async fn reconcile_retired(&mut self) -> Result<()> {
        let count = self.saved.retired_entries.len();
        if count == 0 {
            return Ok(());
        }
        if self.simulation.is_some() || std::time::Instant::now() >= self.next_retired_poll {
            self.next_retired_poll = std::time::Instant::now() + Duration::from_secs(2);
            for _ in 0..count.min(4) {
                let index = self.retired_cursor % count;
                self.retired_cursor = (self.retired_cursor + 1) % count;
                let entry = self.saved.retired_entries[index].clone();
                if entry.received_sats.is_some() || entry.state == PayoutState::Paid {
                    continue;
                }
                if let Ok(InvoiceState::Paid { amount_sats }) =
                    self.wallet.invoice_state(&entry.incoming_hash).await
                {
                    self.saved.retired_entries[index].received_sats = Some(amount_sats);
                    self.event(format!(
                        "Late Player {} invoice paid — {} sats; returning to original address",
                        entry.player + 1,
                        amount_sats
                    ));
                    self.save()?;
                }
                // Expired/unknown entries remain monitored: settlement can race expiry.
            }
        }
        for index in 0..count {
            let entry = self.saved.retired_entries[index].clone();
            let Some(sats) = entry.received_sats else {
                continue;
            };
            if entry.state == PayoutState::Paid {
                continue;
            }
            if sats == 0 {
                self.saved.retired_entries[index].state = PayoutState::Paid;
                self.save()?;
                continue;
            }
            if entry.state == PayoutState::Failed {
                return Err(Error::Rejected(
                    "A late entry return failed and needs attention".into(),
                ));
            }
            let id = format!("late-entry-{}", entry.incoming_hash);
            if entry.prepared.is_none() {
                if entry.state == PayoutState::InFlight {
                    return Err(Error::Rejected(
                        "Missing prepared invoice for a late return".into(),
                    ));
                }
                let address = parse_address(&entry.address).map_err(|_| {
                    Error::Rejected(
                        "Late entry needs its original receiving address restored".into(),
                    )
                })?;
                let prepared = tokio::time::timeout(
                    Duration::from_secs(15),
                    prepare_payout_with_comment(
                        self.wallet.as_ref(),
                        self.provider.as_ref(),
                        &address,
                        sats,
                        &id,
                        "Sats Melee — Entry refunded",
                    ),
                )
                .await
                .map_err(|_| Error::WalletUnavailable)??;
                if !self
                    .saved
                    .used_payment_hashes
                    .insert(prepared.payment_hash.clone())
                {
                    return Err(Error::Rejected(
                        "Recipient reused a payment hash for a late entry return".into(),
                    ));
                }
                self.saved.retired_entries[index].prepared = Some(prepared);
                self.save()?;
            }
            let prepared = self.saved.retired_entries[index].prepared.clone().unwrap();
            let state = if entry.state == PayoutState::Created {
                self.saved.retired_entries[index].state = PayoutState::InFlight;
                self.save()?;
                match tokio::time::timeout(
                    Duration::from_secs(15),
                    self.wallet
                        .pay_invoice(&prepared.invoice, self.saved.max_fee_sats),
                )
                .await
                {
                    Ok(Ok(state)) => state,
                    _ => PaymentState::Pending,
                }
            } else {
                tokio::time::timeout(
                    Duration::from_secs(15),
                    self.wallet.payment_state(&prepared.payment_hash),
                )
                .await
                .map_err(|_| Error::WalletUnavailable)??
            };
            match state {
                PaymentState::Succeeded { .. } => {
                    let at_ms = now_ms();
                    self.saved.retired_entries[index].state = PayoutState::Paid;
                    self.saved.retired_entries[index]
                        .prepared
                        .as_mut()
                        .unwrap()
                        .confirmed_at_ms = Some(at_ms);
                    let receipt = Receipt {
                        id,
                        at_ms,
                        player: entry.player,
                        address: entry.address,
                        sats,
                        detail: "Late entry return".into(),
                        memo: if prepared.memo.is_empty() {
                            "Sats Melee — Entry refunded".into()
                        } else {
                            prepared.memo.clone()
                        },
                    };
                    let message = receipt_message(&receipt, self.saved.simulated);
                    self.saved.receipts.push(receipt);
                    if self.saved.receipts.len() > 100 {
                        self.saved.receipts.remove(0);
                    }
                    self.record_activity(at_ms, message);
                    self.save()?;
                }
                PaymentState::Pending => (),
                PaymentState::Failed { .. } => {
                    self.saved.retired_entries[index].state = PayoutState::Failed;
                    self.save()?;
                    return Err(Error::Rejected(
                        "A late entry return failed and needs attention".into(),
                    ));
                }
            }
        }
        Ok(())
    }
    async fn reconcile(&mut self) -> Result<()> {
        self.ensure_entry_invoices().await?;
        let poll_entries =
            self.simulation.is_some() || std::time::Instant::now() >= self.next_invoice_poll;
        if poll_entries {
            self.next_invoice_poll = std::time::Instant::now() + Duration::from_secs(2);
        }
        let previous_count = self.saved.pot.payouts.len();
        self.saved.pot.flush_due(now_ms());
        if previous_count != self.saved.pot.payouts.len() {
            self.save()?;
        }
        for player in 0..4 {
            if self.saved.pot.kinds[player] == 0
                && !self.saved.pot.paid[player]
                && self.saved.pot.phase == Phase::Funding
                && !self.saved.hashes[player].is_empty()
                && poll_entries
            {
                match self
                    .wallet
                    .invoice_state(&self.saved.hashes[player])
                    .await?
                {
                    InvoiceState::Paid { amount_sats } => {
                        // Account for received money before any verification or amount gate.
                        if amount_sats != self.saved.pot.entry_sats
                            || (!self.saved.invoice_addresses[player].is_empty()
                                && self.saved.invoice_addresses[player]
                                    != self.saved.addresses[player])
                        {
                            self.retire_entry(player, Some(amount_sats));
                            self.save()?;
                            continue;
                        }
                        self.saved.pot.confirm_entry(player, amount_sats)?;
                        self.event(format!(
                            "Player {} Invoice Paid — {} sats",
                            player + 1,
                            amount_sats
                        ));
                        self.save()?;
                    }
                    InvoiceState::Pending => (),
                    InvoiceState::Expired => {
                        self.retire_entry(player, None);
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
                    prepare_payout_with_comment(
                        self.wallet.as_ref(),
                        self.provider.as_ref(),
                        &address,
                        payout.sats,
                        &payout.id,
                        &payout_memo(&self.saved.pot, &payout),
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
                        memo: if prepared.memo.is_empty() {
                            payout_memo(&self.saved.pot, &payout)
                        } else {
                            prepared.memo.clone()
                        },
                    });
                    let message =
                        receipt_message(self.saved.receipts.last().unwrap(), self.saved.simulated);
                    self.record_activity(at_ms, message);
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
        self.reconcile_retired().await?;
        self.ensure_entry_invoices().await?;
        if self.saved.pot.phase == Phase::Settled {
            if let Some(roster) = self.saved.next_roster.clone() {
                self.next_match(Some(roster)).await?;
            }
        }
        Ok(())
    }
    fn view(&self) -> Result<LocalView> {
        let pot = &self.saved.pot;
        let mut view = LocalView {
            phase: if self
                .saved
                .retired_entries
                .iter()
                .any(|entry| entry.received_sats.is_some() && entry.state != PayoutState::Paid)
            {
                6
            } else {
                match pot.phase {
                    Phase::Funding => 1,
                    Phase::Ready => 2,
                    Phase::Playing => 3,
                    Phase::Settling => 6,
                    Phase::Settled => 4,
                }
            },
            busy: 0,
            simulated: u32::from(self.simulation.is_some()),
            sandbox: u32::from(self.sandbox_players.is_some()),
            entry_sats: pot.entry_sats,
            sats_per_percent: pot.sats_per_percent,
            paid_sats: pot.paid.map(|paid| if paid { pot.entry_sats } else { 0 }),
            remaining_sats: pot.remaining,
            earned_sats: pot.earned,
            pending_sats: pot.pending(),
            teams: u32::from(pot.teams),
            kinds: pot.kinds.map(u32::from),
            team_ids: pot.team_ids.map(u32::from),
            characters: self
                .saved
                .roster
                .as_ref()
                .map(|r| r.characters.map(u32::from))
                .unwrap_or([255; 4]),
            roster_configured: u32::from(self.saved.roster.is_some()),
            last_credit_sequence: pot.last_credit_sequence,
            ..LocalView::default()
        };
        for i in 0..4 {
            text(&mut view.invoices[i], &self.saved.invoices[i]);
            text(&mut view.addresses[i], &self.saved.addresses[i]);
            let check = &self.saved.verification[i];
            if let Some(report) = &check.report {
                view.min_sendable_msat[i] = report.min_msat;
                view.max_sendable_msat[i] = report.max_msat;
                view.verification_checked_ms[i] = report.checked_at_ms;
                view.verification_state[i] = if report.address != self.saved.addresses[i] {
                    0
                } else if !report.fresh(now_ms()) {
                    4
                } else if !self.supports_report(report) {
                    5
                } else {
                    2
                };
                let message = match view.verification_state[i] {
                    2 => "Receiver and unpaid invoice checked; no payment sent",
                    4 => "Verification expired; verify again before funding",
                    5 => "Receiver limits do not cover 1-sat batches and this match pot",
                    _ => "Verify this receiving address",
                };
                text(&mut view.verification_message[i], message);
            } else if !check.error.is_empty() {
                view.verification_state[i] = 3;
                text(&mut view.verification_message[i], &check.error);
            }
            view.batch_remaining_ms[i] = pot.batch_deadline_ms[i]
                .map(|d| d.saturating_sub(now_ms()))
                .unwrap_or(0);
        }
        let mut log = String::new();
        for entry in self.saved.activity.iter().rev().take(18).rev() {
            let seconds = entry.at_ms / 1000;
            log.push_str(&format!(
                "{:02}:{:02}:{:02}Z {}\n",
                (seconds / 3600) % 24,
                (seconds / 60) % 60,
                seconds % 60,
                entry.message
            ));
        }
        text(&mut view.payment_log, &log);
        Ok(view)
    }
}
#[cfg(not(test))]
fn default_verifier() -> Arc<dyn AddressVerifier> {
    Arc::new(sats_arena_core::wallet::LnurlClient)
}
#[cfg(test)]
fn default_verifier() -> Arc<dyn AddressVerifier> {
    Arc::new(TestVerifier)
}
#[cfg(test)]
struct TestVerifier;
#[cfg(test)]
#[async_trait::async_trait]
impl AddressVerifier for TestVerifier {
    async fn verify(&self, address: &LightningAddress) -> anyhow::Result<AddressVerification> {
        anyhow::ensure!(
            !address.as_str().starts_with("missing@"),
            "receiving address does not exist"
        );
        Ok(AddressVerification {
            address: address.as_str().into(),
            min_msat: 1000,
            max_msat: 1_000_000_000,
            callback_host: "fixture.invalid".into(),
            checked_at_ms: now_ms(),
            test_invoice_msat: 1000,
            description_binding: "test_fixture".into(),
            test_payment_hash: "fixture-unpaid".into(),
            invoice_expires_at_ms: now_ms() + 600_000,
        })
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
    unsafe { arena_local_create_configured(folder, folder) }
}

/// # Safety
/// Both folders are valid NUL-terminated UTF-8 paths. Wallet configuration is
/// consumed by the background worker, so no wallet work blocks the GUI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_create_configured(
    folder: *const c_char,
    live_folder: *const c_char,
) -> *mut LocalHandle {
    ffi_guard(|| {
        if folder.is_null() || live_folder.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(folder) = (unsafe { CStr::from_ptr(folder) }).to_str() else {
            return std::ptr::null_mut();
        };
        let Ok(live_folder) = (unsafe { CStr::from_ptr(live_folder) }).to_str() else {
            return std::ptr::null_mut();
        };
        let live_folder = PathBuf::from(live_folder);
        let folder = PathBuf::from(folder);
        let shared = Arc::new(Mutex::new(LocalView::default()));
        let target = shared.clone();
        let (sender, receiver) = mpsc::sync_channel(16);
        let worker = std::thread::Builder::new()
            .name("arena-local-wallet".into())
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
                // Unit tests inject fake wallets; production reads the main-process
                // handoff or an explicit development environment override.
                #[cfg(test)]
                let wallet_config: Result<config::WalletConfig> = Ok(config::WalletConfig::Fake);
                #[cfg(not(test))]
                let wallet_config = config::load();
                let selected_live = matches!(
                    &wallet_config,
                    Ok(config::WalletConfig::Nwc(_) | config::WalletConfig::Sandbox(_, _))
                );
                let selected_sandbox =
                    matches!(&wallet_config, Ok(config::WalletConfig::Sandbox(_, _)));
                let opening =
                    async { Demo::open_configured(&folder, &live_folder, wallet_config?).await };
                let mut demo = match runtime.block_on(opening) {
                    Ok(value) => value,
                    Err(error) => {
                        let mut view = target.lock().unwrap();
                        view.simulated = u32::from(!selected_live);
                        view.sandbox = u32::from(selected_sandbox);
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
        if handle.is_null() || !(1..=12).contains(&action) {
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LocalCredit {
    pub sequence: u64,
    pub damage_centi: u64,
    pub attacker: u32,
    pub victim: u32,
}
fn enqueue(handle: &LocalHandle, command: Command) -> bool {
    let Ok(mut view) = handle.shared.try_lock() else {
        return false;
    };
    if view.busy != 0 || handle.sender.try_send(command).is_err() {
        return false;
    }
    view.busy = 1;
    true
}
/// # Safety
/// Handle is live; each array points to four readable u32 values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_roster(
    handle: *const LocalHandle,
    teams: u32,
    kinds: *const u32,
    team_ids: *const u32,
    characters: *const u32,
) -> bool {
    ffi_guard(|| {
        if handle.is_null()
            || kinds.is_null()
            || team_ids.is_null()
            || characters.is_null()
            || teams > 1
        {
            return false;
        }
        let read = |ptr: *const u32| -> Option<[u8; 4]> {
            let slice = unsafe { std::slice::from_raw_parts(ptr, 4) };
            let mut out = [0; 4];
            for i in 0..4 {
                out[i] = u8::try_from(slice[i]).ok()?;
            }
            Some(out)
        };
        let (Some(kinds), Some(team_ids), Some(characters)) =
            (read(kinds), read(team_ids), read(characters))
        else {
            return false;
        };
        enqueue(
            unsafe { &*handle },
            Command::Roster(Roster {
                teams: teams != 0,
                kinds,
                team_ids,
                characters,
            }),
        )
    })
    .unwrap_or(false)
}
/// # Safety
/// Handle is live; records points to count readable LocalCredit records.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_credits(
    handle: *const LocalHandle,
    records: *const LocalCredit,
    count: u32,
) -> bool {
    ffi_guard(|| {
        if handle.is_null() || records.is_null() || count == 0 || count > 4096 {
            return false;
        }
        let records = unsafe { std::slice::from_raw_parts(records, count as usize) };
        let credits = records
            .iter()
            .map(|c| Credit {
                sequence: c.sequence,
                attacker: c.attacker as usize,
                victim: c.victim as usize,
                damage_centi: c.damage_centi,
            })
            .collect();
        enqueue(unsafe { &*handle }, Command::Credits(credits))
    })
    .unwrap_or(false)
}

/// # Safety
/// Address is a valid NUL-terminated UTF-8 string. Handle is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arena_local_verify_address(
    handle: *const LocalHandle,
    player: u32,
    address: *const c_char,
) -> bool {
    ffi_guard(|| {
        if handle.is_null() || address.is_null() || player > 3 {
            return false;
        }
        let Ok(address) = (unsafe { CStr::from_ptr(address) }).to_str() else {
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
            .try_send(Command::Address(player as usize, address.into()))
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
    #[test]
    fn changed_unfunded_settings_retire_old_invoice_amounts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut demo = Demo::open(dir.path()).await.unwrap();
            let old_hash = demo.saved.hashes[0].clone();
            demo.saved.addresses[0] = "alice@example.test".into();
            demo.update_unfunded_settings(400, 2).await.unwrap();
            assert_eq!(demo.saved.pot.entry_sats, 400);
            assert_eq!(demo.saved.pot.sats_per_percent, 2);
            let old = demo
                .saved
                .retired_entries
                .iter()
                .find(|e| e.incoming_hash == old_hash)
                .unwrap();
            assert_eq!(old.expected_sats, 500);
            assert_eq!(old.address, "alice@example.test");
            assert_ne!(demo.saved.hashes[0], old_hash);
            assert_eq!(
                demo.wallet
                    .decode_invoice(&demo.saved.invoices[0])
                    .unwrap()
                    .amount_sats,
                Some(400)
            );
        });
    }
    #[test]
    fn changed_settings_preserve_a_partially_funded_agreement() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut demo = Demo::open(dir.path()).await.unwrap();
            demo.saved.pot.confirm_entry(0, 500).unwrap();
            let invoices = demo.saved.invoices.clone();
            demo.update_unfunded_settings(400, 2).await.unwrap();
            assert_eq!(demo.saved.pot.entry_sats, 500);
            assert_eq!(demo.saved.pot.sats_per_percent, 1);
            assert_eq!(demo.saved.invoices, invoices);
            assert!(demo.saved.retired_entries.is_empty());
        });
    }
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
            assert_eq!(self.action(2).paid_sats, [500, 0, 0, 0]);
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
        assert_eq!(std::mem::size_of::<LocalView>(), 14464);
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        let initial = host.wait(|v| v.busy == 0);
        assert!(initial.invoices[..2].iter().all(|v| v[0] != 0));
        assert_ne!(initial.invoices[0], initial.invoices[1]);
        host.fund();
        let live = host.credit([0, 800]);
        assert_eq!(live.earned_sats, [0, 0, 0, 0]);
        assert_eq!(live.pending_sats, [8, 0, 0, 0]);
        assert!(live.batch_remaining_ms[0] > 4_000);
        let paid = host.wait(|v| v.earned_sats == [8, 0, 0, 0]);
        assert!(String::from_utf8_lossy(&paid.payment_log).contains("alice@example.test"));
        assert_eq!(live.remaining_sats, [500, 492, 0, 0]);
        assert_eq!(live.phase, 3);
        assert_eq!(host.credit([0, 800]).earned_sats, [8, 0, 0, 0]);
        let end = host.action(7); // P2 wins: all 992 remaining sats go to P2.
        assert_eq!(end.earned_sats, [8, 992, 0, 0]);
        assert_eq!(end.phase, 4);
        assert_eq!(host.action(7).earned_sats, [8, 992, 0, 0]); // Duplicate end is harmless.
        let next = host.action(1);
        assert_eq!(next.phase, 1);
        assert_eq!(next.paid_sats, [0, 0, 0, 0]);
        assert_eq!(next.remaining_sats, [0, 0, 0, 0]);
        assert_eq!(next.earned_sats, [0, 0, 0, 0]);
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
        assert_eq!(host.credit([0, 800]).pending_sats, [8, 0, 0, 0]);
        assert_eq!(host.action(8).earned_sats, [508, 492, 0, 0]); // Return unspent contributions.
        drop(host);
        let host = Handle::open(dir.path());
        assert_eq!(host.action(1).paid_sats, [0, 0, 0, 0]);
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
            demo.saved.addresses = [
                "alice@example.test".into(),
                "bob@example.test".into(),
                String::new(),
                String::new(),
            ];
            demo.saved.pot.credit_at([0, 800], 0).unwrap();
            demo.saved.pot.flush_due(5_000);
            let payout = &demo.saved.pot.payouts[0];
            let address = parse_address(&demo.saved.addresses[0]).unwrap();
            let prepared = prepare_payout_with_comment(
                demo.wallet.as_ref(),
                demo.provider.as_ref(),
                &address,
                payout.sats,
                &payout.id,
                "Sats Melee — Damage payout",
            )
            .await
            .unwrap();
            demo.saved.prepared.insert(payout.id.clone(), prepared);
            demo.saved.pot.begin_payout(0).unwrap();
            demo.save().unwrap(); // Crash before a conclusive outgoing acknowledgement.
            drop(demo);
            let mut recovered = Demo::open(dir.path()).await.unwrap();
            assert_eq!(
                recovered.saved.prepared.values().next().unwrap().memo,
                "Sats Melee — Damage payout"
            );
            recovered.reconcile().await.unwrap();
            assert_eq!(recovered.simulation.as_ref().unwrap().payment_attempts(), 0);
            assert_eq!(recovered.view().unwrap().pending_sats, [8, 0, 0, 0]);
            assert_eq!(recovered.view().unwrap().earned_sats, [0, 0, 0, 0]);
            recovered.command(Command::Action(7)).await.unwrap();
            recovered.reconcile().await.unwrap();
            assert_eq!(recovered.saved.pot.phase, Phase::Settling);
            assert_eq!(recovered.saved.pot.earned, [0, 992, 0, 0]);
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
            assert_eq!(recovered.saved.pot.paid, [true, false, false, false]);
        });
    }

    #[test]
    fn match_end_coalesces_open_window_with_winners_remaining_pot() {
        let dir = tempfile::tempdir().unwrap();
        let host = Handle::open(dir.path());
        host.fund();
        let pending = host.credit([0, 800]);
        assert_eq!(pending.earned_sats, [0, 0, 0, 0]);
        let end = host.action(6);
        assert_eq!(end.earned_sats, [1000, 0, 0, 0]);
        let saved: Saved =
            serde_json::from_slice(&std::fs::read(dir.path().join("prepaid-match.json")).unwrap())
                .unwrap();
        assert_eq!(saved.pot.payouts.len(), 1);
        assert_eq!(saved.receipts.len(), 1);
        assert_eq!(saved.receipts[0].address, "alice@example.test");
        assert_eq!(saved.receipts[0].sats, 1000);
    }

    #[test]
    fn individual_checks_preserve_other_player_and_gate_start() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let mut demo = Demo::open(dir.path()).await.unwrap();
                demo.command(Command::Address(0, "alice@example.test".into()))
                    .await
                    .unwrap();
                assert_eq!(demo.view().unwrap().verification_state, [2, 0, 0, 0]);
                assert!(demo.command(Command::Action(4)).await.is_err());
                demo.command(Command::Address(1, "missing@example.test".into()))
                    .await
                    .unwrap();
                assert_eq!(demo.view().unwrap().verification_state, [2, 3, 0, 0]);
                assert!(demo.command(Command::Action(4)).await.is_err());
                demo.command(Command::Address(1, "bob@example.test".into()))
                    .await
                    .unwrap();
                assert_eq!(demo.view().unwrap().verification_state, [2, 2, 0, 0]);
                assert_eq!(demo.saved.addresses[0], "alice@example.test");
                demo.command(Command::Action(2)).await.unwrap();
                demo.command(Command::Action(3)).await.unwrap();
                demo.reconcile().await.unwrap();
                demo.command(Command::Action(4)).await.unwrap();
            });
    }

    #[test]
    fn failed_stale_and_incompatible_verification_block_funding() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut demo = Demo::open(dir.path()).await.unwrap();
            demo.command(Command::Addresses([
                "missing@example.test".into(),
                "bob@example.test".into(),
            ]))
            .await
            .unwrap();
            assert_eq!(demo.view().unwrap().verification_state, [3, 2, 0, 0]);
            assert!(demo.command(Command::Action(2)).await.is_err());
            demo.command(Command::Addresses([
                "alice@example.test".into(),
                "bob@example.test".into(),
            ]))
            .await
            .unwrap();
            assert_eq!(demo.view().unwrap().verification_state, [2, 2, 0, 0]);
            demo.saved.verification[0]
                .report
                .as_mut()
                .unwrap()
                .checked_at_ms = now_ms() - sats_arena_core::wallet::VERIFICATION_TTL_MS;
            assert_eq!(demo.view().unwrap().verification_state[0], 4);
            assert!(demo.command(Command::Action(2)).await.is_err());
            let report = demo.saved.verification[0].report.as_mut().unwrap();
            report.checked_at_ms = now_ms();
            report.min_msat = 2000;
            assert_eq!(demo.view().unwrap().verification_state[0], 5);
            assert!(demo.command(Command::Action(2)).await.is_err());
            assert_eq!(demo.saved.pot.paid, [false, false, false, false]);
        });
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

#[cfg(test)]
mod four_player_tests;

#[cfg(test)]
mod retired_tests;
