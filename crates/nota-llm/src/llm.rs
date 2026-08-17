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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Message roles accepted by the Responses API `message` input items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// One unit of a dialogue, mirroring the OpenAI message-item model: text
/// messages (`{role, content}`), function calls, and function outputs.
/// Sessions store items verbatim in this JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

#[derive(Debug, Clone, Serialize)]
pub struct LlmResponse {
    /// The Responses API id of this response, if the provider returns one.
    /// Saved per LLM session for future stateful continuations.
    pub id: Option<String>,
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
