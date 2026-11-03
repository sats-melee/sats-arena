# Phone rooms

The phone server is a WebSocket relay and a React SPA in `server/frontend`.
The native desktop remains the only invoice/payment authority. The room server
has no wallet connection, ledger, funding rules, payment execution, or controller
input stream. It relays player addresses and requests for the desktop's current
entry invoice, and displays the desktop's damage totals and payment activity.

## Deploy on Fly.io

The root `Dockerfile` builds the React frontend and Rust relay into one image.
It runs `sats-arena-server rooms` on port 8080 as an unprivileged user, serving
the built frontend from `/app/frontend`. No wallet credentials, database, volume,
or Node installation are needed at runtime. The build context excludes local
configuration, ledgers, dependencies, and build outputs.

Install [flyctl](https://fly.io/docs/flyctl/install/), then run these commands
from the repository root. Choose a globally unique app name:

```sh
fly auth login
fly launch --copy-config --no-deploy --ha=false --name YOUR-UNIQUE-APP --region iad
fly deploy --remote-only --ha=false
```

Fly builds the container remotely; the desktop does not host the service and
does not need Docker installed. Fly's proxy supplies HTTPS and WebSocket
upgrades. The included `fly.toml` checks `/health` and keeps the relay running.
No reverse proxy container or wallet secrets are required.

Keep **exactly one Machine in one region**: rooms are held in process memory,
so independently routed replicas cannot share codes or slot claims. Always use
`--ha=false` when deploying. If an existing app has extra Machines, run
`fly scale count 1` before using it for rooms. Deployments briefly disconnect
phones and cause the desktop to create a new room code; they do not affect the
desktop payment ledger or gameplay. Automatic stopping is disabled so idle
rooms keep their codes.

After deployment, check the service and logs:

```sh
curl --fail https://YOUR-UNIQUE-APP.fly.dev/health
fly status
fly logs
```

Enable **Phone room** in the launcher and set its URL to
`https://YOUR-UNIQUE-APP.fly.dev`, then reopen LOCAL. Open the same website on
a phone and enter the code displayed by the desktop. This works across different
networks without opening an inbound port on the desktop. Request an invoice
from the phone and verify it matches the desktop's invoice before testing a
payment with the normal Alby Sandbox workflow.

## Run the server for development

Use Node 22.12+ to build the SPA. Node/npm are build tools, not server runtime
dependencies:

```sh
cd server/frontend
npm ci
npm run build
cd ../..
cargo run --release -p sats-arena-server -- rooms --bind 127.0.0.1:8738
```

`rooms` starts only the phone service; it does not load the older balance server's
TOML config or open a wallet. `/health` identifies the room service.
The default asset folder is `server/frontend/dist` in the build checkout. To
distribute the binary, copy that folder too and pass its location:

```sh
./sats-arena-server rooms --bind 127.0.0.1:8738 --frontend /opt/sats-melee/frontend/dist
```

Put the service behind your HTTPS reverse proxy. Forward WebSocket upgrades and
preserve the Host header. Browsers use the same origin for the SPA and socket;
cross-origin socket handshakes are rejected. The desktop accepts plain HTTP only
for localhost development. A server intended for internet hosting should have
request/connection rate limits at the proxy. This first version bounds room count,
message sizes, pending requests, and per-phone command frequency.

For frontend development, run `npm run dev` in `server/frontend`; Vite proxies
`/rooms` and `/health` to the Rust server on port 8738.

### Local Wi-Fi preview

To use a phone on the same Wi-Fi, bind the room service to the LAN interface:

```sh
./target/debug/sats-arena-server rooms --bind 0.0.0.0:8738 --frontend server/frontend/dist
```

Use `http://127.0.0.1:8738` in the desktop's **Phone room** setting. On the phone,
open `http://<this-computer-LAN-IP>:8738` and enter the displayed room code.
The desktop still connects over loopback; a separately hosted desktop connection
still requires HTTPS. On local HTTP, **Copy invoice** selects and reveals the
invoice when the browser does not provide clipboard access. Use the browser's
Copy command or **Open Lightning wallet**. HTTPS supports direct clipboard copying.

## Connect the desktop

Rebuild the launcher and native Dolphin with the updated local Rust library.
In launcher settings, enable **Phone room**, save the room server's HTTPS origin,
and open LOCAL. Changes apply on the next LOCAL launch.
The native sidebar prominently displays the website address, six-character room
code, and total entry invoice, with the playable pot and fees shown underneath.
For a loopback server URL, the website address uses the computer's network IP so
phones on the same Wi-Fi can reach it; the desktop still connects over loopback.
There is no room QR code. Direct native development can set
`SATS_ARENA_ROOM_SERVER_URL=http://127.0.0.1:8738`; the launcher explicitly controls
this variable through its toggle.

On the phone:

1. Open the server website, enter the code, and select your P1–P4 controller slot.
   You can join before selecting a character, including after another player pays.
2. Save your receiving Lightning Address. The desktop performs its existing
   validation, including the fixed-address restriction in Sandbox.
3. Choose **Get entry invoice**. The desktop returns its existing current entry
   invoice once your receiving address is verified. Repeated
   requests do not replace invoices. Open your wallet or copy the invoice to pay.
4. The desktop's usual incoming lookup confirms payment. The website cannot
   simulate funding or start a match. View damage and payment activity while
   continuing to play with the controller. Selecting or changing a character keeps
   your paid entry; Start still requires a complete selected roster and all its payments.

Existing rules still govern entry amounts, surcharge, admission, receiver locks,
invoice expiry, payout batching and settlement. Phone requests include the
current match ID, so a delayed request cannot affect a later game. Only the
claimed player's invoice is sent to that phone. Address errors come from the
desktop, and the room server never asserts that an invoice was paid.

## Room lifetime

Each native session creates its own random room code and a private desktop
resume token. Slot claims are atomic and carry separate private resume tokens.
Refreshing the phone restores its claim from browser storage; closing the page
does not give its slot to someone else. **Leave room** releases it before the
first entry payment. Each paid player's receiving address stays locked;
unpaid players can still join and save theirs.
The same room and claims survive consecutive matches and brief disconnections.

Rooms are in memory. Disconnected desktop rooms can be resumed for ten minutes;
expired rooms are removed when another desktop connects. A server restart causes
the desktop to create a new code. This does not affect the desktop ledger or
ongoing gameplay/payment recovery. Phone status goes offline while the desktop
reconnects, and displayed invoices are disabled until it returns.

The SPA uses the launcher's wordmark, charcoal/red palette, and bundled Rubik and
Maven Pro fonts. Its tab icon and font licenses are included in the built assets;
fonts load from your own server.

The SPA currently shows the current match's pairwise damage and the desktop's
last 100 payment activity entries. It does not add durable account history or
change the existing archived desktop payment records.

## Checks

```sh
cargo test -p sats-arena-server -p sats-arena-local
cd server/frontend && npm run build
```

The room transport test exercises HTTP Upgrade and WebSocket frames over duplex
streams, so it also runs where OS listening sockets are unavailable. Payment
rules and invoice reuse tests use the deterministic unit-test wallet. Actual
invoice/payment acceptance still needs the controller-driven Alby Sandbox
workflow in the launcher's `docs/sandbox.md`.
