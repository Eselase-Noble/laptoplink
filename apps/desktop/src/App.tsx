import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
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

type HistoryItem = { id: number; icon: "down" | "up" | "dot"; text: string };

export default function App() {
  const [id, setId] = useState<Identity | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [tab, setTab] = useState<"send" | "receive">("send");

  // Send state
  const [paths, setPaths] = useState<string[]>([]);
  const [targets, setTargets] = useState<string[]>([""]);
  const [sending, setSending] = useState(false);
  const [dragging, setDragging] = useState(false);

  // Receive state
  const [dest, setDest] = useState("");
  const [acceptNew, setAcceptNew] = useState(true);
  const [allowSenderDest, setAllowSenderDest] = useState(false);
  const [listening, setListening] = useState(false);

  // Activity
  const [total, setTotal] = useState(0);
  const [done, setDone] = useState(0);
  const [active, setActive] = useState("");
  const [history, setHistory] = useState<HistoryItem[]>([]);
  const [toast, setToast] = useState<string | null>(null);
  const doneRef = useRef(0);
  const histId = useRef(0);

  useEffect(() => {
    invoke<Identity>("my_identity").then(setId).catch(() => {});
    refreshPeers();

    const unProgress = listen<ProgressMsg>("ll://progress", (e) => {
      const m = e.payload;
      if (m.kind === "started") {
        setTotal(m.totalBytes);
        setDone(0);
        doneRef.current = 0;
      } else if (m.kind === "fileStarted") {
        doneRef.current += m.resumedBytes;
        setDone(doneRef.current);
        setActive(m.path);
      } else if (m.kind === "progress") {
        doneRef.current += m.deltaBytes;
        setDone(doneRef.current);
      } else if (m.kind === "fileCompleted") {
        pushHistory("down", `received ${m.path}`);
      } else if (m.kind === "completed") {
        if (m.bytesTransferred > 0) showToast(`Received ${human(m.bytesTransferred)}`);
        setActive("");
        refreshPeers();
      } else if (m.kind === "note") {
        pushHistory("dot", m.message);
        refreshPeers();
      }
    });

    // Native drag-and-drop of files onto the window.
    const unDrop = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload as { type: string; paths?: string[] };
      if (p.type === "enter" || p.type === "over") setDragging(true);
      else if (p.type === "leave") setDragging(false);
      else if (p.type === "drop") {
        setDragging(false);
        if (p.paths && p.paths.length) {
          setTab("send");
          setPaths((prev) => Array.from(new Set([...prev, ...(p.paths as string[])])));
        }
      }
    });

    return () => {
      unProgress.then((f) => f());
      unDrop.then((f) => f());
    };
  }, []);

  function pushHistory(icon: HistoryItem["icon"], text: string) {
    histId.current += 1;
    const item = { id: histId.current, icon, text };
    setHistory((h) => [item, ...h].slice(0, 100));
  }
  function showToast(text: string) {
    setToast(text);
    window.setTimeout(() => setToast(null), 2600);
  }
  function refreshPeers() {
    invoke<Peer[]>("list_peers").then(setPeers).catch(() => {});
  }

  async function pickFiles() {
    const sel = await open({ multiple: true, directory: false });
    if (Array.isArray(sel)) addPaths(sel);
    else if (typeof sel === "string") addPaths([sel]);
  }
  async function pickFolder() {
    const sel = await open({ directory: true });
    if (typeof sel === "string") addPaths([sel]);
  }
  function addPaths(ps: string[]) {
    setPaths((prev) => Array.from(new Set([...prev, ...ps])));
  }
  function removePath(p: string) {
    setPaths((prev) => prev.filter((x) => x !== p));
  }
  async function pickDest() {
    const sel = await open({ directory: true });
    if (typeof sel === "string") setDest(sel);
  }

  function setTarget(i: number, v: string) {
    setTargets((t) => t.map((x, j) => (j === i ? v : x)));
  }
  function addTarget() {
    setTargets((t) => [...t, ""]);
  }
  function removeTarget(i: number) {
    setTargets((t) => (t.length === 1 ? t : t.filter((_, j) => j !== i)));
  }

  async function doSend() {
    const addrs = targets.map((t) => t.trim()).filter(Boolean);
    if (!paths.length || !addrs.length) return;
    setSending(true);
    try {
      for (const addr of addrs) {
        try {
          const r = await invoke<SendResult>("send_files", { paths, addr });
          pushHistory("up", `sent ${r.files} file(s), ${human(r.bytes_sent)} → ${addr}`);
          showToast(`Sent to ${addr}`);
        } catch (err) {
          pushHistory("dot", `error → ${addr}: ${String(err)}`);
        }
      }
    } finally {
      setSending(false);
    }
  }

  async function doReceive() {
    if (!dest) return;
    try {
      const bind = await invoke<string>("start_receiver", {
        dest,
        acceptNew,
        allowSenderDest,
      });
      setListening(true);
      pushHistory("dot", `listening on ${bind}${acceptNew ? " (pairing mode)" : ""}`);
    } catch (e) {
      pushHistory("dot", `error: ${String(e)}`);
    }
  }

  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;

  return (
    <div className={`app${dragging ? " dragging" : ""}`}>
      <header className="bar">
        <div className="brand">
          <span className="mark" aria-hidden>
            <svg width="20" height="20" viewBox="0 0 100 100">
              <g stroke="#fff" strokeWidth="9" fill="none">
                <circle cx="30" cy="50" r="12" />
                <circle cx="70" cy="50" r="12" />
                <path d="M42 50 H58" />
              </g>
            </svg>
          </span>
          laplink
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
        <span className={`tab-ink ${tab}`} />
      </nav>

      <main className="panel">
        <section className="col" key={tab}>
          {tab === "send" ? (
            <>
              <div
                className={`drop${dragging ? " hot" : ""}`}
                onClick={pickFiles}
                role="button"
                tabIndex={0}
              >
                <div className="drop-title">Drop files here</div>
                <div className="drop-sub">or click to choose files · <span onClick={(e) => { e.stopPropagation(); pickFolder(); }} className="link">choose a folder</span></div>
              </div>

              {paths.length > 0 && (
                <ul className="chips">
                  {paths.map((p) => (
                    <li key={p} className="chip">
                      <span className="chip-name">{p.split("/").pop()}</span>
                      <button className="chip-x" onClick={() => removePath(p)} aria-label="remove">×</button>
                    </li>
                  ))}
                </ul>
              )}

              <div className="field">
                <span className="field-label">Send to</span>
                {targets.map((t, i) => (
                  <div className="target-row" key={i}>
                    <input
                      value={t}
                      onChange={(e) => setTarget(i, e.target.value)}
                      placeholder="192.168.1.50  (or host:port)"
                    />
                    {targets.length > 1 && (
                      <button className="icon-btn" onClick={() => removeTarget(i)} aria-label="remove device">×</button>
                    )}
                  </div>
                ))}
                <button className="add-target" onClick={addTarget}>+ another device</button>
              </div>

              <button
                className="btn primary send-btn"
                disabled={sending || !paths.length || !targets.some((t) => t.trim())}
                onClick={doSend}
              >
                {sending ? "Sending…" : `Send${targets.filter((t) => t.trim()).length > 1 ? " to all" : ""}`}
              </button>
            </>
          ) : (
            <>
              <div className="field">
                <span className="field-label">Save received files to</span>
                <div className="target-row">
                  <input value={dest} readOnly placeholder="No folder chosen" />
                  <button className="icon-btn wide" onClick={pickDest}>Choose…</button>
                </div>
              </div>
              <label className="check">
                <input type="checkbox" checked={acceptNew} onChange={(e) => setAcceptNew(e.target.checked)} />
                <span>Trust the next device that connects (pairing mode)</span>
              </label>
              <label className="check">
                <input type="checkbox" checked={allowSenderDest} onChange={(e) => setAllowSenderDest(e.target.checked)} />
                <span>Let the sender choose the destination folder</span>
              </label>
              <button className="btn primary send-btn" disabled={listening || !dest} onClick={doReceive}>
                {listening ? "Listening…" : "Start receiving"}
              </button>
              <div className="fp-card">
                <span>Your fingerprint</span>
                <code>{id?.fingerprint_short ?? "…"}</code>
                <p>Share this with the sender so they can pair with you.</p>
              </div>
            </>
          )}
        </section>

        <aside className="side">
          {(() => {
            const busy = sending || active !== "" || (total > 0 && done < total);
            const dir = sending ? "out" : "in";
            return (
              <div className={`rail ${busy ? "busy " + dir : "idle"}`} aria-hidden>
                <span className="rail-node">
                  <svg width="16" height="16" viewBox="0 0 100 100"><rect width="100" height="100" rx="24" fill="currentColor"/></svg>
                </span>
                <span className="rail-wire">
                  <i className="pkt" /><i className="pkt" /><i className="pkt" />
                </span>
                <span className="rail-node">
                  <svg width="16" height="16" viewBox="0 0 100 100"><rect x="6" y="6" width="88" height="88" rx="22" fill="none" stroke="currentColor" stroke-width="10"/></svg>
                </span>
                <span className="rail-label">{busy ? (sending ? "sending…" : "receiving…") : "idle"}</span>
              </div>
            );
          })()}

          <div className="progress">
            <div className={`track${sending || (total > 0 && done < total) ? " live" : ""}`}>
              <div className="fill" style={{ width: `${pct}%` }} />
            </div>
            <div className="pct">
              {active ? <span className="active-file">{active}</span> : "idle"}
              <span className="pct-n">{total > 0 ? `${human(done)} / ${human(total)}` : ""}</span>
            </div>
          </div>

          <div className="peers">
            <div className="side-h">Paired devices</div>
            {peers.length === 0 ? (
              <div className="muted">None yet.</div>
            ) : (
              peers.map((p) => (
                <div className="peer" key={p.fingerprint}>
                  <span>{p.name}</span>
                  <code>{p.fingerprint.slice(0, 12)}</code>
                </div>
              ))
            )}
          </div>

          <div className="activity">
            <div className="side-h">Activity</div>
            <div className="log">
              {history.length === 0 ? (
                <div className="muted">Nothing yet.</div>
              ) : (
                history.map((h) => (
                  <div className="logrow" key={h.id}>
                    <span className={`gi ${h.icon}`}>{h.icon === "up" ? "↑" : h.icon === "down" ? "↓" : "·"}</span>
                    <span className="lt">{h.text}</span>
                  </div>
                ))
              )}
            </div>
          </div>
        </aside>
      </main>

      <div className={`toast${toast ? " show" : ""}`}>{toast}</div>
    </div>
  );
}
