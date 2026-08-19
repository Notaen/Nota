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

impl TryFrom<u8> for MessageRole {
    type Error = anyhow::Error;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(MessageRole::User),
            2 => Ok(MessageRole::Assistant),
            3 => Ok(MessageRole::Context),
            _ => Err(anyhow::anyhow!("invalid message role: {n}")),
        }
    }
}

impl<'de> Deserialize<'de> for MessageRole {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        MessageRole::try_from(n).map_err(D::Error::custom)
    }
}

/// The kind of a tool call, mirroring the provider's output item types.
/// Stored as a plain number (`0` reserved, `1` function_call,
/// `2` web_search_call) like [`MessageRole`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ToolCallKind {
    /// An ordinary function call: the session executes the matching tool.
    FunctionCall = 1,
    /// A built-in `web_search` call: the provider executes it server-side,
    /// so the session records it but runs nothing.
    WebSearchCall = 2,
}

impl Serialize for ToolCallKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl TryFrom<u8> for ToolCallKind {
    type Error = anyhow::Error;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(ToolCallKind::FunctionCall),
            2 => Ok(ToolCallKind::WebSearchCall),
            _ => Err(anyhow::anyhow!("invalid tool call kind: {n}")),
        }
    }
}

impl<'de> Deserialize<'de> for ToolCallKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        ToolCallKind::try_from(n).map_err(D::Error::custom)
    }
}

/// A tool call requested by the model. Fields are per-kind: function calls
/// carry `name` / `arguments`; built-in calls such as `web_search` carry
/// the call id and, when the provider returns one, the search query as
/// `arguments` (`{"query": …}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub kind: ToolCallKind,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// One unit of a dialogue, mirroring the provider's output item model: text
/// messages, reasoning, tool calls, and tool outputs. Sessions store items
/// in this shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionItem {
    Message {
        role: MessageRole,
        content: String,
    },
    /// Model reasoning text, persisted for the record but never re-sent as
    /// input.
    Reasoning {
        content: String,
    },
    ToolCall(ToolCall),
    ToolCallOutput {
        call_id: String,
        output: String,
    },
}

/// One LLM-level dialogue. The caller's only interaction is feeding new
/// content (`send`) and reading history; everything else — the LLM call, the
/// tool loop, the per-session execution context — is internal. Sessions are
/// **conversation-agnostic**: a plain uuid id and flat storage; mapping
/// conversations to sessions is the caller's job.
#[async_trait]
pub trait Session: Send + Sync {
    /// The session id (a plain uuid), unique within its manager.
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

/// Owns the sessions of one scope (typically one persona). The concrete
/// implementation is supplied with a storage path, the system prompt, and
/// the shared tool registry at construction; tools are resolved live from
/// the registry on every call, so mutating the registry takes effect
/// immediately. Sessions are conversation-agnostic: plain uuid ids and flat
/// files — mapping conversations to sessions is the caller's job.
#[async_trait]
pub trait SessionManager: Send + Sync {
    /// Create a fresh session.
    async fn create(&self) -> Result<Arc<dyn Session>>;

    /// Load an existing session by id, or `None` if it does not exist.
    async fn load(&self, id: &str) -> Result<Option<Arc<dyn Session>>>;

    /// Archive a session: it stays readable via `list` / `load` but is no
    /// longer current (a caller-side concern).
    async fn archive(&self, id: &str) -> Result<()>;

    /// Every session, oldest first (active + archived).
    async fn list(&self) -> Result<Vec<Arc<dyn Session>>>;
}
