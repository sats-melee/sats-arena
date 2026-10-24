use std::{path::Path, sync::Arc, time::Duration};

use sats_arena_protocol::{
    Account, Deposit, DepositStatus, Lobby, LobbyRequest, MatchReport, MatchStatus,
    RegisterResponse, Withdrawal, WithdrawalRequest,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    Error, MAX_BALANCE_SATS, Result, Rules,
    ledger::Ledger,
    wallet::{InvoiceState, PaymentState, Wallet},
};

const QUEUE_SIZE: usize = 128;
const WALLET_TIMEOUT: Duration = Duration::from_secs(30);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// Cloneable handle to a single ledger/wallet worker. Local callers call this
/// directly; the remote server translates HTTP requests to these same methods.
///
/// A dropped request/HTTP connection does not cancel an accepted payment.
/// Never open the same ledger in two engines or use two wallets for one ledger.
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::Sender<Command>,
}

type Reply<T> = oneshot::Sender<Result<T>>;
enum LobbyOp {
    Get,
    Ready,
    Start,
    Cancel,
}

enum Command {
    CreateLobby {
        token: String,
        request: LobbyRequest,
        reply: Reply<Lobby>,
    },
    Lobby {
        token: String,
        id: String,
        op: LobbyOp,
        reply: Reply<Lobby>,
    },
    Register {
        name: String,
        reply: Reply<RegisterResponse>,
    },
    Account {
        token: String,
        reply: Reply<Account>,
    },
    Deposit {
        token: String,
        amount: u64,
        reply: Reply<Deposit>,
    },
    GetDeposit {
        token: String,
        id: String,
        reply: Reply<Deposit>,
    },
    Withdraw {
        token: String,
        request: WithdrawalRequest,
        reply: Reply<Withdrawal>,
    },
    GetWithdrawal {
        token: String,
        id: String,
        reply: Reply<Withdrawal>,
    },
    Report {
        token: String,
        report: MatchReport,
        reply: Reply<MatchStatus>,
    },
    Reconcile {
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

impl Engine {
    pub async fn create_lobby(&self, token: &str, request: LobbyRequest) -> Result<Lobby> {
        self.request(|reply| Command::CreateLobby {
            token: token.into(),
            request,
            reply,
        })
        .await
    }

    pub async fn lobby(&self, token: &str, id: &str) -> Result<Lobby> {
        self.lobby_action(token, id, LobbyOp::Get).await
    }
    pub async fn ready_lobby(&self, token: &str, id: &str) -> Result<Lobby> {
        self.lobby_action(token, id, LobbyOp::Ready).await
    }
    pub async fn start_lobby(&self, token: &str, id: &str) -> Result<Lobby> {
        self.lobby_action(token, id, LobbyOp::Start).await
    }
    pub async fn cancel_lobby(&self, token: &str, id: &str) -> Result<Lobby> {
        self.lobby_action(token, id, LobbyOp::Cancel).await
    }

    async fn lobby_action(&self, token: &str, id: &str, op: LobbyOp) -> Result<Lobby> {
        self.request(|reply| Command::Lobby {
            token: token.into(),
            id: id.into(),
            op,
            reply,
        })
        .await
    }
    /// Call from a background Tokio runtime. SQLite I/O is performed by the
    /// worker, never by an emulation-thread callback.
    pub fn open(path: &Path, rules: Rules, wallet: Arc<dyn Wallet>) -> Result<Self> {
        rules.validate()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Rejected("start the engine on a background Tokio runtime".into())
        })?;
        let ledger = Ledger::open(path)?;
        let (commands, receiver) = mpsc::channel(QUEUE_SIZE);
        runtime.spawn(
            State {
                ledger,
                rules,
                wallet,
            }
            .run(receiver),
        );
        Ok(Self { commands })
    }

    pub async fn register(&self, name: &str) -> Result<RegisterResponse> {
        self.request(|reply| Command::Register {
            name: name.into(),
            reply,
        })
        .await
    }

    pub async fn account(&self, token: &str) -> Result<Account> {
        self.request(|reply| Command::Account {
            token: token.into(),
            reply,
        })
        .await
    }

    pub async fn create_deposit(&self, token: &str, amount_sats: u64) -> Result<Deposit> {
        self.request(|reply| Command::Deposit {
            token: token.into(),
            amount: amount_sats,
            reply,
        })
        .await
    }

    pub async fn deposit(&self, token: &str, id: &str) -> Result<Deposit> {
        self.request(|reply| Command::GetDeposit {
            token: token.into(),
            id: id.into(),
            reply,
        })
        .await
    }

