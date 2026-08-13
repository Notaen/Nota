//! Infrastructure adapters for Nota.
//!
//! Implements the ports declared in `nota-core` against concrete technologies
//! (axum, the filesystem, a stub LLM). The CLI wires these together and
//! injects them into the core.

pub mod config;
pub mod http;
pub mod llm;
pub mod persona_store;
pub mod tool;

pub use config::{
    Config, ConfigStore, OnebotConfig,
    provider_default_model, provider_ids, provider_name, provider_url,
};
pub use http::{api::ApiState, serve as http_serve, AppContext};
pub use llm::OpenAiLlm;
pub use persona_store::{FilePersonaStore, FileSessionStore};
pub use tool::{
    ToolRegistryImpl, builtin::register_builtin_tools, chat::register_chat_tools,
};
