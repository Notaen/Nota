//! Infrastructure adapters for Nota.
//!
//! Implements the ports declared in `nota-core` against concrete technologies
//! (axum, the filesystem) and drives the conversation layer through the core
//! [`SessionManager`] abstraction — it never references the llm crate. The
//! CLI (composition root) wires the concrete session manager and tool
//! registry in and injects them via `Arc`.

pub mod config;
pub mod http;
pub mod persona_runtime;
pub mod persona_store;
pub mod scheduler;
pub mod tool;

pub use config::{
    Config, ConfigStore, OnebotConfig,
    provider_default_model, provider_ids, provider_name, provider_url,
};
pub use http::{api::ApiState, serve as http_serve, AppContext};
pub use nota_core::session::{Session, SessionManager};
pub use nota_core::tool::ToolRegistry;
pub use persona_runtime::{ManagerFactory, PersonaRuntime};
pub use persona_store::FilePersonaStore;
pub use scheduler::TokioScheduler;
pub use tool::{builtin::register_builtin_tools, chat::register_chat_tools};
