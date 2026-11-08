# Trusted room payment host

The existing `rooms` server can also custody room entries. Controller inputs
still travel directly between emulators. Each emulator reports its finalized
match manifest, damage sequence, and result; the server pays only matching
observations from every participant. This is a friends-only trust model, not an
independent gameplay referee.

Rooms support two to four human players, free-for-all or teams, with a fresh
funded record for every game in the same room. The host selects the
player count and team assignments before creating the room. This immutable
configuration locks the financial roster before anyone can fund an entry;
actual gameplay must use the same roster. Teams use IDs 0, 1, or 2 with at least
two opposing sides, including uneven three-player teams. Payment configuration
does not alter the phone WebSocket protocol or require wallet credentials on
either emulator.

After confirmed settlement the host chooses **Next match**. The room code,
membership, and receiving addresses stay in place; invoices, receipts, damage
sequences, and game identity start fresh. Winnings do not pay the next entry.
An expected prior `match_id` makes retries idempotent. Old records remain
available through their original durable endpoints. A new peer filling a vacated
slot gets a new membership token and does not inherit the old player's address
or entry. A canceled pregame round must finish refunding before the next round.

## Configuration

Use a dedicated server wallet. Store its configuration in a private file outside
the repository, with permissions `0600`; never reuse a saved client wallet.
For Sandbox development the file has this shape (replace all placeholders):

```json
{
  "backend": "sandbox",
  "nwc_uri": "nostr+walletconnect://SERVER_CONNECTION",
  "entry_sats": 500,
  "sats_per_percent": 1,
  "batch_window_ms": 5000,
  "surcharge_bps": 0,
  "sandbox_addresses": [
    "nwcPLAYER0@getalby.com",
    "nwcPLAYER1@getalby.com",
    "nwcPLAYER2@getalby.com",
    "nwcPLAYER3@getalby.com"
  ]
}
```

Provide two to four assigned player addresses; the list must cover the selected
room size. Sandbox addresses must be distinct lowercase `nwc[a-z0-9]+@getalby.com`
addresses. The server connection must contain another distinct Sandbox `lud16`
address. Each slot accepts only its assigned receiving address. Production
configuration uses `"backend":"nwc"` and omits `sandbox_addresses`; use Sandbox
for development.

```sh
sats-arena-server rooms --bind 127.0.0.1:8738 \
  --payment-state /private/path/room-payments \
  --payment-wallet-file /private/path/server-wallet.json
```

Keep the state directory and dedicated wallet connection available across
restarts. The server locks this directory against another server or recovery
process. It stores wallet identity, terms, hashed member tokens, agreed reports,
and the existing shared payment ledger, without saving the wallet secret.
Restart with the same wallet to recover original outgoing payment attempts.
Temporary receiving-invoice failures retry automatically. Previously held hosted
debts resume this policy on restart, preserving the normal retry budget and
original hashes for uncertain sends; no local **Pay missed** action is required.

Incoming receipts and outgoing invoice hashes belong to one entry or debt
across every room sharing that wallet. The server rebuilds this ownership from
all retained ledgers before accepting requests. Keep settled and closed ledgers;
deleting or archiving them loses that history. The server stops at 4096 records
until a migration preserves the hash history separately.

Every configured member must join and ready, with a verified receiving address
and confirmed entry receipt, before Start. Matching game manifests from every
member are required before the financial game enters Playing. Damage batches are contiguous and duplicate
retries are safe. Matching complete final reports from every participant settle the pot through the
same engine used for local games. Disagreement or incomplete gameplay freezes
remaining funds for review.

## Room configuration

Create requests accept `config` (omitting it defaults to two-player FFA):

```json
{"player_count":4,"teams":true,"team_ids":[0,1,0,1]}
```

This is exposed unchanged as `room.config`. Active slots are the prefix
`0..player_count`; closed slots must use team ID 255, and every FFA slot must
use 255. Four-member rooms cannot Start with only two members, even when both
have already paid. Team damage and winner distribution reuse the local
engine; friendly damage advances the agreed sequence without moving money.
Winning teammates report the same winning side, canonicalized to that side's
lowest active player slot before report comparison. Legacy two-member payment
journals migrate to four storage slots while preserving their original roster,
member credentials and financial ledger.

## Durable member endpoint

Paid room snapshots include `payment` with a durable `match_id`. The normal
room Start descriptor uses this exact identifier. The same private membership
bearer authorizes these routes, even after an ephemeral room disappears:

| Route | Response |
| --- | --- |
| `GET /netplay/matches/{match_id}` | Current payment snapshot |
| `POST /netplay/matches/{match_id}` | Apply `set_address`, `report`, or `leave`, then return snapshot |

A member whose complete final report is already saved can leave without
interrupting other final reports: its existing 120-second lease grace remains.
Leaving without a complete final report retains the normal interruption rules.
Ready and Start remain room commands. Each member must poll its room or durable
financial endpoint within 120 seconds. Polling renews only that member's lease.
An expired room closes the financial admission even if clients still poll the
financial endpoint. Before Start, interruption refunds entries, including late
receipts. After Start, interruption freezes funds rather than guessing a
winner. Tick/recovery continues after room deletion. A server restart cancels
unstarted rooms and allows admitted members 120 seconds to resume reporting.

## Operator refund after review

Stop the payment server, inspect the game and saved observations, then run:

```sh
sats-arena-server rooms \
  --payment-state /private/path/room-payments \
  --payment-wallet-file /private/path/server-wallet.json \
  --refund-after-review MATCH_ID \
  --review-reason 'Both friends confirmed interruption; return remaining entries'
```

The same exclusive state lock is required. This records the review reason,
keeps agreed damage earnings, and returns remaining funds through the existing
NoContest settlement. It does not override an already agreed winner. If payment
confirmation is still pending after 30 seconds, restart the server to continue
recovering the original transfers. There is no HTTP operator recovery route.

## Verification

`cargo test -p sats-arena-server` covers admission, matching reports, duplicate
credits, self damage, disagreement, incomplete history, durable authorization,
room HTTP integration, cancellation, restart boundaries, lease expiry, and
review refunds using the shared host with a fake wallet. These tests do not
exercise external NWC providers or emulator gameplay.
The platform CI also runs these tests on Windows and both Mac architectures,
including cross-room invoice reuse, restart, and shared-wallet submission guards.
