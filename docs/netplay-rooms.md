# Desktop netplay rooms

## In-game Create / Join

The same `sats-arena-server rooms` process serves phone rooms and desktop game
rooms. The native room client now talks directly to that server; ordinary users
do not need a descriptor file or the developer Python helper.

1. Set **Settings → Sats Arena → Room server** in the launcher. This is the same
   HTTP(S) origin used for phone matches; the phone-control toggle is independent.
2. Choose **Rooms** in the launcher. It opens a separate controller profile
   without handing wallet credentials to the emulator.
3. In the sidebar, choose two to four players (default four), FFA or teams, and
   team colors. Choose **Create Room**, pick a character, and press Start. Share
   the displayed eight-character code with the other players.
4. Each other player chooses **Join Room**, picks a character, presses Start,
   enters the code, and confirms. All clients connect using the existing
   Slippi peer gameplay path.
5. If the server enables [room payments](server-room-payments.md), each player
   enters their receiving address and pays their own entry QR. All configured
   receipts are required before the peers connect. The room stays open after
   each game; the host chooses **Next match** after settlement to prepare fresh entries.

There is no public room list. The native flow supports two to four players and
discovers its public UDP endpoint using STUN on the socket retained for gameplay.
The public backend at `https://melee.landaverde.io` was deployed on 2026-11-07.
Two-player internet games and a same-room rematch passed the external test
described at the end of this document. Earlier loopback-only notes below record
the tests available at those stages of development.

Room HTTP requests run on a background worker. It readies every participant,
starts shared match configurations, renews membership between games, and
leaves only on explicit departure or shutdown. Gameplay uses Direct or Teams semantics from
the immutable room configuration. Melee's full-width keyboard characters are normalized
to the server's ASCII hexadecimal codes. Bad codes and server errors appear in
the existing in-game error area.

The worker reuses its HTTP connection. After admission, temporary transport or
server failures retain the same membership and payment identity while pausing
new readiness and payment actions. Recovery is bounded to 90 seconds; an expired
membership still ends the session. Create/join requests are not blindly retried,
and payment deadlines are not extended.

Native room mode uses `SATS_ARENA_NETPLAY_ROOMS=1` with the existing
`SATS_ARENA_ROOM_SERVER_URL`. Optional development overrides are
`SATS_ARENA_ROOM_NAME` and `SATS_ARENA_ROOM_ENDPOINT` (reachable IPv4:UDP-port).
Without an endpoint override, the native client reserves the configured or
randomly selected local netplay port and asks public STUN servers for its mapped
IPv4 address and port. That same ENet socket is kept alive while waiting and
transferred into gameplay without closing/rebinding. Public and local bind
ports may differ. An optional LAN candidate is used when both peers advertise
the same public IPv4 address; shared carrier NAT can still require a reachable
endpoint override. Networks that vary mappings by destination may connect through
the existing incoming-port fallback, but this is not guaranteed. No gameplay
relay is configured. A STUN answer alone is not evidence of internet gameplay.

The multiplayer panel shows **Peer ping** in milliseconds: the highest recent
round-trip time from this client to another player. Its tooltip lists individual
player measurements. Unknown or three-second-old samples show a dash. **HIGH**
appears at 150 ms; values turn red at 200 ms. Room gameplay also keeps this
measurement in the game overlay when the panel is hidden, independently of the
legacy graphics ping toggle. This measures peer gameplay traffic, not HTTP to
the room server.

The Arena codeset exposes room menus only; attempts to use online matchmaking
without configuring room mode fail closed and never fall back to Slippi's
matchmaking. Local phone-match funding remains a separate launch flow.

## Backend and developer tools

Run the existing room server:

```sh
cargo run -p sats-arena-server -- rooms --bind 127.0.0.1:8738
```

The new HTTP routes coexist with the phone-room WebSockets and frontend. Free
rooms need no Slippi service, wallet, or database. An optional dedicated server
wallet enables [trusted room payments](server-room-payments.md). The legacy
balance-server mode does not mount these routes.

The backend coordinates rooms. The developer helper below can launch Dolphin
with an opt-in LAN descriptor. A gameplay relay and Nostr authentication are
not connected. Build and rules identifiers are client assertions, not attestation.
Use it locally for integration; public admission/rate limits still need work.

## Wire contract

Types live in `protocol/src/netplay.rs`. Requests and responses are JSON.

