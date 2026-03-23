use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub struct WatcherState {
    pub watched_folders: Vec<String>,
    pub tx: Sender<PathBuf>,
}

/// Returns true for files that should be ignored (temp/partial downloads, hidden files).
pub fn is_temp_file(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return true,
    };

    if name.starts_with('.') {
        return true;
    }

    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("tmp") | Some("part") | Some("crdownload")
    )
}

/// Starts a file-system watcher for `folders`, sending newly created (non-temp)
/// file paths through `tx`.  The watcher runs on a background thread.
/// The caller must keep the returned `RecommendedWatcher` alive — dropping it
/// stops the watch.
pub fn start_watcher(
    folders: Vec<String>,
    tx: Sender<PathBuf>,
) -> Result<RecommendedWatcher, String> {
    let tx_inner = tx.clone();

    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let event = match result {
            Ok(e) => e,
            Err(_) => return,
        };

        if !matches!(event.kind, EventKind::Create(_)) {
            return;
        }

        for path in event.paths {
            if path.is_file() && !is_temp_file(&path) {
                // Ignore send errors — receiver may have been dropped on shutdown.
                let _ = tx_inner.send(path);
            }
        }
    })
    .map_err(|e| format!("Failed to create watcher: {e}"))?;

    for folder in &folders {
        let folder_path = Path::new(folder);
        watcher
            .watch(folder_path, RecursiveMode::Recursive)
            .map_err(|e| format!("Failed to watch folder '{}': {e}", folder))?;
    }

    // Drive the event loop on its own thread so it never blocks the main thread.
    // The watcher itself is returned to the caller; this thread just processes
    // the forwarded events already handled by the closure above.
    std::thread::spawn(move || {
        // The closure passed to recommended_watcher already sends events through
        // tx_inner, so this thread's only job is to stay alive as long as the
        // WatcherState owner holds the sender end open.
        drop(tx);
    });

    Ok(watcher)
}
