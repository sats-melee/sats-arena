# Five-second payouts and receiving addresses

## Timing

Damage still changes the match accounting as soon as a finalized credit is
observed. Whole sats owed to each recipient collect in an open batch.

The first whole sat owed starts a **five-second deadline** for that recipient.
Later hits add to that batch without extending its deadline. There is no
minimum amount beyond the one-sat granularity of the accounting.

At the deadline, freeze the batch into an immutable payout ID and submit it.
Damage at or after that cutoff starts another batch, even while the previous
payment is in flight. The two recipients have independent deadlines.
Scheduling runs on the background worker; it does not pause emulation.

On match end, combine each recipient's still-open earnings with their final
allocation in one settlement payment per recipient. A batch already frozen or
in flight stays a separate payment with its original ID and invoice. No contest
returns unspent contributions while keeping damage earnings, including unsent
damage earnings, with their recipients.

Deadlines are persisted Unix milliseconds; recovery flushes overdue windows
once, without restarting the timer or duplicating the batch.

## Receiving addresses

Each player enters a Lightning Address such as `alice@example.com`.
Both addresses must be syntactically valid and saved before entry invoices
are displayed or entry payments can be simulated. Domains are normalized to
lowercase; addresses persist across games and application restarts.

Recipients are locked as soon as either entry payment is confirmed. New
recipients can be saved before paying for the next match. A prepared payout
retains its original recipient even if a later match uses different addresses.

The side panel shows pending sats and a countdown separately from confirmed
paid-out sats. The payment log records UTC confirmation time, amount, player,
receiving address and payment type. Fake receipts explicitly say SIMULATED.
Receipts persist across matches; the window displays a bounded recent history.

## Backend-neutral execution

The shared Rust core exposes independent interfaces:

| Interface | Responsibility |
| --- | --- |
| `pot::MatchPot` | Account for damage, close timed batches, allocate the final pot; no networking |
| `wallet::AddressInvoices` | Resolve a receiving address into an exact-amount invoice |
| `wallet::Wallet` | Create/confirm entry invoices, decode outgoing invoices, submit payments, look up outcomes |

The local host accepts injected `Arc<dyn Wallet>` and
`Arc<dyn AddressInvoices>` through `open_with`. The public GUI factory still
selects the fake implementations. Real NWC connection/setup remains a separate
integration; neither entering an address nor generating a fake receipt connects
a real wallet.

A production address provider implements [LUD-16](https://github.com/lnurl/luds/blob/luds/16.md)
and [LUD-06](https://github.com/lnurl/luds/blob/luds/06.md): HTTPS discovery,
callback invoice request in millisats, advertised bounds, metadata binding,
expiry, and protocol error handling. It must not make the payment itself.
The current FakeAddressInvoices never contacts the entered domain.

An NWC wallet adapter maps the wallet operations to make_invoice,
lookup_invoice and pay_invoice as supported by the connected wallet, and uses
a BOLT11 decoder for local validation. The batch scheduler is unchanged.
See [NWC's payment flow](https://docs.nwc.dev/introduction/introduction-to-nwc).

The dispatcher verifies the resolved amount and payment hash. It persists
the recipient, invoice and hash **before** marking an outgoing intent in flight.
It reuses that exact invoice after recovery, never requests another invoice
for an existing prepared payout, and rejects reused payment hashes across
matches. Unknown outcomes are looked up rather than blindly resent. Definitive
failure stays visible and does not unlock a new funded match.

The host has bounded wallet/resolver timeouts. Fees are separate from the
advertised match funds; the adapter receives the configured fee limit
(`SATS_ARENA_MAX_PAYOUT_FEE_SATS`, default 10 sats per outgoing payment).
Fake fees are zero. Fake state is marked and cannot be opened with a real
wallet mode.

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
