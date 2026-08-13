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

#[derive(Debug, Clone, Serialize)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Raw JSON of the message (role / content / tool_calls / tool_call_id),
    /// rendered here in the llm module so history can store the original
    /// tool-call payload without `serde_json` in core.
    pub fn raw_json(&self) -> String {
        let mut s = format!(r#"{{"role":"{}""#, escape_json(&self.role));
        if let Some(content) = &self.content {
            s.push_str(&format!(r#","content":"{}""#, escape_json(content)));
        }
        if let Some(calls) = &self.tool_calls {
            let calls_json = calls
                .iter()
                .map(ToolCall::raw_json)
                .collect::<Vec<_>>()
                .join(",");
            s.push_str(&format!(r#","tool_calls":[{calls_json}]"#));
        }
        if let Some(id) = &self.tool_call_id {
            s.push_str(&format!(r#","tool_call_id":"{}""#, escape_json(id)));
        }
        s.push('}');
        s
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<LlmResponse>;
}
