//! OneBot tools exposed to the persona (toolset design follows dsh-onebot):
//! every name carries the `onebot_` prefix, sending is explicit
//! (`onebot_send_msg` for other chats, `reply` for the current
//! conversation), and inbound non-text segments render with all their
//! `data` fields so the model can fetch content on demand.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams};

use crate::api::OneBotApi;
use crate::types::{
    ActionRequest, FetchPttTextData, GetMsgData, LoginInfoData, MsgHistoryData,
    format_history, identity,
};

/// Compatibility hint for the NapCat/go-cqhttp extended friend-history API.
const FRIEND_HISTORY_HINT: &str =
    "get_friend_msg_history is a NapCat/go-cqhttp extension; this OneBot \
     implementation may not support reading private chat history — for a \
     single message use onebot_get_content instead";

/// Send a message to a specific OneBot chat (private or group) through the
/// conversation bus; the bridge enforces the allowlist and the user
/// approval round-trip for non-allowlisted targets. Positioning: chats
/// OTHER than the current one — reply in the current chat with `reply`.
#[derive(Default)]
pub struct OneBotSendMsgTool;

#[async_trait]
impl Tool for OneBotSendMsgTool {
    fn name(&self) -> &str {
        "onebot_send_msg"
    }

    fn description(&self) -> &str {
        "Send a message to a specific QQ chat. target is private:<QQ> or group:<群号>; \
         the target must be allowlisted (or approved by the user in the current chat). \
         Each call sends immediately, chunked if needed.\n\
         This is for sending OUTSIDE the current conversation — to reply in the chat you \
         are currently talking in, use `reply` instead."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "target".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Target QQ chat: private:<QQ> or group:<群号>".to_string(),
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
                anyhow::anyhow!("missing or invalid 'target' (expected private:<id> or group:<id>)")
            })?
            .to_string();
        let content = args["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'content'"))?;

        ctx.manager
            .route_outbound(
                ctx.conversation_id.as_deref(),
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

/// Read the recent message history of a QQ chat: group history via
/// `get_group_msg_history`, private/friend history via the NapCat /
/// go-cqhttp extension `get_friend_msg_history`. Not gated by the reply
/// allowlist: the bot may read a chat without ever responding in it.
pub struct OneBotGetMsgHistoryTool {
    api: OneBotApi,
}

impl OneBotGetMsgHistoryTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for OneBotGetMsgHistoryTool {
    fn name(&self) -> &str {
        "onebot_get_msg_history"
    }

    fn description(&self) -> &str {
        "Read the recent message history of a QQ chat via the OneBot connection: \
         group history (get_group_msg_history) or private/friend history \
         (get_friend_msg_history). target is private:<QQ> or group:<群号>. Returns \
         the last N messages as text."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "target".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Chat to read: private:<QQ> or group:<群号>".to_string(),
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
        ToolParams::object(props, vec!["target".to_string()])
    }

    async fn run(&self, args: &str, _ctx: ToolContext) -> Result<String> {
        let args: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let target = args["target"]
            .as_str()
            .filter(|t| is_valid_target(t))
            .ok_or_else(|| {
                anyhow::anyhow!("missing or invalid 'target' (expected private:<id> or group:<id>)")
            })?
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 100);

        let action = match target.split_once(':') {
            Some(("private", id)) => {
                ActionRequest::get_friend_msg_history(id.parse::<i64>()?, limit)
            }
            Some(("group", id)) => {
                ActionRequest::get_group_msg_history(id.parse::<i64>()?, limit)
            }
            _ => unreachable!("target validated above"),
        };
        let action_name = action.action.clone();

        let resp = self.api.call(action).await?;
        if resp.retcode != Some(0) {
            let hint = if target.starts_with("private:") {
                FRIEND_HISTORY_HINT
            } else {
                ""
            };
            anyhow::bail!(
                "{} failed: status={:?} retcode={:?}{}",
                action_name,
                resp.status,
                resp.retcode,
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" — {hint}")
                }
            );
        }

        let data: MsgHistoryData = match resp.data {
            Some(v) => serde_json::from_value(v)?,
            None => MsgHistoryData {
                messages: Vec::new(),
            },
        };
        let text = format_history(&data.messages);
        if text.is_empty() {
            Ok(format!("chat {target} has no readable recent messages"))
        } else {
            Ok(format!("Recent messages in {target}:\n{text}"))
        }
    }
}

