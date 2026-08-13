//! OneBot tools exposed to the persona (read group chat history, etc.).

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams};

use crate::api::OneBotApi;
use crate::bridge::Outbound;
use crate::types::{
    ActionRequest, GetMsgData, GroupMsgHistoryData, LoginInfoData, format_history,
    identity, ReplyRoute,
};

/// Actively read recent messages of any QQ group via NapCat's
/// `get_group_msg_history` extended API. Not gated by the reply allowlist:
/// the bot may read a group without ever responding in it.
pub struct ReadGroupChatTool {
    api: OneBotApi,
}

impl ReadGroupChatTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for ReadGroupChatTool {
    fn name(&self) -> &str {
        "read_group_chat"
    }

    fn description(&self) -> &str {
        "Read recent messages of a QQ group via the OneBot connection (NapCat get_group_msg_history). Returns the last N messages as text."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "group_id".to_string(),
            PropertyDef {
                prop_type: "integer".to_string(),
                description: "QQ group id to read".to_string(),
                r#enum: vec![],
            },
        );
        props.insert(
            "limit".to_string(),
            PropertyDef {
                prop_type: "integer".to_string(),
                description: "Max messages to fetch (default 20, max 100)".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["group_id".to_string()])
    }

    async fn run(&self, args: &str, _ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let group_id = args["group_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing or invalid 'group_id'"))?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 100);

        let resp = self
            .api
            .call(ActionRequest::get_group_msg_history(group_id, limit))
            .await?;
        if resp.retcode != Some(0) {
            anyhow::bail!(
                "get_group_msg_history failed: status={:?} retcode={:?}",
                resp.status,
                resp.retcode
            );
        }

        let data: GroupMsgHistoryData = match resp.data {
            Some(v) => serde_json::from_value(v)?,
            None => GroupMsgHistoryData {
                messages: Vec::new(),
            },
        };
        let text = format_history(&data.messages);
        if text.is_empty() {
            Ok(format!("group {group_id} has no readable recent messages"))
        } else {
            Ok(format!("Recent messages in group {group_id}:\n{text}"))
        }
    }
}

/// Query the bot's own QQ account (QQ number + nickname) via the standard
/// OneBot `get_login_info` API, so the persona can answer questions like
/// "what is your QQ number?".
pub struct GetLoginInfoTool {
    api: OneBotApi,
}

impl GetLoginInfoTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for GetLoginInfoTool {
    fn name(&self) -> &str {
        "get_login_info"
    }

    fn description(&self) -> &str {
        "Get the bot's own QQ account info (QQ number and nickname) via the OneBot connection."
    }

    fn parameters(&self) -> ToolParams {
        ToolParams::object(HashMap::new(), Vec::new())
    }

    async fn run(&self, _args: &str, _ctx: ToolContext) -> Result<String> {
        let resp = self.api.call(ActionRequest::get_login_info()).await?;
        if resp.retcode != Some(0) {
            anyhow::bail!(
                "get_login_info failed: status={:?} retcode={:?}",
                resp.status,
                resp.retcode
            );
        }
        let data: LoginInfoData = match resp.data {
            Some(v) => serde_json::from_value(v)?,
            None => anyhow::bail!("get_login_info returned no data"),
        };
        Ok(format!(
            "Bot login info: QQ {} ({})",
            data.user_id, data.nickname
        ))
    }
}

/// Fetch a single message by its message id (e.g. the id from a
/// `[回复消息ID:…]` quote), so the persona can see exactly which message a
/// reply refers to.
pub struct GetMsgTool {
    api: OneBotApi,
}

impl GetMsgTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for GetMsgTool {
    fn name(&self) -> &str {
        "get_msg"
    }

    fn description(&self) -> &str {
        "Get a specific QQ message by its message id (e.g. the id in a [回复消息ID:...] quote) and return sender, time and full text."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "message_id".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message id from a [回复消息ID:...] quote in an incoming message".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["message_id".to_string()])
    }

