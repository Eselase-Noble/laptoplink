import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";

type Identity = { name: string; fingerprint: string; fingerprint_short: string };
type Peer = { name: string; fingerprint: string; revoked: boolean };
type SendResult = { transfer_id: string; files: number; bytes_sent: number; bytes_skipped: number };
type SyncResult = {
  dry_run: boolean;
  files_total: number;
  files_transferred: number;
  files_skipped: number;
  bytes_transferred: number;
  deletions: string[];
  deleted: number;
  conflicts: string[];
};

type ProgressMsg =
  | { kind: "started"; totalFiles: number; totalBytes: number }
  | { kind: "fileStarted"; path: string; size: number; resumedBytes: number }
  | { kind: "progress"; deltaBytes: number }
  | { kind: "fileCompleted"; path: string }
  | { kind: "completed"; bytesTransferred: number; bytesSkipped: number }
  | { kind: "note"; message: string };

type Tab = "send" | "receive" | "sync" | "devices";

function human(n: number): string {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i += 1; }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

type HistoryItem = { id: number; icon: "down" | "up" | "dot"; text: string };

export default function App() {
  const [id, setId] = useState<Identity | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [tab, setTab] = useState<Tab>("send");

  // Send
  const [paths, setPaths] = useState<string[]>([]);
  const [targets, setTargets] = useState<string[]>([""]);
  const [sending, setSending] = useState(false);
  const [dragging, setDragging] = useState(false);

  // Receive
  const [dest, setDest] = useState("");
  const [acceptNew, setAcceptNew] = useState(true);
  const [allowSenderDest, setAllowSenderDest] = useState(false);
  const [listening, setListening] = useState(false);

  // Sync
  const [syncSrc, setSyncSrc] = useState("");
  const [syncAddr, setSyncAddr] = useState("");
  const [syncOpts, setSyncOpts] = useState({ dryRun: true, delete: false, delta: true, checksum: false });
  const [syncing, setSyncing] = useState(false);
  const [syncResult, setSyncResult] = useState<SyncResult | null>(null);

  // Devices
  const [pairFp, setPairFp] = useState("");
  const [pairName, setPairName] = useState("");
  const [copied, setCopied] = useState(false);

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
      if (m.kind === "started") { setTotal(m.totalBytes); setDone(0); doneRef.current = 0; }
      else if (m.kind === "fileStarted") { doneRef.current += m.resumedBytes; setDone(doneRef.current); setActive(m.path); }
      else if (m.kind === "progress") { doneRef.current += m.deltaBytes; setDone(doneRef.current); }
      else if (m.kind === "fileCompleted") pushHistory("down", `received ${m.path}`);
      else if (m.kind === "completed") { if (m.bytesTransferred > 0) showToast(`Received ${human(m.bytesTransferred)}`); setActive(""); refreshPeers(); }
      else if (m.kind === "note") { pushHistory("dot", m.message); refreshPeers(); }
    });
    const unDrop = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload as { type: string; paths?: string[] };
      if (p.type === "enter" || p.type === "over") setDragging(true);
      else if (p.type === "leave") setDragging(false);
      else if (p.type === "drop") {
        setDragging(false);
        if (p.paths && p.paths.length) { setTab("send"); setPaths((prev) => Array.from(new Set([...prev, ...(p.paths as string[])]))); }
      }
    });
    return () => { unProgress.then((f) => f()); unDrop.then((f) => f()); };
  }, []);

  function pushHistory(icon: HistoryItem["icon"], text: string) {
    histId.current += 1;
    setHistory((h) => [{ id: histId.current, icon, text }, ...h].slice(0, 100));
  }
  function showToast(text: string) { setToast(text); window.setTimeout(() => setToast(null), 2600); }
  function refreshPeers() { invoke<Peer[]>("list_peers").then(setPeers).catch(() => {}); }

  async function pickFiles() { const s = await open({ multiple: true, directory: false }); if (Array.isArray(s)) addPaths(s); else if (typeof s === "string") addPaths([s]); }
  async function pickFolder() { const s = await open({ directory: true }); if (typeof s === "string") addPaths([s]); }
  function addPaths(ps: string[]) { setPaths((prev) => Array.from(new Set([...prev, ...ps]))); }
  function removePath(p: string) { setPaths((prev) => prev.filter((x) => x !== p)); }
  async function pickDest() { const s = await open({ directory: true }); if (typeof s === "string") setDest(s); }
  async function pickSyncSrc() { const s = await open({ directory: true }); if (typeof s === "string") setSyncSrc(s); }

  function setTarget(i: number, v: string) { setTargets((t) => t.map((x, j) => (j === i ? v : x))); }
  function addTarget() { setTargets((t) => [...t, ""]); }
  function removeTarget(i: number) { setTargets((t) => (t.length === 1 ? t : t.filter((_, j) => j !== i))); }

  async function doSend() {
    const addrs = targets.map((t) => t.trim()).filter(Boolean);
    if (!paths.length || !addrs.length) return;
    setSending(true);
    try {
      for (const addr of addrs) {
        try { const r = await invoke<SendResult>("send_files", { paths, addr }); pushHistory("up", `sent ${r.files} file(s), ${human(r.bytes_sent)} → ${addr}`); showToast(`Sent to ${addr}`); }
        catch (err) { pushHistory("dot", `error → ${addr}: ${String(err)}`); }
      }
    } finally { setSending(false); }
  }

  async function doReceive() {
    if (!dest) return;
    try { const bind = await invoke<string>("start_receiver", { dest, acceptNew, allowSenderDest }); setListening(true); pushHistory("dot", `listening on ${bind}${acceptNew ? " (pairing mode)" : ""}`); }
    catch (e) { pushHistory("dot", `error: ${String(e)}`); }
  }

  async function doSync() {
    if (!syncSrc || !syncAddr.trim()) return;
    setSyncing(true); setSyncResult(null);
    try {
      const r = await invoke<SyncResult>("run_sync", { source: syncSrc, addr: syncAddr.trim(), delete: syncOpts.delete, dryRun: syncOpts.dryRun, delta: syncOpts.delta, checksum: syncOpts.checksum });
      setSyncResult(r);
      pushHistory("up", `${r.dry_run ? "dry-run" : "synced"}: ${r.files_transferred} sent, ${r.files_skipped} skipped${r.deleted ? `, ${r.deleted} deleted` : ""}`);
    } catch (e) { pushHistory("dot", `sync error: ${String(e)}`); } finally { setSyncing(false); }
  }

  async function doPair() {
    if (!pairFp.trim()) return;
    try { await invoke("pair_peer", { fingerprint: pairFp.trim(), name: pairName.trim() || "peer" }); setPairFp(""); setPairName(""); refreshPeers(); showToast("Device paired"); }
    catch (e) { showToast(`Pair failed: ${String(e)}`); }
  }
  async function doRevoke(fp: string) {
    try { await invoke("revoke_peer", { fingerprint: fp }); refreshPeers(); showToast("Device revoked"); }
    catch (e) { showToast(`Revoke failed: ${String(e)}`); }
  }
  function copyFp() {
    if (id && navigator.clipboard) navigator.clipboard.writeText(id.fingerprint).then(() => { setCopied(true); window.setTimeout(() => setCopied(false), 1400); });
  }

  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  const busy = sending || syncing || active !== "" || (total > 0 && done < total);
  const dir = sending || syncing ? "out" : "in";

  return (
    <div className={`app${dragging ? " dragging" : ""}`}>
      <header className="bar">
        <div className="brand">
          <span className="mark" aria-hidden>
            <svg width="20" height="20" viewBox="0 0 100 100"><g stroke="#fff" strokeWidth="9" fill="none"><circle cx="30" cy="50" r="12" /><circle cx="70" cy="50" r="12" /><path d="M42 50 H58" /></g></svg>
          </span>
          laplink
        </div>
        {id && (<div className="me" title={id.fingerprint}><span className="me-name">{id.name}</span><span className="me-fp">{id.fingerprint_short}</span></div>)}
      </header>

      <nav className="tabs">
        {(["send", "receive", "sync", "devices"] as Tab[]).map((t) => (
          <button key={t} className={tab === t ? "on" : ""} onClick={() => setTab(t)}>
            {t[0].toUpperCase() + t.slice(1)}
          </button>
        ))}
      </nav>

      <main className="panel">
        <section className="col" key={tab}>
          {tab === "send" && (
            <>
              <div className={`drop${dragging ? " hot" : ""}`} onClick={pickFiles} role="button" tabIndex={0}>
                <div className="drop-title">Drop files here</div>
                <div className="drop-sub">or click to choose files · <span onClick={(e) => { e.stopPropagation(); pickFolder(); }} className="link">choose a folder</span></div>
              </div>
              {paths.length > 0 && (
                <ul className="chips">
                  {paths.map((p) => (<li key={p} className="chip"><span className="chip-name">{p.split(/[\\/]/).pop()}</span><button className="chip-x" onClick={() => removePath(p)}>×</button></li>))}
                </ul>
              )}
              <div className="field">
                <span className="field-label">Send to</span>
                {targets.map((t, i) => (
                  <div className="target-row" key={i}>
                    <input value={t} onChange={(e) => setTarget(i, e.target.value)} placeholder="192.168.1.50  (or host:port)" />
                    {targets.length > 1 && (<button className="icon-btn" onClick={() => removeTarget(i)}>×</button>)}
                  </div>
                ))}
                <button className="add-target" onClick={addTarget}>+ another device</button>
              </div>
              <button className="btn primary send-btn" disabled={sending || !paths.length || !targets.some((t) => t.trim())} onClick={doSend}>
                {sending ? "Sending…" : `Send${targets.filter((t) => t.trim()).length > 1 ? " to all" : ""}`}
              </button>
            </>
          )}

          {tab === "receive" && (
            <>
              <div className="field">
                <span className="field-label">Save received files to</span>
                <div className="target-row"><input value={dest} readOnly placeholder="No folder chosen" /><button className="icon-btn wide" onClick={pickDest}>Choose…</button></div>
              </div>
              <label className="check"><input type="checkbox" checked={acceptNew} onChange={(e) => setAcceptNew(e.target.checked)} /><span>Trust the next device that connects (pairing mode)</span></label>
              <label className="check"><input type="checkbox" checked={allowSenderDest} onChange={(e) => setAllowSenderDest(e.target.checked)} /><span>Let the sender choose the destination folder</span></label>
              <button className="btn primary send-btn" disabled={listening || !dest} onClick={doReceive}>{listening ? "Listening…" : "Start receiving"}</button>
              <div className="fp-card"><span>Your fingerprint</span><code>{id?.fingerprint_short ?? "…"}</code><p>Share this with the sender so they can pair with you.</p></div>
            </>
          )}

          {tab === "sync" && (
            <>
              <div className="field">
                <span className="field-label">Folder to sync</span>
                <div className="target-row"><input value={syncSrc} readOnly placeholder="No folder chosen" /><button className="icon-btn wide" onClick={pickSyncSrc}>Choose…</button></div>
              </div>
              <div className="field">
                <span className="field-label">Sync to</span>
                <input value={syncAddr} onChange={(e) => setSyncAddr(e.target.value)} placeholder="192.168.1.50  (or host:port)" />
              </div>
              <div className="opts">
                <label className="check"><input type="checkbox" checked={syncOpts.dryRun} onChange={(e) => setSyncOpts({ ...syncOpts, dryRun: e.target.checked })} /><span>Dry run (preview only)</span></label>
                <label className="check"><input type="checkbox" checked={syncOpts.delta} onChange={(e) => setSyncOpts({ ...syncOpts, delta: e.target.checked })} /><span>Delta (send only changed parts)</span></label>
                <label className="check"><input type="checkbox" checked={syncOpts.checksum} onChange={(e) => setSyncOpts({ ...syncOpts, checksum: e.target.checked })} /><span>Compare by content hash</span></label>
                <label className="check"><input type="checkbox" checked={syncOpts.delete} onChange={(e) => setSyncOpts({ ...syncOpts, delete: e.target.checked })} /><span>Delete extraneous files at destination</span></label>
              </div>
              <button className="btn primary send-btn" disabled={syncing || !syncSrc || !syncAddr.trim()} onClick={doSync}>{syncing ? "Syncing…" : syncOpts.dryRun ? "Preview sync" : "Sync"}</button>
              {syncResult && (
                <div className="fp-card">
                  <span>{syncResult.dry_run ? "Dry run — nothing changed" : "Sync complete"}</span>
                  <p style={{ marginTop: 8 }}>
                    {syncResult.files_transferred} to transfer, {syncResult.files_skipped} identical{syncResult.bytes_transferred ? `, ${human(syncResult.bytes_transferred)} moved` : ""}.
                    {syncResult.deletions.length > 0 && ` ${syncResult.deletions.length} extraneous${syncResult.dry_run ? " (would delete)" : syncResult.deleted ? ` (deleted ${syncResult.deleted})` : ""}.`}
                  </p>
                </div>
              )}
            </>
          )}

          {tab === "devices" && (
            <>
              <div className="fp-card">
                <span>Your fingerprint</span>
                <code>{id?.fingerprint ?? "…"}</code>
                <div className="row" style={{ marginTop: 10 }}>
                  <button className="icon-btn wide" onClick={copyFp}>{copied ? "Copied" : "Copy"}</button>
                  <span className="muted">Give this to another device so it can pair with you.</span>
                </div>
              </div>
              <div className="field">
                <span className="field-label">Pair a device by fingerprint</span>
                <input value={pairFp} onChange={(e) => setPairFp(e.target.value)} placeholder="64-character fingerprint" style={{ fontFamily: "var(--mono)" }} />
                <div className="target-row">
                  <input value={pairName} onChange={(e) => setPairName(e.target.value)} placeholder="name (optional)" />
                  <button className="icon-btn wide" onClick={doPair}>Pair</button>
                </div>
              </div>
              <div className="field">
                <span className="field-label">Paired devices</span>
                {peers.length === 0 ? (<div className="muted">None yet.</div>) : (
                  peers.map((p) => (
                    <div className="peer-row" key={p.fingerprint}>
                      <div><div className="peer-name">{p.name}{p.revoked && <span className="revoked"> revoked</span>}</div><code>{p.fingerprint.slice(0, 20)}…</code></div>
                      {!p.revoked && <button className="icon-btn" onClick={() => doRevoke(p.fingerprint)}>Revoke</button>}
                    </div>
                  ))
                )}
              </div>
            </>
          )}
        </section>

        <aside className="side">
          <div className={`rail ${busy ? "busy " + dir : "idle"}`} aria-hidden>
            <span className="rail-node"><svg width="16" height="16" viewBox="0 0 100 100"><rect width="100" height="100" rx="24" fill="currentColor" /></svg></span>
            <span className="rail-wire"><i className="pkt" /><i className="pkt" /><i className="pkt" /></span>
            <span className="rail-node"><svg width="16" height="16" viewBox="0 0 100 100"><rect x="6" y="6" width="88" height="88" rx="22" fill="none" stroke="currentColor" strokeWidth="10" /></svg></span>
            <span className="rail-label">{busy ? (dir === "out" ? "sending…" : "receiving…") : "idle"}</span>
          </div>

          <div className="progress">
            <div className={`track${busy && total > 0 && done < total ? " live" : ""}`}><div className="fill" style={{ width: `${pct}%` }} /></div>
            <div className="pct">{active ? <span className="active-file">{active}</span> : "idle"}<span className="pct-n">{total > 0 ? `${human(done)} / ${human(total)}` : ""}</span></div>
          </div>

          <div className="activity">
            <div className="side-h">Activity</div>
            <div className="log">
              {history.length === 0 ? (<div className="muted">Nothing yet.</div>) : history.map((h) => (
                <div className="logrow" key={h.id}><span className={`gi ${h.icon}`}>{h.icon === "up" ? "↑" : h.icon === "down" ? "↓" : "·"}</span><span className="lt">{h.text}</span></div>
              ))}
            </div>
          </div>
        </aside>
      </main>

      <div className={`toast${toast ? " show" : ""}`}>{toast}</div>
    </div>
  );
}