| Request | Authentication | Result |
| --- | --- | --- |
| `POST /netplay/rooms` | None | Create a room; return `{room, slot, token}` |
| `POST /netplay/rooms/{code}/join` | Invite code in path | Claim a free slot; return `{room, slot, token}` |
| `GET /netplay/rooms/{code}` | `Authorization: Bearer <token>` | Current room; also renew this member's lease |
| `POST /netplay/rooms/{code}` | Same bearer token | Apply command; return room, or `null` when closed |

Create and join use the same body:

```json
{
  "name": "Alice",
  "build": "arena-lan-test-1",
  "rules": "vanilla-1v1-test-1",
  "endpoint": "192.168.1.10:43113"
}
```

`build` must identify a compatible emulator/game/patch combination. `rules`
must identify identical gameplay settings. The eventual desktop client must
derive these from its actual configuration. Both identifiers must exactly match
the creator's values. Endpoints are literal IP:port socket addresses; use each
client's reachable LAN address for this milestone. The backend never connects
to submitted addresses or checks reachability. Duplicate endpoints are rejected.

Commands:

```json
{"type":"ready","ready":true}
```

```json
{"type":"start"}
```

```json
{"type":"leave"}
```

The host can close the room with `{"type":"close"}`. After a completed game,
`{"type":"next_match","match_id":"PREVIOUS_MATCH_ID"}` prepares the next
round without changing the code, membership tokens, endpoints, or configuration.
For a canceled pregame paid round, use its `payment.match_id`. Retrying that
same command returns the already prepared round. `room.round` starts at one
and advances only on this explicit command; `room.match_complete` indicates when
the host can advance. Free clients acknowledge their result with
`{"type":"finish","match_id":"CURRENT_MATCH_ID"}`; paid completion instead
requires the agreed financial reports and confirmed settlement.

The creator gets slot 0 and owns Start. Create accepts immutable `config`:
`{"player_count":4,"teams":true,"team_ids":[0,1,0,1]}` selects four humans in two
teams; omitting it defaults to two-player FFA. Two to four desktops can join,
one per configured slot. Joining or leaving resets readiness for everyone.
Starting requires exactly the configured number of players, all ready. Start
freezes the roster/endpoints and assigns one
32-character match ID (within Slippi’s 51-byte protocol limit). Repeating Start returns that same ID. The other clients poll GET to
receive the identical start descriptor. Character/stage selection, RNG setup,
and the actual netplay handshake remain the emulator integration's job.

Tokens are unique to each membership, never included in shared room snapshots,
and revoked on leave. The room code is an invitation: anyone possessing it can
join an available slot and then see the room's endpoints. Keep tokens private;
use HTTPS outside local development.

## Lifetime and disconnects

Poll GET every 15 seconds while in the room, including during gameplay. Every
member must renew within 120 seconds. A guest leaving or expiring removes only
that guest, keeping the room and code for the remaining players. The host can
prepare the next round before a replacement joins. A host closing, leaving, or
expiring closes the room; there is no host migration. A departed free-game peer
ends that roster, allowing an explicit next round. Paid interruptions refund
unstarted entries or retain uncertain active funds for review. Completed-report
grace and durable settlement continue even after the room closes.

The desktop client reuses its HTTP connection and retains its room token, slot,
gameplay endpoint, and payment match during temporary transport failures or server
errors. It pauses readiness and new payment actions until fresh state returns,
stops recovery after 90 seconds without a healthy polling cycle, and treats
401/404 as ended membership. This does not extend invoice or payment admission
deadlines. Ambiguous create/join requests are not replayed automatically.

Rooms are in memory, capped at 128, and lost on restart. A lost create/join
response can leave an abandoned membership until its lease expires. Optional
[trusted room payments](server-room-payments.md) use a separate durable financial
identity and the shared payment engine; start with Sandbox testing.

## Verification

```sh
cargo test -p sats-arena-server -p sats-arena-protocol
cargo clippy -p sats-arena-server -p sats-arena-protocol --all-targets -- -D warnings
```

HTTP router tests cover shared start configuration, readiness, build/settings
mismatches, host authorization, duplicate start, roster locking, four-slot
capacity, token isolation/revocation, malformed input, and lease expiry.

The native developer path below feeds this descriptor into the existing
`SlippiNetplayClient` setup path. The native HTTP client also forwards finalized
game observations to the shared `MatchPot` engine when the server enables
payments. Separate-machine LAN and internet traversal testing remain pending.

## Developer room client

`scripts/netplay-room.py` creates/joins a two-player LAN test room, readies the
client, writes a private start descriptor, and keeps the membership lease alive.
The creator starts automatically when both players are ready. It is a developer
helper, not the eventual terms-acceptance UI. Each run needs a new descriptor path.

