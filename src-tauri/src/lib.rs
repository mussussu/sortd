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
    /// Also Arc so it can be updated from inside a spawned async task.
    pub watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

#[derive(Clone, serde::Serialize)]
struct ScanProgress {
    current: usize,
    total: usize,
    current_file: String,
}

// ── Path safety helpers ───────────────────────────────────────────────────────

/// Strip leading path components from an AI-suggested folder that would
/// duplicate what `get_category_base_dir` already provides.
///
/// The AI frequently returns paths like "Documents/PDFs" or "Music/Rock"
/// when the base dir is already `…\Documents\Sortd\PDFs` or `…\Music`.
/// We strip any leading segment that is:
///   - a generic container ("Documents", "Downloads"), or
///   - equal to the category name (case-insensitive).
///
/// Stripping stops at the first segment that isn't redundant.
///
/// Examples:
///   ("PDFs",        "Documents/PDFs")        → ""
///   ("Documents",   "Documents/Reports")     → "Reports"
///   ("Spreadsheets","Documents/Spreadsheets")→ ""
///   ("Music",       "Music/Rock")            → "Rock"
///   ("Images",      "Photos/Vacation")       → "Photos/Vacation"  (no strip)
fn strip_redundant_prefix(category: &str, folder: &str) -> String {
    let redundant = |seg: &str| -> bool {
        let low = seg.to_lowercase();
        low == "documents"
            || low == "downloads"
            || low == category.to_lowercase()
    };

    let segments: Vec<&str> = folder.split('/').collect();
    let mut start = 0;
    while start < segments.len() && redundant(segments[start]) {
        start += 1;
    }
    segments[start..].join("/")
}

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

// ── Category → Windows standard folder mapping ────────────────────────────────

/// Maps a classification category to the appropriate Windows standard folder.
/// Falls back gracefully if the standard dir is unavailable.
fn get_category_base_dir(category: &str) -> PathBuf {
    let doc = dirs::document_dir().unwrap_or_else(|| PathBuf::from("."));
    let sortd = doc.join("Sortd");
    let dl = dirs::download_dir().unwrap_or_else(|| sortd.clone());

    match category {
        "Images" | "Photos" => {
            dirs::picture_dir().unwrap_or_else(|| sortd.clone())
        }
        "Videos" => {
            dirs::video_dir().unwrap_or_else(|| sortd.clone())
        }
        "Music" | "Audio" => {
            dirs::audio_dir().unwrap_or_else(|| sortd.clone())
        }
        "Documents" => sortd.clone(),
        "PDFs" => sortd.join("PDFs"),
        "Spreadsheets" => sortd.join("Spreadsheets"),
        "Code" => sortd.join("Code"),
        "Archives" => dl.join("Sortd").join("Archives"),
        "Installers" => dl.join("Sortd").join("Installers"),
        _ => sortd.join("Other"),
    }
}

// ── Agent loop ────────────────────────────────────────────────────────────────

/// Classify one file and either auto-move it or add it to the staging queue.
/// Shared by both the initial folder scan and the live watcher loop.
async fn process_file(
    file_path: &Path,
    db_arc: &Arc<Mutex<Database>>,
    app: &AppHandle,
) {
    let classification = match classifier::classify(file_path).await {
        Ok(c) => c,
        Err(_) => return,
    };

    let path_str = file_path.to_string_lossy().to_string();
    let file_name = match file_path.file_name() {
        Some(n) => n.to_owned(),
        None => return,
    };

    // Route to the Windows standard folder for this category, then append
    // the AI-suggested subfolder (sanitized against path traversal, and with
    // any prefix that duplicates the category base stripped out).
    let base_dir = get_category_base_dir(&classification.category);
    let safe_folder = sanitize_folder_path(&classification.suggested_folder);
    let safe_folder = strip_redundant_prefix(&classification.category, &safe_folder);
    let raw_dest = if safe_folder.is_empty() {
        base_dir.join(&file_name)
    } else {
        base_dir.join(&safe_folder).join(&file_name)
    };

    if !raw_dest.starts_with(&base_dir) {
        return;
    }

    let dest_path = unique_dest(&raw_dest);
    let dest_str = dest_path.to_string_lossy().to_string();

    if classification.confidence > 0.90 {
        if move_file(file_path, &dest_path).is_ok() {
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

    // Collect all pre-existing files now (fast directory walk, no I/O per file).
    // This list is processed in Phase 1 before the live watcher starts,
    // so there are no duplicate events.
    let existing_files: Vec<PathBuf> = folders
        .iter()
        .flat_map(|folder| {
            walkdir::WalkDir::new(folder)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && !watcher::is_temp_file(e.path()))
                .map(|e| e.into_path())
        })
        .collect();

    let db_arc = Arc::clone(&state.db);
    let watcher_arc = Arc::clone(&state.watcher);

    tokio::spawn(async move {
        // ── Phase 1: scan existing files ──────────────────────────────────────
        let total = existing_files.len();
        for (idx, file_path) in existing_files.into_iter().enumerate() {
            let current_file = file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            process_file(&file_path, &db_arc, &app).await;
            let _ = app.emit(
                "scan-progress",
                ScanProgress { current: idx + 1, total, current_file },
            );
        }
        let _ = app.emit("scan-complete", ());

        // ── Phase 2: start live watcher (after scan, no duplicate events) ─────
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let new_watcher = match watcher::start_watcher(folders, tx) {
            Ok(w) => w,
            Err(_) => return,
        };
        if let Ok(mut guard) = watcher_arc.lock() {
            *guard = Some(new_watcher);
        }

        // ── Phase 3: live event loop ───────────────────────────────────────────
        let db_arc2 = Arc::clone(&db_arc);
        tokio::task::spawn_blocking(move || {
            let handle = tokio::runtime::Handle::current();
            for file_path in rx {
                handle.block_on(process_file(&file_path, &db_arc2, &app));
            }
        });
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

// ── Open Sortd folder ─────────────────────────────────────────────────────────

#[tauri::command]
fn open_sortd_folder() -> Result<(), String> {
    let path = dirs::document_dir()
        .ok_or("Cannot resolve Documents folder")?
        .join("Sortd");
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("Failed to create Sortd folder: {e}"))?;
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("Failed to open Explorer: {e}"))?;
    Ok(())
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
                watcher: Arc::new(Mutex::new(None)),
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
            open_sortd_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
