//! Durable coordination around the shared match host. This owns no wallet rules.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, StatusCode, header};
use fs2::FileExt;
use sats_arena_core::{
    arena_pot::{Credit, MatchPot},
    pot::{Outcome, Phase},
    wallet::{LnurlClient, NwcWallet},
};
use sats_arena_local::match_host::{HostRules, HostWallet, MatchHost};
use sats_arena_protocol::netplay::{
    Command, DamageCredit, EndReport, MatchObservation, PaymentPlayer, PaymentSnapshot, RoomConfig,
    StartManifest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    api::ApiError,
    payment_wallet::{PaymentWallet, UniqueInvoices},
};

const LEASE: Duration = Duration::from_secs(120);
const MAX_CREDITS: usize = 16_384;
const MAX_RECORDS: usize = 4096;

#[derive(Clone)]
pub(crate) struct Payments(Arc<Mutex<Coordinator>>);
struct Coordinator {
    root: PathBuf,
    rules: HostRules,
    wallet: HostWallet,
    sandbox_addresses: Option<Vec<String>>,
    records: BTreeMap<String, Record>,
    _lock: File,
}
struct Record {
    path: PathBuf,
    saved: Saved,
    host: MatchHost,
    seen: [Option<Instant>; 4],
}
#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    version: u32,
    id: String,
    wallet_identity: String,
    rules: HostRules,
    mode: String,
    sandbox_addresses: Option<Vec<String>>,
    #[serde(default)]
    config: RoomConfig,
    #[serde(deserialize_with = "legacy_four")]
    token_hashes: [Option<String>; 4],
    #[serde(deserialize_with = "legacy_four")]
    observations: [Observation; 4],
    admitted: bool,
    closed: bool,
    frozen: bool,
    error: Option<String>,
    #[serde(default)]
    review_reason: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Observation {
    manifest: Option<StartManifest>,
    credits: Vec<DamageCredit>,
    end: Option<EndReport>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WalletFile {
    backend: String,
    nwc_uri: String,
    entry_sats: u64,
    sats_per_percent: u64,
    #[serde(default = "batch_default")]
    batch_window_ms: u64,
    #[serde(default)]
    surcharge_bps: u32,
    #[serde(default)]
    sandbox_addresses: Option<Vec<String>>,
}
fn legacy_four<'de, D, T>(deserializer: D) -> Result<[T; 4], D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let values = Vec::<T>::deserialize(deserializer)?;
    if values.len() != 2 && values.len() != 4 {
        return Err(serde::de::Error::custom(
            "Expected two legacy or four player slots",
        ));
    }
    let mut values = values.into_iter();
    Ok(std::array::from_fn(|_| values.next().unwrap_or_default()))
}
fn batch_default() -> u64 {
    5000
}
fn status(code: StatusCode, message: &str) -> ApiError {
    ApiError::Status(code, message.into())
}
fn rejected(message: &str) -> ApiError {
    status(StatusCode::CONFLICT, message)
}
fn fingerprint(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}
fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|token| token.len() == 64 && token.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| status(StatusCode::UNAUTHORIZED, "Room token required"))
}
fn persist(path: &Path, saved: &Saved) -> anyhow::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(saved)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

