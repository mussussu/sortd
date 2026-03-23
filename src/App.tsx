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

  const refresh = useCallback(async () => {
    try {
      const queue = await invoke<StagingItem[]>("get_staging_queue");
      setItems(queue);
    } catch {
      // backend not ready yet — silently ignore
    }
  }, []);

  // Initial load + polling every 5 s
  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  }, [refresh]);

  // Refresh immediately when backend emits a new file
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

  async function reject(id: string) {
    setBusy((s) => new Set(s).add(id));
    try {
      await invoke("reject_staging_item", { id, newDest: null });
      setItems((prev) => prev.filter((i) => i.id !== id));
      onHistoryChange();
    } finally {
      setBusy((s) => { const n = new Set(s); n.delete(id); return n; });
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
                disabled={isbusy}
                onClick={() => approve(item.id)}
              >
                Approve
              </button>
              <button
                className="btn btn-reject"
                disabled={isbusy}
                onClick={() => reject(item.id)}
              >
                Reject
              </button>
            </div>
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
  const [undoing, setUndoing] = useState(false);

  async function undo() {
    setUndoing(true);
    try {
      await invoke("undo_last_move");
      loadHistory();
    } finally {
      setUndoing(false);
    }
  }

  if (events.length === 0) {
    return <p className="empty-state">No history yet</p>;
  }

  return (
    <ul className="history-list" style={{ listStyle: "none" }}>
      {events.map((ev, idx) => (
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
          {idx === 0 && ev.action.startsWith("auto-moved") && (
            <button
              className="btn btn-undo"
              disabled={undoing}
              onClick={undo}
            >
              Undo
            </button>
          )}
        </li>
      ))}
    </ul>
  );
}

// ── Settings tab ───────────────────────────────────────────────────────────

function SettingsTab() {
  const [folderInput, setFolderInput] = useState("");
  const [watchedFolders, setWatchedFolders] = useState<string[]>([]);
  const [ollamaStatus, setOllamaStatus] = useState<OllamaStatus>("checking");
  const [adding, setAdding] = useState(false);

  // Load persisted folders on mount
  useEffect(() => {
    invoke<string[]>("get_watched_folders").then(setWatchedFolders).catch(() => {});
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
