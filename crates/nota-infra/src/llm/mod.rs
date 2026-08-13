use anyhow::Result;
use async_trait::async_trait;
use nota_core::llm::{ChatMessage, LlmClient, LlmResponse, ToolCall, ToolDef};
use serde::{Deserialize, Serialize};

/// LLM API format names, matching `Config.api_mode`.
pub const MODE_RESPONSES: &str = "responses";
pub const MODE_CHAT: &str = "chat";

// ── Chat Completions wire types (legacy `chat` mode) ──────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiTool>>,
}

#[derive(Serialize)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    tool_type: String,
    function: WireToolCallFunction,
}

#[derive(Serialize)]
struct WireToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: ApiToolFunction,
}

#[derive(Serialize)]
struct ApiToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCall>,
}

#[derive(Deserialize)]
struct ChatToolCall {
    id: String,
    function: ToolCallFunction,
}

#[derive(Deserialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

// ── Responses API wire types (default `responses` mode) ────────────────

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

#[derive(Serialize)]
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

#[derive(Serialize)]
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
    /// "responses" (default) or "chat".
    api_mode: String,
    /// Attach the built-in `web_search` tool in responses mode.
    web_search: bool,
    client: reqwest::Client,
}

impl OpenAiLlm {
    pub fn new(
        api_url: &str,
        api_key: &str,
        model: &str,
        api_mode: &str,
        web_search: bool,
    ) -> Self {
        Self {
            api_url: api_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            api_mode: if api_mode.is_empty() {
                MODE_RESPONSES.to_string()
            } else {
                api_mode.to_string()
            },
            web_search,
            client: reqwest::Client::new(),
        }
    }

    async fn chat_responses(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let instructions = (!system.is_empty()).then(|| system.to_string());
        let input = to_responses_input(messages);

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

/// Convert the core `ChatMessage` history into Responses API input items.
///
/// DeepSeek's Responses endpoint strictly requires each `function_call_output`
/// to directly follow its matching `function_call` (OpenAI tolerates
/// interleaving, DeepSeek does not), so tool calls are paired with their
/// results on sight rather than batched after the assistant message.
fn to_responses_input(messages: &[ChatMessage]) -> Vec<ResponsesInputItem> {
    let mut items = Vec::new();
    let mut pending_calls: Vec<ToolCall> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "user" | "system" | "developer" => {
                if let Some(content) = &msg.content {
                    items.push(ResponsesInputItem::Message {
                        role: msg.role.clone(),
                        content: vec![ResponsesContentPart::InputText {
                            text: content.clone(),
                        }],
                    });
                }
            }
            "assistant" => {
                if let Some(content) = &msg.content {
                    items.push(ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content: vec![ResponsesContentPart::OutputText {
                            text: content.clone(),
                        }],
                    });
                }
                if let Some(calls) = &msg.tool_calls {
                    pending_calls.extend(calls.iter().cloned());
                }
            }
            "tool" => {
                if let Some(call_id) = &msg.tool_call_id
                    && let Some(pos) = pending_calls.iter().position(|c| &c.id == call_id)
                {
                    let call = pending_calls.remove(pos);
                    items.push(ResponsesInputItem::FunctionCall {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    items.push(ResponsesInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: msg.content.clone().unwrap_or_default(),
                    });
                } else {
                    log::debug!("tool result with unknown call_id dropped");
                }
            }
            _ => {
                // Unknown role: forward as-is so the provider can decide.
                if let Some(content) = &msg.content {
                    items.push(ResponsesInputItem::Message {
                        role: msg.role.clone(),
                        content: vec![ResponsesContentPart::InputText {
                            text: content.clone(),
                        }],
                    });
                }
            }
        }
    }

    // Calls without a matching result yet (should not happen in the agent
    // loop) are still sent so the model sees its pending function calls.
    for call in pending_calls {
        items.push(ResponsesInputItem::FunctionCall {
            call_id: call.id,
            name: call.name,
            arguments: call.arguments,
        });
    }

    items
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

fn to_wire_message(msg: &ChatMessage) -> WireMessage {
    WireMessage {
        role: msg.role.clone(),
        content: msg.content.clone(),
        tool_calls: msg.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| WireToolCall {
                    id: tc.id.clone(),
                    tool_type: "function".to_string(),
                    function: WireToolCallFunction {
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    },
                })
                .collect()
        }),
        tool_call_id: msg.tool_call_id.clone(),
    }
}

#[async_trait]
impl LlmClient for OpenAiLlm {
    async fn chat(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        match self.api_mode.as_str() {
            MODE_RESPONSES => self.chat_responses(system, messages, tools).await,
            MODE_CHAT => self.chat_completions(system, messages, tools).await,
            other => anyhow::bail!(
                "unknown llm api_mode: {other} (expected \"responses\" or \"chat\")"
            ),
        }
    }
}

impl OpenAiLlm {
    /// Legacy Chat Completions implementation, kept as the `chat` fallback.
    async fn chat_completions(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let mut chat_messages: Vec<ChatMessage> = Vec::new();

        if !system.is_empty() {
            chat_messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(system.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        chat_messages.extend(messages.iter().cloned());

        let api_tools: Vec<ApiTool> = tools
            .iter()
            .map(|t| ApiTool {
                tool_type: "function".to_string(),
                function: ApiToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: serde_json::to_value(&t.parameters)
                        .unwrap_or(serde_json::Value::Null),
                },
            })
            .collect();

        let wire_messages: Vec<WireMessage> = chat_messages.iter().map(to_wire_message).collect();

        let req = ChatRequest {
            model: self.model.clone(),
            messages: wire_messages,
            tools: if api_tools.is_empty() { None } else { Some(api_tools) },
        };

        let url = format!("{}/chat/completions", self.api_url);
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

        let chat_resp: ChatResponse = resp.json().await?;
        let choice = chat_resp.choices.into_iter().next();
        let msg = choice.map(|c| c.message);

        let content = msg.as_ref().and_then(|m| m.content.clone());
        let tool_calls = msg
            .map(|m| {
                m.tool_calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(LlmResponse { content, tool_calls })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.map(|s| s.to_string()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn responses_input_pairs_each_call_with_its_output() {
        let messages = vec![
            msg("user", Some("hi")),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_A".to_string(),
                        name: "foo".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "call_B".to_string(),
                        name: "bar".to_string(),
                        arguments: "{}".to_string(),
                    },
                ]),
                tool_call_id: None,
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some("result A".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_A".to_string()),
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some("result B".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_B".to_string()),
            },
        ];

        let items = to_responses_input(&messages);
        let types: Vec<&str> = items
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
    }

    #[test]
    fn responses_input_serializes_with_expected_shape() {
        let items = to_responses_input(&[msg("user", Some("hi"))]);
        let value = serde_json::to_value(&items).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hi" }],
                }
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
