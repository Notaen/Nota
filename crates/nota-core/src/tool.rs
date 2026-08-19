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
use serde::{Deserialize, Serialize};

use crate::conversation::ConversationManager;
use crate::permissions::PermissionRegistry;

/// A JSON-like value owned by `nota-core`, mirroring the shape of
/// `serde_json::Value` so tools can inspect parsed arguments without core
/// depending on `serde_json`. Serialization to/from JSON happens at the
/// boundary: the llm layer parses the model's raw arguments into this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value as an integer, when it is a whole number.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n)
                if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 =>
            {
                Some(*n as i64)
            }
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&HashMap<String, Value>> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, Value::Bool(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }
}

/// Per-turn execution context passed to every tool call: who the persona is,
/// how to route outbound messages, and how to request user approval. It is
/// **conversation-agnostic** — conversation-bound tools (e.g. `reply`) carry
/// their own conversation id and are registered per conversation.
#[derive(Clone)]
pub struct ToolContext {
    pub persona_name: String,
    /// Conversation-scoped router: replies and permission requests go through it.
    pub manager: Arc<ConversationManager>,
    pub request_id: Option<String>,
    pub permissions: Arc<PermissionRegistry>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("persona_name", &self.persona_name)
            .field("request_id", &self.request_id)
            .finish()
    }
}

impl ToolContext {
    /// Send a permission request to the user and await their decision.
    /// `conversation_id` identifies the chat to ask in; conversation-bound
    /// tools know it from their own construction.
    /// Returns `true` if approved, `false` if denied or on timeout.
    pub async fn request_permission(&self, conversation_id: &str, prompt: String) -> bool {
        let (id, rx) = self.permissions.register().await;
        self.manager
            .send_permission(conversation_id, &id, &prompt, self.request_id.clone())
            .await;
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
    /// Execute the tool with parsed, validated arguments. The session layer
    /// resolves the model's raw JSON arguments against [`Tool::parameters`]
    /// before calling; values are core's own [`Value`], mirroring the
    /// declared JSON Schema property types.
    async fn run(&self, args: HashMap<String, Value>, ctx: ToolContext) -> Result<String>;
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

        async fn run(
            &self,
            _args: HashMap<String, Value>,
            _ctx: ToolContext,
        ) -> Result<String> {
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