```sh
python3 scripts/netplay-room.py --name Alice --endpoint 192.168.1.10:43113 \
  --build arena-lan-test-1 --rules vanilla-1v1-test-1 \
  --descriptor /tmp/alice-room.json
# The first output line contains the room code. On the other client:
python3 scripts/netplay-room.py --join ROOMCODE --name Bob \
  --endpoint 192.168.1.11:43114 --build arena-lan-test-1 \
  --rules vanilla-1v1-test-1 --descriptor /tmp/bob-room.json
```

Leave these processes running. The descriptor contains the public room snapshot
and local slot, never the membership bearer token. An optional command after
`--` launches the emulator with `SATS_ARENA_NETPLAY_FILE` set, inherited Arena
wallet variables removed, and `SATS_ARENA_FAKE_LOCAL=0` to disable the payment host.
If that command exits, the helper leaves the room.

The matching emulator branch `feat/room-multiplayer` reads the descriptor in
Direct mode and uses the existing Slippi peer connection code. The descriptor
parser accepts two to four ready players in contiguous slots with IPv4 endpoints
and matching room configuration. Use identical builds and vanilla gameplay rules for testing;
the identifiers alone do not apply game settings or prove compatibility.

`Ishiiruka/Tools/arena/netplay-smoke.py --players 4` prepares four isolated
pipe-controller profiles without copying normal wallet settings. Its `--launch`
option takes one descriptor path per player; `--rooms URL` uses the in-game
Create/Join flow and can test server payments. The profiles disable the embedded
payment host, retain the online menu, and write separate logs/replays. Select
Direct when using descriptor files. Official login/match reporting is bypassed in
this opt-in test path. Use `--stop` to stop only these test instances.

Run `python3 scripts/test-netplay-room.py --server http://127.0.0.1:8738`
against a local room server to exercise two real helper processes, descriptor
permissions, shared match state, and the child process's payment-disabled environment.
The native parser checks are included in `Ishiiruka/Tools/arena/test.sh`.

## Online payment capture

The private E6 damage-credit and E8 result packets cover local VS and online
games. The assembly guards are in `Arena/CaptureCredit.asm`,
`Recording/SendFrameStart.s`, `Recording/FlushFrameBuffer.asm`, and
`Arena/SendResult.s` in the sibling `slippi-ssbm-asm` repository. The local CSS
payment gate is likewise restricted to local VS. Local gameplay scene is 0x0202;
online gameplay is 0x0208.

Online simulation resets and replaces predicted E6 batches on resimulation.
Only finalized frames generate tracker credits. Normal results wait beyond the
rollback window; missing history, unfinalized damage, or incomplete results
freeze payments. Local admission remains confined to local VS. Room admission
instead requires every configured entry verified on the server, which feeds matching
client observations into the shared `MatchPot` engine once.

## Local gameplay evidence (2026-11-06)

Two isolated Dolphin instances connected through the local room backend and
played a normal stock match on Fountain of Dreams using FIFO controllers.
Alice dealt 19.3% damage; Bob lost four stocks through controller input. Both
completed replays agree on all 9,426 recorded frame/player states, initial
settings, and normal end result (Alice first, Bob second). No wallet was used.

That first run exposed match-ID truncation: a 64-character room ID was reduced
to 51 bytes in replay data. The backend now generates 32-character match IDs;
the native parser rejects IDs over 51 bytes. Tests cover both boundaries.

Private local evidence, logs, controller notes, and replays are under
`~/.local/share/sats-arena/netplay-test/completed-first/`. These artifacts are
not repository fixtures. This was same-machine loopback, not a test across two
physical machines or the public internet.

A second short gameplay run on the hardened build confirmed the room match ID
is preserved exactly in both replays (32 characters); the process was stopped
without a recorded game-end event. An occupied UDP port then produced the
in-game `LAN room connection failed` message while both emulator processes
stayed alive. These artifacts are in `completed-second/` and `occupied-port/`.
All test emulators, room clients, test sockets, and the temporary server were
stopped after verification.

## In-game menu verification (2026-11-06)

A later live test used the real Create Room menu and the existing controller
keyboard to enter its generated eight-character code in Join Room. Both clients
connected, played a complete stock match, and produced replays matching on all
4,621 recorded frame/player states, initial settings, damage, match ID, and normal
result. Alice dealt 14.65% damage and finished with four stocks; Bob finished
with zero. This used no descriptor files and no payments. Evidence is under
`~/.local/share/sats-arena/netplay-test/rooms-success/`; the comparison report
is `rooms-result.json` in the parent test directory.

