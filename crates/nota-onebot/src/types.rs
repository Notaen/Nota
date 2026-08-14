//! OneBot 11 wire types (protocol boundary; JSON serialization lives here).
//!
//! This covers the subset of the OneBot 11 spec the bridge needs: message
//! events (private/group), message segments, and the `send_private_msg` /
//! `send_group_msg` actions. Events we don't act on (notice / request /
//! meta_event) still parse so the client can tell them apart from action
//! responses.

use std::collections::HashMap;

use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// OneBot 11 post payload, discriminated by `post_type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "post_type", rename_all = "snake_case")]
pub enum PostEvent {
    Message(MessageEvent),
    Notice,
    Request,
    #[serde(rename = "meta_event")]
    MetaEvent,
}

/// A private or group message event.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEvent {
    /// QQ number of the bot itself.
    pub self_id: i64,
    /// Event timestamp (Unix seconds).
    pub time: i64,
    pub message_id: i64,
    /// `"private"` or `"group"`.
    pub message_type: String,
    #[serde(default)]
    pub sub_type: Option<String>,
    /// Sender QQ number.
    pub user_id: i64,
    /// Message body; implementations may send an array of segments or a string.
    #[serde(default)]
    pub message: Option<MessageContent>,
    /// Present only for group messages.
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub sender: Option<Sender>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sender {
    pub user_id: i64,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub card: Option<String>,
}

impl Sender {
    /// Best available display name: group card > nickname; empty if unknown.
    pub fn display_name(&self) -> String {
        self.card
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| Some(self.nickname.clone()))
            .filter(|n| !n.is_empty())
            .unwrap_or_default()
    }
}

/// Render a chat participant as `nickname(QQ)` (card preferred over
/// nickname), falling back to the bare QQ number when no name is known.
pub fn identity(sender: Option<&Sender>, user_id: i64) -> String {
    match sender.map(Sender::display_name).filter(|n| !n.is_empty()) {
        Some(name) => format!("{name}({user_id})"),
        None => user_id.to_string(),
    }
}

/// OneBot 11 sends `message` either as a plain string or as an array of
/// segments depending on the implementation's `message_format` setting.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Segments(Vec<MessageSegment>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageSegment {
    #[serde(rename = "type")]
    pub segment_type: String,
    #[serde(default)]
    pub data: HashMap<String, serde_json::Value>,
}

impl MessageContent {
    /// Flatten to plain text for local parsing (e.g. approval commands);
    /// non-text segments become `[{type}]` placeholders without a message id.
    pub fn to_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Segments(segs) => {
                segs.iter().map(segment_to_text).collect()
            }
        }
    }

    /// Flatten to plain text for the LLM. Text segments keep their content;
    /// every other segment is rendered uniformly as
    /// `[{segment_type} msg id:<id>]` so the persona knows what kind of
    /// media arrived and can fetch its content with a tool (`get_msg`,
    /// `get_voice_text`, …).
    pub fn to_text_with_id(&self, message_id: &str) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Segments(segs) => {
                segs.iter()
                    .map(|seg| segment_to_text_with_id(seg, message_id))
                    .collect()
            }
        }
    }
}

fn segment_to_text(seg: &MessageSegment) -> String {
    let data = &seg.data;
    match seg.segment_type.as_str() {
        "text" => data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        other => format!("[{other}]"),
    }
}

fn json_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Render a segment for [`MessageContent::to_text_with_id`]: text segments
/// keep their content, every other segment becomes
/// `[{segment_type} msg id:{message_id}]` so the persona can fetch the real
/// content (image OCR, voice transcription, …) with a tool.
fn segment_to_text_with_id(seg: &MessageSegment, message_id: &str) -> String {
    let data = &seg.data;
    match seg.segment_type.as_str() {
        "text" => data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        other => format!("[{other} msg id:{message_id}]"),
    }
}

