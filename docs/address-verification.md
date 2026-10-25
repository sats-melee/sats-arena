# Receiving-address verification

Each address has its own **Verify** button that performs a live receiving
check independently of the funding wallet and the other player's address. Both players need a current,
compatible result before entry QR codes are displayed or funding is accepted.

For each address, the background worker:

1. Normalizes the address and discovers its HTTPS LUD-16 endpoint.
2. Validates the LNURL-pay response, callback, metadata and amount limits.
3. Requests a minimum whole-satoshi **unpaid test invoice**.
4. Parses BOLT11, checks its signature, exact amount, Bitcoin mainnet currency,
   creation time and at least 30 seconds of remaining validity.
5. Checks that a description hash commits to the raw LNURL metadata, or that a
   directly signed description exactly matches its advertised text/plain value.

No payment is sent. Verification does not establish ownership of the address,
prove successful routing, or guarantee that a later payment will succeed.
Each eventual payout requests and validates a fresh invoice.

A red message at the top blocks match start until both current addresses are
verified. Editing one address invalidates only its displayed verification.

## Sidebar states

- **Not verified**: no network check for the current input.
- **Checking receiver…**: background network checks are running.
- **Verified**: valid receiving endpoint and test invoice; advertised range shown.
- **Failed**: provider/network/protocol error shown beside that player.
- **Expired**: more than ten minutes since verification; verify again.
- **Incompatible**: receiving limits cannot cover one-sat batches through the
  complete current match pot. Funding stays disabled.

Changing an input hides the old verification and entry QR codes until checked
again. After the first entry payment, addresses stay locked; the same addresses
can still be rechecked if their verification expires before Start. Results are
persisted with their bound address and time, and carried across matches only
while fresh. Syntax-only legacy address records need a new live check.

HTTPS redirects are limited and revalidated, including cross-domain receiving
callbacks such as Alby. DNS answers are checked for public addresses and pinned
for each request. HTTP downgrades, private/local targets, URL credentials,
unexpected ports, oversized responses and mandatory unsupported payer fields
are rejected. Requests have bounded timeouts; the GUI and emulation remain
responsive. No endpoint is checked automatically while typing.

## Live checks requested by the user

On 2026-10-25, both issued valid, signed **1-sat unpaid mainnet invoices**:

| Address | Advertised receiving range | Callback | Invoice description |
| --- | --- | --- | --- |
| nic@landaverde.io | 1–10,000,000 sats | getalby.com | Metadata hash |
| moldyshelf891@walletofsatoshi.com | 1–100,000,000 sats | livingroomofsatoshi.com | Matching signed text |

The landaverde.io endpoint redirects to its www host before returning the Alby
callback. Wallet of Satoshi returns a direct signed description; it is checked
against its advertised text rather than silently skipping description checks.

These are time-specific capability checks, not permanent assurances. Current
results and test-invoice hashes are in the native evidence directory. Full
test invoices are not exposed as funding invoices or submitted to a wallet.

## Reproduce without paying

```sh
cargo run -p sats-arena-core --example verify_addresses -- \
  nic@landaverde.io moldyshelf891@walletofsatoshi.com
```

This command discovers endpoints and requests unpaid invoices. Ordinary tests
use signed local fixtures and an injected verifier; they do not contact public
services. Tests cover amount/currency/signature/description/expiry checks,
unsupported limits and payer data, unsafe URLs, stale checks, failed providers
and funding gates.

The `LnurlClient` implements both `AddressVerifier` and `AddressInvoices`.
It can resolve real receiving invoices for a future NWC wallet adapter without
changing gameplay or batching. The GUI still uses a simulated funding/payout
wallet; its live address verification does not enable real payments.

Protocol references:
[LUD-16](https://github.com/lnurl/luds/blob/luds/16.md),
[LUD-06](https://github.com/lnurl/luds/blob/luds/06.md),
[BOLT11 parser](https://docs.rs/lightning-invoice/0.34.1/lightning_invoice/struct.Bolt11Invoice.html).