/// Fetch the full content of a specific QQ message by its message id (e.g.
/// the id from a `[reply msg id:…]` / `[record msg id:…]` segment), so the
/// persona can see exactly what a message refers to.
pub struct OneBotGetContentTool {
    api: OneBotApi,
}

impl OneBotGetContentTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for OneBotGetContentTool {
    fn name(&self) -> &str {
        "onebot_get_content"
    }

    fn description(&self) -> &str {
        "Get the full content of a specific QQ message by its message id (e.g. the id in a \
         [reply msg id:...], [image msg id:...] or [record msg id:...] segment) and return \
         sender, time and full text."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "message_id".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message id from a [reply msg id:...] or [record msg id:...] segment".to_string(),
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
            .map(|m| m.to_text_with_id(&data.message_id))
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

/// Report the OneBot connection status and the bot's own QQ account info
/// (user id + nickname), so the persona can answer questions like "what is
/// your user id?" and know whether the bridge is connected.
pub struct OneBotStatusTool {
    api: OneBotApi,
    ws_url: String,
}

impl OneBotStatusTool {
    pub fn new(api: OneBotApi, ws_url: String) -> Self {
        Self { api, ws_url }
    }
}

#[async_trait]
impl Tool for OneBotStatusTool {
    fn name(&self) -> &str {
        "onebot_status"
    }

    fn description(&self) -> &str {
        "Get the OneBot connection status and the bot's own QQ account info (QQ number \
         and nickname) via the OneBot connection."
    }

    fn parameters(&self) -> ToolParams {
        ToolParams::object(HashMap::new(), Vec::new())
    }

    async fn run(&self, _args: &str, _ctx: ToolContext) -> Result<String> {
        let connected = self.api.is_connected();
        let (mut user_id, mut nickname) = (0i64, String::new());
        if connected {
            // 连接状态只是快照；查询失败（如刚断开）就按现状报告。
            if let Ok(resp) = self.api.call(ActionRequest::get_login_info()).await
                && resp.retcode == Some(0)
                && let Some(data) = resp.data
                && let Ok(info) = serde_json::from_value::<LoginInfoData>(data)
            {
                user_id = info.user_id;
                nickname = info.nickname;
            }
        }
        Ok(format!(
            "Bot status: QQ {user_id} ({nickname}) · connected: {connected} · {}",
            self.ws_url
        ))
    }
}

/// Transcribe a voice message (语音) into text via NapCat's
/// `fetch_ptt_text`, so the persona can read the content of a
/// `[record msg id:…]` segment from an incoming or historical message.
pub struct OneBotVoiceTextTool {
    api: OneBotApi,
}

impl OneBotVoiceTextTool {
    pub fn new(api: OneBotApi) -> Self {
        Self { api }
    }
}

#[async_trait]
impl Tool for OneBotVoiceTextTool {
    fn name(&self) -> &str {
        "onebot_voice_text"
    }

    fn description(&self) -> &str {
        "Transcribe a voice message (语音) into text via the OneBot connection (NapCat \
         fetch_ptt_text). Pass the message id from a [record msg id:...] segment."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "message_id".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message id from a [record msg id:...] segment in an incoming or historical message".to_string(),
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

        // NapCat 的语音转写偶尔会因语音尚未处理完而临时失败，重试几次再放弃。
        const ATTEMPTS: usize = 3;
        const RETRY_DELAY: Duration = Duration::from_secs(2);
        let mut last_err = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match self.transcribe(message_id).await {
                Ok(text) => return Ok(text),
                Err(err) => {
                    last_err = format!("{err:#}");
                    log::warn!(
                        "onebot_voice_text attempt {}/{} failed for {message_id}: {err:#}",
                        attempt + 1,
                        ATTEMPTS
                    );
                }
            }
        }
        anyhow::bail!(
            "语音 {message_id} 转写失败（重试 {ATTEMPTS} 次）：{last_err}。语音可能还在处理中，稍后重试或让用户重新发送"
        )
    }
}

impl OneBotVoiceTextTool {
    /// One `fetch_ptt_text` call, rendered as a persona-facing string.
    async fn transcribe(&self, message_id: i64) -> Result<String> {
        let resp = self
            .api
            .call(ActionRequest::fetch_ptt_text(message_id))
            .await?;
        if resp.retcode != Some(0) {
            anyhow::bail!(
                "fetch_ptt_text failed: status={:?} retcode={:?}",
                resp.status,
                resp.retcode
            );
        }
        let data: FetchPttTextData = match resp.data {
            Some(v) => serde_json::from_value(v)?,
            None => anyhow::bail!("fetch_ptt_text returned no data"),
        };
        let text = data.text.trim();
        if text.is_empty() {
            Ok(format!("语音 {message_id} 没有可转写的文字内容"))
        } else {
            Ok(format!("语音 {message_id} 转文字: {text}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Outbound;
    use crate::client::PendingResponses;
    use crate::config::OnebotConfig;
    use crate::types::{ActionParams, ActionResponse};
    use nota_core::conversation::{AdapterEvent, ConversationManager};
    use nota_core::permissions::PermissionRegistry;
    use nota_core::tool::ToolContext;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

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

    /// OneBotApi wired to an action channel; returns the receiver and the
    /// pending map so tests can answer actions like the WS loop would.
    /// Connected is set to `true` so status tools can query login info.
    fn test_api() -> (OneBotApi, mpsc::UnboundedReceiver<ActionRequest>, PendingResponses) {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending: PendingResponses = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        (OneBotApi::new(tx, pending.clone(), connected), rx, pending)
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

    #[tokio::test]
    async fn onebot_send_msg_routes_target_event() {
        let tool = OneBotSendMsgTool;
        let context = tool_ctx();
        let mut rx = context.manager.subscribe_adapter("onebot");

        tool.run(r#"{"target":"group:30003","content":"yo"}"#, context)
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        let AdapterEvent::Outbound(e) = event else {
            panic!("expected outbound event");
        };
        assert_eq!(e.conversation_id.as_deref(), Some("onebot_private_42"));
        assert_eq!(e.target.as_deref(), Some("group:30003"));
        assert_eq!(e.content, "yo");
    }

    #[tokio::test]
    async fn onebot_send_msg_rejects_bad_target() {
        let tool = OneBotSendMsgTool;
        let err = tool
            .run(r#"{"target":"bogus","content":"yo"}"#, tool_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("target"));
    }

    #[tokio::test]
    async fn onebot_get_msg_history_reads_group() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotGetMsgHistoryTool::new(api);

        tokio::spawn(async move {
            let action = action_rx.recv().await.unwrap();
            assert_eq!(action.action, "get_group_msg_history");
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("ok".to_string()),
                retcode: Some(0),
                echo: Some(action.echo),
                data: Some(serde_json::json!({
                    "messages": [
                        {"message_id": 1, "user_id": 10001, "time": 1700000000,
                         "message": [{"type":"text","data":{"text":"hi"}}],
                         "sender": {"user_id": 10001, "nickname": "Alice"}}
                    ]
                })),
            })
            .unwrap();
        });

        let out = tool
            .run(r#"{"target":"group:30003","limit":10}"#, tool_ctx())
            .await
            .unwrap();
        assert!(out.contains("Recent messages in group:30003"));
        assert!(out.contains("Alice(10001) 消息ID:1: hi"));
    }

    #[tokio::test]
    async fn onebot_get_msg_history_reads_private() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotGetMsgHistoryTool::new(api);

        tokio::spawn(async move {
            let action = action_rx.recv().await.unwrap();
            assert_eq!(action.action, "get_friend_msg_history");
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("ok".to_string()),
                retcode: Some(0),
                echo: Some(action.echo),
                data: Some(serde_json::json!({"messages": []})),
            })
            .unwrap();
        });

        let out = tool
            .run(r#"{"target":"private:10001"}"#, tool_ctx())
            .await
            .unwrap();
        assert_eq!(out, "chat private:10001 has no readable recent messages");
    }

    #[tokio::test]
    async fn onebot_status_reports_connection_and_login_info() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotStatusTool::new(api, "ws://127.0.0.1:3001".to_string());

        tokio::spawn(async move {
            let action = action_rx.recv().await.unwrap();
            assert_eq!(action.action, "get_login_info");
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("ok".to_string()),
                retcode: Some(0),
                echo: Some(action.echo),
                data: Some(serde_json::json!({"user_id": 20002, "nickname": "Nota"})),
            })
            .unwrap();
        });

        let out = tool.run("{}", tool_ctx()).await.unwrap();
        assert!(out.contains("QQ 20002 (Nota)"));
        assert!(out.contains("connected: true"));
        assert!(out.contains("ws://127.0.0.1:3001"));
    }

