use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::bus::{BusEvent, EventBus, EventKind};
use crate::permissions::PermissionRegistry;

#[derive(Clone)]
pub struct ToolContext {
    pub persona_name: String,
    pub bus: Arc<EventBus>,
    pub request_id: Option<String>,
    pub permissions: Arc<PermissionRegistry>,
    /// Outbound chat target for replying to the current message, encoded by
    /// the channel (e.g. `"private:2961354039"` / `"group:30003"`). `None`
    /// when the turn was not driven by a chat message.
    pub reply_target: Option<String>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("persona_name", &self.persona_name)
            .field("request_id", &self.request_id)
            .field("reply_target", &self.reply_target)
            .finish()
    }
}

impl ToolContext {
    /// Send a permission request to the user and await their decision.
    /// Returns `true` if approved, `false` if denied or on timeout.
    pub async fn request_permission(&self, prompt: String) -> bool {
        let (id, rx) = self.permissions.register().await;
        self.bus.send(BusEvent::permission_request(
            self.persona_name.clone(),
            prompt,
            id,
            self.request_id.clone(),
        ));
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

// Keep EventKind import to suppress unused warning on rebuilds
#[allow(dead_code)]
fn _ensure_event_kind_used(_: EventKind) {}
