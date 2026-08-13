//! Chat sessions: an isolated conversation between a persona and one channel
//! endpoint (e.g. a QQ friend, a QQ group, a web client).
//!
//! Each session has **two layers** of history:
//! - **deep**: the full conversation history fed to the LLM (inbound
//!   messages, assistant turns, tool calls/results — everything the persona
//!   "thought").
//! - **shallow**: only the messages actually delivered to the user (real
//!   outbound content: text / images / stickers). Future `dream` runs will
//!   learn from this layer — what the persona actually said — rather than
//!   its internal reasoning.

use anyhow::Result;
use async_trait::async_trait;

use crate::persona::ChatLogEntry;

/// Identifies one conversation: a persona plus an adapter-assigned session id.
/// The session id is opaque and must be safe to use as a filesystem path
/// segment (e.g. `onebot_private_2961354039`, `web_<uuid>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Session {
    pub persona: String,
    pub session_id: String,
}

impl Session {
    pub fn new(persona: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            persona: persona.into(),
            session_id: session_id.into(),
        }
    }
}

/// Per-session chatlog storage: history is isolated per conversation, which
/// keeps contexts from bleeding across channels and lets each session manage
/// its own LLM history.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Append to the deep layer (LLM context).
    async fn append_deep(
        &self,
        session: &Session,
        entries: &[ChatLogEntry],
    ) -> Result<()>;

    /// Read the deep layer (LLM context).
    async fn read_deep(
        &self,
        session: &Session,
        since: Option<i64>,
    ) -> Result<Vec<ChatLogEntry>>;

    /// Append to the shallow layer (what was actually delivered to the user).
    async fn append_shallow(
        &self,
        session: &Session,
        entries: &[ChatLogEntry],
    ) -> Result<()>;

    /// Read the shallow layer (actual outbound messages).
    async fn read_shallow(
        &self,
        session: &Session,
        since: Option<i64>,
    ) -> Result<Vec<ChatLogEntry>>;
}