/// Deserialize a message id that implementations may send as either a
/// JSON number or a string.
fn de_id_as_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(json_value_to_string(&v))
}

/// Action request sent to the OneBot implementation over the same WS
/// connection; `echo` correlates the response.
#[derive(Debug, Clone, Serialize)]
pub struct ActionRequest {
    pub action: String,
    pub params: ActionParams,
    pub echo: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ActionParams {
    Private {
        user_id: i64,
        message: Vec<TextSegment>,
    },
    Group {
        group_id: i64,
        message: Vec<TextSegment>,
    },
    GroupHistory {
        group_id: i64,
        message_seq: i64,
        count: i64,
    },
    /// No parameters (e.g. `get_login_info`).
    LoginInfo {},
    GetMsg {
        message_id: i64,
    },
    FetchPttText {
        message_id: String,
    },
}

/// Outbound message segment; always a single text segment for now.
#[derive(Debug, Clone, Serialize)]
pub struct TextSegment {
    #[serde(rename = "type")]
    pub segment_type: String,
    pub data: TextSegmentData,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextSegmentData {
    pub text: String,
}

impl ActionRequest {
    pub fn send_private_msg(user_id: i64, text: &str) -> Self {
        Self {
            action: "send_private_msg".to_string(),
            params: ActionParams::Private {
                user_id,
                message: text_segments(text),
            },
            echo: Uuid::new_v4().to_string(),
        }
    }

    pub fn send_group_msg(group_id: i64, text: &str) -> Self {
        Self {
            action: "send_group_msg".to_string(),
            params: ActionParams::Group {
                group_id,
                message: text_segments(text),
            },
            echo: Uuid::new_v4().to_string(),
        }
    }

    /// NapCat / go-cqhttp extended API: fetch recent messages of a group.
    pub fn get_group_msg_history(group_id: i64, count: i64) -> Self {
        Self {
            action: "get_group_msg_history".to_string(),
            params: ActionParams::GroupHistory {
                group_id,
                message_seq: 0,
                count,
            },
            echo: Uuid::new_v4().to_string(),
        }
    }

    /// Standard OneBot API: query the bot's own account info.
    pub fn get_login_info() -> Self {
        Self {
            action: "get_login_info".to_string(),
            params: ActionParams::LoginInfo {},
            echo: Uuid::new_v4().to_string(),
        }
    }

    /// Standard OneBot API: fetch a single message by its message id (e.g.
    /// the id from a `[回复消息ID:…]` quote).
    pub fn get_msg(message_id: i64) -> Self {
        Self {
            action: "get_msg".to_string(),
            params: ActionParams::GetMsg { message_id },
            echo: Uuid::new_v4().to_string(),
        }
    }

