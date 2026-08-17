//! LLM-level sessions: one self-contained dialogue, conversation-agnostic.
//!
//! A **session** is a single LLM dialogue: an ordered list of OpenAI-style
//! message items (user/assistant messages plus tool calls and results), with
//! a uuid v4 id and its own SQLite file. The llm crate knows nothing about
//! conversations or personas and has no default store path: the caller
//! supplies a directory, and typically gives each conversation its own
//! directory (so clearing a conversation means removing that directory).
//! Creation and retrieval are explicit — there is no implicit get-or-create
//! and no "current session" concept here: the caller decides which session id
//! is current (e.g. a pointer file in its conversation directory) and simply
//! reads it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::llm::LlmItem;
use crate::store::SqliteSessionStore;

/// One LLM-level session: an ordered dialogue of message items, backed by
/// its own `<session_id>.db` file.
pub struct LlmSession {
    /// uuid v4 session id; the caller never needs to parse it.
    pub id: String,
    store: Arc<SqliteSessionStore>,
}

impl LlmSession {
    pub(crate) fn new(id: String, store: Arc<SqliteSessionStore>) -> Self {
        Self { id, store }
    }

    /// Append dialogue items (messages and tool calls/results) to this
    /// session's raw history.
    pub async fn append(&self, items: &[LlmItem]) -> Result<()> {
        self.store.append(&self.id, items).await
    }

    /// The full dialogue of this session, oldest first.
    pub async fn context(&self) -> Result<Vec<LlmItem>> {
        self.store.read_items(&self.id).await
    }

    /// All items with their storage row ids, oldest first.
    pub async fn raw_history(&self) -> Result<Vec<(i64, LlmItem)>> {
        self.store.read_raw(&self.id).await
    }

    /// The last Responses API id of this session, if any (saved for future
    /// stateful continuations; DeepSeek's Responses endpoint is stateless).
    pub async fn response_id(&self) -> Result<Option<String>> {
        self.store.response_id(&self.id).await
    }

    /// Save the last Responses API id of this session.
    pub async fn set_response_id(&self, id: &str) -> Result<()> {
        self.store.set_response_id(&self.id, id).await
    }
}

/// Owns the sessions in one caller-specified directory (typically one
/// conversation's sessions). Creation and retrieval are explicit.
pub struct LlmSessionManager {
    store: Arc<SqliteSessionStore>,
    current: Mutex<HashMap<String, Arc<LlmSession>>>,
}

impl LlmSessionManager {
    /// Open the session store rooted at `dir` (caller-specified; the llm
    /// crate has no default store path). Sessions live flat inside as
    /// `<session_id>.db`.
    pub fn new(dir: &Path) -> Result<Self> {
        Ok(Self {
            store: Arc::new(SqliteSessionStore::new(dir)?),
            current: Mutex::new(HashMap::new()),
        })
    }

    /// Create a fresh session with a new uuid v4 id.
    pub async fn create(&self) -> Result<Arc<LlmSession>> {
        let id = self.store.create().await?;
        let session = Arc::new(LlmSession::new(id.clone(), self.store.clone()));
        self.current.lock().unwrap().insert(id, session.clone());
        Ok(session)
    }

    /// Get an existing session by id, or `None` if it does not exist.
    pub async fn session(&self, id: &str) -> Result<Option<Arc<LlmSession>>> {
        let key = id.to_string();
        if let Some(session) = self.current.lock().unwrap().get(&key).cloned() {
            return Ok(Some(session));
        }
        if !self.store.has(id) {
            return Ok(None);
        }
        let session = Arc::new(LlmSession::new(key.clone(), self.store.clone()));
        self.current.lock().unwrap().insert(key, session.clone());
        Ok(Some(session))
    }

    /// Every session in this store: `(id, seq, created_at)`, in creation
    /// order.
    pub async fn list(&self) -> Result<Vec<(String, i64, i64)>> {
        self.store.list().await
    }

    /// Raw history of a session by id: `(row_id, item)`.
    pub async fn raw_history(
        &self,
        session_id: &str,
    ) -> Result<Vec<(i64, LlmItem)>> {
        self.store.read_raw(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MessageRole;
    use uuid::Uuid;

    fn temp_base(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nota_llm_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user_item(content: &str) -> LlmItem {
        LlmItem::Message {
            role: MessageRole::User,
            content: content.to_string(),
        }
    }

    #[tokio::test]
    async fn create_uses_uuid_and_get_roundtrips() {
        let base = temp_base("uuid");
        let manager = LlmSessionManager::new(&base).unwrap();

        let s1 = manager.create().await.unwrap();
        assert!(Uuid::parse_str(&s1.id).is_ok(), "session id must be a uuid");

        // Retrieval is explicit: the id resolves, anything else is None.
        let got = manager.session(&s1.id).await.unwrap().expect("session exists");
        assert_eq!(got.id, s1.id);
        assert!(manager.session("missing-session").await.unwrap().is_none());

        // One flat file per session in the caller-specified directory.
        let sessions = manager.list().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, s1.id);
        assert!(base.join(format!("{}.db", s1.id)).exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn append_and_context_roundtrip() {
        let base = temp_base("roundtrip");
        let manager = LlmSessionManager::new(&base).unwrap();

        let s = manager.create().await.unwrap();
        let items = vec![user_item("hello"), user_item("world")];
        s.append(&items).await.unwrap();

        assert_eq!(s.context().await.unwrap(), items);
        let raw = s.raw_history().await.unwrap();
        assert_eq!(raw.len(), 2);
        assert!(raw[0].0 < raw[1].0, "row ids must ascend");
        assert_eq!(raw[1].1, user_item("world"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn sessions_persist_across_restart() {
        let base = temp_base("persist");
        let manager = LlmSessionManager::new(&base).unwrap();

        let s1 = manager.create().await.unwrap();
        s1.append(&[user_item("hello")]).await.unwrap();
        s1.set_response_id("resp_x").await.unwrap();
        let s2 = manager.create().await.unwrap();

        // A fresh manager over the same directory still finds both sessions
        // by id; the caller decides which one is current.
        let manager2 = LlmSessionManager::new(&base).unwrap();
        let resumed = manager2
            .session(&s1.id)
            .await
            .unwrap()
            .expect("s1 exists after restart");
        assert_eq!(resumed.response_id().await.unwrap().as_deref(), Some("resp_x"));
        assert_eq!(resumed.context().await.unwrap(), vec![user_item("hello")]);

        let sessions = manager2.list().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, s1.id);
        assert_eq!(sessions[1].0, s2.id);

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn directories_are_isolated() {
        let base_a = temp_base("iso_a");
        let base_b = temp_base("iso_b");
        let a = LlmSessionManager::new(&base_a).unwrap();
        let b = LlmSessionManager::new(&base_b).unwrap();

        a.create().await.unwrap();
        assert_eq!(a.list().await.unwrap().len(), 1);
        assert!(b.list().await.unwrap().is_empty());

        std::fs::remove_dir_all(&base_a).ok();
        std::fs::remove_dir_all(&base_b).ok();
    }
}