    async fn run(&self, args: &str, _ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let message_id = args["message_id"]
            .as_str()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .or_else(|| args["message_id"].as_i64())
            .ok_or_else(|| anyhow::anyhow!("missing or invalid 'message_id'"))?;

        let resp = self
            .api
            .call(ActionRequest::get_msg(message_id))
            .await?;
        if resp.retcode != Some(0) {
            anyhow::bail!(
                "get_msg failed: status={:?} retcode={:?}",
                resp.status,
                resp.retcode
            );
        }

        let data: GetMsgData = match resp.data {
            Some(v) => serde_json::from_value(v)?,
            None => anyhow::bail!("get_msg returned no data"),
        };
        let who = identity(data.sender.as_ref(), data.user_id);
        let text = data
            .message
            .as_ref()
            .map(|m| m.to_text())
            .unwrap_or_default();
        let ts = chrono::Local
            .timestamp_opt(data.time, 0)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "--".to_string());
        let kind = data.message_type.as_deref().unwrap_or("unknown");
        Ok(format!(
            "消息 {}（{}，{}）{}: {}",
            data.message_id, kind, ts, who, text
        ))
    }
}

/// Reply to the message that triggered this turn. The target chat comes from
/// the turn context, so the persona never has to guess the QQ number.
pub struct ReplyTool {
    outbound: Outbound,
}

impl ReplyTool {
    pub fn new(outbound: Outbound) -> Self {
        Self { outbound }
    }
}

#[async_trait]
impl Tool for ReplyTool {
    fn name(&self) -> &str {
        "reply"
    }

    fn description(&self) -> &str {
        "Reply to the message that triggered this turn. The target chat is already known. Call this to send a reply; do NOT call it if the user asked you not to reply."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "content".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "The reply text to send".to_string(),
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
        let target = ctx
            .reply_target
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no reply target for this turn"))?;
        let route = ReplyRoute::from_context(target)
            .ok_or_else(|| anyhow::anyhow!("invalid reply target {target}"))?;
        match route {
            ReplyRoute::Private { user_id } => {
                self.outbound.send_private(user_id, content)?;
            }
            ReplyRoute::Group { group_id } => {
                self.outbound.send_group(group_id, content)?;
            }
        }
        Ok("已发送回复".to_string())
    }
}

/// Proactively send a private message to a friend (must be allowlisted).
pub struct SendPrivateMsgTool {
    outbound: Outbound,
}

impl SendPrivateMsgTool {
    pub fn new(outbound: Outbound) -> Self {
        Self { outbound }
    }
}

#[async_trait]
impl Tool for SendPrivateMsgTool {
    fn name(&self) -> &str {
        "send_private_msg"
    }

    fn description(&self) -> &str {
        "Proactively send a private message to a QQ friend. The target must be in the friend allowlist."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "user_id".to_string(),
            PropertyDef {
                prop_type: "integer".to_string(),
                description: "QQ number of the friend to message".to_string(),
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
        ToolParams::object(props, vec!["user_id".to_string(), "content".to_string()])
    }

    async fn run(&self, args: &str, _ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let user_id = args["user_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing or invalid 'user_id'"))?;
        let content = args["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;
        self.outbound.send_private(user_id, content)?;
        Ok("已发送".to_string())
    }
}

/// Proactively send a message to a group (must be allowlisted).
pub struct SendGroupMsgTool {
    outbound: Outbound,
}

impl SendGroupMsgTool {
    pub fn new(outbound: Outbound) -> Self {
        Self { outbound }
    }
}

#[async_trait]
impl Tool for SendGroupMsgTool {
    fn name(&self) -> &str {
        "send_group_msg"
    }

