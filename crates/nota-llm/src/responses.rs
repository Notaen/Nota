//! Responses API client (internal to the llm crate — there is no public
//! LLM client abstraction; the session manager owns the only client).

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nota_core::session::{MessageRole, SessionItem, ToolCall, ToolCallKind};
use nota_core::tool::ToolParams;

// ── LLM-facing wire shapes (internal) ────────────────────────────────

/// One tool attached to an LLM request, built from a core `Tool`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: ToolParams,
}

/// The model's answer to one chat call.
#[derive(Debug, Clone, Serialize)]
pub struct LlmResponse {
    /// The Responses API id of this response, if the provider returns one.
    /// Saved per session for future stateful continuations.
    pub id: Option<String>,
    pub content: Option<String>,
    /// Reasoning text emitted by the model (e.g. DeepSeek `reasoning`
    /// items), persisted per session and passed back verbatim on subsequent
    /// requests — DeepSeek's thinking mode requires it.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// Test seam inside the crate: the concrete session manager talks to the
/// model through this internal trait, so tests can substitute a mock.
#[async_trait]
pub(crate) trait ChatLlm: Send + Sync {
    async fn chat(
        &self,
        system: &str,
        items: &[SessionItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse>;
}

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
    Reasoning {
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
    WebSearchCall {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<ResponsesWebSearchCallAction>,
    },
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type")]
enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    Input { text: String },
    #[serde(rename = "output_text")]
    Output { text: String },
    #[serde(rename = "reasoning_text")]
    Reasoning { text: String },
}

/// The action payload of a `web_search_call` input item: the query that was
/// searched. Passing the call back as-is lets DeepSeek restore the
/// server-side search results on later turns.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesWebSearchCallAction {
    WebSearchCallAction { query: String },
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
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    /// Provider convenience field: concatenated assistant text.
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

/// Provider usage counters; DeepSeek reports prefix-cache hit/miss tokens so
/// callers can observe how much of the request was served from cache.
#[derive(Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
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
    Reasoning {
        #[serde(default)]
        content: Vec<ResponsesReasoningPart>,
        #[serde(default)]
        summary: Vec<ResponsesSummaryPart>,
    },
    WebSearchCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        action: Option<WebSearchCallAction>,
    },
    #[serde(other)]
    Unknown,
}

/// The action payload of a `web_search_call` output item: the provider runs
/// the search server-side, so only the query is returned to the client.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebSearchCallAction {
    WebSearchCallAction {
        #[serde(default)]
        query: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// Reasoning text parts inside a `reasoning` output item.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesReasoningPart {
    ReasoningText { text: String },
    #[serde(other)]
    Unknown,
}

/// Summary text parts inside a `reasoning` output item.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesSummaryPart {
    SummaryText { text: String },
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
        items: &[SessionItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let instructions = build_instructions(system);
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
        let mut reasoning = String::new();
        let mut reasoning_present = false;
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
                        kind: ToolCallKind::FunctionCall,
                        name: Some(name),
                        arguments: Some(arguments),
                    });
                }
                ResponsesOutputItem::Reasoning { content: parts, summary } => {
                    // Keep even empty reasoning: DeepSeek thinking mode
                    // requires every prior reasoning item to be passed back,
                    // including empty ones on tool-call turns.
                    reasoning_present = true;
                    for part in parts {
                        if let ResponsesReasoningPart::ReasoningText { text } = part {
                            reasoning.push_str(&text);
                        }
                    }
                    for part in summary {
                        if let ResponsesSummaryPart::SummaryText { text } = part {
                            reasoning.push_str(&text);
                        }
                    }
                }
                ResponsesOutputItem::WebSearchCall { id, action } => {
                    let Some(id) = id else {
                        log::warn!("Responses API returned web_search_call without an id");
                        continue;
                    };
                    // The search runs server-side; the only content returned
                    // is the query. Save it as the call's arguments for the
                    // record — there is no tool_call_output (the results are
                    // injected into the model context, not returned).
                    let arguments = match action {
                        Some(WebSearchCallAction::WebSearchCallAction {
                            query: Some(query),
                        }) => Some(serde_json::json!({ "query": query }).to_string()),
                        _ => None,
                    };
                    tool_calls.push(ToolCall {
                        id,
                        kind: ToolCallKind::WebSearchCall,
                        name: Some("web_search".to_string()),
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

        if let Some(usage) = parsed.usage {
            log::debug!(
                "LLM cache: hit={} miss={}",
                usage.prompt_cache_hit_tokens.unwrap_or(0),
                usage.prompt_cache_miss_tokens.unwrap_or(0)
            );
        }

        let reasoning = reasoning_present.then_some(reasoning);

        Ok(LlmResponse {
            id: parsed.id,
            content,
            reasoning,
            tool_calls,
        })
    }
}

#[async_trait]
impl ChatLlm for OpenAiLlm {
    async fn chat(
        &self,
        system: &str,
        items: &[SessionItem],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        self.chat_responses(system, items, tools).await
    }
}

/// The system slot sent to the API: exactly the system prompt passed at
/// `SessionManager` construction. Stored session items never contribute —
/// the system prompt is not a stored role.
fn build_instructions(system: &str) -> Option<String> {
    (!system.trim().is_empty()).then(|| system.to_string())
}

/// The provider string role for a stored message item. `Context` items
/// (persona content injected at session creation) are emitted as `system`
/// input messages.
fn wire_role(role: MessageRole) -> Option<&'static str> {
    match role {
        MessageRole::User => Some("user"),
        MessageRole::Assistant => Some("assistant"),
        MessageRole::Context => Some("system"),
    }
}

