//! Tool abstractions for the agent loop.
//!
//! Tools are LLM-agent concepts, so they live in `nota-llm`, not in
//! `nota-core`. External crates (built-in tools in `nota-infra`, adapter
//! tools in `nota-onebot`) implement [`Tool`] and register instances on a
//! shared [`ToolRegistry`]; the agent loop attaches the registered
//! definitions to every LLM request automatically.
//!
//! The execution context still references core domain ports (the
//! conversation router and the permission registry) because tools may send
//! messages or request user approval; sessions themselves never see these.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use nota_core::conversation::ConversationManager;
use nota_core::permissions::PermissionRegistry;

#[derive(Clone)]
pub struct ToolContext {
    pub persona_name: String,
    /// Conversation-scoped router: replies and permission requests go through it.
    pub manager: Arc<ConversationManager>,
    pub request_id: Option<String>,
    pub permissions: Arc<PermissionRegistry>,
    /// The user-visible conversation this turn belongs to (adapter-assigned).
    pub conversation_id: Option<String>,
    /// Set by the persona (e.g. via the `skip_reply` tool) to suppress the
    /// automatic reply at the end of this turn.
    pub suppress_reply: Arc<AtomicBool>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("persona_name", &self.persona_name)
            .field("request_id", &self.request_id)
            .field("conversation_id", &self.conversation_id)
            .finish()
    }
}

impl ToolContext {
    /// Send a permission request to the user and await their decision.
    /// Returns `true` if approved, `false` if denied or on timeout.
    pub async fn request_permission(&self, prompt: String) -> bool {
        let (id, rx) = self.permissions.register().await;
        if let Some(conversation_id) = &self.conversation_id {
            self.manager
                .send_permission(
                    conversation_id,
                    &id,
                    &prompt,
                    self.request_id.clone(),
                )
                .await;
        }
        rx.await.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolParams {
    #[serde(rename = "type")]
    pub schema_type: String,
    pub properties: HashMap<String, PropertyDef>,
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropertyDef {
    #[serde(rename = "type")]
    pub prop_type: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub r#enum: Vec<String>,
}

impl ToolParams {
    pub fn object(
        properties: HashMap<String, PropertyDef>,
        required: Vec<String>,
    ) -> Self {
        Self {
            schema_type: "object".to_string(),
            properties,
            required,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> ToolParams;
    async fn run(&self, args: &str, ctx: ToolContext) -> Result<String>;
}

#[async_trait]
pub trait ToolRegistry: Send + Sync {
    fn register(&self, tool: Arc<dyn Tool>);
    fn unregister(&self, name: &str);
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    fn list(&self) -> Vec<Arc<dyn Tool>>;
}

/// Default in-memory registry, shareable across agent runners (e.g. built-in
/// tools and adapter tools registered after the runtimes started).
pub struct ToolRegistryImpl {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistryImpl {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for ToolRegistryImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolRegistry for ToolRegistryImpl {
    fn register(&self, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().unwrap();
        tools.insert(tool.name().to_string(), tool);
    }

    fn unregister(&self, name: &str) {
        let mut tools = self.tools.write().unwrap();
        tools.remove(name);
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = self.tools.read().unwrap();
        tools.get(name).cloned()
    }

    fn list(&self) -> Vec<Arc<dyn Tool>> {
        // Stable ordering by name: the tool list is part of the LLM request
        // prefix, and DeepSeek's automatic prefix cache only hits when the
        // prefix is byte-identical between requests.
        let mut tools: Vec<Arc<dyn Tool>> = self
            .tools
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        tools
    }
}
