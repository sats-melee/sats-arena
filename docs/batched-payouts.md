# Five-second payouts and receiving addresses

## Timing

Damage still changes the match accounting as soon as a finalized credit is
observed. Whole sats owed to each recipient collect in an open batch.

The first whole sat owed starts a **five-second deadline** for that recipient.
Later hits add to that batch without extending its deadline. There is no
minimum amount beyond the one-sat granularity of the accounting.

At the deadline, close the batch into the recipient's outbound queue. Further
closed batches join any never-submitted amount for that recipient. Consolidation
creates a new payout ID and marks original intents `Superseded`, retaining their
amounts, original invoices and replacement IDs for audit. Superseded invoices
are never paid. Recipients have independent deadlines.
Scheduling runs on the background worker; it does not pause emulation.

On match end, combine each recipient's open earnings, never-submitted queued
intents, and final allocation into one settlement per recipient. An in-flight
payment retains its original amount, ID and invoice; uncertain outcomes are
looked up without resending. Definitively failed payments remain separate for
review. No contest
returns unspent contributions while keeping damage earnings, including unsent
damage earnings, with their recipients.

The dispatcher defaults to one new payment per second and at most two concurrent
submission requests. Receiving-invoice preparation has four separate slots;
outgoing confirmation checks have two. Old status checks cannot consume slots
reserved for submitting payouts. Pending/error lookups back off from two to
thirty seconds. Finalizing matches receive priority, with older queued recipients
first within each priority. The launcher developer setting and
`SATS_ARENA_PAYMENTS_PER_SECOND` select 1–20 starts/second.

Deadlines are persisted Unix milliseconds; recovery flushes overdue windows
once, without restarting the timer or duplicating the batch.

## Receiving addresses

Each player enters a Lightning Address such as `alice@example.com`.
Every participating human must have a current compatible live receiver check
before entry invoices are displayed or entry payments can be simulated. CPUs
require no address by default. **Developer options → CPU wallets** makes CPU
slots full financial participants: each needs its own verified receiver and paid
entry, can earn damage payments, and shares winning-side settlements. Its game
control kind remains CPU. Sandbox supplies the existing player test wallets;
**Pay from test wallet** sends a real NWC test-sat entry for that CPU. Live mode
shows the usual entry invoice and receiving-address input. The option is frozen
before funding and applies when LOCAL is next launched. Domains are normalized to
lowercase; addresses persist across games and application restarts.

Recipients are locked as soon as any entry payment is confirmed. New
recipients can be saved before paying for the next match. A prepared payout
retains its original recipient even if a later match uses different addresses.

The side panel shows pending sats and a countdown separately from confirmed
paid-out sats. The payment log records UTC confirmation time, amount, player,
receiving address and payment type. Fake receipts explicitly say SIMULATED.
Receipts persist across matches; the window displays a bounded recent history.

The optional **Show outbound payments** checkbox expands a read-only debugging
view in the native payment panel. It lists every payout from the current match,
recovering earlier matches, and the selected wallet's local match archives,
including received late-entry refunds and open damage batches. Each record shows
its full ID, player, amount, bound receiving address, type, and execution state:
queued, requesting an invoice, sending, awaiting/checking confirmation, paid,
failed, or consolidated. Consolidated originals link to their replacements.
Outstanding jobs show their observed wait; confirmed receipts show UTC
confirmation time. Unreadable archives are reported as incomplete history.
The view is disabled by default, refreshes while open, and uses a variable-size
snapshot so long matches are not truncated to the activity log's recent entries.
Selecting text pauses display updates for copying. Enabling it makes no wallet
requests and changes no ledger or payment state.

Schema 4 stores CPU financial participation and consolidation audit records.
Existing funded agreements retain their terms. Older builds reject schema 4
rather than silently applying their earlier CPU payment rules.

## Backend-neutral execution

The shared Rust core exposes independent interfaces:

| Interface | Responsibility |
| --- | --- |
| `arena_pot::MatchPot` | Account for damage, close timed batches, allocate the final pot; no networking |
| `wallet::AddressInvoices` | Resolve a receiving address into an exact-amount invoice |
| `wallet::Wallet` | Create/confirm entry invoices, decode outgoing invoices, submit payments, look up outcomes |

The local host accepts injected `Arc<dyn Wallet>` and
`Arc<dyn AddressInvoices>` through `open_with`. The public GUI factory still
selects fake implementations or the real NWC adapter according to the launcher
handoff (with an explicit developer environment override). Entering a receiving
address alone does not select a live funding wallet.

A production address provider implements [LUD-16](https://github.com/lnurl/luds/blob/luds/16.md)
and [LUD-06](https://github.com/lnurl/luds/blob/luds/06.md): HTTPS discovery,
callback invoice request in millisats, advertised bounds, metadata binding,
expiry, and protocol error handling. It must not make the payment itself.
The current fake payout provider never contacts the entered domain. The real
`LnurlClient` now implements receiving discovery, invoice validation and the
separate live `AddressVerifier` used by the GUI. See
[address verification](address-verification.md).

The NWC wallet adapter maps the wallet operations to make_invoice,
lookup_invoice and pay_invoice as supported by the connected wallet, and uses
a BOLT11 decoder for local validation.
See [NWC's payment flow](https://docs.nwc.dev/introduction/introduction-to-nwc).

The dispatcher verifies the resolved amount and payment hash. It persists
the recipient, invoice and hash **before** marking an outgoing intent in flight.
It reuses that exact invoice after recovery, never requests another invoice
for an existing prepared payout, and rejects reused payment hashes across
matches. Unknown outcomes are looked up rather than blindly resent. Definitive
failure stays reserved and visible for review while subsequent matches use
separate fresh entries.

The host has bounded wallet/resolver timeouts. Fees are separate from the
advertised match funds; the adapter receives the configured fee limit
(`SATS_ARENA_MAX_PAYOUT_FEE_SATS`, default 10 sats per outgoing payment).
Fake fees are zero. Standard NWC cannot enforce that per-payment cap; NWC uses
an explicit wallet-managed policy instead and rejects a finite app cap before
submission. Fake and live states are separate and live state is bound to public
wallet-connection identity. See [NWC execution](nwc-local-wallet.md).

## Verification

- Injected-clock core tests check just-before/at-deadline behavior, late hits,
  independent recipients, one-sat batches, end flush, and restart recovery.
- Wallet/provider tests check address validation, exact invoice amounts and
  resolving without payment submission.
- Local integration tests check five-second asynchronous receipts, recipient
  locking, end-of-match coalescing and recovery of unknown outgoing payments.
- Native evidence: `Ishiiruka/docs/evidence/batching/`. Three game hits produced
  one 232-sat payment to alice@example.test, with confirmation 5.009 seconds
  after the first observed credit (20ms after the persisted deadline). P2 then
  won and received the remaining 768 sats at bob@example.test. All payments
  in that run were simulated.
