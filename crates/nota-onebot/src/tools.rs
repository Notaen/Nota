//! OneBot tools exposed to the persona (read group chat history, etc.).

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams};

use crate::api::OneBotApi;
use crate::types::{
    ActionRequest, GetMsgData, GroupMsgHistoryData, LoginInfoData, format_history,
    identity,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Outbound;
    use crate::config::OnebotConfig;
    use crate::types::ActionParams;
    use tokio::sync::mpsc;

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
    async fn outbound_rejects_non_allowlisted_targets() {
        let (out, mut rx) = outbound(vec![42], vec![]);
        let err = out.send_private(99, "hi").unwrap_err();
        assert!(err.to_string().contains("allowlist"));
        let err = out.send_group(99999, "hi").unwrap_err();
        assert!(err.to_string().contains("allowlist"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );
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
