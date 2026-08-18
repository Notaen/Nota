//! Tool abstractions for the agent loop.
//!
//! A **tool** is the application-level capability contract: external crates
//! (built-in tools in `nota-infra`, adapter tools in `nota-onebot`)
//! implement [`Tool`] and register instances on the shared in-memory
//! [`ToolRegistry`]. The concrete session manager (in `nota-llm`) attaches
//! the registered definitions to every LLM request and executes tool calls,
//! resolving tools **live** from the registry on each call.
//!
//! The execution context references the core domain ports (the conversation
//! router and the permission registry) because tools may send messages or
//! request user approval; sessions themselves never see these.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::conversation::ConversationManager;
use crate::permissions::PermissionRegistry;

/// Per-turn execution context passed to every tool call: who the persona is,
/// which conversation the turn belongs to, how to route outbound messages,
/// and how to request user approval.
#[derive(Clone)]
pub struct ToolContext {
    pub persona_name: String,
    /// Conversation-scoped router: replies and permission requests go through it.
    pub manager: Arc<ConversationManager>,
    pub request_id: Option<String>,
    pub permissions: Arc<PermissionRegistry>,
    /// The user-visible conversation this turn belongs to (adapter-assigned).
    pub conversation_id: Option<String>,
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

/// Default in-memory tool registry, shareable across session managers and
/// adapter tools. Tools are resolved live from here on every LLM call:
/// registering / unregistering takes effect immediately, no restart needed.
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool. Fails when a tool with the same name is already
    /// registered: a duplicate would silently shadow the original in the
    /// LLM-facing tool list, so startup must abort instead.
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let mut tools = self.tools.write().unwrap();
        let name = tool.name().to_string();
        if tools.contains_key(&name) {
            anyhow::bail!(
                "duplicate tool name '{name}': a tool with this name is already \
                 registered; refusing to start with conflicting tools"
            );
        }
        tools.insert(name, tool);
        Ok(())
    }

    pub fn unregister(&self, name: &str) {
        self.tools.write().unwrap().remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().unwrap().get(name).cloned()
    }

    /// Stable ordering by name: the tool list is part of the LLM request
    /// prefix, and DeepSeek's automatic prefix cache only hits when the
    /// prefix is byte-identical between requests.
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
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

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool(&'static str);

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters(&self) -> ToolParams {
            ToolParams::object(HashMap::new(), vec![])
        }

        async fn run(&self, _args: &str, _ctx: ToolContext) -> Result<String> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn duplicate_registration_fails_loudly() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool("dup"))).unwrap();

        let err = registry
            .register(Arc::new(DummyTool("dup")))
            .unwrap_err();
        assert!(
            err.to_string().contains("duplicate tool name 'dup'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn re_register_after_unregister_is_allowed() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool("tmp"))).unwrap();
        registry.unregister("tmp");
        registry.register(Arc::new(DummyTool("tmp"))).unwrap();
    }

    #[test]
    fn list_is_sorted_by_name() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool("zeta"))).unwrap();
        registry.register(Arc::new(DummyTool("alpha"))).unwrap();
        let names: Vec<String> = registry
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }
}
