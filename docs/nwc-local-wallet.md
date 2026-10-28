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

Wallet settings check a saved NWC connection when opened and after saving.
The green check means the wallet replied to `get_info` and advertises the
required methods. **Check connection** retries that read-only check.
**Generate test invoice** separately creates a 1-sat invoice with a ten-minute
expiry, validates its BOLT11 signature/amount, looks up its pending state,
and displays the invoice and a scannable QR. Neither action pays an invoice or
opens a match ledger. A saved string alone is not treated as verified.

The bundled `sats-arena-wallet-check` executable uses the same Rust NWC adapter
as the game. Its credential arrives over private stdin, and only a redacted
result and optional invoice/QR return to the renderer. The developer wrapper
is `Ishiiruka/Tools/arena/check-wallet.sh`; `--invoice` explicitly opts into
invoice creation. Do not pass credentials on its command line.

A timeout means the wallet did not answer; it does not prove the secret is
invalid. Check that its wallet/Hub is online and unlocked and the connection
has not expired or been revoked. Reachable relays can still serve an old
public capabilities event while the wallet is offline. `core/examples/nwc_probe.rs`
can diagnose read-only replies with negotiated, NIP44, and NIP04 encryption,
without printing wallet errors or sending payments. Live diagnostics require
explicit user authorization; routine development uses Sandbox.

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

Payouts carry a saved, human-readable label: **Sats Melee — Damage payout**,
**Sats Melee — Match won**, the winning team's color and **team victory**, or
**Sats Melee — Entry refunded**. Losing players' unbatched damage paid at the
end remains labeled as damage; no-contests and CPU-only victories return entries.

When resolving the recipient's Lightning Address, the LNURL provider sends the
label as a LUD-12 `comment` only if `commentAllowed` is positive. It respects
the provider's character limit and the callback URL size, using proper URL
encoding. Providers without comments continue to receive payments normally.
Invoice signatures, amount and expiry remain checked. Matching description
hashes/text are recorded when available. Forwarding services whose upstream
wallet uses different descriptions remain bound to their validated HTTPS
callback; the report distinguishes this from a matching metadata commitment.

The full label is saved with the prepared invoice before payment and copied
to the confirmed receipt. Recovery reuses that invoice and label; older prepared
invoices without labels remain readable and are not replaced or resent to add
a comment. The payment log shows the reason and team without repeating the
application name on every line. Whether a receiving wallet displays the comment
depends on its provider and UI. `core/examples/nwc_receipt_memos.rs` can check
receiver-side NWC metadata read-only, with credentials supplied on private stdin.

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

After the game ends, unresolved payouts move into durable `pending_matches`
snapshots. A new game requires fresh entries and can proceed while those earlier
payouts recover. Each snapshot keeps the original pot, recipients, invoices and
receipts. InFlight payments are looked up by their original hash, never resent
because of a timeout. Failed or unknown payouts keep their reserved amount and
remain visible without blocking the current match. Settled recovery snapshots
are archived only after their receipts have been persisted.

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

## Responsive payment worker and timing diagnostics

The coordinator owns all accounting and durable writes. It runs a continuous
async runtime, publishes snapshots on a 20 ms tick, and accepts damage credits
while network jobs are in flight. Up to four payout jobs and four incoming
invoice/receipt jobs run concurrently. A slow recipient does not serialize the
other recipients. The configured damage window remains unchanged. Old incoming
invoice monitoring has a global budget of four requests per two seconds; history
growth does not multiply the polling rate.

Each payout advances through recipient invoice preparation, durable InFlight,
wallet submission, and confirmed receipt. Network jobs carry immutable IDs and
return facts; they never mutate funds. The invoice/hash and InFlight transition
are saved before submission. Timeouts and restarts only look up that original
hash. Replies for an abandoned entry invoice retain its old amount/address for
late returns. Generated incoming invoice hashes are durably remembered across
players and rounds, so a reused invoice cannot fund two entries. Command errors and background payment errors are separate ABI
fields, so network trouble cannot be mistaken for a rejected damage credit.

The shared LNURL client caches validated discovery metadata for five minutes
and reuses HTTPS connections with DNS answers pinned to validated public IPs.
Pinned clients expire after one minute, then DNS must be validated again.
Redirects still require separate HTTPS/public-host checks. A failed callback or
invoice validation invalidates discovery metadata. Payable invoices are never
cached or reused across payout IDs.

`payment-timings.jsonl` beside `prepaid-match.json` records queue wait, recipient
invoice retrieval, NWC submission/lookup, incoming receipt lookup, and time from
a due payout to confirmation. `started_at_ms`/`at_ms` measure actual network
intervals; `observed_at_ms` shows when the coordinator processed the reply.
These logs contain IDs and timings, never credentials, invoices or addresses.
They rotate at approximately 2 MiB, keeping one previous file.

Use `dolphin-controller-mcp/scripts/arena_payment_timings.py PATH` for per-stage
latencies and observed payment concurrency (`--match-id ID` filters one match).
Timing success does not replace
receipt checks or the accounting audit. Sandbox funding buttons and initial
receiving verification are lobby commands; the in-game hot path never waits on
them. The worker is concurrent in its I/O while fund mutations remain serialized.

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
