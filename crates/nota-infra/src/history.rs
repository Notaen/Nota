//! SQLite-backed LLM conversation history, one database per session.
//!
//! Each session gets its own `sessions/<session_id>/history.db`. Rows are
//! stored verbatim with a `kind` column (`HistoryKind`); `//clear` appends a
//! clear-boundary row instead of deleting anything. The LLM context query only
//! returns rows after the last boundary.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::history::{HistoryEntry, HistoryKind, HistoryStore};
use nota_core::session::Session;
use rusqlite::{Connection, params};

pub struct SqliteHistoryStore {
    sessions_dir: std::path::PathBuf,
    conns: Mutex<HashMap<String, Connection>>,
}

impl SqliteHistoryStore {
    pub fn new(base_dir: &Path) -> Result<Self> {
        Ok(Self {
            sessions_dir: base_dir.join("sessions"),
            conns: Mutex::new(HashMap::new()),
        })
    }

    fn with_conn<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut conns = self.conns.lock().unwrap();
        if !conns.contains_key(session_id) {
            let path = self.sessions_dir.join(session_id).join("history.db");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind INTEGER NOT NULL,
                    content TEXT NOT NULL,
                    timestamp INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_history ON history(kind, id);
                ",
            )?;
            conns.insert(session_id.to_string(), conn);
        }
        f(conns.get(session_id).unwrap())
    }
}

fn row_to_entry(kind: i64, content: &str, timestamp: i64) -> Option<HistoryEntry> {
    Some(HistoryEntry {
        kind: HistoryKind::from_i64(kind)?,
        content: content.to_string(),
        timestamp,
    })
}

#[async_trait]
impl HistoryStore for SqliteHistoryStore {
    async fn append(
        &self,
        session: &Session,
        entries: &[HistoryEntry],
    ) -> Result<()> {
        let session_id = session.session_id.clone();
        self.with_conn(&session_id, |conn| {
            for entry in entries {
                conn.execute(
                    "INSERT INTO history (kind, content, timestamp)
                     VALUES (?1, ?2, ?3)",
                    params![
                        entry.kind.as_i64(),
                        entry.content,
                        entry.timestamp
                    ],
                )?;
            }
            Ok(())
        })
    }

    async fn read_context(&self, session: &Session) -> Result<Vec<HistoryEntry>> {
        let session_id = session.session_id.clone();
        self.with_conn(&session_id, |conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, content, timestamp FROM history
                 WHERE id > (SELECT COALESCE(MAX(id), 0) FROM history WHERE kind = 0)
                 ORDER BY id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (kind, content, timestamp) = row?;
                if let Some(entry) = row_to_entry(kind, &content, timestamp) {
                    out.push(entry);
                }
            }
            Ok(out)
        })
    }

    async fn read_raw(&self, session: &Session) -> Result<Vec<(i64, HistoryEntry)>> {
        let session_id = session.session_id.clone();
        self.with_conn(&session_id, |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, content, timestamp FROM history ORDER BY id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, kind, content, timestamp) = row?;
                if let Some(entry) = row_to_entry(kind, &content, timestamp) {
                    out.push((id, entry));
                }
            }
            Ok(out)
        })
    }

    async fn add_clear_boundary(&self, session: &Session) -> Result<()> {
        let session_id = session.session_id.clone();
        self.with_conn(&session_id, |conn| {
            conn.execute(
                "INSERT INTO history (kind, content, timestamp) VALUES (0, '', ?1)",
                params![chrono::Utc::now().timestamp()],
            )?;
            Ok(())
        })
    }
}