The test exposed two fixed regressions: full-width keyboard code encoding, and
a redundant Ready request racing the host's Start. Native code now sends Ready
only when needed. The backend accepts repeated Ready(true) after Start when the
slot is already ready, while still refusing readiness changes to a started match.

The native HTTP regression test also exercises full-width code input and checks
Ready requests through a local proxy. Use a **free** room server, without
payment configuration, on a separate loopback port from payment tests. From the
`Ishiiruka` directory, compile and run inside the build container:

```sh
g++ -std=c++14 -fno-exceptions -DSATS_ARENA -Wall -Wextra -Werror -pedantic -pthread \
  -I Source/Core -I Externals/nlohmann \
  Tools/arena/RoomClientTest.cpp Source/Core/Core/Slippi/SatsArenaRoomClient.cpp \
  Source/Core/Core/Slippi/SatsArenaTracker.cpp -lcurl \
  -o build-arena/tests/room-client-test
python3 Tools/arena/test-room-client.py --server http://127.0.0.1:18749
```

The tracker source is required even for free rooms because the Arena room worker
also observes tracker state. In a separate terminal, from `sats-arena`, start
`target/debug/sats-arena-server rooms --bind 127.0.0.1:18749`; stop that process
after the test. This test uses native HTTP clients only and launches no emulator.

Launcher checks cover room launch, separate profiles, no wallet credential
handoff, imported controller bindings, and keeping local phone-match behavior.
Native tracker/payment-gate checks and assembly source/original-hook validation
also pass. Public internet connections remain untested.

## Server payment verification (2026-11-06)

Two native clients created/joined a room through the in-game menus using the
dedicated Sandbox server wallet. A confirmed 100-sat entry left the room blocked;
the second confirmed entry admitted the game. Both clients reported the same
three finalized credits totaling 10% damage on Battlefield. Independent wallet
receipts confirmed 10 Sandbox sats paid during play and 190 at normal settlement
to the agreed winner, with no outstanding payouts. Both final reports were
complete and identical. A server restart preserved that settled record.

A second room funded one 100-sat entry, then canceled through the controller.
The server recorded NoContest and returned the full 100 sats; independent
receipts confirmed the refund. No live wallet was used. Evidence is under the
launcher's ignored `.release/netplay-payments/`, including replays, screenshots,
durable observations, and receiver-side receipt audits. These were loopback
tests, not separate-machine or public-internet tests.

## Four-client verification (2026-11-06)

Four actual emulators joined a four-player FFA through the controller menus.
Three confirmed 100-sat entries kept the game blocked; the fourth admitted all
four peers. Seven agreed damage credits included hits after another fighter had
been eliminated. Independent receiving-wallet receipts confirmed 21 sats during
play and the remaining 379 to the winner, conserving the full 400-sat pot.
All four replays match on settings, normal result, and all 11,344 finalized
frames and player states.

A second four-client game used two teams with IDs `[0,1,0,1]`. Real teammate
hits caused 10% damage without reducing that teammate's entry. Enemy hits paid
13 sats. All four complete final reports agreed on the winning side; receiving
wallets confirmed totals of 207 and 193 sats to the two winning teammates.
All four team replays match on settings, normal result, and all 12,565 finalized
frames and player states. Neither test had outstanding payouts.

Evidence is under the launcher's ignored `.release/netplay-payments/`, in
`completed-four-ffa/`, `completed-four-teams/`, and the corresponding replay
comparison JSON files. These are same-machine loopback tests using Sandbox.

The native HTTP checks also cover four-player FFA/teams waiting for every member,
and a completed-result cleanup test proves the final credit/result reaches the
server before Leave, including retry after one rejected request:

```sh
python3 Tools/arena/test-room-client-finalize.py
```

The cleanup test is built and run by `Ishiiruka/Tools/arena/test.sh`; the command
above reruns it inside the build container from `Ishiiruka`. It uses a loopback
mock server without wallets. A member's accepted complete result receives lease
grace while other members finish reporting. Incomplete reports, peer loss, and
lease expiry still freeze remaining funds for review.

