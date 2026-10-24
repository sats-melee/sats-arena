# Sats Arena server

A small, self-hostable server that holds balances for a group of friends
playing Slippi with Lightning stakes. Gameplay stays peer-to-peer through
Slippi; this server only keeps the ledger, settles matches and talks to a
Lightning wallet.

**Whoever runs a server holds their players' sats.** Back up the database.

## Layout

- `protocol/`: wire types shared with the Dolphin client.
- `server/`: ledger (SQLite), match settlement, HTTP API, wallet backends.

## Run

```sh
cp sats-arena.example.toml sats-arena.toml   # set invite_code
cargo run --release -p sats-arena-server -- sats-arena.toml
```

The `fake` wallet moves no real sats, so the whole flow can be tested
without a node. NWC support is next.

## How matches settle

Every player in a match sends a `MatchReport` with their own token. The
server pays out only once every player has reported and all reports agree.
Disagreement marks the match `disputed` and moves nothing.

- **Netplay:** each Dolphin reports for its own player.
- **Local:** the one Dolphin holds every player's token and sends one report
  per player.

Payouts follow `[rules]`: the victim pays `sats_per_percent` per 1% of
attributed damage, then each loser pays the winner `win_sats`. Nobody pays
more than their balance.

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

Withdrawals reserve the amount plus a fee buffer before paying. Unspent fees
are refunded, and a payment with an unclear result stays pending until the
server confirms it, so nothing is paid twice.
