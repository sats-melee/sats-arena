import { useEffect, useRef, useState, type FormEvent } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

type Player = {
  kind: number;
  address: string;
  verification: number;
  message: string;
  invoice: string;
  paid_sats: number;
  earned_sats: number;
  pending_sats: number;
};
type Snapshot = {
  match_id: string;
  phase: number;
  mode: string;
  roster_ready: boolean;
  locked: boolean;
  entry_sats: number;
  invoice_sats: number;
  players: Player[];
  damage_centi: number[][];
  logs: { at_ms: number; message: string }[];
};
type Membership = { code: string; player: number; token?: string };
const storageKey = "sats-melee-room";
function restore(): Membership | null {
  try {
    const saved = JSON.parse(localStorage.getItem(storageKey) || "null");
    return saved &&
      /^[A-Z2-9]{6}$/.test(saved.code) &&
      Number.isInteger(saved.player) &&
      saved.player >= 0 &&
      saved.player < 4 &&
      typeof saved.token === "string"
      ? saved
      : null;
  } catch {
    return null;
  }
}
const phases: Record<number, string> = {
  0: "Connecting",
  1: "Getting ready",
  2: "Ready to play",
  3: "Match in progress",
  4: "Match complete",
  6: "Payouts pending",
  7: "Needs attention",
};
const sats = (value: number) => value.toLocaleString();
const percent = (value: number) =>
  (value / 100).toLocaleString(undefined, { maximumFractionDigits: 2 });

