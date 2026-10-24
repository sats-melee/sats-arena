# Initial payment flow

## One engine, two hosts

`sats-arena-core::Engine` is the authority for all money changes. A local host
calls it directly; the remote server forwards authenticated HTTP requests to
those same methods. No local HTTP service is necessary. Both use a durable
SQLite ledger and the same `Wallet` trait.

The Rust command worker serializes wallet and ledger operations. Calls use a
bounded queue; wallet calls have a 30-second timeout. Put its Tokio runtime on
a background worker, never the emulator thread. Opening the engine also opens
the database synchronously, so initialization belongs on that worker too.

The engine's player IDs/tokens identify balances. They are **not Slippi account
logins**. For a local game the trusted device holds both players' tokens; for
remote play each client holds its own token. Only the engine host has the NWC
spending credential. An NWC adapter is not implemented yet.

Use one engine and wallet per ledger. Local and remote ledgers are not synced,
and switching modes does not transfer balances between them. Test and real
wallets must use separate databases.

## Deposits: accept an ordinary payment

```text
create invoice → Pending → wallet confirms paid amount → Credited
                        └→ wallet confirms expiry     → Expired
```

- The payment is actually received into the operator's wallet. There is no
  held HTLC, hold invoice, invoice settlement call, or automatic cancellation.
- Showing/scanning a QR code or receiving a client "paid" message is not proof.
  The engine looks up its stored invoice hash through the wallet adapter.
- The credited amount must match the expected whole-sat amount. A mismatch
  stays uncredited for review rather than silently creating a wrong balance.
- The paid status and credit entry are committed in the same transaction.
  Repeated polling/notifications/restarts cannot credit an invoice twice.
- A deposit is not tied to one match. If the game ends, crashes, or is disputed,
  the sats remain owed to that player until spent or withdrawn. A refund is a
  separate outgoing payment, not cancellation of the original invoice.

## Games: ledger transfers, not Lightning transfers

Each participant reports the same per-game ID, controller assignments,
finalized damage events, and outcome. The local host sends a report under each
player's token; remote clients each submit theirs. Reports must agree before
settlement. Disagreement moves no money. This is agreement checking, not
anti-cheat or independent proof of the game's outcome.

The report ID must be unique **per game**, not just per matchmaking session.
An agreed game settles atomically once. Rules are saved when its lobby is created
so a restart with changed config cannot reprice a pending report.

Before play, the host creates a lobby with a unique game ID, controller roster,
and a maximum-loss budget per player. Each participant explicitly readies;
that transaction deducts their budget from available funds and records the
reservation. Inadequately funded players cannot ready. Duplicate readiness
does not reserve twice. Once all reservations exist the lobby becomes Ready,
and its creator can atomically consume the single-use start transition.

```text
Funding → Ready → Playing → Settled (unused budget returned)
   └────────┘        └───→ Disputed (budgets remain locked)
       ↓
   Cancelled (reservations released; deposits remain accepted payments)
```

Withdrawal checks only available funds, so it cannot spend reserved budgets.
Cancellation is allowed in Funding/Ready, never after Start or a dispute.
Agreed reports settle against the reserved pots and release their remaining
amounts atomically. Reports before admission/start or with a different roster
or mode are rejected. Funding, readiness, playing state, and reservations
survive restarts. A restarted Playing session cannot be started a second time.

Terms are frozen when the lobby is created, before readiness. Damage
attribution is the reporting layer's responsibility; an uncertain attacker
must not be billed as a verified hit.

The native `local/` host is intentionally **fake-only**: explicit simulated
payments, persistent fake balances, a 500-sat budget and 100-sat win bonus,
and manual test results with damage billing disabled. It links into Dolphin
and gates initial emulator boot, not each subsequent in-game stage selection.
That finer game-side hook and real NWC integration remain separate work.

## Withdrawals: reserve, attempt once, reconcile

```text
validate invoice and balance
  → persist Pending + debit amount and fee reserve
  → attempt Lightning payment
       ├→ Succeeded: Completed; refund unused fee reserve
       ├→ definitely Failed: Failed; refund entire reservation
       └→ timeout/lost response/not found: remain Pending; look up by hash
```

The initial routing-fee reserve is 1% of the amount, minimum 10 sats. A wallet
adapter must enforce that maximum and report the actual fee (rounded up to
sats if its native unit is millisats). Amountless/fractional-sat invoices are
not supported by the engine's current whole-sat invoice interface.

The invoice hash is the retry key. Resubmitting the same payout returns its
stored result, including pending or failed results, without paying or debiting
again. Another player's token cannot claim that payout. A failed invoice needs
a fresh invoice for a new attempt.

Once queued, a dropped caller or HTTP connection doesn't abort a payment.
After a process crash, the pending reservation is still in SQLite. The engine
checks the wallet; **"not found" is not proof that it can safely refund**.
If the process died between recording the reservation and sending the request,
manual review may be required. This initial version deliberately prefers stuck
funds to an unsafe refund/resend. Wallet-side terminal failure is the only
automatic failure refund path.

## Persistence and recovery

Every credit/debit has an entry in the ledger and every balance mutation is
transactional and bounded. A pending payout is not spendable money, so two
concurrent requests can't spend the same sats. Reconciliation handles each
item independently so a bad invoice doesn't stop checks for other players.

Graceful host shutdown first stops new requests, then calls `engine.shutdown()`
to finish accepted work and close SQLite. Preserve the database after a crash;
do not "fix" pending payments by deleting it. Database backups, restoring the
matching wallet, and recovery against a real NWC backend still need testing
before using real sats. The fake backend's own wallet history is in-memory.
