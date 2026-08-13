use anyhow::Result;
use async_trait::async_trait;
use nota_core::llm::{LlmClient, LlmItem, LlmResponse, MessageRole, ToolCall, ToolDef};
use serde::{Deserialize, Serialize};

// ── Responses API wire types ─────────────────────────────────────────

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesInputItem {
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContentPart {
    InputText { text: String },
    OutputText { text: String },
}

/// Tools in the Responses API. Function tools are flat (`type`, `name`,
/// `description`, `parameters`) instead of nested under `function`; the
/// built-in `web_search` tool has no extra fields (DeepSeek executes it
/// server-side).
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesTool {
    Function {
        name: String,
        description: String,
        parameters: serde_json::Value,
    },
    WebSearch,
}

#[derive(Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    /// Provider convenience field: concatenated assistant text.
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponsesOutputPart>,
    },
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesOutputPart {
    OutputText { text: String },
    InputText { text: String },
    #[serde(other)]
    Unknown,
}

pub struct OpenAiLlm {
    api_url: String,
    api_key: String,
    model: String,
    /// Attach the built-in `web_search` tool to every request.
    web_search: bool,
    client: reqwest::Client,
}

impl OpenAiLlm {
    pub fn new(api_url: &str, api_key: &str, model: &str, web_search: bool) -> Self {
        Self {
            api_url: api_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            web_search,
            client: reqwest::Client::new(),
        }
    }

    async fn chat_responses(
        &self,
        system: &str,
        items: &[LlmItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let instructions = (!system.is_empty()).then(|| system.to_string());
        let input = to_responses_input(items);

        let api_tools = build_responses_tools(tools, self.web_search);

        let req = ResponsesRequest {
            model: self.model.clone(),
            instructions,
            input,
            tools: if api_tools.is_empty() { None } else { Some(api_tools) },
        };

        let url = format!("{}/responses", self.api_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API error ({}): {}", status, body);
        }

        let parsed: ResponsesResponse = resp.json().await?;
        if let Some(status) = &parsed.status
            && status != "completed"
        {
            log::warn!("Responses API returned status: {status}");
        }

        let mut content = String::new();
        let mut tool_calls = Vec::new();

        for item in parsed.output {
            match item {
                ResponsesOutputItem::Message { content: parts, .. } => {
                    for part in parts {
                        match part {
                            ResponsesOutputPart::OutputText { text }
                            | ResponsesOutputPart::InputText { text } => content.push_str(&text),
                            ResponsesOutputPart::Unknown => {}
                        }
                    }
                }
                ResponsesOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } => {
                    let Some(call_id) = call_id.or(id) else {
                        log::warn!("Responses API returned function_call without an id");
                        continue;
                    };
                    tool_calls.push(ToolCall {
                        id: call_id,
                        name,
                        arguments,
                    });
                }
                ResponsesOutputItem::Unknown => {}
            }
        }

        let content = if content.is_empty() {
            parsed.output_text.filter(|t| !t.is_empty())
        } else {
            Some(content)
        };

        Ok(LlmResponse { content, tool_calls })
    }
}

#[async_trait]
impl LlmClient for OpenAiLlm {
    async fn chat(
        &self,
        system: &str,
        items: &[LlmItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        self.chat_responses(system, items, tools).await
    }
}

/// Map core `LlmItem`s one-to-one onto Responses API input items. The agent
/// loop already emits each `function_call` immediately followed by its
/// `function_call_output`, satisfying DeepSeek's strict adjacency rule.
fn to_responses_input(items: &[LlmItem]) -> Vec<ResponsesInputItem> {
    items
        .iter()
        .map(|item| match item {
            LlmItem::Message { role, content } => {
                let part = match role {
                    MessageRole::User => ResponsesContentPart::InputText {
                        text: content.clone(),
                    },
                    MessageRole::Assistant => ResponsesContentPart::OutputText {
                        text: content.clone(),
                    },
                };
                ResponsesInputItem::Message {
                    role: role.as_str().to_string(),
                    content: vec![part],
                }
            }
            LlmItem::FunctionCall(call) => ResponsesInputItem::FunctionCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
            LlmItem::FunctionCallOutput { call_id, output } => {
                ResponsesInputItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: output.clone(),
                }
            }
        })
        .collect()
}