    /// NapCat extended API: transcribe the voice message of `message_id`
    /// (`fetch_ptt_text`, requires NapCat >= 4.18.2).
    pub fn fetch_ptt_text(message_id: i64) -> Self {
        Self {
            action: "fetch_ptt_text".to_string(),
            params: ActionParams::FetchPttText {
                message_id: message_id.to_string(),
            },
            echo: Uuid::new_v4().to_string(),
        }
    }
}

pub fn text_segments(text: &str) -> Vec<TextSegment> {
    vec![TextSegment {
        segment_type: "text".to_string(),
        data: TextSegmentData {
            text: text.to_string(),
        },
    }]
}

/// Action response from the implementation (matched by `echo`).
#[derive(Debug, Clone, Deserialize)]
pub struct ActionResponse {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub retcode: Option<i64>,
    #[serde(default)]
    pub echo: Option<String>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// `data` payload of `get_group_msg_history`.
#[derive(Debug, Clone, Deserialize)]
pub struct GroupMsgHistoryData {
    #[serde(default)]
    pub messages: Vec<HistoryMessage>,
}

/// A message as returned by the history APIs (OneBot message object).
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryMessage {
    #[serde(default, deserialize_with = "de_id_as_string")]
    pub message_id: String,
    #[serde(default)]
    pub message_seq: i64,
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub time: i64,
    #[serde(default)]
    pub message: Option<MessageContent>,
    #[serde(default)]
    pub sender: Option<Sender>,
    #[serde(default)]
    pub group_id: Option<i64>,
}

/// `data` payload of `get_msg`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetMsgData {
    #[serde(default, deserialize_with = "de_id_as_string")]
    pub message_id: String,
    #[serde(default)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub time: i64,
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub message: Option<MessageContent>,
    #[serde(default)]
    pub sender: Option<Sender>,
}

/// `data` payload of `get_login_info`.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginInfoData {
    pub user_id: i64,
    #[serde(default)]
    pub nickname: String,
}

/// `data` payload of `fetch_ptt_text` (NapCat voice-to-text).
#[derive(Debug, Clone, Deserialize)]
pub struct FetchPttTextData {
    pub text: String,
}

/// Render history messages as readable text for the LLM, one per line:
/// `[HH:MM] nickname(QQ) 消息ID:{id}: text`.
pub fn format_history(messages: &[HistoryMessage]) -> String {
    let mut out = Vec::new();
    for msg in messages {
        let text = msg
            .message
            .as_ref()
            .map(|m| m.to_text_with_id(&msg.message_id))
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let who = identity(msg.sender.as_ref(), msg.user_id);
        let ts = chrono::Local
            .timestamp_opt(msg.time, 0)
            .single()
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_string());
        out.push(format!("[{ts}] {who} 消息ID:{}: {text}", msg.message_id));
    }
    out.join("\n")
}

/// Where a persona reply must be delivered.
#[derive(Debug, Clone)]
pub enum ReplyRoute {
    Private { user_id: i64 },
    Group { group_id: i64 },
}

impl ReplyRoute {
    pub fn to_action(&self, text: &str) -> ActionRequest {
        match self {
            ReplyRoute::Private { user_id } => {
                ActionRequest::send_private_msg(*user_id, text)
            }
            ReplyRoute::Group { group_id } => {
                ActionRequest::send_group_msg(*group_id, text)
            }
        }
    }
}

