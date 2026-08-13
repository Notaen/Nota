//! LLM conversation history: append-only records with a **kind** field.
//!
//! History is stored verbatim (the raw content). `//clear` never deletes
//! rows: it appends a `ClearBoundary` (kind 0). When building the LLM
//! context, everything before the last boundary is omitted.

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::session::Session;

/// The kind of a history record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(i64)]
pub enum HistoryKind {
    /// Clear boundary: messages before this must not be fed to the LLM.
    ClearBoundary = 0,
    /// A user message.
    User = 1,
    /// A persona (assistant) message.
    Assistant = 2,
    /// A tool call / tool result.
    Tool = 3,
}

impl HistoryKind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::ClearBoundary),
            1 => Some(Self::User),
            2 => Some(Self::Assistant),
            3 => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One record of conversation history.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub kind: HistoryKind,
    pub content: String,
    pub timestamp: i64,
}

/// Persistent history storage. Implementations must keep rows verbatim and
/// treat a clear boundary as a marker, never as a deletion.
#[async_trait]
pub trait HistoryStore: Send + Sync {
    /// Append raw history entries to a session.
    async fn append(
        &self,
        session: &Session,
        entries: &[HistoryEntry],
    ) -> Result<()>;

    /// History visible to the LLM: rows after the last clear boundary.
    async fn read_context(&self, session: &Session) -> Result<Vec<HistoryEntry>>;

    /// All raw history (boundaries included), oldest first, with row ids.
    async fn read_raw(&self, session: &Session) -> Result<Vec<(i64, HistoryEntry)>>;

    /// Append a clear boundary. Previous messages stay stored but are no
    /// longer part of the LLM context.
    async fn add_clear_boundary(&self, session: &Session) -> Result<()>;
}
