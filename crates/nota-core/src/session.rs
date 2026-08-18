//! Session abstractions: one LLM-level dialogue, owned by the concrete
//! implementation (e.g. `nota-llm`'s SQLite-backed manager).
//!
//! A **session** is one self-contained dialogue: an ordered list of message
//! items (user/assistant messages plus tool calls and results). Callers
//! (the conversation layer) never see the LLM client or the turn loop — they
//! only create / load / archive sessions and feed new content into one:
//! `send(content)` runs the whole turn internally, and the model speaks
//! exclusively through tools.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Message roles in a dialogue. Stored and carried as **plain numbers**
/// (`0` reserved, `1` user, `2` assistant, `3` context) so the shape is
/// stable and cheap to extend. The system prompt is **not** a stored role —
/// it is passed to the `SessionManager` at construction and injected per
/// request; the llm crate maps stored roles to the provider's string roles
/// only when building an API request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageRole {
    User = 1,
    Assistant,
    /// Injected context (e.g. persona files: solo.md / memory.md), stored as
    /// session items and emitted as `system` input messages at API time.
    Context = 3,
}

impl Serialize for MessageRole {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for MessageRole {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        match n {
            1 => Ok(MessageRole::User),
            2 => Ok(MessageRole::Assistant),
            3 => Ok(MessageRole::Context),
            _ => Err(D::Error::custom(format!("invalid message role: {n}"))),
        }
    }
}

/// A tool call requested by the model, to be executed by the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One unit of a dialogue, mirroring the OpenAI message-item model: text
/// messages (`{role, content}`), function calls, and function outputs.
/// Sessions store items verbatim in this JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionItem {
    Message {
        role: MessageRole,
        content: String,
    },
    FunctionCall(ToolCall),
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

/// One LLM-level dialogue. The caller's only interaction is feeding new
/// content (`send`) and reading history; everything else — the LLM call, the
/// tool loop, the per-session execution context — is internal.
#[async_trait]
pub trait Session: Send + Sync {
    /// The session id, unique within its manager (conversation-namespaced).
    fn id(&self) -> &str;

    /// Creation time of this session (Unix millis), from its metadata.
    async fn created_at(&self) -> Result<i64>;

    /// Feed one user message into the dialogue and run the whole turn.
    /// Nothing is returned: the model speaks exclusively through tools
    /// (`ToolContext`), so delivery happens inside the session.
    async fn send(&self, content: String, request_id: Option<String>) -> Result<()>;

    /// All items of this session with their storage row ids, oldest first.
    async fn raw_history(&self) -> Result<Vec<(i64, SessionItem)>>;
}

/// Owns the sessions of one conversation scope. The concrete implementation
/// is supplied with a storage path, the system prompt, and the shared tool
/// registry at construction; tools are resolved live from the registry on
/// every call, so mutating the registry takes effect immediately.
#[async_trait]
pub trait SessionManager: Send + Sync {
    /// Create a fresh session for a conversation and make it the current one.
    async fn create(&self, conversation_id: &str) -> Result<Arc<dyn Session>>;

    /// The current session of a conversation, if a pointer exists.
    async fn current(&self, conversation_id: &str) -> Result<Option<Arc<dyn Session>>>;

    /// Load an existing session by id, or `None` if it does not exist.
    async fn load(&self, id: &str) -> Result<Option<Arc<dyn Session>>>;

    /// Archive a session (e.g. `//clear`): it stays readable via `list` /
    /// `load` but is no longer current.
    async fn archive(&self, id: &str) -> Result<()>;

    /// Every session of a conversation, oldest first (active + archived).
    async fn list(&self, conversation_id: &str) -> Result<Vec<Arc<dyn Session>>>;
}