    #[test]
    fn status_tracks_connected_flag() {
        let connected = Arc::new(AtomicBool::new(false));
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: PendingResponses = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let api = OneBotApi::new(tx, pending, connected.clone());
        assert!(!api.is_connected());
        connected.store(true, Ordering::SeqCst);
        assert!(api.is_connected());
    }

    #[tokio::test]
    async fn get_voice_text_returns_transcription() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotVoiceTextTool::new(api);

        tokio::spawn(async move {
            let action = action_rx.recv().await.unwrap();
            assert_eq!(action.action, "fetch_ptt_text");
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("ok".to_string()),
                retcode: Some(0),
                echo: Some(action.echo),
                data: Some(serde_json::json!({"text": "今天的天气真好"})),
            })
            .unwrap();
        });

        let out = tool
            .run(r#"{"message_id":"99"}"#, tool_ctx())
            .await
            .unwrap();
        assert_eq!(out, "语音 99 转文字: 今天的天气真好");
    }

    #[tokio::test]
    async fn get_voice_text_bails_on_failure() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotVoiceTextTool::new(api);

        tokio::spawn(async move {
            for _ in 0..3 {
                let action = action_rx.recv().await.unwrap();
                assert_eq!(action.action, "fetch_ptt_text");
                let tx = pending
                    .lock()
                    .unwrap()
                    .remove(&action.echo)
                    .expect("pending response registered");
                tx.send(ActionResponse {
                    status: Some("failed".to_string()),
                    retcode: Some(1200),
                    echo: Some(action.echo),
                    data: None,
                })
                .unwrap();
            }
        });

        let err = tool
            .run(r#"{"message_id":"99"}"#, tool_ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("转写失败（重试 3 次）"));
        assert!(err.to_string().contains("retcode=Some(1200)"));
    }

    #[tokio::test]
    async fn get_voice_text_retries_then_succeeds() {
        let (api, mut action_rx, pending) = test_api();
        let tool = OneBotVoiceTextTool::new(api);

        tokio::spawn(async move {
            // 第一次转写失败，第二次成功。
            let action = action_rx.recv().await.unwrap();
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("failed".to_string()),
                retcode: Some(1200),
                echo: Some(action.echo),
                data: None,
            })
            .unwrap();

            let action = action_rx.recv().await.unwrap();
            let tx = pending
                .lock()
                .unwrap()
                .remove(&action.echo)
                .expect("pending response registered");
            tx.send(ActionResponse {
                status: Some("ok".to_string()),
                retcode: Some(0),
                echo: Some(action.echo),
                data: Some(serde_json::json!({"text": "现在能听见了吗？"})),
            })
            .unwrap();
        });

        let out = tool
            .run(r#"{"message_id":"1193185804"}"#, tool_ctx())
            .await
            .unwrap();
        assert_eq!(out, "语音 1193185804 转文字: 现在能听见了吗？");
    }

    #[tokio::test]
    async fn get_voice_text_rejects_missing_id() {
        let (api, _, _) = test_api();
        let tool = OneBotVoiceTextTool::new(api);

        let err = tool.run("{}", tool_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("message_id"));
    }
}
