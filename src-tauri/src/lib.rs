pub mod classifier;
pub mod db;
pub mod watcher;

use db::{Database, FileEvent, StagingItem};
use notify::RecommendedWatcher;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

// ── AppState ──────────────────────────────────────────────────────────────────

pub struct AppState {
    /// Wrapped in Arc so the Arc can be cloned into background threads without
    /// going through State<'_> (whose lifetime can't escape the thread closure).
    pub db: Arc<Mutex<Database>>,
    pub watcher: Mutex<Option<RecommendedWatcher>>,
}

// ── Path safety helpers ───────────────────────────────────────────────────────

/// Sanitize a folder path that may come from untrusted AI output.
/// Keeps only `Normal` path components (drops `..`, absolute roots, drive
/// prefixes) and rejects any segment containing characters outside
/// `[A-Za-z0-9 _-]`.  Falls back to `"Other"` if nothing survives.
fn sanitize_folder_path(folder: &str) -> String {
    use std::path::Component;
    let segments: Vec<String> = Path::new(folder)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if !s.is_empty()
                    && !s.starts_with('.')
                    && s.chars().all(|ch| ch.is_alphanumeric() || " _-".contains(ch))
                {
                    Some(s.into_owned())
                } else {
                    None
                }
            }
            // Silently drop .., /, C:\, etc.
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        "Other".to_string()
    } else {
        segments.join("/")
    }
}

/// If `path` already exists, find a non-conflicting name by appending (1), (2), …
/// e.g. document.pdf → document(1).pdf → document(2).pdf
fn unique_dest(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str());
    let parent = path.parent().unwrap_or(Path::new("."));
    for i in 1u32.. {
        let name = match ext {
            Some(e) => format!("{stem}({i}).{e}"),
            None => format!("{stem}({i})"),
        };
        let candidate = parent.join(&name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf() // unreachable in practice
}

// ── File-move helper ──────────────────────────────────────────────────────────

/// Move a file from `from` to `to`.
/// - Errors if the source does not exist.
/// - Creates the destination directory tree if needed.
/// - Never overwrites an existing file; caller should pass a path from `unique_dest`.
/// - Tries `std::fs::rename` first (atomic on same filesystem);
///   falls back to copy-then-delete for cross-device moves.
fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Err(format!("Source file does not exist: {}", from.display()));
    }

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create destination dir: {e}"))?;
    }

    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }

    // rename failed (likely cross-device) — fall back to copy + delete
    std::fs::copy(from, to).map_err(|e| format!("Failed to copy file: {e}"))?;
    std::fs::remove_file(from).map_err(|e| format!("Failed to remove source after copy: {e}"))?;

    Ok(())
}

// ── Watched-folders command ───────────────────────────────────────────────────

#[tauri::command]
async fn get_watched_folders(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.get_watched_folders()
}

// ── Agent loop ────────────────────────────────────────────────────────────────

#[tauri::command]
async fn start_watching(
    folders: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Persist before starting so the list is saved even if the watcher fails.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| format!("DB lock poisoned: {e}"))?;
        db.save_watched_folders(&folders)?;
    }

    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();

    let new_watcher = watcher::start_watcher(folders, tx)?;

    {
        let mut w = state
            .watcher
            .lock()
            .map_err(|e| format!("Watcher lock poisoned: {e}"))?;
        *w = Some(new_watcher);
    }

    let base_dir: PathBuf = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Cannot resolve app data dir: {e}"))?
        .join("Sorted");

    // Clone the Arc so the background thread owns it independently of State<'_>.
    let db_arc = Arc::clone(&state.db);

    tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();

        for file_path in rx {
            let classification =
                match handle.block_on(classifier::classify(&file_path)) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

            let path_str = file_path.to_string_lossy().to_string();
            let file_name = match file_path.file_name() {
                Some(n) => n.to_owned(),
                None => continue,
            };

            // Sanitize the AI-supplied folder before joining — prevents path traversal.
            let safe_folder = sanitize_folder_path(&classification.suggested_folder);
            let raw_dest = base_dir.join(&safe_folder).join(&file_name);

            // Final containment check: dest must remain under base_dir.
            if !raw_dest.starts_with(&base_dir) {
                continue;
            }

            // Avoid silently overwriting an existing file.
            let dest_path = unique_dest(&raw_dest);
            let dest_str = dest_path.to_string_lossy().to_string();

            if classification.confidence > 0.90 {
                if move_file(&file_path, &dest_path).is_ok() {
                    if let Ok(guard) = db_arc.lock() {
                        let action = format!("auto-moved to {dest_str}");
                        let _ = guard.log_event(
                            &path_str,
                            &classification.category,
                            classification.confidence,
                            &action,
                        );
                    }
                }
            } else {
                // 0.70–0.90 → staging queue (user approves)
                // <0.70     → also staging queue (low-confidence flag via confidence value)
                if let Ok(guard) = db_arc.lock() {
                    if guard
                        .add_to_staging(&path_str, &dest_str, classification.confidence)
                        .is_ok()
                    {
                        let _ = app.emit("file-staged", &path_str);
                    }
                }
            }
        }
    });

    Ok(())
}

