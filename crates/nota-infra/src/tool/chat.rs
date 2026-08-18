//! Channel-agnostic chat tools available to every persona.
//!
//! Sending is **explicit**: nothing is delivered automatically at the end of
//! a turn. `reply` delivers a message into the current conversation; sending
//! to another chat is the job of adapter-specific tools (e.g.
//! `onebot_send_msg` in `nota-onebot`). The bus carries the intent and the
//! owning adapter bridge (e.g. OneBot) performs the actual delivery and
//! enforces its allowlist.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry};

/// Reply to the current conversation — the explicit way for the persona to
/// speak in the chat it is currently talking in.
#[derive(Default)]
pub struct ReplyTool;

#[async_trait]
impl Tool for ReplyTool {
    fn name(&self) -> &str {
        "reply"
    }

    fn description(&self) -> &str {
        "Send a message to the CURRENT conversation — the ONLY way to deliver your reply. \
         NOTHING is sent automatically at the end of a turn; if you want to say something, \
         call this tool. Each call sends immediately, and you may call it multiple times \
         in one turn (reply first, look things up, reply again).\n\
         To send to another chat, use the adapter's send tool (e.g. onebot_send_msg).\n\
         Stay silent by simply NOT calling this tool — e.g. when the user asked you not to \
         reply (「不要回复」) or there is nothing meaningful to say."
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

    async fn run(&self, args: &str, ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let content = args["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;

        let conversation_id = ctx.conversation_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no current conversation; use an adapter send tool with a target")
        })?;
        ctx.manager
            .route_outbound(
                Some(conversation_id),
                None,
                content,
                ctx.request_id.clone(),
            )
            .await;
        Ok("已发送".to_string())
    }
}

/// Register the channel-agnostic chat tools.
pub fn register_chat_tools(registry: &ToolRegistry) -> Result<()> {
    registry.register(std::sync::Arc::new(ReplyTool))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::conversation::{AdapterEvent, ConversationManager};
    use nota_core::permissions::PermissionRegistry;
    use std::sync::Arc;

    fn tool_ctx() -> ToolContext {
        let manager = Arc::new(ConversationManager::new());
        ToolContext {
            persona_name: "bob".to_string(),
            manager,
            request_id: None,
            permissions: Arc::new(PermissionRegistry::new()),
            conversation_id: Some("onebot_private_42".to_string()),
        }
    }

    #[tokio::test]
    async fn reply_emits_outbound_to_current_conversation() {
        let tool = ReplyTool;
        let context = tool_ctx();
        let mut rx = context.manager.subscribe_adapter("onebot");

        tool.run(r#"{"content":"hi"}"#, context).await.unwrap();

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
        let tool = ReplyTool;
        let err = tool.run("{}", tool_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("content"));
    }

    #[tokio::test]
    async fn reply_requires_current_conversation() {
        let tool = ReplyTool;
        let mut ctx = tool_ctx();
        ctx.conversation_id = None;

        let err = tool.run(r#"{"content":"hi"}"#, ctx).await.unwrap_err();
        assert!(err.to_string().contains("current conversation"));
    }

    #[test]
    fn reply_description_explains_explicit_send_contract() {
        let desc = ReplyTool.description();
        assert!(desc.contains("ONLY way to deliver your reply"));
        assert!(desc.contains("NOTHING is sent automatically"));
        assert!(desc.contains("onebot_send_msg"));
        assert!(desc.contains("NOT calling this tool"));
    }
}