function App() {
  const [membership, setMembership] = useState<Membership | null>(restore);
  const credentials = useRef(membership);
  const [code, setCode] = useState(membership?.code ?? "");
  const [player, setPlayer] = useState(membership?.player ?? 0);
  const [joined, setJoined] = useState(false);
  const [connected, setConnected] = useState(false);
  const [online, setOnline] = useState(false);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [address, setAddress] = useState("");
  const dirty = useRef(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [pending, setPending] = useState(false);
  const [requestedFor, setRequestedFor] = useState("");
  const socket = useRef<WebSocket | null>(null);
  const invoiceField = useRef<HTMLTextAreaElement>(null);
  const matchId = useRef("");

  useEffect(() => {
    if (!membership) return;
    let active = true;
    let retry: ReturnType<typeof setTimeout>;
    let current: WebSocket;
    function connect() {
      let accepted = false;
      current = new WebSocket(
        `${location.protocol === "https:" ? "wss" : "ws"}://${
          location.host
        }/rooms/join`
      );
      socket.current = current;
      current.onopen = () =>
        current.send(JSON.stringify({ type: "join", ...credentials.current }));
      current.onmessage = (event) => {
        if (!active) return;
        const message = JSON.parse(event.data);
        if (message.type === "joined") {
          accepted = true;
          const saved = {
            code: message.code,
            player: message.player,
            token: message.token,
          };
          credentials.current = saved;
          try {
            localStorage.setItem(storageKey, JSON.stringify(saved));
          } catch {
            /* This tab still owns its slot. */
          }
          setJoined(true);
          setConnected(true);
          setError("");
          setPending(false);
        } else if (message.type === "state") {
          setOnline(message.online);
          setSnapshot(message.snapshot);
          if (message.snapshot?.match_id !== matchId.current) {
            matchId.current = message.snapshot?.match_id ?? "";
            setPending(false);
            setRequestedFor("");
            setNotice("");
          }
          if (!message.online) {
            setPending(false);
            setNotice("Desktop is reconnecting. Your player slot is saved.");
          } else {
            setNotice(previous => previous === "Desktop is reconnecting. Your player slot is saved." ? "" : previous);
          }
        } else if (message.type === "error") {
          setError(message.message);
          setPending(false);
          if (!accepted && !message.message.includes("reconnecting")) {
            setMembership(null);
            credentials.current = null;
            setJoined(false);
            setSnapshot(null);
            try {
              localStorage.removeItem(storageKey);
            } catch {
              /* Storage is optional. */
            }
          }
        } else if (message.type === "result") {
          setPending(false);
          setError(message.error ?? "");
          setNotice(message.error ? "" : "Request received by the desktop.");
        } else if (message.type === "left") {
          try {
            localStorage.removeItem(storageKey);
          } catch {
            /* Storage is optional. */
          }
          credentials.current = null;
          setMembership(null);
          setJoined(false);
          setSnapshot(null);
          setPending(false);
        }
      };
      current.onclose = () => {
        if (!active) return;
        setConnected(false);
        setOnline(false);
        setPending(false);
        retry = setTimeout(connect, 2000);
      };
      current.onerror = () => current.close();
    }
    connect();
    return () => {
      active = false;
      clearTimeout(retry);
      current?.close();
      socket.current = null;
      setConnected(false);
    };
  }, [membership]);

  const own = snapshot?.players[membership?.player ?? 0];
  useEffect(() => {
    if (!dirty.current) setAddress(own?.address ?? "");
    if (own?.address === address) dirty.current = false;
  }, [own?.address, address]);

  function send(type: string, fields: object = {}) {
    if (
      !connected ||
      !online ||
      socket.current?.readyState !== WebSocket.OPEN
    ) {
      setError("Wait for the desktop to reconnect.");
      return;
    }
    setError("");
    setNotice("");
    setPending(true);
    socket.current.send(JSON.stringify({ type, ...fields }));
  }
  function join(event: FormEvent) {
    event.preventDefault();
    const next = { code: code.trim().toUpperCase(), player };
    credentials.current = next;
    dirty.current = false;
    setError("");
    setJoined(false);
    setSnapshot(null);
    setMembership(next);
  }
  const canEdit =
    connected &&
    online &&
    snapshot &&
    !snapshot.locked &&
    [1, 2].includes(snapshot.phase) &&
    own?.kind === 0;
  const canInvoice =
    connected &&
    online &&
    snapshot &&
    [1, 2].includes(snapshot.phase) &&
    own?.kind === 0 &&
    !own.paid_sats &&
    !!own.address;
  const activePlayers =
    snapshot?.players
      .map((p, i) => ({ ...p, index: i }))
      .filter((p) => p.kind < 2) ?? [];

  return (
    <main>
      <header>
        <a className="brand" href="/">
          SATS<span>MELEE</span>
        </a>
        <span className="eyebrow">PHONE ROOM</span>
      </header>
      {!joined ? (
        <section className="join card">
          <p className="eyebrow accent">LOCAL PLAY. YOUR PHONE.</p>
          <h1>Grab your spot.</h1>
          <p className="muted">
            Enter the room code shown in Sats Melee, then choose your controller
            slot.
          </p>
          <form onSubmit={join}>
            <label htmlFor="code">Room code</label>
            <input
              id="code"
              className="code-input"
              value={code}
              maxLength={6}
              minLength={6}
              pattern="[A-Za-z2-9]{6}"
              onChange={(e) => setCode(e.target.value.toUpperCase())}
              autoCapitalize="characters"
              autoComplete="off"
              spellCheck={false}
              required
              placeholder="ABCDEF"
            />
            <label htmlFor="player">Your controller</label>
            <select
              id="player"
              value={player}
              onChange={(e) => setPlayer(Number(e.target.value))}
            >
              {[0, 1, 2, 3].map((i) => (
                <option value={i} key={i}>
                  Player {i + 1}
                </option>
              ))}
            </select>
            <button type="submit" disabled={!!membership}>
              {membership ? "Connecting…" : "Join room"}
            </button>
            {membership && (
              <button
                type="button"
                className="secondary"
                onClick={() => {
                  credentials.current = null;
                  setMembership(null);
                }}
              >
                Cancel reconnect
              </button>
            )}
          </form>
        </section>
      ) : (
        <>
          <div className="room-heading">
            <div>
              <p className="eyebrow">ROOM</p>
              <h1 className="room-code">{membership?.code}</h1>
            </div>
            <span
              className={`status ${connected && online ? "live" : ""}`}
              role="status"
            >
              {connected && online ? "Connected" : "Reconnecting…"}
            </span>
          </div>
          {snapshot && (
            <>
              <p className="muted">
                {phases[snapshot.phase] ?? "Waiting for game"}
                {snapshot.mode !== "lightning" && (
                  <strong className="test-mode">
                    {" "}
                    ·{" "}
                    {snapshot.mode === "sandbox"
                      ? "Sandbox · test sats only"
                      : "Simulated · no bitcoin"}
                  </strong>
                )}
              </p>
              <section className="card personal">
                <div className="section-heading">
                  <h2>Player {(membership?.player ?? 0) + 1}</h2>
                  <span className="pill">
                    {own?.paid_sats ? "Entry paid" : "Entry unpaid"}
                  </span>
                </div>
                <form
                  onSubmit={(e) => {
                    e.preventDefault();
                    if (canEdit)
                      send("set_address", {
                        match_id: snapshot.match_id,
                        address: address.trim(),
                      });
                  }}
                >
                  <label htmlFor="address">Receiving Lightning Address</label>
                  <input
                    id="address"
                    type="text"
                    inputMode="email"
                    autoCapitalize="none"
                    autoCorrect="off"
                    spellCheck={false}
                    placeholder="you@example.com"
                    value={address}
                    maxLength={127}
                    disabled={!canEdit || pending}
                    onChange={(e) => {
                      dirty.current = true;
                      setAddress(e.target.value);
                    }}
                    required
                  />
                  <button
                    className="secondary"
                    disabled={!canEdit || pending || !address.trim()}
                  >
                    Save address
                  </button>
                </form>
                <p className="muted" role="status">
                  {own?.message ||
                    "Save the address where you want to receive your payouts."}
                </p>
                <div className="entry">
                  <div>
                    <span className="eyebrow">ENTRY</span>
                    <p className="amount">
                      {sats(snapshot.invoice_sats)} <small>sats</small>
                    </p>
                  </div>
                  <button
                    disabled={!canInvoice || pending}
                    onClick={() => {
                      setRequestedFor(snapshot.match_id);
                      send("request_invoice", { match_id: snapshot.match_id });
                    }}
                  >
                    {own?.paid_sats ? "Paid" : "Get entry invoice"}
                  </button>
                </div>
                {snapshot.invoice_sats > snapshot.entry_sats && (
                  <p className="muted small">
                    {sats(snapshot.entry_sats)} sats into play +{" "}
                    {sats(snapshot.invoice_sats - snapshot.entry_sats)} sats
                    retained for routing.
                  </p>
                )}
                {requestedFor === snapshot.match_id && !own?.paid_sats && (
                  <div className="invoice">
                    {own?.invoice && connected && online ? (
                      <>
                        <a className="button" href={`lightning:${own.invoice}`}>
                          Open Lightning wallet
                        </a>
                        <button
                          className="secondary"
                          onClick={async () => {
                            setError("");
                            try {
                              await navigator.clipboard.writeText(own.invoice);
                              setNotice("Invoice copied.");
                            } catch {
                              const field = invoiceField.current;
                              const details = field?.closest("details");
                              if (details) details.open = true;
                              field?.focus();
                              field?.select();
                              setNotice("Invoice selected. Use your browser’s Copy command.");
                            }
                          }}
                        >
                          Copy invoice
                        </button>
                        <details>
                          <summary>Show invoice</summary>
                          <textarea
                            ref={invoiceField}
                            aria-label="Entry invoice"
                            readOnly
                            value={own.invoice}
                          />
                        </details>
                      </>
                    ) : (
                      <p className="muted">
                        Waiting for the desktop invoice. Every participating
                        receiver must be verified first.
                      </p>
                    )}
                  </div>
                )}
              </section>
              <section aria-label="Player totals" className="players">
                {activePlayers.map((p) => (
                  <article className="card score" key={p.index}>
                    <p className="eyebrow">
                      PLAYER {p.index + 1}
                      {p.kind === 1 ? " · CPU" : ""}
                    </p>
                    <h2>
                      {percent(
                        snapshot.damage_centi[p.index].reduce(
                          (sum, n) => sum + n,
                          0
                        )
                      )}
                      <small>% dealt</small>
                    </h2>
                    <p>
                      <strong>{sats(p.earned_sats)}</strong> sats earned
                    </p>
                    <p className="muted small">
                      {sats(p.pending_sats)} sats pending
                    </p>
                  </article>
                ))}
              </section>
              <section className="card">
                <h2>Damage</h2>
                <p className="muted small">
                  Finalized damage dealt, across all stocks.
                </p>
                <div className="table-scroll">
                  <table>
                    <thead>
                      <tr>
                        <th scope="col">From / to</th>
                        {activePlayers.map((p) => (
                          <th scope="col" key={p.index}>
                            P{p.index + 1}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {activePlayers.map((p) => (
                        <tr key={p.index}>
                          <th scope="row">P{p.index + 1}</th>
                          {activePlayers.map((v) => (
                            <td key={v.index}>
                              {p.index === v.index
                                ? "—"
                                : `${percent(
                                    snapshot.damage_centi[p.index][v.index]
                                  )}%`}
                            </td>
                          ))}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </section>
              <section className="card">
                <h2>Payment activity</h2>
                <p className="muted small">
                  Confirmed payments and pending activity reported by the
                  desktop.
                </p>
                {snapshot.logs.length ? (
                  <ol className="logs">
                    {snapshot.logs.map((log, i) => (
                      <li key={`${log.at_ms}-${i}`}>
                        <time dateTime={new Date(log.at_ms).toISOString()}>
                          {new Date(log.at_ms).toLocaleTimeString()}
                        </time>
                        <span>{log.message}</span>
                      </li>
                    ))}
                  </ol>
                ) : (
                  <p className="muted">Payment activity will appear here.</p>
                )}
              </section>
            </>
          )}
          <button
            className="secondary leave"
            disabled={!connected || !online || snapshot?.locked || pending}
            onClick={() => send("leave")}
          >
            Leave room
          </button>
        </>
      )}
      <div className="feedback" aria-live="polite">
        {error && (
          <p role="alert" className="error">
            {error}
          </p>
        )}
        {notice && <p className="muted">{notice}</p>}
      </div>
      <footer>Keep playing on your controller. Your receipts stay here.</footer>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