fn build_responses_tools(tools: &[ToolDef], web_search: bool) -> Vec<ResponsesTool> {
    let mut api_tools: Vec<ResponsesTool> = tools
        .iter()
        .map(|t| ResponsesTool::Function {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: serde_json::to_value(&t.parameters).unwrap_or(serde_json::Value::Null),
        })
        .collect();
    if web_search {
        api_tools.push(ResponsesTool::WebSearch);
    }
    api_tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, content: &str) -> LlmItem {
        LlmItem::Message {
            role,
            content: content.to_string(),
        }
    }

    #[test]
    fn responses_input_keeps_call_and_output_pairing() {
        // This is exactly the order the agent loop emits: each function_call
        // is immediately followed by its function_call_output.
        let items = vec![
            msg(MessageRole::User, "hi"),
            LlmItem::FunctionCall(ToolCall {
                id: "call_A".to_string(),
                name: "foo".to_string(),
                arguments: "{}".to_string(),
            }),
            LlmItem::FunctionCallOutput {
                call_id: "call_A".to_string(),
                output: "result A".to_string(),
            },
            LlmItem::FunctionCall(ToolCall {
                id: "call_B".to_string(),
                name: "bar".to_string(),
                arguments: "{}".to_string(),
            }),
            LlmItem::FunctionCallOutput {
                call_id: "call_B".to_string(),
                output: "result B".to_string(),
            },
        ];

        let wire = to_responses_input(&items);
        let types: Vec<&str> = wire
            .iter()
            .map(|item| match item {
                ResponsesInputItem::Message { .. } => "message",
                ResponsesInputItem::FunctionCall { .. } => "function_call",
                ResponsesInputItem::FunctionCallOutput { .. } => "function_call_output",
            })
            .collect();

        // Each function_call_output must directly follow its function_call.
        assert_eq!(
            types,
            vec![
                "message",
                "function_call",
                "function_call_output",
                "function_call",
                "function_call_output",
            ]
        );

        // Each output carries the call id of the call directly before it.
        assert_eq!(wire.len(), 5);
        assert_eq!(
            wire[1],
            ResponsesInputItem::FunctionCall {
                call_id: "call_A".to_string(),
                name: "foo".to_string(),
                arguments: "{}".to_string(),
            }
        );
        assert_eq!(
            wire[2],
            ResponsesInputItem::FunctionCallOutput {
                call_id: "call_A".to_string(),
                output: "result A".to_string(),
            }
        );
    }

    #[test]
    fn responses_input_serializes_with_expected_shape() {
        let items = to_responses_input(&[
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Assistant, "hello"),
        ]);
        let value = serde_json::to_value(&items).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hi" }],
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "hello" }],
                },
            ])
        );
    }

    #[test]
    fn responses_tools_include_web_search_by_default() {
        let tools = vec![ToolDef {
            name: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: nota_core::tool::ToolParams::object(
                std::collections::HashMap::new(),
                Vec::new(),
            ),
        }];

        let value = serde_json::to_value(build_responses_tools(&tools, true)).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": { "type": "object", "properties": {}, "required": [] },
                },
                { "type": "web_search" },
            ])
        );
    }

    #[test]
    fn responses_tools_omit_web_search_when_disabled() {
        let value = serde_json::to_value(build_responses_tools(&[], false)).unwrap();
        assert_eq!(value, serde_json::json!([]));
    }

    #[test]
    fn parses_responses_output() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "hi there" }],
                },
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"sh\"}",
                },
                {
                    "type": "function_call",
                    "id": "fc_2",
                    "name": "other",
                    "arguments": "{}",
                },
                { "type": "reasoning", "summary": [] },
            ]
        });

        let parsed: ResponsesResponse = serde_json::from_value(body).unwrap();
        let mut content = String::new();
        let mut calls = Vec::new();
        for item in parsed.output {
            match item {
                ResponsesOutputItem::Message { content: parts, .. } => {
                    for part in parts {
                        if let ResponsesOutputPart::OutputText { text } = part {
                            content.push_str(&text);
                        }
                    }
                }
                ResponsesOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } => {
                    let id = call_id.or(id).expect("call id");
                    calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                ResponsesOutputItem::Unknown => {}
            }
        }

        assert_eq!(content, "hi there");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "fc_2");
    }
}
