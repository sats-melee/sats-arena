# Per-match payments

This is the intended local Arena payment model. It replaces the reusable-balance
and budget-reservation UX. The remote balance/lobby API is an older prototype;
it does not define the match-sidebar flow.

## Player experience

- At the local character-select **PRESS START** prompt, show two fresh invoice
  QR codes: one for each player, for this match only.
- Both players save their receiving Lightning Addresses, then pay the full entry
  amount before Start unlocks. Prior winnings
  never replace an entry invoice. A paid QR stays visible with a Paid label.
- During play, finalized attributed damage accrues a payout to the attacker
  from the victim's unspent entry funds. The first whole sat starts a five-second
  window; later hits join without extending it. Each due batch is sent once. Display **Paid**, **Match funds left**, and **Sats earned**.
  Do not present a reusable account balance or a Reserved label.
- A normal winner receives all unspent funds from both entries, in addition to
  anything earned during play. There is no separate fixed win bonus.
- The next match always creates two new invoices and repeats this flow.

Initial configurable values: **500 sats per player**, **10 sats per 1% credited
damage**. These are defaults, not the definition of the model.

## Accounting

Each entry is a completed ordinary invoice payment. It is not a hold invoice.
The host holds the unspent match pot until it has paid the recipients. Payouts
are outgoing payments, not credits to a balance used to fund another match.

A damage payout consumes only the victim's remaining entry funds. Earnings do
not refill the recipient's match funds and cannot be lost again. Payouts stop
at zero; nobody is charged beyond the entry they paid. Fractional sat amounts
accumulate per victim before rounding down to whole sats. Only finalized,
attributed credits count; self/unattributed damage does not.

Example: both players pay 500 sats. P1 deals 8% to P2, earning 80 sats. Remaining
funds are P1=500, P2=420. If P2 wins, P2 receives the remaining 920 sats. Total
payouts are P1=80, P2=920. The next match still requires two fresh 500-sat payments.

For ties, no-contest, cancellation, or stopping emulation: retain payouts already
earned and return each player's unspent contribution to that player. Do not
invent a winner. An ambiguous/failed outgoing payment stays tracked and must
not be paid twice or treated as available pot money.

The conservation rule is: paid entries = unspent match funds + confirmed payouts
+ payouts owed/in flight. Routing fees require a separate operator policy and
are not silently deducted from this advertised pot.

## Integration and current limits

The gate remains at PRESS START. Game credits feed the payment worker during
play; game-end placements select the recipient of the leftover pot. Payouts use
stable IDs and durable state before calling a wallet. Replayed events, duplicate
end messages, and worker restarts must not produce duplicate payouts.

The current local host has a fake wallet. Test invoices and test outgoing
payments must be clearly labelled as simulated. Real use requires an outgoing
wallet adapter and a configured receiving destination for each player; drawing
a QR or updating a counter is not proof of an outgoing Lightning payment.

Detailed timing, address locking, receipt logging and wallet-adapter contracts:
[Five-second payouts](batched-payouts.md).
