//! SQLite-backed LLM session store: one database file per session.
//!
//! The caller supplies the storage directory — the llm crate has no default
//! store path. Inside that directory sessions are stored flat as
//! `<session_id>.db` files, so a caller can give each conversation its own
//! directory and clean up a whole conversation by removing that directory.
//! Each file holds the session's metadata (`meta`) and its dialogue items
//! (`messages`) verbatim as OpenAI-style message items (JSON).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::llm::LlmItem;

pub struct SqliteSessionStore {
    dir: PathBuf,
    conns: Mutex<HashMap<String, Connection>>,
}

impl SqliteSessionStore {
    /// Open a session store rooted at `dir` (caller-specified). Sessions are
    /// stored flat inside as `<session_id>.db`.
    pub fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            conns: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn session_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.db"))
    }

    pub(crate) fn has(&self, session_id: &str) -> bool {
        self.session_path(session_id).exists()
    }

    fn with_conn<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut conns = self.conns.lock().unwrap();
        if !conns.contains_key(session_id) {
            let path = self.session_path(session_id);
            let conn = Connection::open(&path)?;
            ensure_schema(&conn)?;
            conns.insert(session_id.to_string(), conn);
        }
        f(conns.get(session_id).unwrap())
    }

    /// Create a fresh session with a new uuid v4 id and return its id.
    pub async fn create(&self) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp_millis();
        // A monotonic per-directory sequence gives sessions a deterministic
        // creation order even when two are created within the same millisecond.
        let seq = self.next_seq().await?;
        self.with_conn(&id, |conn| {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('created_at', ?1)",
                params![now.to_string()],
            )?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('seq', ?1)",
                params![seq.to_string()],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    async fn next_seq(&self) -> Result<i64> {
        let mut max = 0i64;
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(1),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let seq = self.meta_i64(id, "seq").await?.unwrap_or(0);
            max = max.max(seq);
        }
        Ok(max + 1)
    }

    async fn meta_i64(&self, session_id: &str, key: &str) -> Result<Option<i64>> {
        let session_id = session_id.to_string();
        let key = key.to_string();
        self.with_conn(&session_id, |conn| {
            let raw = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            raw.map(|v| {
                v.parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("bad {key} for {session_id}: {e}"))
            })
            .transpose()
        })
    }

    /// Append dialogue items to a session, verbatim, in order.
    pub async fn append(&self, session_id: &str, items: &[LlmItem]) -> Result<()> {
        let session_id = session_id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.with_conn(&session_id, |conn| {
            let tx = conn.unchecked_transaction()?;
            for item in items {
                tx.execute(
                    "INSERT INTO messages (item, timestamp) VALUES (?1, ?2)",
                    params![serde_json::to_string(item)?, now],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// The full dialogue of a session, oldest first.
    pub async fn read_items(&self, session_id: &str) -> Result<Vec<LlmItem>> {
        Ok(self
            .read_raw(session_id)
            .await?
            .into_iter()
            .map(|(_, item)| item)
            .collect())
    }

    /// All items of a session with their row ids, oldest first.
    pub async fn read_raw(
        &self,
        session_id: &str,
    ) -> Result<Vec<(i64, LlmItem)>> {
        let session_id = session_id.to_string();
        self.with_conn(&session_id, |conn| {
            let mut stmt = conn.prepare("SELECT id, item FROM messages ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, raw) = row?;
                match serde_json::from_str::<LlmItem>(&raw) {
                    Ok(item) => out.push((id, item)),
                    Err(e) => log::warn!("skipping unparseable session item: {e}"),
                }
            }
            Ok(out)
        })
    }

    /// The saved Responses API id of a session, if any.
    pub async fn response_id(&self, session_id: &str) -> Result<Option<String>> {
        let session_id = session_id.to_string();
        self.with_conn(&session_id, |conn| {
            conn.query_row(
                "SELECT value FROM meta WHERE key = 'response_id'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
        })
    }

    /// Save the last Responses API id of a session.
    pub async fn set_response_id(&self, session_id: &str, id: &str) -> Result<()> {
        let session_id = session_id.to_string();
        self.with_conn(&session_id, |conn| {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('response_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![id],
            )?;
            Ok(())
        })
    }

    /// Every session in this directory: `(id, seq, created_at)`, in creation
    /// order.
    pub async fn list(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let seq = self.meta_i64(id, "seq").await?.unwrap_or(0);
            let created_at = self.meta_i64(id, "created_at").await?.unwrap_or(0);
            out.push((id.to_string(), seq, created_at));
        }
        out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item TEXT NOT NULL,
            timestamp INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}