    fn description(&self) -> &str {
        "Proactively send a message to a QQ group. The target must be in the group allowlist."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "group_id".to_string(),
            PropertyDef {
                prop_type: "integer".to_string(),
                description: "QQ group id to message".to_string(),
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
        ToolParams::object(props, vec!["group_id".to_string(), "content".to_string()])
    }

    async fn run(&self, args: &str, _ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let group_id = args["group_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing or invalid 'group_id'"))?;
        let content = args["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;
        self.outbound.send_group(group_id, content)?;
        Ok("已发送".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OnebotConfig;
    use crate::types::ActionParams;
    use nota_core::bus::EventBus;
    use nota_core::permissions::PermissionRegistry;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn tool_context(reply_target: Option<String>) -> ToolContext {
        ToolContext {
            persona_name: "bob".to_string(),
            bus: Arc::new(EventBus::new()),
            request_id: None,
            permissions: Arc::new(PermissionRegistry::new()),
            reply_target,
        }
    }

    fn outbound(
        friend_ids: Vec<i64>,
        group_ids: Vec<i64>,
    ) -> (Outbound, mpsc::UnboundedReceiver<ActionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Outbound {
            action_tx: tx,
            cfg: OnebotConfig {
                enabled: true,
                mode: "ws".to_string(),
                ws_url: String::new(),
                access_token: String::new(),
                persona: "bob".to_string(),
                prefix: String::new(),
                friend_ids,
                group_ids,
            },
        };
        (out, rx)
    }

    #[tokio::test]
    async fn reply_tool_sends_to_context_target() {
        let (out, mut rx) = outbound(vec![42], vec![]);
        let tool = ReplyTool::new(out);
        let ctx = tool_context(Some("private:42".to_string()));

        tool.run(r#"{"content":"hello"}"#, ctx).await.unwrap();

        let action = rx.recv().await.unwrap();
        assert_eq!(action.action, "send_private_msg");
        let ActionParams::Private { user_id, message } = action.params else {
            panic!("expected private action");
        };
        assert_eq!(user_id, 42);
        assert_eq!(message[0].data.text, "hello");
    }

    #[tokio::test]
    async fn reply_tool_without_target_fails() {
        let (out, _rx) = outbound(vec![42], vec![]);
        let tool = ReplyTool::new(out);
        let ctx = tool_context(None);

        let err = tool.run(r#"{"content":"hi"}"#, ctx).await.unwrap_err();
        assert!(err.to_string().contains("reply target"));
    }

    #[tokio::test]
    async fn proactive_send_respects_allowlist() {
        let (out, mut rx) = outbound(vec![42], vec![30003]);
        let tool = SendPrivateMsgTool::new(out.clone());

        // Allowlisted target sends.
        tool.run(r#"{"user_id":42,"content":"hi"}"#, tool_context(None))
            .await
            .unwrap();
        let action = rx.recv().await.unwrap();
        assert_eq!(action.action, "send_private_msg");

        // Non-allowlisted target is rejected.
        let err = tool
            .run(r#"{"user_id":99,"content":"hi"}"#, tool_context(None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("allowlist"));

        let group_tool = SendGroupMsgTool::new(out);
        group_tool
            .run(r#"{"group_id":30003,"content":"yo"}"#, tool_context(None))
            .await
            .unwrap();
        let action = rx.recv().await.unwrap();
        assert_eq!(action.action, "send_group_msg");
    }

    #[tokio::test]
    async fn outbound_chunks_long_text() {
        let (out, mut rx) = outbound(vec![42], vec![]);
        out.send_private(42, &"a".repeat(9000)).unwrap();
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        let third = rx.recv().await.unwrap();
        let ActionParams::Private { message, .. } = first.params else {
            panic!("expected private action");
        };
        let ActionParams::Private { message: m2, .. } = second.params else {
            panic!("expected private action");
        };
        let ActionParams::Private { message: m3, .. } = third.params else {
            panic!("expected private action");
        };
        assert_eq!(message[0].data.text.len(), 4000);
        assert_eq!(m2[0].data.text.len(), 4000);
        assert_eq!(m3[0].data.text.len(), 1000);
    }
}
