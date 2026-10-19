import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

type Identity = { name: string; fingerprint: string; fingerprint_short: string };
type Peer = { name: string; fingerprint: string; revoked: boolean };
type SendResult = { transfer_id: string; files: number; bytes_sent: number; bytes_skipped: number };

type ProgressMsg =
  | { kind: "started"; totalFiles: number; totalBytes: number }
  | { kind: "fileStarted"; path: string; size: number; resumedBytes: number }
  | { kind: "progress"; deltaBytes: number }
  | { kind: "fileCompleted"; path: string }
  | { kind: "completed"; bytesTransferred: number; bytesSkipped: number }
  | { kind: "note"; message: string };

function human(n: number): string {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

export default function App() {
  const [id, setId] = useState<Identity | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [tab, setTab] = useState<"send" | "receive">("send");

  // Send state
  const [paths, setPaths] = useState<string[]>([]);
  const [addr, setAddr] = useState("");
  const [sending, setSending] = useState(false);

  // Receive state
  const [dest, setDest] = useState("");
  const [acceptNew, setAcceptNew] = useState(false);
  const [listening, setListening] = useState(false);

  // Shared progress
  const [total, setTotal] = useState(0);
  const [done, setDone] = useState(0);
  const [log, setLog] = useState<string[]>([]);
  const doneRef = useRef(0);

  useEffect(() => {
    invoke<Identity>("my_identity").then(setId).catch(() => {});
    refreshPeers();
    const un = listen<ProgressMsg>("ll://progress", (e) => {
      const m = e.payload;
      if (m.kind === "started") {
        setTotal(m.totalBytes);
        setDone(0);
        doneRef.current = 0;
        pushLog(`transfer started — ${m.totalFiles} file(s), ${human(m.totalBytes)}`);
      } else if (m.kind === "fileStarted") {
        doneRef.current += m.resumedBytes;
        setDone(doneRef.current);
        pushLog(`sending ${m.path} (${human(m.size)})`);
      } else if (m.kind === "progress") {
        doneRef.current += m.deltaBytes;
        setDone(doneRef.current);
      } else if (m.kind === "fileCompleted") {
        pushLog(`done ${m.path}`);
      } else if (m.kind === "completed") {
        pushLog(`complete — moved ${human(m.bytesTransferred)}`);
        refreshPeers();
      } else if (m.kind === "note") {
        pushLog(m.message);
        refreshPeers();
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  function pushLog(line: string) {
    setLog((l) => [line, ...l].slice(0, 80));
  }
  function refreshPeers() {
    invoke<Peer[]>("list_peers").then(setPeers).catch(() => {});
  }

  async function pickFiles() {
    const sel = await open({ multiple: true, directory: false });
    if (Array.isArray(sel)) setPaths(sel);
    else if (typeof sel === "string") setPaths([sel]);
  }
  async function pickFolder() {
    const sel = await open({ directory: true });
    if (typeof sel === "string") setPaths([sel]);
  }
  async function pickDest() {
    const sel = await open({ directory: true });
    if (typeof sel === "string") setDest(sel);
  }

  async function doSend() {
    if (!paths.length || !addr) return;
    setSending(true);
    try {
      const r = await invoke<SendResult>("send_files", { paths, addr });
      pushLog(`sent ${r.files} file(s), ${human(r.bytes_sent)}`);
    } catch (e) {
      pushLog(`error: ${String(e)}`);
    } finally {
      setSending(false);
    }
  }

  async function doReceive() {
    if (!dest) return;
    try {
      const bind = await invoke<string>("start_receiver", { dest, acceptNew });
      setListening(true);
      pushLog(`listening on ${bind}${acceptNew ? " (pairing mode)" : ""}`);
    } catch (e) {
      pushLog(`error: ${String(e)}`);
    }
  }

  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;

  return (
    <div className="app">
      <header className="bar">
        <div className="brand">
          <span className="mark">L</span> laplink
        </div>
        {id && (
          <div className="me" title={id.fingerprint}>
            <span className="me-name">{id.name}</span>
            <span className="me-fp">{id.fingerprint_short}</span>
          </div>
        )}
      </header>

      <nav className="tabs">
        <button className={tab === "send" ? "on" : ""} onClick={() => setTab("send")}>
          Send
        </button>
        <button className={tab === "receive" ? "on" : ""} onClick={() => setTab("receive")}>
          Receive
        </button>
      </nav>

      <main className="panel">
        {tab === "send" ? (
          <section className="col">
            <div className="row">
              <button className="btn" onClick={pickFiles}>
                Choose files
              </button>
              <button className="btn ghost" onClick={pickFolder}>
                Choose folder
              </button>
            </div>
            <ul className="paths">
              {paths.length === 0 ? (
                <li className="empty">Nothing selected yet.</li>
              ) : (
                paths.map((p) => <li key={p}>{p}</li>)
              )}
            </ul>
            <label className="field">
              <span>Send to</span>
              <input
                value={addr}
                onChange={(e) => setAddr(e.target.value)}
                placeholder="192.168.1.50  (or host:port)"
              />
            </label>
            <button className="btn primary" disabled={sending || !paths.length || !addr} onClick={doSend}>
              {sending ? "Sending…" : "Send"}
            </button>
          </section>
        ) : (
          <section className="col">
            <div className="row">
              <button className="btn" onClick={pickDest}>
                Choose destination
              </button>
              <span className="dest">{dest || "No folder chosen"}</span>
            </div>
            <label className="check">
              <input type="checkbox" checked={acceptNew} onChange={(e) => setAcceptNew(e.target.checked)} />
              <span>Trust the next device that connects (pairing mode)</span>
            </label>
            <button className="btn primary" disabled={listening || !dest} onClick={doReceive}>
              {listening ? "Listening…" : "Start receiving"}
            </button>
            <p className="hint">
              Share your fingerprint <strong>{id?.fingerprint_short}</strong> with the sender so they can
              pair you.
            </p>
          </section>
        )}

        <aside className="side">
          <div className="progress">
            <div className="track">
              <div className="fill" style={{ width: `${pct}%` }} />
            </div>
            <div className="pct">
              {pct.toFixed(0)}% {total > 0 && <span>· {human(done)} / {human(total)}</span>}
            </div>
          </div>
          <div className="peers">
            <div className="peers-h">Paired devices</div>
            {peers.length === 0 ? (
              <div className="empty">None yet.</div>
            ) : (
              peers.map((p) => (
                <div className="peer" key={p.fingerprint}>
                  <span>{p.name}</span>
                  <code>{p.fingerprint.slice(0, 12)}</code>
                </div>
              ))
            )}
          </div>
          <div className="logbox">
            {log.map((l, i) => (
              <div className="logline" key={i}>
                {l}
              </div>
            ))}
          </div>
        </aside>
      </main>
    </div>
  );
}