The same-machine tests exposed an older GLX graphics issue: the renderer used
the SGI swap-control call for VSync-off, although SGI rejects interval zero.
Graphics stalls also reproduced at the title menu before any room existed.
The renderer now prefers advertised EXT/MESA swap control. All four rebuilt
clients reported requested/effective interval zero without environment
overrides. The diagnostic Mesa-override run exercised four identical green
Luigis, shades 0–3, on Yoshi's Story, Dream Land, and Final Destination. The
rebuilt default client then ran the same four-fighter roster on Fountain of
Dreams without overrides, with matching replay states across all four peers. No
fighter shades or gameplay rules were changed to work around the graphics
wait. The blocked funded attempt was canceled and all four 100-sat refunds
were independently confirmed.

The final funded four-client interruption check paused only P4 with SIGSTOP
after all four matching game manifests reached Playing. While P4 remained
stopped, another client's incomplete report froze the server's pot in 7.326
seconds, before the 120-second membership lease. The saved result named no
winner and created no payouts. Frozen observations and ledger were archived
before any resume, Leave, or operator review, proving the freeze came from
peer-loss reporting rather than the later cleanup.
Offline reviewed NoContest settlement then returned 100 Sandbox sats to each
of the four participants. Independent wallet receipts confirmed every refund,
with nothing outstanding. Evidence is in `completed-four-disconnect/`; all
isolated clients and test servers were stopped afterward.

## Public internet check — 2026-11-07

Deployed the room backend to the existing Fly app, preserving the public site's
English/Spanish pages and links. Public create/join, ready/start, next-round,
guest Leave, and host Close contract checks passed. The public server remains
wallet-free; this check moved no money.

An actual native client behind the home NAT used automatic STUN, without an
endpoint override. A second native client ran on an isolated Fly VM in IAD with
an explicitly reachable UDP endpoint. Both used the same rebuilt client and the
public room server. They completed two normal two-player FFA games in room
`33E79DFB`: Dream Land, then Final Destination. The host clicked Next match;
room code, emulator processes, and each gameplay socket remained unchanged.
The two games received distinct match IDs and replay game numbers 1 and 2.
Guest Leave preserved the host's room. Host Close then displayed ROOM CLOSED;
neither action exited its emulator.

Both peers' finalized replay settings, results, player states, frame metadata,
and item fields agreed across 4,192 and 2,400 compared frames, including the
pre-game frames. The replay parser appends item records from repeated rollback
passes. The audit therefore clears the preceding pass's items on FRAME_START
before comparing the final pass; the original recordings are preserved. A raw
command audit verified complete passes, no items outside their frame, and no
replacement of an already finalized frame. No unused-item-field exception was
needed for these two games.

This was a connectivity and continuity check, not a smooth-play benchmark. The
route measured roughly 194–205 ms ping with visible slowdowns (about 40–47 FPS
in captured gameplay). The remote client used software rendering. Four-player
WAN, two residential NATs, restrictive/symmetric NAT, and WAN payments were not
validated by this run. No gameplay relay is configured.

Evidence is in the launcher's ignored `.release/internet-play/` directory, with
viewable clips and results on the Desktop in `Sats-Melee-Internet-Test/`. The
updated client was packaged and installed locally. A GitHub release was not
published by this test. The isolated remote VM and its private game copy were
removed after the evidence was collected.

## Brazil and peer-ping display — 2026-11-07

A second external run used a retained Fly VM in São Paulo (`gru`) and the same
home NAT client/public room server. Normal gameplay ping sampled 35–48 ms
(median 37 ms), compared with roughly 200 ms to the earlier IAD peer. The remote
VM's software rendering remained CPU-limited at around 30 FPS; this was not a
smooth-play benchmark.

Two free two-player games on Final Destination completed normally in room
`F4C741AF`, with opposite winners. Next match retained the room, emulator processes
and UDP sockets. Strict final replay states agreed across 7,285 and 4,446 compared
frames, including pre-game frames, without metadata exceptions. Native guest
Leave and host Close passed again without exiting either emulator.

The new peer-ping line and overlay were visible during gameplay. An explicitly
labelled eight-second, 180-ms outbound-delay injection on the isolated test VM
showed red HIGH labels around 215–221 ms. The original queue configuration was
restored exactly and normal ping returned. The overlay also remained visible
with the panel hidden and the legacy graphics ping toggle disabled.

The updated AppImage was installed locally. Clips and results are on the Desktop
in `Sats-Melee-Brazil-Test/`. A reusable testing skill is installed outside the
repositories at `~/.codex/skills/sats-arena-internet-tests/`. The Brazil VM is kept
running at the user's request; its game assets remain private and its emulator
can be stopped between runs. Four-player WAN, two residential NATs and WAN
payments remain untested.
