//! Channel-agnostic chat tools available to every persona.
//!
//! Auto-delivery is disabled: the turn's final assistant text is not routed
//! back automatically. `reply` is the only way to send into the conversation
//! that created it, and adapter send tools (e.g. `onebot_send_msg`) cover
//! other chats. The bus carries the intent and the owning adapter bridge
//! (e.g. OneBot) performs the actual delivery and enforces its allowlist.
//!
//! `reply` is a **conversation-layer** tool: the conversation layer bakes the
//! conversation id into the tool instance and registers it in that
//! conversation's tool set — sessions themselves stay conversation-agnostic.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry, Value};

/// Reply to the conversation that created this tool. The conversation id is
/// baked in at construction, so the session never needs to know it.
pub struct ReplyTool {
    pub conversation_id: String,
}

impl ReplyTool {
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for ReplyTool {
    fn name(&self) -> &str {
        "reply"
    }

    fn description(&self) -> &str {
        "Send a message to the current chat. Your reply reaches the user only through this \
         tool, so call it once per message and again for every message you want to send.\n\
         You can call this tool several times in one turn, for example to answer, then \
         check a fact, then answer again.\n\
         People in chat apps usually send short messages, one or two sentences at a time. \
         If your reply is long, prefer a few short calls over one long text.\n\
         Call this tool when you have something worth saying. If the user asks you to stay \
         quiet, or you have nothing useful to add, say nothing and end the turn.\n\
         To send a message to another chat, use that adapter's send tool (such as \
         onebot_send_msg)."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "content".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message text to send".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["content".to_string()])
    }

    async fn run(
        &self,
        args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;

        ctx.manager
            .route_outbound(
                Some(&self.conversation_id),
                None,
                content,
                ctx.request_id.clone(),
            )
            .await;
        Ok("已发送".to_string())
    }
}

/// Register the channel-agnostic chat tools for one conversation.
pub fn register_chat_tools(
    registry: &ToolRegistry,
    conversation_id: impl Into<String>,
) -> Result<()> {
    registry.register(Arc::new(ReplyTool::new(conversation_id)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::conversation::{AdapterEvent, ConversationManager};
    use nota_core::permissions::PermissionRegistry;

    fn args(json: &str) -> HashMap<String, Value> {
        serde_json::from_str(json).unwrap()
    }

    fn tool_ctx() -> ToolContext {
        let manager = Arc::new(ConversationManager::new());
        ToolContext {
            persona_name: "bob".to_string(),
            manager,
            request_id: None,
            permissions: Arc::new(PermissionRegistry::new()),
        }
    }

    #[tokio::test]
    async fn reply_emits_outbound_to_baked_conversation() {
        let tool = ReplyTool::new("onebot_private_42");
        let context = tool_ctx();
        let mut rx = context.manager.subscribe_adapter("onebot");

        tool.run(args(r#"{"content":"hi"}"#), context).await.unwrap();

        let event = rx.recv().await.unwrap();
        let AdapterEvent::Outbound(e) = event else {
            panic!("expected outbound event");
        };
        assert_eq!(e.conversation_id.as_deref(), Some("onebot_private_42"));
        assert_eq!(e.target, None);
        assert_eq!(e.content, "hi");
    }

    #[tokio::test]
    async fn reply_rejects_missing_content() {
        let tool = ReplyTool::new("onebot_private_42");
        let err = tool.run(HashMap::new(), tool_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("content"));
    }
}
