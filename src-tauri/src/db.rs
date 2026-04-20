use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct Database {
    conn: Connection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEvent {
    pub id: String,
    pub path: String,
    pub detected_category: String,
    pub confidence: f64,
    pub action: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StagingItem {
    pub id: String,
    pub file_path: String,
    pub proposed_dest: String,
    pub confidence: f64,
    pub status: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub category: String,
    pub hits: i64,
}

impl Database {
    pub fn new(app_data_dir: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(app_data_dir)
            .map_err(|e| format!("Failed to create app data dir: {e}"))?;

        let db_path = app_data_dir.join("sortd.db");
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("Failed to open database: {e}"))?;

        let db = Self { conn };
        db.init_tables()?;
        Ok(db)
    }

    fn init_tables(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS file_events (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    detected_category TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    action TEXT NOT NULL,
                    timestamp TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS rules_cache (
                    pattern TEXT PRIMARY KEY,
                    category TEXT NOT NULL,
                    hits INTEGER DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS staging_queue (
                    id TEXT PRIMARY KEY,
                    file_path TEXT NOT NULL,
                    proposed_dest TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    status TEXT NOT NULL,
                    timestamp TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                ",
            )
            .map_err(|e| format!("Failed to initialize tables: {e}"))
    }

    pub fn log_event(
        &self,
        path: &str,
        detected_category: &str,
        confidence: f64,
        action: &str,
    ) -> Result<String, String> {
        let id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();

        self.conn
            .execute(
                "INSERT INTO file_events (id, path, detected_category, confidence, action, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, path, detected_category, confidence, action, timestamp],
            )
            .map_err(|e| format!("Failed to log event: {e}"))?;

        Ok(id)
    }

    pub fn add_to_staging(
        &self,
        file_path: &str,
        proposed_dest: &str,
        confidence: f64,
    ) -> Result<String, String> {
        let id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();

        self.conn
            .execute(
                "INSERT INTO staging_queue (id, file_path, proposed_dest, confidence, status, timestamp)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
                params![id, file_path, proposed_dest, confidence, timestamp],
            )
            .map_err(|e| format!("Failed to add to staging: {e}"))?;

        Ok(id)
    }

    pub fn get_staging_queue(&self) -> Result<Vec<StagingItem>, String> {
        let mut stmt = self.conn
            .prepare(
                "SELECT id, file_path, proposed_dest, confidence, status, timestamp
                 FROM staging_queue WHERE status = 'pending'
                 ORDER BY timestamp DESC",
            )
            .map_err(|e| format!("Failed to prepare staging query: {e}"))?;

        let items = stmt
            .query_map([], |row| {
                Ok(StagingItem {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    proposed_dest: row.get(2)?,
                    confidence: row.get(3)?,
                    status: row.get(4)?,
                    timestamp: row.get(5)?,
                })
            })
            .map_err(|e| format!("Failed to query staging queue: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect staging items: {e}"))?;

        Ok(items)
    }

    pub fn update_staging_status(&self, id: &str, status: &str) -> Result<(), String> {
        let rows = self.conn
            .execute(
                "UPDATE staging_queue SET status = ?1 WHERE id = ?2",
                params![status, id],
            )
            .map_err(|e| format!("Failed to update staging status: {e}"))?;

        if rows == 0 {
            return Err(format!("No staging item found with id: {id}"));
        }

        Ok(())
    }

    pub fn add_rule(&self, pattern: &str, category: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO rules_cache (pattern, category, hits) VALUES (?1, ?2, 0)
                 ON CONFLICT(pattern) DO UPDATE SET category = excluded.category",
                params![pattern, category],
            )
            .map_err(|e| format!("Failed to add rule: {e}"))?;

        Ok(())
    }

    pub fn save_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| format!("Failed to save setting '{key}': {e}"))?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        match self.conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Failed to get setting '{key}': {e}")),
        }
    }

    pub fn get_watched_folders(&self) -> Result<Vec<String>, String> {
        match self.get_setting("watched_folders")? {
            None => Ok(vec![]),
            Some(json) => serde_json::from_str::<Vec<String>>(&json)
                .map_err(|e| format!("Failed to parse watched_folders: {e}")),
        }
    }

    pub fn save_watched_folders(&self, folders: &[String]) -> Result<(), String> {
        let json = serde_json::to_string(folders)
            .map_err(|e| format!("Failed to serialize watched_folders: {e}"))?;
        self.save_setting("watched_folders", &json)
    }

    pub fn get_history(&self, limit: usize) -> Result<Vec<FileEvent>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, path, detected_category, confidence, action, timestamp
                 FROM file_events
                 ORDER BY timestamp DESC
                 LIMIT ?1",
            )
            .map_err(|e| format!("Failed to prepare history query: {e}"))?;

        let events = stmt
            .query_map([limit as i64], |row| {
                Ok(FileEvent {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    detected_category: row.get(2)?,
                    confidence: row.get(3)?,
                    action: row.get(4)?,
                    timestamp: row.get(5)?,
                })
            })
            .map_err(|e| format!("Failed to query history: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect history: {e}"))?;

        Ok(events)
    }

    pub fn get_staging_item(&self, id: &str) -> Result<StagingItem, String> {
        self.conn
            .query_row(
                "SELECT id, file_path, proposed_dest, confidence, status, timestamp
                 FROM staging_queue WHERE id = ?1",
                [id],
                |row| {
                    Ok(StagingItem {
                        id: row.get(0)?,
                        file_path: row.get(1)?,
                        proposed_dest: row.get(2)?,
                        confidence: row.get(3)?,
                        status: row.get(4)?,
                        timestamp: row.get(5)?,
                    })
                },
            )
            .map_err(|e| format!("Staging item '{id}' not found: {e}"))
    }

    pub fn get_last_auto_move(&self) -> Result<FileEvent, String> {
        self.conn
            .query_row(
                "SELECT id, path, detected_category, confidence, action, timestamp
                 FROM file_events
                 WHERE action LIKE 'auto-moved to%'
                 ORDER BY timestamp DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(FileEvent {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        detected_category: row.get(2)?,
                        confidence: row.get(3)?,
                        action: row.get(4)?,
                        timestamp: row.get(5)?,
                    })
                },
            )
            .map_err(|e| format!("No auto-moved file found: {e}"))
    }

    pub fn get_file_event(&self, id: &str) -> Result<FileEvent, String> {
        self.conn
            .query_row(
                "SELECT id, path, detected_category, confidence, action, timestamp
                 FROM file_events
                 WHERE id = ?1",
                [id],
                |row| {
                    Ok(FileEvent {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        detected_category: row.get(2)?,
                        confidence: row.get(3)?,
                        action: row.get(4)?,
                        timestamp: row.get(5)?,
                    })
                },
            )
            .map_err(|e| format!("File event '{id}' not found: {e}"))
    }

    pub fn update_event_action(&self, id: &str, action: &str) -> Result<(), String> {
        let rows = self
            .conn
            .execute(
                "UPDATE file_events SET action = ?1 WHERE id = ?2",
                params![action, id],
            )
            .map_err(|e| format!("Failed to update event action: {e}"))?;

        if rows == 0 {
            return Err(format!("No file_event found with id: {id}"));
        }
        Ok(())
    }

    pub fn get_rules(&self) -> Result<Vec<Rule>, String> {
        let mut stmt = self.conn
            .prepare("SELECT pattern, category, hits FROM rules_cache ORDER BY hits DESC")
            .map_err(|e| format!("Failed to prepare rules query: {e}"))?;

        let rules = stmt
            .query_map([], |row| {
                Ok(Rule {
                    pattern: row.get(0)?,
                    category: row.get(1)?,
                    hits: row.get(2)?,
                })
            })
            .map_err(|e| format!("Failed to query rules: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect rules: {e}"))?;

        Ok(rules)
    }
}
