//! Infrastructure adapters for Nota.
//!
//! Implements the ports declared in `nota-core` against concrete technologies
//! (axum, the filesystem), and calls into `nota-llm` for the LLM client,
//! sessions, and the agent loop (alongside `nota-onebot`, which uses
//! `nota-llm`'s tool abstractions). The CLI wires these together and injects
//! them into the core.

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
pub use nota_llm::{
    AgentRunner, LlmClient, LlmItem, LlmSessionManager, MessageRole, OpenAiLlm,
    PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry, ToolRegistryImpl,
};
pub use persona_runtime::PersonaRuntime;
pub use persona_store::FilePersonaStore;
pub use scheduler::TokioScheduler;
pub use tool::{builtin::register_builtin_tools, chat::register_chat_tools};
