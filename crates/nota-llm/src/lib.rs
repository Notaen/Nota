//! LLM capability for Nota: the model client, conversation-agnostic session
//! management (one dialogue per session), the agent loop, and the tool
//! abstractions external crates implement and register.
//!
//! The term **session** here always means one LLM-level dialogue: an ordered
//! list of OpenAI-style message items (system prompt excluded — callers
//! inject it per request). Sessions never know about conversations or
//! personas, and the crate has no default store path: callers supply a
//! directory (typically one per conversation) and track session ids
//! themselves.

pub mod agent;
pub mod llm;
pub mod responses;
pub mod session;
pub mod store;
pub mod tool;

pub use agent::AgentRunner;
pub use llm::{LlmClient, LlmItem, LlmResponse, MessageRole, ToolCall, ToolDef};
pub use responses::OpenAiLlm;
pub use session::{LlmSession, LlmSessionManager};
pub use store::SqliteSessionStore;
pub use tool::{
    PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry, ToolRegistryImpl,
};
