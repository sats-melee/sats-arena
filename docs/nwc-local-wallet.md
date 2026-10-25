# Native local wallet: launcher first, environment for development

## Sandbox development

Use `make sandbox` in the adjacent launcher/workspace. It provisions five
reusable Alby NWC test wallets, prefills the four player receiving addresses,
and keeps Sandbox state separate from live funds. The native Sandbox uses the
same invoice creation, incoming confirmation, damage batching and payout logic
as live NWC. Its **Pay from test wallet** button sends a real NWC request from
the corresponding sandbox wallet; it never marks an invoice paid locally.
Only the internal unit-test fake wallet has a simulated-payment action.

The launcher sends `backend: "sandbox"`, the Arena NWC URI, and four
`sandbox_players` (each `nwc_uri` and `address`) over the private stdin pipe.
Credentials are not persisted in match state or exposed to the renderer.
Sandbox receiving addresses stay bound to these test wallets. Its ledgers live
under `<Dolphin user>/SatsArenaSandbox/<public-wallet-fingerprint>/`.
See `slippi-launcher/docs/sandbox.md` for agent-friendly setup and test commands.

## Configure and launch

In the launcher, choose **Wallet → Nostr Wallet Connect**, save the connection,
select the Melee ISO, then **Play local Arena**. The main process reads the
existing protected secret store. The renderer can inspect saved/encrypted status,
but cannot retrieve a previously saved connection string.

The main process sends one private stdin JSON line to the native child and
closes the pipe. `SATS_ARENA_WALLET_CONFIG=stdin` identifies this transport.
The payload is version 1, backend fake/nwc, and optional nwc_uri. It is not written
to command-line arguments, the production child environment, settings JSON,
Dolphin INI, or logs. Replay/netplay/configuration launches strip Arena wallet
environment variables and do not read the saved credential.

The neighboring Linux development build launches through
`Ishiiruka/Tools/arena/launch-native.sh` to use its build container's libraries.
Packaged distributions need the matching native build and runtime dependencies.
The generic Host/Connect server preview is a separate older integration.

## Developer override

`SATS_ARENA_NWC_URI` deliberately overrides the saved launcher connection and
selects NWC for local play. `SATS_ARENA_WALLET_BACKEND=fake|nwc` explicitly selects
a backend; fake plus an NWC URI is rejected as conflicting. No .env file is
required or auto-loaded. Developer .env files are git-ignored.

Run the existing native launcher script from a host shell with the environment
variable set, or start the Electron launcher from that shell. Keep the actual
connection in the local environment/store; it contains a spending credential.
Invalid/missing configuration and failed capability checks block payments.
They never silently switch to a fake wallet.

## Wallet behavior and fees

The NWC adapter uses the maintained Rust NWC SDK with NIP44 encryption and NIP04
compatibility. Startup probes get_info, requiring make_invoice, lookup_invoice
and pay_invoice, plus Bitcoin-mainnet invoice validation. A startup probe creates
no invoices and sends no payment. The connection identity is a hash of public
wallet/client identities and relay addresses; the full URI is not persisted in
match state.

Standard NWC pay_invoice has no universal per-payment fee cap. NWC therefore
uses the wallet connection's fee/spending policy, explicitly represented by
`NWC_WALLET_MANAGED_FEES`. A finite `SATS_ARENA_MAX_PAYOUT_FEE_SATS` is rejected
for this backend. Routing fees come from the arena wallet outside player pots;
provide wallet liquidity for them. The sidebar labels live mode and wallet-managed
fees. One process holds an exclusive lock on each financial state folder, so
a second process cannot replay the same ledger. No GUI **Simulate payment** buttons appear in NWC mode.

Real entry invoices are created only after all human destinations are verified,
and each invoice is bound to that generation's recipient. Confirmations use the
wallet's incoming settled status. An outgoing payment acknowledgement requires a valid preimage; recovery uses
the wallet's confirmed outgoing transaction state.
unknown/lost responses remain pending and recovered intents are looked up rather
than sent again. Ambiguous outcomes never release funds for a replacement payout.

## State and late payments

Default paths are `<Dolphin user>/SatsArenaFake/` and
`<Dolphin user>/SatsArenaLive/<public-connection-fingerprint>/`.
`SATS_ARENA_FAKE_DATA_DIR` and `SATS_ARENA_LIVE_DATA_DIR` override these roots for
tests. A live connection cannot open fake state or another connection's state.
Rotating to another connection preserves the previous folder for recovery.

Unpaid retired hashes remain monitored after an address/team/roster change,
invoice replacement or room reset. A late paid invoice is recorded durably and
returned to its originally bound recipient. It never funds the current pot.
Wrong-amount receipts similarly become return obligations. A pending known return
blocks new admission; prepared returns use immutable invoices and persist
InFlight before sending. Unknown old lookups stay monitored without blocking
other accounting. Current incoming polling is throttled; retired hashes use a
bounded rotating scan, so an old QR's refund can take longer than a current entry
confirmation. Keep the arena host running to process those returns.

## Validation

Encrypted local mock-relay tests exercise NIP44 and NIP04 requests, capability
checks, invoice creation/settlement, valid/invalid proofs, a lost payment response
and recovery without a second send. Local host tests cover late-paid stale QRs,
immutable old recipients, expiry, overpayment and recovered returns. Launcher
synthetic-child tests verify exact private handoff and environment isolation;
an opt-in test launches the actual native Dolphin through the same local action
with a fake wallet. No real credential or Lightning payment is used by these tests.

An actual mainnet end-to-end match still requires a user-configured connection
and funded arena wallet. A small live trial should check invoice confirmations,
recipient wallet receipts, final pot conservation and fees against the log.
