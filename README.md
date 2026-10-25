# Sats Arena payment engine

A shared Rust engine for local games and a self-hostable remote server.
Gameplay stays in Dolphin; the engine keeps player balances, settles agreed
game reports, and talks to a Lightning wallet.

**The wallet operator holds the players' sats in both modes.** Back up the
database and keep it paired with its wallet. Ordinary deposits are accepted
payments, not hold invoices: closing a game does not automatically refund them.

## Layout

- `protocol/`: wire types shared with the Dolphin client.
- `core/`: the engine, SQLite ledger, settlement, and wallet interface. No HTTP
  listener, Electron, or dependency on a specific node installation.
- `server/`: a thin HTTP wrapper and host configuration for that same engine.
- `local/`: fake-only C ABI host for Dolphin, with its own background worker.
  No local listener or server process. `local/include/sats_arena_local.h` is
  the C++ boundary; the payment rules still live only in `core/`.

## Local mode (no server)

```sh
cargo run -p sats-arena-core --example local
```

This fake-wallet demo deposits funds for two players, settles a local game,
and withdraws winnings by calling the engine directly. It opens no network
listener and moves no real sats. The same crate can be linked into Dolphin
through its existing Rust/C++ bridge on a background runtime.

The native Dolphin build links the fake-only embedded host into its sidebar.
Melee boots immediately. At the character-select **PRESS START** prompt, the
sidebar displays invoices and QR codes; the game transition waits for both
500-sat budgets. A queued Start continues once both are funded. Normal game-end
placements settle the win bonus automatically, then the next match repeats
the funding loop without restarting emulation. Confirmed balances are reused.
See `Ishiiruka/docs/LocalFundingPreview.md`. The host uses a fake wallet and
test QR codes; damage billing is off.

**Not wired yet:** launcher Local/Remote mode selection and the real NWC wallet
adapter. The launcher's current Host button still starts the server preview.

## Remote mode

```sh
cp sats-arena.example.toml sats-arena.toml   # set invite_code
cargo run --release -p sats-arena-server -- sats-arena.toml
```

The server preview explicitly uses an automatically paid fake wallet. The
engine's default test wallet instead starts invoices unpaid, so confirmation
and expiry can be tested. NWC support is next; its config is accepted, but the
server refuses to start that backend rather than pretending to make payments.

## Payment flow

1. Create a normal invoice for a player's deposit. Their balance stays unchanged.
2. Look it up with the wallet. Only a confirmed paid invoice with the matching
   amount credits the balance, once.
3. Create a lobby and have each player ready/reserve their maximum-loss budget.
   The host can start it only once both are ready; Start is single-use.
4. Games move reserved sats between database balances, not Lightning payments
   per hit. Unused budgets return to available balances on settlement.
5. A withdrawal reserves its amount and a routing-fee budget before payment.
   Success refunds unused fees; a definite failure returns the reservation.
   An unknown result stays pending and is looked up, never blindly sent again.

Both modes use the same bounded command worker, which also rechecks pending
payments every 15 seconds. See [payment-flow.md](docs/payment-flow.md) for the
state transitions and current limits.

## How matches settle

Every player in a match sends a `MatchReport` with their own token. The
server pays out only once every player has reported and all reports agree.
Disagreement marks the match `disputed` and moves nothing.

- **Netplay:** each Dolphin reports for its own player.
- **Local:** the one Dolphin holds every player's token and sends one report
  per player.

Payouts follow `[rules]`: the victim pays `sats_per_percent` per 1% of
attributed damage, then each loser pays the winner `win_sats`. Nobody pays
more than their reserved budget (plus winnings received in that game).
Reports without an admitted, started lobby are rejected. Protocol version 2
requires this funding flow; legacy unadmitted games aren't settled implicitly.

## API

All routes except `register` and `health` take `Authorization: Bearer <token>`.

| Method | Path | Body |
|---|---|---|
| `POST` | `/v1/register` | `{name, invite_code}` → `{player_id, token}` |
| `GET` | `/v1/account` | |
| `POST` | `/v1/deposits` | `{amount_sats}` → invoice |
| `GET` | `/v1/deposits/{id}` | |
| `POST` | `/v1/withdrawals` | `{bolt11, amount_sats}` |
| `GET` | `/v1/withdrawals/{id}` | |
| `POST` | `/v1/matches/report` | `MatchReport` |
| `POST` | `/v1/lobbies` | `LobbyRequest` (ID, mode, roster, budget per player) |
| `GET` | `/v1/lobbies/{id}` | Current status and available/reserved balances |
| `POST` | `/v1/lobbies/{id}/ready` | Reserve the authenticated player's budget |
| `POST` | `/v1/lobbies/{id}/start` | Creator consumes the ready lobby's start authorization |
| `POST` | `/v1/lobbies/{id}/cancel` | Release reservations before play only |

Withdrawals reserve the amount plus a fee buffer before paying. Unspent fees
are refunded, and a payment with an unclear result stays pending until the
server confirms it, so nothing is paid twice.

## Check

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Tests exercise ordinary deposit confirmation, duplicate requests, expiration,
concurrent withdrawals, lost responses, timeouts, caller cancellation, restart
recovery, identical local/netplay settlement, and the HTTP wrapper. This is an
early prototype, not an audited real-money service.
