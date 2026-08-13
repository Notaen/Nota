//! SQLite-backed LLM conversation history.
//!
//! Rows are stored verbatim with a `kind` column (`HistoryKind`); `//clear`
//! appends a clear-boundary row instead of deleting anything. The LLM context
//! query only returns rows after the last boundary.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::history::{HistoryEntry, HistoryKind, HistoryStore};
use nota_core::session::Session;
use rusqlite::{Connection, params};

pub struct SqliteHistoryStore {
    conn: Mutex<Connection>,
}

impl SqliteHistoryStore {
    pub fn new(base_dir: &Path) -> Result<Self> {
        let db_path = base_dir.join("history.db");
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind INTEGER NOT NULL,
                content TEXT NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_history_session ON history(session_id, id);
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
        let conn = self.conn.lock().unwrap();
        for entry in entries {
            conn.execute(
                "INSERT INTO history (session_id, kind, content, timestamp)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session.session_id,
                    entry.kind.as_i64(),
                    entry.content,
                    entry.timestamp
                ],
            )?;
        }
        Ok(())
    }

    async fn read_context(&self, session: &Session) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, content, timestamp FROM history
             WHERE session_id = ?1
               AND id > (SELECT COALESCE(MAX(id), 0) FROM history
                         WHERE session_id = ?1 AND kind = 0)
             ORDER BY id",
        )?;
        let rows = stmt.query_map([&session.session_id], |row| {
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
    }

    async fn read_raw(&self, session: &Session) -> Result<Vec<(i64, HistoryEntry)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content, timestamp FROM history
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([&session.session_id], |row| {
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
    }

    async fn add_clear_boundary(&self, session: &Session) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO history (session_id, kind, content, timestamp)
             VALUES (?1, 0, '', ?2)",
            params![session.session_id, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }
}
