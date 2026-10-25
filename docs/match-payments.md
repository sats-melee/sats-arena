# Local match payments: 2–4 players, FFA and teams

The embedded game uses `arena_pot::MatchPot`. The older balance/server API and
legacy two-slot pot are separate prototypes and do not define this flow.

Each active human verifies a receiving Lightning Address and funds a fresh entry
(default 500 sats); CPUs require neither and have no financial pot. All humans
must have fresh compatible verification and confirmed entries before Start.
The CSS roster, mode and team IDs are bound to admission and checked again
against the actual match-start setup. Changing a funded lobby settles returns
before a new roster receives fresh invoices. The four player cards scroll.

## Damage and batches

Finalized game credits carry attacker, victim, damage and an ordered sequence.
Opponent damage steals from the victim's remaining contribution at the configured
rate (default 10 sats per 1%). Self/environmental damage, friendly fire, CPU attacks
and hits against CPUs transfer nothing. CPU/friendly observations still advance
the sequence. Fractional sats accumulate per attacker/victim pair; victim funds
cannot go negative. Ordered processing preserves which attacker exhausted a pot.

Each recipient's first whole sat starts a five-second deadline. Later hits join
without extending it. Freeze each due batch under an immutable payout ID. New
hits start a new batch even if an older outgoing payment remains unresolved.
Duplicate credits are ignored; gaps are rejected before mutation. A missing or
incomplete stream cannot authorize a winner payout.

## Results

Flush unbatched damage earnings into final settlement payments, retaining all
previously frozen payout IDs. In FFA, return the winning human's own remainder
and award all losers' remaining contributions to that player. In teams, return
each winning human's own remainder and split all losing sides' remaining funds
equally among all winning humans, including eliminated teammates. CPU teammates
never take a share. Assign indivisible sats in ascending player-port order.

CPU-only winning side, draw, no contest, interrupted game or incomplete data:
keep accrued damage earnings and return unspent funds to their owners. There
is no fixed victory bonus, reusable balance, or re-spending of damage earnings.

Example: four humans pay 500 sats each. P1 steals 232 from P2 during play.
P1/P3 are Red and P2/P4 Blue. Blue wins: P1 keeps 232; P2 gets 268 of its own
remaining entry plus 500 from Red; P4 gets 500 of its own plus 500 from Red.
Final payouts: `[232,768,0,1000]`, totaling 2000 sats.

## Persistence and wallet boundary

The host persists received entries, ordered credits, recipient-bound invoices,
payout intent and confirmation. A recovered InFlight payment is looked up and
never blindly resent. An unresolved return blocks new funding. Schema 2 local
state migrates from two slots while retaining funds, payout IDs and fractional
carry. Completed games are archived before creating the next pot.

The wallet/provider interfaces are independent of NWC or another backend.
The GUI currently uses simulated funding and outgoing payments; receiving
verification alone uses live LNURL/BOLT11. No real sats move in gameplay tests.
Fees belong to the wallet execution budget, outside the advertised match pots.

At every transition, contributed sats equal remaining contributions plus open
or pending payouts plus confirmed payouts. See [batch contracts](batched-payouts.md)
and native `Ishiiruka/docs/evidence/four-player-payments/` for live evidence.