impl Record {
    fn authenticate(&mut self, token: &str) -> Result<usize, ApiError> {
        let hash = fingerprint(token);
        let slot = self
            .saved
            .token_hashes
            .iter()
            .position(|value| value.as_deref() == Some(&hash))
            .ok_or_else(|| status(StatusCode::UNAUTHORIZED, "Invalid match token"))?;
        self.seen[slot] = Some(Instant::now());
        Ok(slot)
    }
    fn snapshot(&self) -> PaymentSnapshot {
        let host = self.host.snapshot();
        PaymentSnapshot {
            mode: self.saved.mode.clone(),
            payment_log: host.payment_log.clone(),
            match_id: self.saved.id.clone(),
            phase: match host.phase {
                Phase::Funding => 1,
                Phase::Ready => 2,
                Phase::Playing => 3,
                Phase::Settled => 4,
                Phase::Settling => 6,
            },
            frozen: self.saved.frozen || host.frozen,
            entry_sats: host.entry_sats,
            invoice_sats: host.invoice_sats,
            sats_per_percent: host.sats_per_percent,
            batch_window_ms: host.batch_window_ms,
            surcharge_bps: host.surcharge_bps,
            players: std::array::from_fn(|i| PaymentPlayer {
                address: host.addresses[i].clone(),
                invoice: host.invoices[i].clone(),
                verified: host.verified[i],
                verification_state: u8::try_from(host.verification_state[i]).unwrap_or(3),
                verification_message: host.verification_message[i].clone(),
                paid_sats: if host.paid[i] { host.invoice_sats } else { 0 },
                remaining_sats: host.remaining_sats[i],
                earned_sats: host.earned_sats[i],
                pending_sats: host.pending_sats[i],
            }),
            teams: host.teams,
            kinds: host.kinds,
            team_ids: host.team_ids,
            characters: host.characters,
            batch_remaining_ms: host.batch_remaining_ms,
            damage_centi: host.damage_centi,
            agreed_sequence: host.last_credit_sequence,
            own_next_sequence: std::array::from_fn(|i| {
                self.saved.observations[i].credits.len() as u64 + 1
            }),
            error: self.saved.error.clone().or(host.error),
        }
    }
    fn freeze(&mut self, message: &str) -> Result<(), ApiError> {
        self.host.freeze()?;
        self.saved.frozen = true;
        self.saved.error = Some(message.into());
        persist(&self.path, &self.saved)?;
        Ok(())
    }
    async fn abandon(&mut self) -> Result<(), ApiError> {
        if !self.saved.closed {
            self.saved.closed = true;
            persist(&self.path, &self.saved)?;
        }
        if self.saved.admitted {
            if !matches!(self.host.snapshot().phase, Phase::Settled | Phase::Settling) {
                self.freeze("Match interrupted; funds frozen for operator review")?;
            }
        } else if matches!(self.host.snapshot().phase, Phase::Funding | Phase::Ready) {
            self.host.cancel().await?;
        }
        Ok(())
    }
    async fn report(&mut self, slot: usize, mut report: MatchObservation) -> Result<(), ApiError> {
        if self.saved.frozen || self.host.snapshot().frozen {
            return Err(rejected("Match is frozen for review"));
        }
        if !self.saved.admitted || self.saved.closed {
            return Err(rejected("Match has no active paid admission"));
        }
        if report.match_id != self.saved.id || report.credits.len() > 64 {
            return Err(rejected("Invalid match report"));
        }
        if let Some(end) = &mut report.end {
            if let Some(winner) = end.winner {
                if winner >= self.saved.config.player_count {
                    return Err(rejected("Invalid winning player"));
                }
                if self.saved.config.teams {
                    end.winner = (0..self.saved.config.player_count).find(|&i| {
                        self.saved.config.team_ids[i as usize]
                            == self.saved.config.team_ids[winner as usize]
                    });
                }
            }
        }
        if matches!(self.host.snapshot().phase, Phase::Settling | Phase::Settled) {
            let previous = &self.saved.observations[slot];
            let identical = previous.manifest.as_ref() == Some(&report.manifest)
                && report.credits.iter().all(|credit| {
                    credit.sequence > 0
                        && previous.credits.get(credit.sequence as usize - 1) == Some(credit)
                })
                && report
                    .end
                    .as_ref()
                    .is_none_or(|end| previous.end.as_ref() == Some(end));
            return if identical {
                Ok(())
            } else {
                Err(rejected("Match result is already finalized"))
            };
        }
        validate_manifest(&report.manifest, &self.saved.config)?;
        let mut next = self.saved.observations[slot].clone();
        if next
            .manifest
            .as_ref()
            .is_some_and(|m| m != &report.manifest)
        {
            self.freeze("A client changed its match manifest")?;
            return Err(rejected("Match report disagreement"));
        }
        next.manifest = Some(report.manifest);
        for credit in report.credits {
            if credit.sequence == 0
                || credit.sequence > MAX_CREDITS as u64
                || credit.attacker >= self.saved.config.player_count
                || credit.victim >= self.saved.config.player_count
                || credit.damage_centi == 0
                || credit.damage_centi > 999_900
                || !(-123..=3_600_000).contains(&credit.frame)
            {
                return Err(rejected("Invalid damage credit"));
            }
            let index = credit.sequence as usize - 1;
            if index < next.credits.len() {
                if next.credits[index] != credit {
                    self.freeze("A client changed a previously reported damage credit")?;
                    return Err(rejected("Match report disagreement"));
                }
            } else if index != next.credits.len() {
                return Err(rejected("Damage report has a sequence gap"));
            } else {
                if next.end.is_some() || next.credits.last().is_some_and(|c| c.frame > credit.frame)
                {
                    return Err(rejected("Damage report is out of order"));
                }
                next.credits.push(credit);
            }
        }
        if let Some(end) = report.end {
            if end.last_sequence > MAX_CREDITS as u64
                || (end.complete && end.last_sequence != next.credits.len() as u64)
                || end
                    .winner
                    .is_some_and(|w| w >= self.saved.config.player_count)
            {
                return Err(rejected("Final report does not cover its damage stream"));
            }
            if next.end.as_ref().is_some_and(|previous| previous != &end) {
                self.freeze("A client changed its final result")?;
                return Err(rejected("Match report disagreement"));
            }
            next.end = Some(end);
        }
        self.saved.observations[slot] = next;
        // Write observations before applying them. The host's sequence deduplicates crash recovery.
        persist(&self.path, &self.saved)?;
        if self.saved.observations[slot]
            .end
            .as_ref()
            .is_some_and(|end| !end.complete)
        {
            self.freeze("A client reported incomplete gameplay; funds frozen for review")?;
            return Ok(());
        }
        self.reconcile().await
    }
    async fn reconcile(&mut self) -> Result<(), ApiError> {
        if self.saved.frozen || self.host.snapshot().frozen {
            return Ok(());
        }
        let observations = &self.saved.observations[..self.saved.config.player_count as usize];
        if observations
            .iter()
            .any(|o| o.end.as_ref().is_some_and(|end| !end.complete))
        {
            self.freeze("A client reported incomplete gameplay; funds frozen for review")?;
            return Ok(());
        }
        let manifests: Vec<_> = observations
            .iter()
            .filter_map(|o| o.manifest.as_ref())
            .collect();
        if manifests.windows(2).any(|m| m[0] != m[1]) {
            self.freeze("Clients disagree about the match setup")?;
            return Ok(());
        }
        let longest = observations
            .iter()
            .map(|o| o.credits.len())
            .max()
            .unwrap_or(0);
        if observations.iter().any(|o| {
            o.end
                .as_ref()
                .is_some_and(|e| e.last_sequence < longest as u64)
        }) {
            self.freeze("A final report omits damage reported by another player")?;
            return Ok(());
        }
        let ends: Vec<_> = observations.iter().filter_map(|o| o.end.as_ref()).collect();
        if ends.windows(2).any(|end| end[0] != end[1]) {
            self.freeze("Clients disagree about the result")?;
            return Ok(());
        }
        for (i, a) in observations.iter().enumerate() {
            for b in &observations[i + 1..] {
                let common = a.credits.len().min(b.credits.len());
                if a.credits[..common] != b.credits[..common] {
                    self.freeze("Clients disagree about finalized damage")?;
                    return Ok(());
                }
            }
        }
        if manifests.len() != observations.len() {
            return Ok(());
        }
        let manifest = manifests[0].clone();
        let common = observations
            .iter()
            .map(|o| o.credits.len())
            .min()
            .unwrap_or(0);
        let acknowledged = self.host.snapshot().last_credit_sequence as usize;
        if acknowledged > common {
            self.freeze("Stored damage history is incomplete; funds retained for review")?;
            return Ok(());
        }
        let credits: Vec<_> = observations[0].credits[acknowledged..common]
            .iter()
            .map(|c| Credit {
                sequence: c.sequence,
                attacker: c.attacker as usize,
                victim: c.victim as usize,
                damage_centi: c.damage_centi,
            })
            .collect();
        let final_report = if ends.len() == observations.len() {
            Some(ends[0].clone())
        } else {
            None
        };
        if self.host.snapshot().phase == Phase::Ready {
            self.host
                .configure_roster(
                    manifest.teams,
                    manifest.kinds,
                    manifest.team_ids,
                    manifest.characters,
                )
                .await?;
            self.host.start().await?;
        }
        if self.host.snapshot().phase == Phase::Playing {
            if !credits.is_empty() {
                self.host.credits(credits).await?;
            }
            if let Some(end) = final_report {
                if end.last_sequence != self.host.snapshot().last_credit_sequence {
                    self.freeze("Final reports omit agreed damage credits")?;
                } else {
                    self.host
                        .finish(
                            end.winner
                                .map_or(Outcome::NoContest, |w| Outcome::Winner(w as usize)),
                            true,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}
fn validate_manifest(manifest: &StartManifest, config: &RoomConfig) -> Result<(), ApiError> {
    // Slippi increments retained sessions and resets on reconnect; each record binds one immutable manifest.
    if manifest.game_number == 0
        || manifest.teams != config.teams
        || manifest.kinds != config.kinds()
        || manifest.team_ids != config.team_ids
        || manifest.characters.iter().enumerate().any(|(i, &c)| {
            if i < config.player_count as usize {
                c > 32
            } else {
                c != 255
            }
        })
        || ![2, 3, 8, 28, 31, 32].contains(&manifest.stage)
    {
        return Err(rejected(
            "Game roster must match the funded room configuration",
        ));
    }
    Ok(())
}

impl Payments {
    pub(crate) async fn from_file(root: PathBuf, path: &Path) -> anyhow::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() <= 16 * 1024,
            "Invalid wallet configuration file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "Wallet configuration must be private (mode 0600)"
            );
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let config: WalletFile = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid wallet configuration"))?;
        anyhow::ensure!(
            matches!(config.backend.as_str(), "sandbox" | "nwc"),
            "Choose sandbox or nwc wallet mode"
        );
        anyhow::ensure!(
            (config.backend == "sandbox") == config.sandbox_addresses.is_some(),
            "Sandbox needs two to four receiving addresses; nwc must omit them"
        );
        if let Some(addresses) = &config.sandbox_addresses {
            let isolated = |address: &str| {
                address.strip_suffix("@getalby.com").is_some_and(|name| {
                    name.starts_with("nwc")
                        && name.len() > 3
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                })
            };
            let uri = url::Url::parse(&config.nwc_uri)
                .map_err(|_| anyhow::anyhow!("Invalid Sandbox wallet connection"))?;
            let host_address = uri
                .query_pairs()
                .find(|(key, _)| key == "lud16")
                .map(|(_, value)| value.into_owned());
            anyhow::ensure!(
                (2..=4).contains(&addresses.len())
                    && addresses
                        .iter()
                        .enumerate()
                        .all(|(i, address)| !addresses[..i].contains(address))
                    && addresses.iter().all(|address| isolated(address))
                    && host_address
                        .as_ref()
                        .is_some_and(|address| isolated(address) && !addresses.contains(address)),
                "Sandbox must use distinct isolated player and server wallet addresses"
            );
        }
        let wallet = Arc::new(
            NwcWallet::connect(&config.nwc_uri)
                .await
                .map_err(|_| anyhow::anyhow!("Could not connect the server wallet"))?,
        );
        let identity = wallet.identity().to_owned();
        let lnurl = Arc::new(LnurlClient::default());
        let rules = HostRules {
            entry_sats: config.entry_sats,
            sats_per_percent: config.sats_per_percent,
            batch_window_ms: config.batch_window_ms,
            surcharge_bps: config.surcharge_bps,
        };
        Self::open(
            root,
            rules,
            HostWallet {
                wallet,
                provider: lnurl.clone(),
                verifier: lnurl,
                identity,
                mode: config.backend,
            },
            config.sandbox_addresses,
        )
        .await
    }
    pub(crate) async fn open(
        root: PathBuf,
        rules: HostRules,
        mut wallet: HostWallet,
        sandbox_addresses: Option<Vec<String>>,
    ) -> anyhow::Result<Self> {
        let mut validated =
            MatchPot::new("0".repeat(32), rules.entry_sats, rules.sats_per_percent)?;
        validated.set_batch_window_ms(rules.batch_window_ms)?;
        validated.set_surcharge_bps(rules.surcharge_bps)?;
        anyhow::ensure!(
            wallet.identity.len() == 64
                && wallet.identity.bytes().all(|c| c.is_ascii_hexdigit())
                && matches!(wallet.mode.as_str(), "nwc" | "sandbox" | "fake"),
            "Invalid host wallet identity or mode"
        );
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(root.join("server.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("Another payment server owns this state directory"))?;
        let invoices = Arc::new(UniqueInvoices::new(
            wallet.provider.clone(),
            wallet.wallet.clone(),
        ));
        wallet.provider = invoices.clone();
        wallet.wallet = Arc::new(PaymentWallet::new(wallet.wallet.clone(), invoices.clone()));
        let mut records = BTreeMap::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_id(&id) {
                continue;
            }
            anyhow::ensure!(
                entry.file_type()?.is_dir() && records.len() < MAX_RECORDS,
                "Invalid or excessive payment state"
            );
            let path = entry.path().join("observations.json");
            if !path.try_exists()? && !entry.path().join("prepaid-match.json").try_exists()? {
                // Creation journals admission before opening the host, so this orphan held no funds.
                continue;
            }
            let bytes = std::fs::read(&path)?;
            anyhow::ensure!(
                bytes.len() < 8 * 1024 * 1024,
                "Payment observation state too large"
            );
            let saved: Saved = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                matches!(saved.version, 1 | 2)
                    && saved.config.is_valid()
                    && (saved.version != 1 || saved.config == RoomConfig::default())
                    && saved.id == id
                    && saved.wallet_identity == wallet.identity
                    && saved.mode == wallet.mode,
                "Payment state belongs to another wallet or version"
            );
            let count = saved.config.player_count as usize;
            anyhow::ensure!(
                (!saved.admitted || saved.token_hashes[..count].iter().all(Option::is_some))
                    && saved.token_hashes[count..].iter().all(Option::is_none)
                    && saved.observations[count..]
                        .iter()
                        .all(|o| o.manifest.is_none() && o.credits.is_empty() && o.end.is_none())
                    && saved
                        .sandbox_addresses
                        .as_ref()
                        .is_none_or(|addresses| addresses.len() >= count),
                "Payment journal has members outside its roster"
            );
            let migrated = saved.version == 1;
            let mut saved = saved;
            saved.version = 2;
            let mut host =
                MatchHost::open(&entry.path(), &id, saved.rules.clone(), wallet.clone()).await?;
            invoices.restore(host.used_payment_hashes())?;
            invoices.restore(host.used_entry_hashes())?;
            if saved.review_reason.is_some() && saved.closed && saved.frozen {
                host.refund_after_review().await?;
                saved.frozen = false;
                saved.error = None;
                persist(&path, &saved)?;
            } else if saved.frozen {
                host.freeze()?;
            }
            let seen = saved
                .token_hashes
                .each_ref()
                .map(|hash| hash.as_ref().map(|_| Instant::now()));
            let mut record = Record {
                path,
                saved,
                host,
                seen,
            };
            if !record.saved.admitted || record.saved.closed {
                record
                    .abandon()
                    .await
                    .map_err(|_| anyhow::anyhow!("Could not recover closed or unstarted match"))?;
            }
            record
                .reconcile()
                .await
                .map_err(|_| anyhow::anyhow!("Could not recover match reports"))?;
            if migrated {
                persist(&record.path, &record.saved)?;
            }
            records.insert(id, record);
        }
        Ok(Self(Arc::new(Mutex::new(Coordinator {
            root,
            rules,
            wallet,
            sandbox_addresses,
            records,
            _lock: lock,
        }))))
    }
    pub(crate) fn start_worker(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                this.maintain().await;
            }
        });
    }
    async fn maintain(&self) {
        let mut coordinator = self.0.lock().await;
        for record in coordinator.records.values_mut() {
            if !record.saved.closed
                && record
                    .seen
                    .iter()
                    .flatten()
                    .any(|seen| seen.elapsed() >= LEASE)
                && record.abandon().await.is_err()
            {
                tracing::error!("Could not close expired payment match; funds retained");
            }
            if record.host.tick().await.is_err() {
                tracing::warn!("Payment recovery needs attention; funds remain reserved");
            }
        }
    }
    pub(crate) async fn create(
        &self,
        id: &str,
        token: &str,
        config: RoomConfig,
    ) -> Result<PaymentSnapshot, ApiError> {
        let mut coordinator = self.0.lock().await;
        if coordinator.records.len() >= MAX_RECORDS {
            return Err(rejected(
                "Payment history capacity reached; retain journals and migrate payment state",
            ));
        }
        if !valid_id(id) || coordinator.records.contains_key(id) {
            return Err(rejected("Invalid or duplicate match ID"));
        }
        if !config.is_valid()
            || coordinator
                .sandbox_addresses
                .as_ref()
                .is_some_and(|addresses| addresses.len() < config.player_count as usize)
        {
            return Err(rejected(
                "Room roster is invalid or lacks assigned Sandbox wallets",
            ));
        }
        let saved = Saved {
            version: 2,
            config: config.clone(),
            id: id.into(),
            wallet_identity: coordinator.wallet.identity.clone(),
            rules: coordinator.rules.clone(),
            mode: coordinator.wallet.mode.clone(),
            sandbox_addresses: coordinator.sandbox_addresses.clone(),
            token_hashes: [Some(fingerprint(token)), None, None, None],
            observations: Default::default(),
            admitted: false,
            closed: false,
            frozen: false,
            error: None,
            review_reason: None,
        };
        let folder = coordinator.root.join(id);
        std::fs::create_dir(&folder).map_err(anyhow::Error::from)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
                .map_err(anyhow::Error::from)?;
        }
        let path = folder.join("observations.json");
        // Journal identity and refund intent before the shared host creates its ledger.
        persist(&path, &saved)?;
        let mut host = MatchHost::open(
            &folder,
            id,
            coordinator.rules.clone(),
            coordinator.wallet.clone(),
        )
        .await?;
        host.configure_roster(config.teams, config.kinds(), config.team_ids, [255; 4])
            .await?;
        let record = Record {
            path,
            saved,
            host,
            seen: [Some(Instant::now()), None, None, None],
        };
        let snapshot = record.snapshot();
        coordinator.records.insert(id.into(), record);
        Ok(snapshot)
    }
    pub(crate) async fn add_member(&self, id: &str, slot: u8, token: &str) -> Result<(), ApiError> {
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| status(StatusCode::NOT_FOUND, "Payment match missing"))?;
        let slot = slot as usize;
        if slot == 0
            || slot >= record.saved.config.player_count as usize
            || record.saved.token_hashes[slot].is_some()
            || record.saved.closed
            || record.saved.admitted
        {
            return Err(rejected(
                "Paid room participants are fixed by its configuration",
            ));
        }
        record.saved.token_hashes[slot] = Some(fingerprint(token));
        record.seen[slot] = Some(Instant::now());
        persist(&record.path, &record.saved)?;
        Ok(())
    }
    pub(crate) async fn next_match(
        &self,
        previous: &str,
        id: &str,
        config: RoomConfig,
        members: &[(u8, String)],
    ) -> Result<PaymentSnapshot, ApiError> {
        let old = self.view(previous).await?;
        if old.phase != 4 || old.frozen || old.players.iter().any(|p| p.pending_sats != 0) {
            return Err(rejected(
                "Previous match must finish settling before the next entry",
            ));
        }
        let host = members
            .iter()
            .find(|(slot, _)| *slot == 0)
            .ok_or_else(|| rejected("Room host is missing"))?;
        self.create(id, &host.1, config).await?;
        let result = async {
            for (slot, token) in members {
                if *slot != 0 {
                    self.add_member(id, *slot, token).await?;
                }
                let address = &old.players[*slot as usize].address;
                if !address.is_empty() {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        header::AUTHORIZATION,
                        format!("Bearer {token}")
                            .parse()
                            .map_err(|_| rejected("Invalid room member"))?,
                    );
                    self.authenticated(
                        id,
                        &headers,
                        Some(Command::SetAddress {
                            address: address.clone(),
                        }),
                    )
                    .await?;
                }
            }
            self.view(id).await
        }
        .await;
        if result.is_err() {
            self.abandon(id).await?;
        }
        result
    }
    pub(crate) async fn view(&self, id: &str) -> Result<PaymentSnapshot, ApiError> {
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| status(StatusCode::NOT_FOUND, "Payment match missing"))?;
        Ok(record.snapshot())
    }
    pub(crate) async fn admitted(&self, id: &str) -> Result<(), ApiError> {
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| status(StatusCode::NOT_FOUND, "Payment match missing"))?;
        let snapshot = record.snapshot();
        if record.saved.closed
            || snapshot.frozen
            || snapshot.phase != 2
            || record.saved.token_hashes[..record.saved.config.player_count as usize]
                .iter()
                .any(Option::is_none)
            || snapshot.players[..record.saved.config.player_count as usize]
                .iter()
                .any(|p| !p.verified || p.paid_sats != snapshot.invoice_sats)
        {
            return Err(rejected(
                "Both players must have verified, confirmed entries",
            ));
        }
        record.saved.admitted = true;
        persist(&record.path, &record.saved)?;
        Ok(())
    }
    pub(crate) async fn abandon(&self, id: &str) -> Result<(), ApiError> {
        let mut coordinator = self.0.lock().await;
        coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| status(StatusCode::NOT_FOUND, "Payment match missing"))?
            .abandon()
            .await
    }
    pub(crate) async fn authenticated(
        &self,
        id: &str,
        headers: &HeaderMap,
        command: Option<Command>,
    ) -> Result<PaymentSnapshot, ApiError> {
        if !valid_id(id) {
            return Err(status(StatusCode::NOT_FOUND, "Payment match missing"));
        }
        let token = bearer(headers)?;
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| status(StatusCode::NOT_FOUND, "Payment match missing"))?;
        let slot = record.authenticate(token)?;
        match command {
            None => {}
            Some(Command::SetAddress { address }) => {
                if record.saved.closed || record.saved.frozen {
                    return Err(rejected("Match no longer accepts address changes"));
                }
                if record
                    .saved
                    .sandbox_addresses
                    .as_ref()
                    .is_some_and(|addresses| addresses[slot] != address)
                {
                    return Err(rejected(
                        "Sandbox requires the assigned test receiving address",
                    ));
                }
                record.host.set_address(slot, address).await?;
            }
            Some(Command::Report { report }) => record.report(slot, report).await?,
            Some(Command::Leave) => {
                // A completed member may exit before peers publish their final bookends.
                // Keep its existing lease grace; missing/incomplete gameplay still freezes.
                if !record.saved.observations[slot]
                    .end
                    .as_ref()
                    .is_some_and(|end| end.complete)
                {
                    record.abandon().await?;
                }
            }
            _ => return Err(rejected("Use the room route for readiness and Start")),
        }
        Ok(record.snapshot())
    }
    pub(crate) async fn refund_after_review(&self, id: &str, reason: String) -> anyhow::Result<()> {
        anyhow::ensure!(
            reason.trim().len() >= 8
                && reason.len() <= 256
                && !reason.chars().any(char::is_control),
            "Provide a review reason between 8 and 256 characters"
        );
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Payment match missing"))?;
        anyhow::ensure!(
            record.saved.frozen || record.host.snapshot().frozen,
            "Only frozen funds need operator review"
        );
        record.saved.review_reason = Some(reason);
        record.saved.closed = true;
        record.saved.frozen = true;
        persist(&record.path, &record.saved)?;
        record.host.refund_after_review().await?;
        record.saved.frozen = false;
        record.saved.error = None;
        persist(&record.path, &record.saved)?;
        Ok(())
    }
    pub(crate) async fn recover_until_settled(&self, id: &str) -> anyhow::Result<bool> {
        let mut coordinator = self.0.lock().await;
        let record = coordinator
            .records
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Payment match missing"))?;
        record.host.tick().await?;
        Ok(record.host.snapshot().phase == Phase::Settled)
    }
}

#[cfg(test)]
#[path = "payments_tests.rs"]
pub(crate) mod tests;
