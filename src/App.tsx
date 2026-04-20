import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// ── Types ──────────────────────────────────────────────────────────────────

interface StagingItem {
  id: string;
  file_path: string;
  proposed_dest: string;
  confidence: number;
  status: string;
  timestamp: string;
}

interface FileEvent {
  id: string;
  path: string;
  detected_category: string;
  confidence: number;
  action: string;
  timestamp: string;
}

type Tab = "queue" | "history" | "settings";
type OllamaStatus = "checking" | "online" | "offline";

// ── Helpers ────────────────────────────────────────────────────────────────

function fileName(path: string): string {
  return path.replace(/\\/g, "/").split("/").pop() ?? path;
}

function confidenceClass(c: number): "high" | "medium" | "low" {
  if (c > 0.9) return "high";
  if (c >= 0.7) return "medium";
  return "low";
}

function formatTimestamp(ts: string): string {
  try {
    return new Date(ts).toLocaleString(undefined, {
      month: "short", day: "numeric",
      hour: "2-digit", minute: "2-digit",
    });
  } catch {
    return ts;
  }
}

// ── Queue tab ──────────────────────────────────────────────────────────────

function QueueTab({ onHistoryChange }: { onHistoryChange: () => void }) {
  const [items, setItems] = useState<StagingItem[]>([]);
  const [busy, setBusy] = useState<Set<string>>(new Set());
  // id of the card currently showing the reject form, plus its destination input
  const [rejectingId, setRejectingId] = useState<string | null>(null);
  const [rejectDest, setRejectDest] = useState("");

  const refresh = useCallback(async () => {
    try {
      const queue = await invoke<StagingItem[]>("get_staging_queue");
      setItems(queue);
    } catch {
      // backend not ready yet — silently ignore
    }
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<string>("file-staged", () => refresh()).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [refresh]);

  async function approve(id: string) {
    setBusy((s) => new Set(s).add(id));
    try {
      await invoke("approve_staging_item", { id });
      setItems((prev) => prev.filter((i) => i.id !== id));
      onHistoryChange();
    } finally {
      setBusy((s) => { const n = new Set(s); n.delete(id); return n; });
    }
  }

  function openRejectForm(id: string) {
    setRejectingId(id);
    setRejectDest("");
  }

  function cancelReject() {
    setRejectingId(null);
    setRejectDest("");
  }

  async function confirmReject(id: string) {
    setBusy((s) => new Set(s).add(id));
    try {
      const newDest = rejectDest.trim() || null;
      await invoke("reject_staging_item", { id, newDest });
      setItems((prev) => prev.filter((i) => i.id !== id));
      onHistoryChange();
    } finally {
      setBusy((s) => { const n = new Set(s); n.delete(id); return n; });
      setRejectingId(null);
      setRejectDest("");
    }
  }

  if (items.length === 0) {
    return <p className="empty-state">No pending files</p>;
  }

  return (
    <ul className="card-list" style={{ listStyle: "none" }}>
      {items.map((item) => {
        const cls = confidenceClass(item.confidence);
        const pct = Math.round(item.confidence * 100);
        const isbusy = busy.has(item.id);
        const isRejecting = rejectingId === item.id;
        return (
          <li key={item.id} className="file-card">
            <div className="card-header">
              <span className="file-name">{fileName(item.file_path)}</span>
              <span className={`confidence-badge ${cls}`}>{pct}%</span>
            </div>
            <div className="card-dest">
              → <span>{item.proposed_dest}</span>
            </div>
            <div className="card-actions">
              <button
                className="btn btn-approve"
                disabled={isbusy || isRejecting}
                onClick={() => approve(item.id)}
              >
                Approve
              </button>
              <button
                className="btn btn-reject"
                disabled={isbusy}
                onClick={() => isRejecting ? cancelReject() : openRejectForm(item.id)}
              >
                {isRejecting ? "Cancel" : "Reject"}
              </button>
            </div>
            {isRejecting && (
              <div className="reject-form">
                <input
                  className="path-input"
                  type="text"
                  placeholder="Where should this go? (e.g. Documents/TextFiles)"
                  value={rejectDest}
                  autoFocus
                  onChange={(e) => setRejectDest(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") confirmReject(item.id);
                    if (e.key === "Escape") cancelReject();
                  }}
                />
                <button
                  className="btn btn-browse"
                  disabled={isbusy}
                  onClick={async () => {
                    const picked = await invoke<string | null>("browse_for_folder");
                    if (picked) setRejectDest(picked);
                  }}
                >
                  Browse
                </button>
                <button
                  className="btn btn-reject"
                  disabled={isbusy}
                  onClick={() => confirmReject(item.id)}
                >
                  Confirm Reject
                </button>
              </div>
            )}
          </li>
        );
      })}
    </ul>
  );
}

// ── History tab ────────────────────────────────────────────────────────────

function HistoryTab({
  events,
  loadHistory,
}: {
  events: FileEvent[];
  loadHistory: () => void;
}) {
  const [restoringId, setRestoringId] = useState<string | null>(null);
  const [localEvents, setLocalEvents] = useState<FileEvent[]>([]);

  // Sync props to local state whenever events change
  useEffect(() => {
    setLocalEvents(events);
  }, [events]);

  async function restore(id: string) {
    setRestoringId(id);
    try {
      await invoke("restore_file", { id });
      // Update local state immediately - mark this event as restored
      setLocalEvents((prev) =>
        prev.map((ev) =>
          ev.id === id ? { ...ev, action: "restored" } : ev
        )
      );
    } finally {
      setRestoringId(null);
    }
  }

  if (localEvents.length === 0) {
    return <p className="empty-state">No history yet</p>;
  }

  return (
    <ul className="history-list" style={{ listStyle: "none" }}>
      {localEvents.map((ev) => {
        const isAutoMoved = ev.action.startsWith("auto-moved to");
        const isRestored = ev.action === "undone" || ev.action === "restored";
        const showRestoreButton = isAutoMoved && !isRestored;

        return (
          <li key={ev.id} className="history-row">
            <div className="history-meta">
              <div className="history-filename">{fileName(ev.path)}</div>
              <div className="history-detail">
                <span className="cat">{ev.detected_category}</span>
                {" · "}
                {ev.action}
              </div>
              <div className="history-time">{formatTimestamp(ev.timestamp)}</div>
            </div>
            {showRestoreButton && (
              <button
                className="btn btn-undo"
                disabled={restoringId !== null}
                onClick={() => restore(ev.id)}
              >
                Restore
              </button>
            )}
            {isRestored && (
              <span className="btn btn-undo" style={{ opacity: 0.5, cursor: "default" }}>
                Restored
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
}

// ── Settings tab ───────────────────────────────────────────────────────────

interface ScanProgress {
  current: number;
  total: number;
  current_file: string;
}

function SettingsTab() {
  const [folderInput, setFolderInput] = useState("");
  const [watchedFolders, setWatchedFolders] = useState<string[]>([]);
  const [ollamaStatus, setOllamaStatus] = useState<OllamaStatus>("checking");
  const [adding, setAdding] = useState(false);
  const [scan, setScan] = useState<ScanProgress | null>(null);
  const [scanDone, setScanDone] = useState(false);

  // Load persisted folders on mount
  useEffect(() => {
    invoke<string[]>("get_watched_folders").then(setWatchedFolders).catch(() => {});
  }, []);

  // Listen for scan progress / completion events
  useEffect(() => {
    let unlistenProgress: (() => void) | undefined;
    let unlistenComplete: (() => void) | undefined;

    listen<ScanProgress>("scan-progress", (e) => {
      setScan(e.payload);
      setScanDone(false);
    }).then((fn) => { unlistenProgress = fn; });

    listen<void>("scan-complete", () => {
      setScan(null);
      setScanDone(true);
      // Auto-hide the "done" message after 4 s
      setTimeout(() => setScanDone(false), 4000);
    }).then((fn) => { unlistenComplete = fn; });

    return () => {
      unlistenProgress?.();
      unlistenComplete?.();
    };
  }, []);

  // Check Ollama reachability
  useEffect(() => {
    async function check() {
      try {
        const res = await fetch("http://localhost:11434/api/tags", {
          signal: AbortSignal.timeout(3000),
        });
        setOllamaStatus(res.ok ? "online" : "offline");
      } catch {
        setOllamaStatus("offline");
      }
    }
    check();
    const id = setInterval(check, 15000);
    return () => clearInterval(id);
  }, []);

  async function addFolder() {
    const path = folderInput.trim();
    if (!path) return;
    setAdding(true);
    setScanDone(false);
    try {
      await invoke("start_watching", { folders: [path] });
      setWatchedFolders((prev) =>
        prev.includes(path) ? prev : [...prev, path]
      );
      setFolderInput("");
    } finally {
      setAdding(false);
    }
  }

  const ollamaLabel =
    ollamaStatus === "checking" ? "Checking…"
    : ollamaStatus === "online"  ? "Ollama reachable (localhost:11434)"
    :                              "Ollama unreachable — AI classification unavailable";

  const scanPct = scan && scan.total > 0
    ? Math.round((scan.current / scan.total) * 100)
    : 0;

  return (
    <>
      <div className="settings-section">
        <div className="settings-label">Watch folders</div>
        <div className="add-folder-row">
          <input
            className="path-input"
            type="text"
            placeholder="/path/to/folder"
            value={folderInput}
            onChange={(e) => setFolderInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && addFolder()}
          />
          <button
            className="btn btn-primary"
            disabled={adding || folderInput.trim() === ""}
            onClick={addFolder}
          >
            Add Folder
          </button>
        </div>
        {watchedFolders.length > 0 && (
          <ul className="folder-list" style={{ listStyle: "none" }}>
            {watchedFolders.map((f) => (
              <li key={f} className="folder-item">{f}</li>
            ))}
          </ul>
        )}

        {/* Initial scan progress bar */}
        {scan && (
          <div className="scan-progress">
            <div className="scan-label">
              Scanning existing files… {scan.current}/{scan.total}
            </div>
            <div className="scan-bar-track">
              <div className="scan-bar-fill" style={{ width: `${scanPct}%` }} />
            </div>
            <div className="scan-file">{scan.current_file}</div>
          </div>
        )}

        {/* Completion toast */}
        {scanDone && (
          <div className="scan-toast">
            Initial scan complete — check Queue and History.
          </div>
        )}
      </div>

      {/* Where files go */}
      <div className="settings-section">
        <div className="settings-label">Where your files go</div>
        <ul className="dest-map" style={{ listStyle: "none" }}>
          {[
            ["Images",      "Pictures\\"],
            ["Videos",      "Videos\\"],
            ["Music",       "Music\\"],
            ["Documents",   "Documents\\Sortd\\"],
            ["PDFs",        "Documents\\Sortd\\PDFs\\"],
            ["Spreadsheets","Documents\\Sortd\\Spreadsheets\\"],
            ["Code",        "Documents\\Sortd\\Code\\"],
            ["Archives",    "Downloads\\Sortd\\Archives\\"],
            ["Installers",  "Downloads\\Sortd\\Installers\\"],
            ["Other",       "Documents\\Sortd\\Other\\"],
          ].map(([cat, dest]) => (
            <li key={cat} className="dest-map-row">
              <span className="dest-cat">{cat}</span>
              <span className="dest-arrow">→</span>
              <span className="dest-path">{dest}</span>
            </li>
          ))}
        </ul>
        <button
          className="btn btn-primary"
          style={{ marginTop: 12 }}
          onClick={() => invoke("open_sortd_folder")}
        >
          Open Sortd Folder
        </button>
      </div>

      <div className="settings-section">
        <div className="settings-label">AI status</div>
        <div className="status-row">
          <span className={`status-dot ${ollamaStatus}`} />
          <span className="status-text">{ollamaLabel}</span>
        </div>
      </div>
    </>
  );
}

// ── Root ───────────────────────────────────────────────────────────────────

export default function App() {
  const [tab, setTab] = useState<Tab>("queue");
  const [history, setHistory] = useState<FileEvent[]>([]);

  const loadHistory = useCallback(async () => {
    try {
      const raw = await invoke<FileEvent[]>("get_history");
      // Deduplicate by id in case the backend returns repeated entries
      const seen = new Set<string>();
      const unique = raw.filter((ev) => {
        if (seen.has(ev.id)) return false;
        seen.add(ev.id);
        return true;
      });
      setHistory(unique);
    } catch {
      // backend not ready yet
    }
  }, []);

  // Load history on mount and whenever the history tab is opened
  useEffect(() => {
    loadHistory();
  }, [loadHistory]);

  return (
    <div className="app">
      <header className="app-header">
        <span className="app-title">sortd</span>
        {(["queue", "history", "settings"] as Tab[]).map((t) => (
          <button
            key={t}
            className={`tab-btn ${tab === t ? "active" : ""}`}
            onClick={() => {
              setTab(t);
              if (t === "history") loadHistory();
            }}
          >
            {t.charAt(0).toUpperCase() + t.slice(1)}
          </button>
        ))}
      </header>

      <div className="tab-content">
        {tab === "queue"    && <QueueTab onHistoryChange={loadHistory} />}
        {tab === "history"  && <HistoryTab events={history} loadHistory={loadHistory} />}
        {tab === "settings" && <SettingsTab />}
      </div>
    </div>
  );
}
