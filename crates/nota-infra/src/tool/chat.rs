//! Channel-agnostic chat tools available to every persona.
//!
//! Auto-delivery is disabled: the turn's final assistant text is not routed
//! back automatically. `reply` is the only way to send into the conversation
//! that created it, and adapter send tools (e.g. `onebot_send_msg`) cover
//! other chats. The bus carries the intent and the owning adapter bridge
//! (e.g. OneBot) performs the actual delivery and enforces its allowlist.
//! `wait` holds the conversation open when a message looks incomplete; the
//! llm turn loop treats a successful call specially (see `nota-llm`'s
//! turn-loop docs).
//!
//! `reply` is a **conversation-layer** tool: the conversation layer bakes the
//! conversation id into the tool instance and registers it in that
//! conversation's tool set — sessions themselves stay conversation-agnostic.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry, Value};

use crate::wait::{DEFAULT_WAIT_SECONDS, WaitHub};

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
         If the latest message looks incomplete (a half-finished sentence), call `wait` \
         instead of replying to a guess.\n\
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

/// Hold the current conversation open because the latest message looks
/// semantically incomplete. Calling `wait` ends the turn immediately; the
/// llm layer rolls the turn back to just the user message (see
/// `nota-llm`'s turn-loop docs), so nothing the model produced while
/// waiting stays in context. The model is woken by the next real message or by a
/// `[等待超时]` notice when the wait expires, and then decides what to do.
pub struct WaitTool {
    pub conversation_id: String,
    pub hub: Arc<WaitHub>,
}

impl WaitTool {
    pub fn new(conversation_id: impl Into<String>, hub: Arc<WaitHub>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            hub,
        }
    }
}

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        "Hold the current conversation open because the latest message looks incomplete \
         (for example a bare '你' with unrelated context before it) — call this instead of \
         guessing what the user meant. The turn stops immediately and nothing from this \
         turn stays in context. You are woken when a new message arrives, and can then \
         answer knowing the user's utterance arrived in pieces; if the wait times out you \
         receive a [等待超时] notice, where you may ask a clarifying question (like '？'), \
         wait again, or stay silent. You may wait at most 3 times in a row; after that you \
         must reply or end the turn silently. Call this instead of replying to an \
         incomplete message — do not combine it with a reply in the same turn."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "seconds".to_string(),
            PropertyDef {
                prop_type: "integer".to_string(),
                description: "How long to wait for the next message (0 = until the next \
                              message, default 10)".to_string(),
                r#enum: vec![],
            },
        );
        props.insert(
            "reason".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Why you are waiting (optional)".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, Vec::new())
    }

    async fn run(
        &self,
        args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let seconds = args
            .get("seconds")
            .and_then(|v| v.as_i64())
            .map(|s| s.clamp(0, 3600) as u64)
            .unwrap_or(DEFAULT_WAIT_SECONDS);
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        self.hub
            .register(&ctx.persona_name, &self.conversation_id, seconds, reason)?;
        Ok(format!(
            "已安排等待：{seconds} 秒内没有新消息会收到超时通知，新消息到达会立即唤醒"
        ))
    }
}

/// Register the channel-agnostic chat tools for one conversation.
pub fn register_chat_tools(
    registry: &ToolRegistry,
    conversation_id: impl Into<String>,
    waits: Arc<WaitHub>,
) -> Result<()> {
    let conversation_id = conversation_id.into();
    registry.register(Arc::new(ReplyTool::new(conversation_id.clone())))?;
    registry.register(Arc::new(WaitTool::new(conversation_id, waits)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::conversation::{AdapterEvent, ConversationManager};
    use nota_core::permissions::PermissionRegistry;

    use crate::wait::{MAX_CONSECUTIVE_WAITS, WaitHub};

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

    #[tokio::test]
    async fn wait_tool_registers_until_budget_is_exhausted() {
        let hub = Arc::new(WaitHub::new(Arc::new(ConversationManager::new())));
        let tool = WaitTool::new("onebot_private_42", hub);
        let ctx = tool_ctx();

        for _ in 0..MAX_CONSECUTIVE_WAITS {
            tool.run(args(r#"{"seconds":0}"#), ctx.clone())
                .await
                .unwrap();
        }
        let err = tool
            .run(args(r#"{"seconds":0}"#), ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("consecutive waits"));
    }
}