    pub async fn withdraw(&self, token: &str, request: WithdrawalRequest) -> Result<Withdrawal> {
        self.request(|reply| Command::Withdraw {
            token: token.into(),
            request,
            reply,
        })
        .await
    }

    pub async fn withdrawal(&self, token: &str, id: &str) -> Result<Withdrawal> {
        self.request(|reply| Command::GetWithdrawal {
            token: token.into(),
            id: id.into(),
            reply,
        })
        .await
    }

    pub async fn report_match(&self, token: &str, report: MatchReport) -> Result<MatchStatus> {
        self.request(|reply| Command::Report {
            token: token.into(),
            report,
            reply,
        })
        .await
    }

    /// Runs automatically every 15 seconds, and can also be requested by a host.
    pub async fn reconcile(&self) -> Result<()> {
        self.request(|reply| Command::Reconcile { reply }).await
    }

    /// Finish previously accepted operations and release the database. Call
    /// after the host has stopped accepting new work (e.g. during app exit).
    pub async fn shutdown(&self) -> Result<()> {
        self.request(|reply| Command::Shutdown { reply }).await
    }

    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| Error::Closed)?;
        response.await.map_err(|_| Error::Closed)?
    }
}

struct State {
    ledger: Ledger,
    rules: Rules,
    wallet: Arc<dyn Wallet>,
}

