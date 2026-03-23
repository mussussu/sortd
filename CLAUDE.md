# Sortd — Local-First AI File Organization Agent

## Project Overview
Sortd is a local-first desktop app (Tauri 2 + React + Rust) that watches folders,
classifies files using a local AI (Ollama/llama3.2), and organizes them automatically.
All processing happens on-device. No cloud. No telemetry.

## Tech Stack
- **Frontend**: React + TypeScript (Vite) in src/
- **Backend**: Rust (Tauri 2) in src-tauri/
- **AI**: Ollama HTTP API (localhost:11434) with llama3.2
- **Database**: SQLite via rusqlite crate (stores history, rules, corrections)
- **File watching**: notify crate (cross-platform kernel-level FS events)
- **IPC**: Tauri invoke() frontend → #[tauri::command] backend

## Architecture
Two-process model:
1. React WebView (UI) — staging queue, settings, history panel
2. Rust backend — file watcher, AI classifier, SQLite, file mover

Communication: frontend calls invoke('command_name', {args}) → Rust handler returns Result<T, String>

## Key Entry Points
- src/App.tsx — root React component
- src-tauri/src/main.rs — Tauri app setup, command registration
- src-tauri/src/lib.rs — core business logic (watcher, classifier, db)
- src-tauri/Cargo.toml — Rust dependencies

## Commands
```bash
# Dev
npm run tauri dev          # start app in dev mode (hot reload)

# Build
npm run tauri build        # production build + installer

# Rust only
cd src-tauri && cargo check        # fast type check
cd src-tauri && cargo test         # run Rust tests
cd src-tauri && cargo clippy       # linter

# Frontend only
npm run dev                # Vite dev server only
npm test                   # Vitest frontend tests
```

## Agent Loop (core logic)
watch folder → detect new file → extract metadata → call Ollama →
get confidence score → if >90% auto-move, if 70-90% add to staging queue,
if <70% notify user → store result in SQLite → learn from corrections

## Confidence Thresholds
- HIGH (>90%): auto-move silently, log to history
- MEDIUM (70-90%): add to staging queue, user approves/rejects
- LOW (<70%): system tray notification, user decides

## Database Tables (SQLite)
- file_events: id, path, detected_category, confidence, action, timestamp
- rules_cache: pattern, category, hits (learned from corrections)
- staging_queue: id, file_path, proposed_dest, confidence, status

## Coding Conventions
- Rust: snake_case, return Result<T, String> from all commands
- React: functional components only, no class components
- No unwrap() in production Rust code — use ? operator or match
- All Tauri commands must be registered in main.rs
- Keep Rust functions small and testable
- Frontend never does file I/O directly — always via invoke()

## What Claude Gets Wrong (update as we go)
- Always register new Tauri commands in main.rs or they won't be callable
- SQLite must be opened with a connection pool, not per-call
- notify watcher must run in its own thread, not blocking main