/// Map session items one-to-one onto Responses API input items. The turn
/// loop already groups a response's `function_call`s before their
/// `function_call_output`s, mirroring the provider's output order so the
/// single preceding reasoning item covers the whole tool-call turn.
fn to_responses_input(items: &[SessionItem]) -> Vec<ResponsesInputItem> {
    items
        .iter()
        .filter_map(|item| match item {
            SessionItem::Message { role, content } => {
                let role = wire_role(*role)?;
                let part = match role {
                    "user" | "system" => ResponsesContentPart::Input {
                        text: content.clone(),
                    },
                    "assistant" => ResponsesContentPart::Output {
                        text: content.clone(),
                    },
                    _ => unreachable!("wire_role only returns user/assistant/system"),
                };
                Some(ResponsesInputItem::Message {
                    role: role.to_string(),
                    content: vec![part],
                })
            }
            SessionItem::Reasoning { content } => Some(ResponsesInputItem::Reasoning {
                content: vec![ResponsesContentPart::Reasoning {
                    text: content.clone(),
                }],
            }),
            SessionItem::ToolCall(call) if call.kind == ToolCallKind::FunctionCall => {
                let (Some(name), Some(arguments)) = (&call.name, &call.arguments) else {
                    log::warn!("function_call item missing name or arguments; skipped");
                    return None;
                };
                Some(ResponsesInputItem::FunctionCall {
                    call_id: call.id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                })
            }
            // Pass built-in web_search calls back as input items: DeepSeek
            // restores the server-side search results from the call id, so
            // follow-up turns keep the search context.
            SessionItem::ToolCall(call) if call.kind == ToolCallKind::WebSearchCall => {
                let action = call.arguments.as_deref().and_then(|raw| {
                    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
                    let query = value.get("query")?.as_str()?.to_string();
                    Some(ResponsesWebSearchCallAction::WebSearchCallAction { query })
                });
                Some(ResponsesInputItem::WebSearchCall {
                    id: call.id.clone(),
                    action,
                })
            }
            SessionItem::ToolCall(_) => None,
            SessionItem::ToolCallOutput { call_id, output } => {
                Some(ResponsesInputItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: output.clone(),
                })
            }
            // Wait markers are a local trace only: never re-sent to the LLM.
            // The turn that produced one was rolled back to the user message.
            SessionItem::Wait { .. } => None,
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

    fn msg(role: MessageRole, content: &str) -> SessionItem {
        SessionItem::Message {
            role,
            content: content.to_string(),
        }
    }

    #[test]
    fn responses_input_groups_calls_before_outputs() {
        // This is exactly the order the turn loop emits: a response's
        // function_calls are persisted before their function_call_outputs,
        // so one reasoning item covers every call of the turn.
        let items = vec![
            msg(MessageRole::User, "hi"),
            SessionItem::Reasoning {
                content: "think".to_string(),
            },
            SessionItem::ToolCall(ToolCall {
                id: "call_A".to_string(),
                kind: ToolCallKind::FunctionCall,
                name: Some("foo".to_string()),
                arguments: Some("{}".to_string()),
            }),
            SessionItem::ToolCall(ToolCall {
                id: "call_B".to_string(),
                kind: ToolCallKind::FunctionCall,
                name: Some("bar".to_string()),
                arguments: Some("{}".to_string()),
            }),
            SessionItem::ToolCallOutput {
                call_id: "call_A".to_string(),
                output: "result A".to_string(),
            },
            SessionItem::ToolCallOutput {
                call_id: "call_B".to_string(),
                output: "result B".to_string(),
            },
        ];

        let wire = to_responses_input(&items);
        let types: Vec<&str> = wire
            .iter()
            .map(|item| match item {
                ResponsesInputItem::Message { .. } => "message",
                ResponsesInputItem::Reasoning { .. } => "reasoning",
                ResponsesInputItem::FunctionCall { .. } => "function_call",
                ResponsesInputItem::FunctionCallOutput { .. } => "function_call_output",
                ResponsesInputItem::WebSearchCall { .. } => "web_search_call",
            })
            .collect();

        // All calls come first, then their outputs, in the provider's own
        // response order.
        assert_eq!(
            types,
            vec![
                "message",
                "reasoning",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output",
            ]
        );

        // Each output carries the call id of the call it answers.
        assert_eq!(wire.len(), 6);
        assert_eq!(
            wire[2],
            ResponsesInputItem::FunctionCall {
                call_id: "call_A".to_string(),
                name: "foo".to_string(),
                arguments: "{}".to_string(),
            }
        );
        assert_eq!(
            wire[4],
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
            parameters: ToolParams::object(
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
    fn input_emits_context_as_system_message() {
        let items = vec![
            msg(MessageRole::Context, "# solo.md\npersona"),
            msg(MessageRole::User, "hi"),
        ];
        let wire = to_responses_input(&items);
        assert_eq!(wire.len(), 2);
        assert!(matches!(
            &wire[0],
            ResponsesInputItem::Message { role, .. } if role == "system"
        ));
        assert!(matches!(
            &wire[1],
            ResponsesInputItem::Message { role, .. } if role == "user"
        ));
    }

    #[test]
    fn context_serializes_as_system_input_message() {
        let wire = to_responses_input(&[msg(MessageRole::Context, "# solo.md\npersona")]);
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "input_text", "text": "# solo.md\npersona" }],
                },
            ])
        );
    }

    #[test]
    fn instructions_come_from_system_param_only() {
        // The system prompt is a `SessionManager` constructor argument, never
        // a stored session role, so stored items cannot influence it.
        assert_eq!(
            build_instructions("You are Nota.").as_deref(),
            Some("You are Nota.")
        );
        assert_eq!(build_instructions("   "), None);
    }

    #[test]
    fn roles_serialize_as_numbers_with_zero_reserved() {
        assert_eq!(serde_json::to_value(MessageRole::User).unwrap(), 1);
        assert_eq!(serde_json::to_value(MessageRole::Assistant).unwrap(), 2);
        assert_eq!(serde_json::to_value(MessageRole::Context).unwrap(), 3);

        assert_eq!(
            serde_json::from_value::<MessageRole>(serde_json::json!(2)).unwrap(),
            MessageRole::Assistant
        );
        // 0 is reserved: unknown numbers are rejected, ready for future roles.
        assert!(serde_json::from_value::<MessageRole>(serde_json::json!(0)).is_err());
        // 4 was the old Context numbering before the System role was removed.
        assert!(serde_json::from_value::<MessageRole>(serde_json::json!(4)).is_err());
        assert!(serde_json::from_value::<MessageRole>(serde_json::json!(9)).is_err());
    }

    #[test]
    fn parses_responses_output() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "step by step" }],
                    "content": [{ "type": "reasoning_text", "text": "keep going" }],
                },
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
                { "type": "web_search_call", "id": "ws_1" },
                { "type": "reasoning", "summary": [] },
            ]
        });

        let parsed: ResponsesResponse = serde_json::from_value(body).unwrap();
        let mut content = String::new();
        let mut reasoning = String::new();
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
                        kind: ToolCallKind::FunctionCall,
                        name: Some(name),
                        arguments: Some(arguments),
                    });
                }
                ResponsesOutputItem::WebSearchCall { id, action } => {
                    let arguments = match action {
                        Some(WebSearchCallAction::WebSearchCallAction {
                            query: Some(query),
                        }) => Some(serde_json::json!({ "query": query }).to_string()),
                        _ => None,
                    };
                    calls.push(ToolCall {
                        id: id.expect("web_search call id"),
                        kind: ToolCallKind::WebSearchCall,
                        name: Some("web_search".to_string()),
                        arguments,
                    });
                }
                ResponsesOutputItem::Reasoning { content: parts, summary } => {
                    for part in parts {
                        if let ResponsesReasoningPart::ReasoningText { text } = part {
                            reasoning.push_str(&text);
                        }
                    }
                    for part in summary {
                        if let ResponsesSummaryPart::SummaryText { text } = part {
                            reasoning.push_str(&text);
                        }
                    }
                }
                ResponsesOutputItem::Unknown => {}
            }
        }

        assert_eq!(content, "hi there");
        assert_eq!(reasoning, "keep goingstep by step");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].kind, ToolCallKind::FunctionCall);
        assert_eq!(calls[1].id, "fc_2");
        assert_eq!(calls[2].id, "ws_1");
        assert_eq!(calls[2].kind, ToolCallKind::WebSearchCall);
    }

    #[test]
    fn parses_web_search_call_action_query_as_arguments() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [
                {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {
                        "type": "web_search_call_action",
                        "query": "today's weather in shanghai",
                    },
                },
                {
                    "type": "web_search_call",
                    "id": "ws_2",
                    "action": { "type": "web_search_call_action" },
                },
            ]
        });

        let parsed: ResponsesResponse = serde_json::from_value(body).unwrap();
        let mut calls = Vec::new();
        for item in parsed.output {
            if let ResponsesOutputItem::WebSearchCall { id, action } = item {
                let arguments = match action {
                    Some(WebSearchCallAction::WebSearchCallAction {
                        query: Some(query),
                    }) => Some(serde_json::json!({ "query": query }).to_string()),
                    _ => None,
                };
                calls.push(ToolCall {
                    id: id.expect("web_search call id"),
                    kind: ToolCallKind::WebSearchCall,
                    name: Some("web_search".to_string()),
                    arguments,
                });
            }
        }

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "ws_1");
        assert_eq!(calls[0].kind, ToolCallKind::WebSearchCall);
        assert_eq!(calls[0].name.as_deref(), Some("web_search"));
        assert_eq!(
            calls[0].arguments.as_deref(),
            Some(r#"{"query":"today's weather in shanghai"}"#)
        );
        // A call without a query is still recorded, just without arguments.
        assert_eq!(calls[1].id, "ws_2");
        assert_eq!(calls[1].arguments, None);
    }

    #[test]
    fn input_passes_back_reasoning_and_web_search_call() {
        let items = vec![
            SessionItem::Reasoning {
                content: "think".to_string(),
            },
            SessionItem::ToolCall(ToolCall {
                id: "ws_1".to_string(),
                kind: ToolCallKind::WebSearchCall,
                name: Some("web_search".to_string()),
                arguments: Some(r#"{"query":"news"}"#.to_string()),
            }),
            SessionItem::ToolCall(ToolCall {
                id: "call_1".to_string(),
                kind: ToolCallKind::FunctionCall,
                name: Some("foo".to_string()),
                arguments: Some("{}".to_string()),
            }),
            SessionItem::ToolCallOutput {
                call_id: "call_1".to_string(),
                output: "ok".to_string(),
            },
        ];

        let wire = to_responses_input(&items);
        assert_eq!(wire.len(), 4);
        assert!(matches!(
            &wire[0],
            ResponsesInputItem::Reasoning { content } if matches!(
                &content[..],
                [ResponsesContentPart::Reasoning { text }] if text == "think"
            )
        ));
        assert!(matches!(
            &wire[1],
            ResponsesInputItem::WebSearchCall { id, .. } if id == "ws_1"
        ));
        assert!(matches!(
            &wire[2],
            ResponsesInputItem::FunctionCall { call_id, .. } if call_id == "call_1"
        ));
        assert!(matches!(
            &wire[3],
            ResponsesInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
    }

    #[test]
    fn web_search_call_serializes_back_with_action_query() {
        let items = vec![
            SessionItem::ToolCall(ToolCall {
                id: "ws_1".to_string(),
                kind: ToolCallKind::WebSearchCall,
                name: Some("web_search".to_string()),
                arguments: Some(r#"{"query":"today's weather"}"#.to_string()),
            }),
            SessionItem::ToolCall(ToolCall {
                id: "ws_2".to_string(),
                kind: ToolCallKind::WebSearchCall,
                name: Some("web_search".to_string()),
                arguments: None,
            }),
        ];
        let value = serde_json::to_value(to_responses_input(&items)).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {
                        "type": "web_search_call_action",
                        "query": "today's weather",
                    },
                },
                { "type": "web_search_call", "id": "ws_2" },
            ])
        );
    }

    #[test]
    fn reasoning_serializes_as_reasoning_input_item() {
        let wire = to_responses_input(&[SessionItem::Reasoning {
            content: "think".to_string(),
        }]);
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {
                    "type": "reasoning",
                    "content": [{ "type": "reasoning_text", "text": "think" }],
                },
            ])
        );
    }

    #[test]
    fn wait_markers_are_never_sent_to_the_llm() {
        let wire = to_responses_input(&[
            SessionItem::Message {
                role: MessageRole::User,
                content: "你".to_string(),
            },
            SessionItem::Wait {
                arguments: r#"{"seconds":10}"#.to_string(),
            },
        ]);
        assert_eq!(wire.len(), 1);
        assert!(matches!(wire[0], ResponsesInputItem::Message { .. }));
    }
}