// ── Staging commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn get_staging_queue(state: State<'_, AppState>) -> Result<Vec<StagingItem>, String> {
    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.get_staging_queue()
}

#[tauri::command]
async fn approve_staging_item(id: String, state: State<'_, AppState>) -> Result<(), String> {
    let item = {
        let db = state
            .db
            .lock()
            .map_err(|e| format!("DB lock poisoned: {e}"))?;
        db.get_staging_item(&id)?
    };

    let from = PathBuf::from(&item.file_path);
    let to = unique_dest(&PathBuf::from(&item.proposed_dest));
    move_file(&from, &to)?;

    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.update_staging_status(&id, "approved")?;

    // Learn: record the extension → destination folder as a rule
    if let Some(ext) = from.extension().and_then(|e| e.to_str()) {
        let category = PathBuf::from(&item.proposed_dest)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Other")
            .to_string();
        let _ = db.add_rule(&format!("*.{ext}"), &category);
    }

    Ok(())
}

#[tauri::command]
async fn reject_staging_item(
    id: String,
    new_dest: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let item = {
        let db = state
            .db
            .lock()
            .map_err(|e| format!("DB lock poisoned: {e}"))?;
        db.get_staging_item(&id)?
    };

    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.update_staging_status(&id, "rejected")?;

    // Derive the category from the top-level folder of the proposed destination
    let category = PathBuf::from(&item.proposed_dest)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("Other")
        .to_string();

    let _ = db.log_event(&item.file_path, &category, item.confidence, "rejected");

    if let Some(dest) = new_dest {
        let from = PathBuf::from(&item.file_path);
        if let Some(ext) = from.extension().and_then(|e| e.to_str()) {
            let correction_category = PathBuf::from(&dest)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("Other")
                .to_string();
            let _ = db.add_rule(&format!("*.{ext}"), &correction_category);
        }
    }

    Ok(())
}

// ── Folder picker ─────────────────────────────────────────────────────────────

#[tauri::command]
async fn browse_for_folder() -> Result<Option<String>, String> {
    let handle = rfd::AsyncFileDialog::new()
        .set_title("Select destination folder")
        .pick_folder()
        .await;

    Ok(handle.map(|f| f.path().to_string_lossy().into_owned()))
}

// ── History commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn get_history(state: State<'_, AppState>) -> Result<Vec<FileEvent>, String> {
    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.get_history(100)
}

#[tauri::command]
async fn undo_last_move(state: State<'_, AppState>) -> Result<(), String> {
    let event = {
        let db = state
            .db
            .lock()
            .map_err(|e| format!("DB lock poisoned: {e}"))?;
        db.get_last_auto_move()?
    };

    // action format: "auto-moved to <dest>"
    let dest_str = event
        .action
        .strip_prefix("auto-moved to ")
        .ok_or("Last event has unexpected action format")?;

    let from = PathBuf::from(dest_str);
    let to = PathBuf::from(&event.path);
    move_file(&from, &to)?;

    let db = state
        .db
        .lock()
        .map_err(|e| format!("DB lock poisoned: {e}"))?;
    db.update_event_action(&event.id, "undone")?;

    Ok(())
}

// ── Bootstrap ─────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Cannot resolve app data directory");

            let db = Database::new(&app_data_dir).expect("Failed to initialise database");
            let saved_folders = db.get_watched_folders().unwrap_or_default();

            app.manage(AppState {
                db: Arc::new(Mutex::new(db)),
                watcher: Mutex::new(None),
            });

            // Resume watching folders from the previous session
            if !saved_folders.is_empty() {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_handle.state::<AppState>();
                    let _ = start_watching(saved_folders, app_handle.clone(), state).await;
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_watching,
            get_watched_folders,
            get_staging_queue,
            approve_staging_item,
            reject_staging_item,
            get_history,
            undo_last_move,
            browse_for_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