/// Split text into chunks of at most `max_chars` characters, so long replies
/// stay under the QQ message length limit.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count == max_chars {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_private_message_with_segments() {
        let json = r#"{
            "post_type": "message",
            "message_type": "private",
            "sub_type": "friend",
            "message_id": 123,
            "user_id": 10001,
            "self_id": 20002,
            "time": 1700000000,
            "message": [
                {"type": "text", "data": {"text": "hello "}},
                {"type": "face", "data": {"id": "1"}},
                {"type": "text", "data": {"text": " world"}}
            ],
            "sender": {"user_id": 10001, "nickname": "Alice"}
        }"#;
        let event: PostEvent = serde_json::from_str(json).unwrap();
        let PostEvent::Message(msg) = event else {
            panic!("expected message event");
        };
        assert_eq!(msg.message_type, "private");
        assert_eq!(msg.user_id, 10001);
        assert_eq!(msg.message.unwrap().to_text(), "hello [face] world");
    }

    #[test]
    fn parses_group_message_with_string_message() {
        let json = r#"{
            "post_type": "message",
            "message_type": "group",
            "message_id": 456,
            "user_id": 10001,
            "self_id": 20002,
            "time": 1700000001,
            "group_id": 30003,
            "message": "@someone 你好",
            "sender": {"user_id": 10001, "nickname": "Alice", "card": "A"}
        }"#;
        let event: PostEvent = serde_json::from_str(json).unwrap();
        let PostEvent::Message(msg) = event else {
            panic!("expected message event");
        };
        assert_eq!(msg.message_type, "group");
        assert_eq!(msg.group_id, Some(30003));
        assert_eq!(msg.message.unwrap().to_text(), "@someone 你好");
    }

    #[test]
    fn ignores_notice_and_meta_events() {
        let notice: PostEvent =
            serde_json::from_str(r#"{"post_type":"notice","notice_type":"group_upload"}"#)
                .unwrap();
        assert!(matches!(notice, PostEvent::Notice));
        let meta: PostEvent =
            serde_json::from_str(r#"{"post_type":"meta_event","meta_event_type":"heartbeat"}"#)
                .unwrap();
        assert!(matches!(meta, PostEvent::MetaEvent));
    }

    #[test]
    fn serializes_group_action() {
        let action = ActionRequest::send_group_msg(30003, "hi");
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["action"], "send_group_msg");
        assert_eq!(value["params"]["group_id"], 30003);
        assert_eq!(value["params"]["message"][0]["type"], "text");
        assert_eq!(value["params"]["message"][0]["data"]["text"], "hi");
        assert!(value["echo"].is_string());
    }

    #[test]
    fn serializes_group_history_action() {
        let action = ActionRequest::get_group_msg_history(30003, 20);
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["action"], "get_group_msg_history");
        assert_eq!(value["params"]["group_id"], 30003);
        assert_eq!(value["params"]["count"], 20);
        assert_eq!(value["params"]["message_seq"], 0);
    }

    #[test]
    fn parses_history_response_and_formats() {
        let json = r#"{
            "status": "ok",
            "retcode": 0,
            "data": {
                "messages": [
                    {
                        "message_id": 1,
                        "user_id": 10001,
                        "time": 1700000000,
                        "message": [{"type":"text","data":{"text":"hello"}}],
                        "sender": {"user_id": 10001, "nickname": "Alice"}
                    },
                    {
                        "message_id": 2,
                        "user_id": 10002,
                        "time": 1700000001,
                        "message": "world",
                        "sender": {"user_id": 10002, "nickname": "Bob", "card": "B"}
                    }
                ]
            }
        }"#;
        let resp: ActionResponse = serde_json::from_str(json).unwrap();
        let data: GroupMsgHistoryData =
            serde_json::from_value(resp.data.unwrap()).unwrap();
        let text = format_history(&data.messages);
        assert!(text.contains("Alice(10001) 消息ID:1: hello"));
        assert!(text.contains("B(10002) 消息ID:2: world"));
        assert!(text.lines().count() == 2);
    }

    #[test]
    fn parses_reply_segment_with_id() {
        let content = MessageContent::Segments(vec![
            MessageSegment {
                segment_type: "reply".to_string(),
                data: HashMap::from([("id".to_string(), serde_json::json!("1234567890"))]),
            },
            MessageSegment {
                segment_type: "text".to_string(),
                data: HashMap::from([("text".to_string(), serde_json::json!(" 收到"))]),
            },
        ]);
        assert_eq!(content.to_text(), "[reply] 收到");
    }

    #[test]
    fn parses_reply_segment_with_numeric_id() {
        let content = MessageContent::Segments(vec![MessageSegment {
            segment_type: "reply".to_string(),
            data: HashMap::from([("id".to_string(), serde_json::json!(42))]),
        }]);
        assert_eq!(content.to_text(), "[reply]");
    }

    #[test]
    fn renders_record_segment_with_message_id() {
        let voice = MessageContent::Segments(vec![MessageSegment {
            segment_type: "record".to_string(),
            data: HashMap::from([("file".to_string(), serde_json::json!("voice.amr"))]),
        }]);
        assert_eq!(voice.to_text(), "[record]");
        assert_eq!(voice.to_text_with_id("99"), "[record msg id:99]");

        let mixed = MessageContent::Segments(vec![
            MessageSegment {
                segment_type: "text".to_string(),
                data: HashMap::from([("text".to_string(), serde_json::json!("收到 "))]),
            },
            MessageSegment {
                segment_type: "record".to_string(),
                data: HashMap::new(),
            },
        ]);
        assert_eq!(mixed.to_text_with_id("42"), "收到 [record msg id:42]");
    }

    #[test]
    fn renders_all_non_text_segments_uniformly_with_message_id() {
        let content = MessageContent::Segments(vec![
            MessageSegment {
                segment_type: "image".to_string(),
                data: HashMap::new(),
            },
            MessageSegment {
                segment_type: "face".to_string(),
                data: HashMap::from([("id".to_string(), serde_json::json!(1))]),
            },
            MessageSegment {
                segment_type: "video".to_string(),
                data: HashMap::new(),
            },
            MessageSegment {
                segment_type: "at".to_string(),
                data: HashMap::from([("qq".to_string(), serde_json::json!("10001"))]),
            },
            MessageSegment {
                segment_type: "forward".to_string(),
                data: HashMap::new(),
            },
            MessageSegment {
                segment_type: "some_future_type".to_string(),
                data: HashMap::new(),
            },
        ]);
        assert_eq!(
            content.to_text_with_id("77"),
            "[image msg id:77][face msg id:77][video msg id:77][at msg id:77][forward msg id:77][some_future_type msg id:77]"
        );
        assert_eq!(
            content.to_text(),
            "[image][face][video][at][forward][some_future_type]"
        );
    }

    #[test]
    fn serializes_login_info_action() {
        let action = ActionRequest::get_login_info();
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["action"], "get_login_info");
        assert!(value["echo"].is_string());
        let params = value["params"].as_object().unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn serializes_get_msg_action() {
        let action = ActionRequest::get_msg(1234567890);
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["action"], "get_msg");
        assert_eq!(value["params"]["message_id"], 1234567890);
        assert!(value["echo"].is_string());
    }

    #[test]
    fn serializes_fetch_ptt_text_action() {
        let action = ActionRequest::fetch_ptt_text(99);
        let value = serde_json::to_value(&action).unwrap();
        assert_eq!(value["action"], "fetch_ptt_text");
        assert_eq!(value["params"]["message_id"], "99");
        assert!(value["echo"].is_string());
    }

    #[test]
    fn parses_get_msg_response() {
        let json = r#"{
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 1234567890,
                "message_type": "group",
                "time": 1700000000,
                "user_id": 10001,
                "message": [{"type":"text","data":{"text":"具体回复了这句"}}],
                "sender": {"user_id": 10001, "nickname": "Alice"}
            }
        }"#;
        let resp: ActionResponse = serde_json::from_str(json).unwrap();
        let data: GetMsgData = serde_json::from_value(resp.data.unwrap()).unwrap();
        assert_eq!(data.message_id, "1234567890");
        assert_eq!(data.message_type.as_deref(), Some("group"));
        assert_eq!(data.message.unwrap().to_text(), "具体回复了这句");
    }

    #[test]
    fn parses_fetch_ptt_text_response() {
        let json = r#"{
            "status": "ok",
            "retcode": 0,
            "data": {"text": "语音转写结果"}
        }"#;
        let resp: ActionResponse = serde_json::from_str(json).unwrap();
        let data: FetchPttTextData = serde_json::from_value(resp.data.unwrap()).unwrap();
        assert_eq!(data.text, "语音转写结果");
    }

    #[test]
    fn parses_login_info_response() {
        let json = r#"{
            "status": "ok",
            "retcode": 0,
            "data": {"user_id": 20002, "nickname": "Nota"}
        }"#;
        let resp: ActionResponse = serde_json::from_str(json).unwrap();
        let data: LoginInfoData = serde_json::from_value(resp.data.unwrap()).unwrap();
        assert_eq!(data.user_id, 20002);
        assert_eq!(data.nickname, "Nota");
    }

    #[test]
    fn chunks_long_text() {
        let text = "a".repeat(9000);
        let chunks = chunk_text(&text, 4000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.iter().map(String::len).sum::<usize>(), 9000);
        assert_eq!(chunk_text("short", 4000), vec!["short".to_string()]);
    }

}
