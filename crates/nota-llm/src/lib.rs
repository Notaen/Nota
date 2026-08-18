//! Concrete LLM session management for Nota — the only public surface is
//! [`SqliteSessionManager`], implementing the core [`SessionManager`]
//! abstraction. Other modules never see the LLM client or the turn loop;
//! the composition root (nota-cli) constructs one manager per persona with
//! the storage root, system prompt, shared tool registry, and routing
//! ports, then injects it as `Arc<dyn SessionManager>`.

pub mod responses;
pub mod session;
pub mod store;

pub use session::{LlmConfig, SqliteSessionManager};
