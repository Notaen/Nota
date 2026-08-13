//! Channel-agnostic chat tools available to every persona.
//!
//! These tools speak only in session terms (target / suppress flag) and never
//! touch a concrete adapter; the bus carries the intent and the owning
//! adapter bridge (e.g. OneBot) performs the actual delivery.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry};

/// Send a message to any conversation session. The target is an
/// adapter-independent reference (`private:<QQ>` / `group:<QQ>`); each
/// adapter maps it to one of its own sessions and enforces its allowlist.
#[derive(Default)]
pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to a conversation session. target is private:<QQ> or group:<QQ>. The target must be allowlisted by its channel."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "target".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Target session, e.g. private:2961354039 or group:551947633".to_string(),
                r#enum: vec![],
            },
        );
        props.insert(
            "content".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message text to send".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["target".to_string(), "content".to_string()])
    }

    async fn run(&self, args: &str, ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let target = args["target"]
            .as_str()
            .filter(|t| is_valid_target(t))
            .ok_or_else(|| {
                anyhow::anyhow!("missing or invalid 'target' (expected private:<QQ> or group:<QQ>)")
            })?
            .to_string();
        let content = args["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;

        ctx.manager
            .route_outbound(
                ctx.session_id.as_deref(),
                Some(&target),
                content,
                ctx.request_id.clone(),
            )
            .await;
        Ok("已发送".to_string())
    }
}

fn is_valid_target(target: &str) -> bool {
    match target.split_once(':') {
        Some(("private", id)) => id.parse::<i64>().is_ok(),
        Some(("group", id)) => id.parse::<i64>().is_ok(),
        _ => false,
    }
}

/// Explicitly suppress the automatic reply for the current turn. This is the
/// only way for the persona to stay silent — the final assistant text is
/// always delivered as a reply unless this flag is set.
#[derive(Default)]
pub struct SkipReplyTool;

#[async_trait]
impl Tool for SkipReplyTool {
    fn name(&self) -> &str {
        "skip_reply"
    }

    fn description(&self) -> &str {
        "Suppress the automatic reply for this turn — the ONLY way to stay silent.\n\
         IMPORTANT: unless you call skip_reply, your final text WILL be sent to the chat as your reply.\n\
         Call it (no arguments) when: the user explicitly asked you not to reply (e.g. 「不要回复」); \
         the message needs no answer (addressed to someone else, just an acknowledgment, casual chatter, \
         or you are thinking out loud); or you have nothing meaningful to say.\n\
         Silence is normal and human — don't reply just for the sake of replying. After calling it, your \
         turn ends and nothing is sent."
    }

    fn parameters(&self) -> ToolParams {
        ToolParams::object(HashMap::new(), Vec::new())
    }

    async fn run(&self, _args: &str, ctx: ToolContext) -> Result<String> {
        ctx.suppress_reply.store(true, Ordering::SeqCst);
        Ok("已跳过回复".to_string())
    }
}

/// Register the channel-agnostic chat tools.
pub fn register_chat_tools(registry: &dyn ToolRegistry) {
    registry.register(std::sync::Arc::new(SendMessageTool));
    registry.register(std::sync::Arc::new(SkipReplyTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::permissions::PermissionRegistry;
    use nota_core::history::{HistoryEntry, HistoryStore};
    use nota_core::permissions::PathPolicy;
    use nota_core::session::{AdapterEvent, Session, SessionManager};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[derive(Default)]
    struct MemHistoryStore {
        entries: std::sync::Mutex<Vec<HistoryEntry>>,
    }

    #[async_trait]
    impl HistoryStore for MemHistoryStore {
        async fn append(
            &self,
            _s: &Session,
            entries: &[HistoryEntry],
        ) -> Result<()> {
            self.entries
                .lock()
                .unwrap()
                .extend(entries.iter().cloned());
            Ok(())
        }
        async fn read_context(&self, _s: &Session) -> Result<Vec<HistoryEntry>> {
            Ok(self.entries.lock().unwrap().clone())
        }
        async fn read_raw(
            &self,
            _s: &Session,
        ) -> Result<Vec<(i64, HistoryEntry)>> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .enumerate()
                .map(|(i, e)| (i as i64 + 1, e))
                .collect())
        }
        async fn add_clear_boundary(&self, _s: &Session) -> Result<()> {
            Ok(())
        }
    }

    fn tool_ctx() -> ToolContext {
        let manager = Arc::new(SessionManager::new(
            Arc::new(MemHistoryStore::default()),
            Arc::new(PathPolicy::default()),
        ));
        ToolContext {
            persona_name: "bob".to_string(),
            manager,
            request_id: None,
            permissions: Arc::new(PermissionRegistry::new()),
            session_id: Some("onebot_private_42".to_string()),
            suppress_reply: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn send_message_emits_outbound_event_with_target() {
        let tool = SendMessageTool;
        let context = tool_ctx();
        let mut rx = context
            .manager
            .subscribe_adapter("onebot");

        tool.run(r#"{"target":"group:30003","content":"yo"}"#, context)
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        let AdapterEvent::Outbound(e) = event else {
            panic!("expected outbound event");
        };
        assert_eq!(e.session_id.as_deref(), Some("onebot_private_42"));
        assert_eq!(e.target.as_deref(), Some("group:30003"));
        assert_eq!(e.content, "yo");
    }

    #[tokio::test]
    async fn send_message_rejects_bad_target() {
        let tool = SendMessageTool;
        let err = tool
            .run(r#"{"target":"bogus","content":"yo"}"#, tool_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("target"));
    }

    #[tokio::test]
    async fn skip_reply_sets_suppress_flag() {
        let tool = SkipReplyTool;
        let context = tool_ctx();
        let flag = context.suppress_reply.clone();

        tool.run("{}", context).await.unwrap();

        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn skip_reply_description_explains_silence_contract() {
        let desc = SkipReplyTool.description();
        assert!(desc.contains("ONLY way to stay silent"));
        assert!(desc.contains("final text WILL be sent"));
        assert!(desc.contains("nothing meaningful to say"));
        assert!(desc.contains("Silence is normal and human"));
    }
}
