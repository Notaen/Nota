use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::ToolParams;

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: ToolParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// Render the raw tool-call JSON (`{"id":…,"name":…,"arguments":…}`)
    /// without pulling `serde_json` into core.
    pub fn raw_json(&self) -> String {
        format!(
            r#"{{"id":"{}","name":"{}","arguments":{}}}"#,
            self.id, self.name, self.arguments
        )
    }
}

/// Message roles accepted by the Responses API `message` input items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }
}

/// One unit of conversation history, mirroring the Responses API `input`
/// item model: text messages, function calls, and function outputs.
#[derive(Debug, Clone, Serialize)]
pub enum LlmItem {
    Message {
        role: MessageRole,
        content: String,
    },
    FunctionCall(ToolCall),
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

impl LlmItem {
    /// Render the raw JSON payload for history storage (no `serde_json` in
    /// core): messages keep their role/content, tool items keep their
    /// original call/output payload.
    pub fn raw_json(&self) -> String {
        match self {
            LlmItem::Message { role, content } => format!(
                r#"{{"role":"{}","content":"{}"}}"#,
                role.as_str(),
                escape_json(content)
            ),
            LlmItem::FunctionCall(call) => call.raw_json(),
            LlmItem::FunctionCallOutput { call_id, output } => format!(
                r#"{{"call_id":"{}","output":"{}"}}"#,
                escape_json(call_id),
                escape_json(output)
            ),
        }
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(
        &self,
        system: &str,
        items: &[LlmItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse>;
}
