# Local match payments: 2–4 players, FFA and teams

The embedded game uses `arena_pot::MatchPot`. The older balance/server API and
legacy two-slot pot are separate prototypes and do not define this flow.

Each active human verifies a receiving Lightning Address and funds a fresh entry
(default 500 sats); CPUs are excluded unless the launcher developer option
**CPU wallets** is enabled. With that option, CPUs use the same verification,
entry, damage, winnings and refund rules as humans. Every financial participant
must have fresh compatible verification and a confirmed entry before Start.
The CSS roster, mode and team IDs are bound to admission and checked again
against the actual match-start setup. Changing a funded lobby settles returns
before a new roster receives fresh invoices. The four player cards scroll.

## Damage and batches

Finalized game credits carry attacker, victim, damage and an ordered sequence.
Opponent damage steals from the victim's remaining contribution at the configured
rate (default 1 sat per 1%). Self/environmental damage, friendly fire, and hits
involving CPUs without wallets transfer nothing. All observations still advance
the sequence. Fractional sats accumulate per attacker/victim pair; victim funds
cannot go negative. Ordered processing preserves which attacker exhausted a pot.

Each recipient's first whole sat starts a five-second deadline. Later hits join
without extending it. Due amounts join one never-submitted queue total per
recipient. Original intents retain an audit link when consolidated into a new
payment. Submitted payments retain their immutable amount, ID and invoice.
Duplicate credits are ignored; gaps are rejected before mutation. A missing or
incomplete stream cannot authorize a winner payout.

## Results

Combine unbatched earnings, never-submitted queue totals, and final allocations
into one final payment per recipient. In-flight payments remain separate. In
FFA, return the winning financial participant's own remainder
and award all losers' remaining contributions to that player. In teams, return
each winner's own remainder and split all losing sides' remaining funds equally
among winning financial participants, including eliminated teammates. CPU
teammates with wallets take the same share as humans. Assign indivisible sats in
ascending player-port order.

Winning side with no financial participants, draw, no contest, interrupted game or incomplete data:
keep accrued damage earnings and return unspent funds to their owners. There
is no fixed victory bonus, reusable balance, or re-spending of damage earnings.

Example: four humans pay 500 sats each. P1 steals 232 from P2 during play.
P1/P3 are Red and P2/P4 Blue. Blue wins: P1 keeps 232; P2 gets 268 of its own
remaining entry plus 500 from Red; P4 gets 500 of its own plus 500 from Red.
Final payouts: `[232,768,0,1000]`, totaling 2000 sats.

## Persistence and wallet boundary

The host persists received entries, ordered credits, recipient-bound invoices,
payout intent and confirmation. A recovered InFlight payment is looked up and
never blindly resent. Unresolved earlier games recover in the background while
the next match uses fresh entries. Their status never gates a new match. A
separate note near the top of the native panel shows earlier pending payouts
and any failures needing attention, and says that play can continue. The note
clears once those payments are resolved; wallet errors cannot replace it.
Older local state migrates to schema 4 while
retaining funds, payout IDs and fractional carry. Schema 4 also persists the CPU
wallet option and consolidation audit links. Completed games are archived.

The wallet/provider interfaces are independent of NWC or another backend.
The launcher selects either simulated execution or a saved NWC connection.
An explicit developer environment URI can override that selection. The NWC
adapter uses actual receiving invoices, confirmations and payouts; verification
alone never selects a live wallet. Automated tests use simulated wallets or
an encrypted local mock relay, without moving real sats.
Fees belong to the wallet execution budget, outside the advertised match pots.

At every transition, contributed sats equal remaining contributions plus open
or pending payouts plus confirmed payouts. See [batch contracts](batched-payouts.md)
and native `Ishiiruka/docs/evidence/four-player-payments/` for live evidence.