impl State {
    async fn run(mut self, mut receiver: mpsc::Receiver<Command>) {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + RECONCILE_INTERVAL,
            RECONCILE_INTERVAL,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = receiver.recv() => match command {
                    None => break,
                    Some(Command::CreateLobby { token, request, reply }) => {
                        let result = self.player(&token).and_then(|id| self.ledger.create_lobby(&id, &request, &self.rules));
                        let _ = reply.send(result);
                    }
                    Some(Command::Lobby { token, id, op, reply }) => {
                        let result = self.player(&token).and_then(|player| match op {
                            LobbyOp::Get => self.ledger.lobby(&player, &id),
                            LobbyOp::Ready => self.ledger.ready_lobby(&player, &id),
                            LobbyOp::Start => self.ledger.start_lobby(&player, &id),
                            LobbyOp::Cancel => self.ledger.cancel_lobby(&player, &id),
                        });
                        let _ = reply.send(result);
                    }
                    Some(Command::Register { name, reply }) => {
                        let result = self.ledger.register(&name).map(|(player_id, token)| RegisterResponse { player_id, token });
                        let _ = reply.send(result);
                    }
                    Some(Command::Account { token, reply }) => {
                        let result = self.player(&token).and_then(|id| self.ledger.account(&id));
                        let _ = reply.send(result);
                    }
                    Some(Command::Deposit { token, amount, reply }) => {
                        let result = self.create_deposit(&token, amount).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::GetDeposit { token, id, reply }) => {
                        let result = self.deposit(&token, &id).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::Withdraw { token, request, reply }) => {
                        let result = self.withdraw(&token, request).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::GetWithdrawal { token, id, reply }) => {
                        let result = self.player(&token).and_then(|player| self.ledger.withdrawal(&player, &id)?.ok_or(Error::NotFound));
                        let _ = reply.send(result);
                    }
                    Some(Command::Report { token, report, reply }) => {
                        let result = self.player(&token).and_then(|id| self.ledger.submit_report(&id, &report, &self.rules));
                        let _ = reply.send(result);
                    }
                    Some(Command::Reconcile { reply }) => { let result = self.reconcile().await; let _ = reply.send(result); }
                    Some(Command::Shutdown { reply }) => {
                        // Drop the database before acknowledging shutdown.
                        receiver.close();
                        drop(self);
                        let _ = reply.send(Ok(()));
                        break;
                    }
                },
                _ = interval.tick() => {
                    if let Err(error) = self.reconcile().await { tracing::warn!("payment reconciliation failed: {error}"); }
                }
            }
        }
    }

    fn player(&self, token: &str) -> Result<String> {
        self.ledger.authenticate(token)?.ok_or(Error::Unauthorized)
    }

    async fn create_deposit(&mut self, token: &str, amount: u64) -> Result<Deposit> {
        let player = self.player(token)?;
        if amount == 0 || amount > self.rules.max_deposit_sats {
            return Err(Error::Rejected(format!(
                "deposit must be 1-{} sats",
                self.rules.max_deposit_sats
            )));
        }
        let invoice = tokio::time::timeout(
            WALLET_TIMEOUT,
            self.wallet.create_invoice(
                amount,
                "Sats Arena deposit",
                self.rules.invoice_expiry_secs,
            ),
        )
        .await
        .map_err(|_| Error::WalletUnavailable)?
        .map_err(|_| Error::WalletUnavailable)?;
        // Creating an invoice does NOT credit a balance. Only a paid lookup does.
        self.ledger
            .insert_deposit(&player, amount, &invoice.bolt11, &invoice.payment_hash)
    }

    async fn deposit(&mut self, token: &str, id: &str) -> Result<Deposit> {
        let player = self.player(token)?;
        let (deposit, hash) = self.ledger.deposit(&player, id)?.ok_or(Error::NotFound)?;
        if deposit.status == DepositStatus::Pending {
            self.check_deposit(id, &hash, deposit.amount_sats).await?;
        }
        self.ledger
            .deposit(&player, id)?
            .map(|(deposit, _)| deposit)
            .ok_or(Error::NotFound)
    }

    async fn check_deposit(&mut self, id: &str, hash: &str, expected: u64) -> Result<()> {
        let state = tokio::time::timeout(WALLET_TIMEOUT, self.wallet.invoice_state(hash))
            .await
            .map_err(|_| Error::WalletUnavailable)?
            .map_err(|_| Error::WalletUnavailable)?;
        match state {
            InvoiceState::Pending => Ok(()),
            InvoiceState::Expired => self.ledger.expire_deposit(id),
            InvoiceState::Paid { amount_sats } if amount_sats == expected => {
                self.ledger.credit_deposit(id)
            }
            InvoiceState::Paid { .. } => Err(Error::Rejected(
                "wallet-reported deposit amount doesn't match; manual review is required".into(),
            )),
        }
    }

    async fn withdraw(&mut self, token: &str, request: WithdrawalRequest) -> Result<Withdrawal> {
        let player = self.player(token)?;
        if request.amount_sats == 0 || request.amount_sats > MAX_BALANCE_SATS {
            return Err(Error::Rejected("invalid withdrawal amount".into()));
        }
        let bolt11 = request.bolt11.trim();
        let invoice = self
            .wallet
            .decode_invoice(bolt11)
            .map_err(|_| Error::Rejected("invalid withdrawal invoice".into()))?;
        if invoice.amount_sats != Some(request.amount_sats) {
            return Err(Error::Rejected(
                "invoice must have the requested whole-sat amount".into(),
            ));
        }
        if let Some(existing) = self
            .ledger
            .withdrawal_for_invoice(&player, &invoice.payment_hash)?
        {
            return Ok(existing);
        }
        let reserve = fee_reserve(request.amount_sats);
        let id = self.ledger.reserve_withdrawal(
            &player,
            request.amount_sats,
            reserve,
            bolt11,
            &invoice.payment_hash,
        )?;
        // Persist the reservation BEFORE calling Lightning. Cancellation, lost
        // responses and timeouts never cause an automatic refund or resend.
        if let Ok(Ok(state)) =
            tokio::time::timeout(WALLET_TIMEOUT, self.wallet.pay_invoice(bolt11, reserve)).await
        {
            self.finish_withdrawal(&id, state)?;
        }
        self.ledger.withdrawal(&player, &id)?.ok_or(Error::NotFound)
    }

    fn finish_withdrawal(&mut self, id: &str, state: PaymentState) -> Result<()> {
        match state {
            PaymentState::Pending => Ok(()),
            PaymentState::Succeeded { fee_sats } => {
                self.ledger.finish_withdrawal(id, Some(fee_sats), None)
            }
            PaymentState::Failed { reason } => {
                self.ledger.finish_withdrawal(id, None, Some(&reason))
            }
        }
    }

    async fn reconcile(&mut self) -> Result<()> {
        for (id, hash, amount) in self.ledger.pending_deposits()? {
            if let Err(error) = self.check_deposit(&id, &hash, amount).await {
                tracing::warn!("couldn't check deposit {id}: {error}");
            }
        }
        for withdrawal in self.ledger.pending_withdrawals()? {
            if let Ok(Ok(state)) = tokio::time::timeout(
                WALLET_TIMEOUT,
                self.wallet.payment_state(&withdrawal.payment_hash),
            )
            .await
            {
                if let Err(error) = self.finish_withdrawal(&withdrawal.id, state) {
                    tracing::warn!("couldn't finish withdrawal {}: {error}", withdrawal.id);
                }
            }
        }
        Ok(())
    }
}

fn fee_reserve(amount_sats: u64) -> u64 {
    (amount_sats / 100).max(10)
}
