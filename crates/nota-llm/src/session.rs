//! Concrete session manager — the only public surface of the llm crate.
//!
//! Other modules never see the LLM client or the turn loop; they hold the
//! core [`SessionManager`] / [`Session`] abstractions and simply feed
//! content into a session. This crate supplies the SQLite-backed
//! implementation:
//!
//! - one manager per storage **path**; the conversation layer decides the
//!   scope (e.g. one directory per conversation) and injects the tool set
//!   for that scope — including conversation-bound tools such as `reply`;
//! - sessions are conversation-agnostic: a plain uuid v4 id, stored flat as
//!   `<uuid>.db` under the manager's root; no conversation naming, no
//!   `current.json` (the caller tracks which session is current);
//! - each session runs the whole turn internally: append the user message →
//!   LLM call (tools resolved live from the registry, sorted by name for
//!   prefix-cache stability) → execute tool calls with a per-session
//!   `ToolContext` → persist items and the response id.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use nota_core::conversation::ConversationManager;
use nota_core::permissions::PermissionRegistry;
use nota_core::session::{
    MessageRole, Session, SessionItem, SessionManager, ToolCall, ToolCallKind,
};
use nota_core::tool::{ToolContext, ToolRegistry, Value};

use crate::responses::{ChatLlm, LlmResponse, OpenAiLlm, ToolDef};
use crate::store::SqliteSessionStore;

const MAX_ITERATIONS: usize = 16;
/// Reserved name of the conversation-layer `wait` tool. The turn loop treats
/// a successful call specially: roll the turn back to just the user message,
/// persist a `Wait` marker as a trace, and stop without any assistant text.
const WAIT_TOOL_NAME: &str = "wait";

/// LLM provider configuration, passed by the composition root.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub web_search: bool,
}

/// One dialogue, backed by its own `<uuid>.db` file under the manager root.
/// Sessions are conversation-agnostic: the id is the plain uuid.
struct SqliteSession {
    id: String,
    persona_name: String,
    system: String,
    api_url: String,
    model: String,
    tools: Arc<ToolRegistry>,
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    llm: Arc<dyn ChatLlm>,
    store: Arc<SqliteSessionStore>,
    created_at: i64,
}

impl SqliteSession {
    #[allow(clippy::too_many_arguments)]
    async fn new(
        session_uuid: String,
        persona_name: String,
        system: String,
        api_url: String,
        model: String,
        tools: Arc<ToolRegistry>,
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        llm: Arc<dyn ChatLlm>,
        store: Arc<SqliteSessionStore>,
    ) -> Result<Self> {
        let created_at = store.created_at(&session_uuid).await?;
        Ok(Self {
            id: session_uuid,
            persona_name,
            system,
            api_url,
            model,
            tools,
            manager,
            permissions,
            llm,
            store,
            created_at,
        })
    }

    async fn run_turn(&self, content: String, request_id: Option<String>) -> Result<()> {
        let user_item = SessionItem::Message {
            role: MessageRole::User,
            content,
        };
        // Row boundary of this turn: the `wait` path deletes everything
        // appended after this point and re-appends only the user message
        // plus the wait marker, so a held-open turn never pollutes context.
        let turn_start_row = self.store.last_row_id(&self.id).await?;
        let mut items = self.store.read_items(&self.id).await?;
        items.push(user_item.clone());
        self.store.append(&self.id, std::slice::from_ref(&user_item)).await?;

        let ctx = ToolContext {
            persona_name: self.persona_name.clone(),
            manager: self.manager.clone(),
            request_id,
            permissions: self.permissions.clone(),
        };

        let mut last_response_id = self.store.response_id(&self.id).await?;

        let tool_defs: Vec<ToolDef> = self
            .tools
            .list()
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();

        for _iteration in 0..MAX_ITERATIONS {
            let resp: LlmResponse = self.llm.chat(&self.system, &items, &tool_defs).await?;
            if let Some(id) = &resp.id {
                last_response_id = Some(id.clone());
            }

            if let Some(reasoning) = &resp.reasoning {
                // Persist even empty reasoning: DeepSeek thinking mode
                // requires every prior reasoning item to be passed back.
                let reasoning_item = SessionItem::Reasoning {
                    content: reasoning.clone(),
                };
                items.push(reasoning_item.clone());
                self.store.append(&self.id, &[reasoning_item]).await?;
            }

            // Only executable function calls force another round-trip.
            // Server-side calls (web_search, ...) may already be answered in
            // this same response, so their text must not be discarded.
            let mut executed_function = false;

            // Persist every call first, then run the tools and persist their
            // outputs. Grouping mirrors the provider's own response order
            // (reasoning, all calls, all outputs): DeepSeek reconstructs each
            // input `function_call` as its own assistant turn, so interleaving
            // outputs between calls would leave every call after the first
            // without a preceding reasoning item and fail with "reasoning_text
            // ... must be passed back". One reasoning item then covers the
            // whole tool-call turn.
            for tc in &resp.tool_calls {
                let call_item = SessionItem::ToolCall(tc.clone());
                items.push(call_item.clone());
                self.store.append(&self.id, &[call_item]).await?;
            }

            for tc in &resp.tool_calls {
                let result = match tc.kind {
                    ToolCallKind::FunctionCall => {
                        executed_function = true;
                        let name = tc.name.as_deref().unwrap_or_default();
                        match self.tools.get(name) {
                            Some(tool) => {
                                let raw = tc.arguments.as_deref().unwrap_or("{}");
                                let def = ToolDef {
                                    name: name.to_string(),
                                    description: tool.description().to_string(),
                                    parameters: tool.parameters(),
                                };
                                match parse_tool_args(raw, &def.parameters) {
                                    Ok(args) => tool.run(args, ctx.clone()).await,
                                    Err(violations) => {
                                        log_rejected_tool_args(
                                            name,
                                            raw,
                                            &violations,
                                            &self.api_url,
                                            &self.model,
                                        );
                                        Err(anyhow::anyhow!(rejected_tool_args_feedback(
                                            name, &def,
                                        )))
                                    }
                                }
                            }
                            None => Err(anyhow::anyhow!("unknown tool: {name}")),
                        }
                    }
                    ToolCallKind::WebSearchCall => {
                        // The provider executes web_search server-side; the
                        // session records the call but has nothing to run and
                        // emits no output item.
                        continue;
                    }
                };
                let output_item = SessionItem::ToolCallOutput {
                    call_id: tc.id.clone(),
                    output: match &result {
                        Ok(out) => out.clone(),
                        Err(e) => format!("tool error: {e}"),
                    },
                };
                items.push(output_item.clone());
                self.store.append(&self.id, &[output_item]).await?;

                // A successful `wait` call holds the conversation open:
                // discard this turn's additions (reasoning, tool calls,
                // outputs, assistant text), keep the user message plus a
                // `Wait` marker, and end the turn immediately. The model
                // will be woken by the next message or the timeout notice.
                if is_wait_call(tc) && result.is_ok() {
                    self.store.delete_after(&self.id, turn_start_row).await?;
                    let wait_item = SessionItem::Wait {
                        arguments: tc.arguments.clone().unwrap_or_default(),
                    };
                    self.store
                        .append(&self.id, &[user_item.clone(), wait_item])
                        .await?;
                    if let Some(id) = last_response_id {
                        self.store.set_response_id(&self.id, &id).await?;
                    }
                    return Ok(());
                }
            }

            if executed_function {
                continue;
            }

            if let Some(content) = resp.content {
                let assistant_item = SessionItem::Message {
                    role: MessageRole::Assistant,
                    content,
                };
                items.push(assistant_item.clone());
                self.store.append(&self.id, &[assistant_item]).await?;
            }
            break;
        }

        if let Some(id) = last_response_id {
            self.store.set_response_id(&self.id, &id).await?;
        }
        Ok(())
    }
}

fn is_wait_call(tc: &ToolCall) -> bool {
    tc.kind == ToolCallKind::FunctionCall && tc.name.as_deref() == Some(WAIT_TOOL_NAME)
}

#[async_trait]
impl Session for SqliteSession {
    fn id(&self) -> &str {
        &self.id
    }

    async fn created_at(&self) -> Result<i64> {
        Ok(self.created_at)
    }

    async fn send(&self, content: String, request_id: Option<String>) -> Result<()> {
        self.run_turn(content, request_id).await
    }

    async fn raw_history(&self) -> Result<Vec<(i64, SessionItem)>> {
        self.store.read_raw(&self.id).await
    }
}

/// Resolve the model's raw JSON arguments against the tool's declared
/// parameters. Returns the parsed object, or the list of violations (missing
/// required properties, unknown properties, wrong types, values outside an
/// enum).
fn parse_tool_args(
    raw: &str,
    params: &nota_core::tool::ToolParams,
) -> Result<HashMap<String, Value>, Vec<String>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| vec![format!("arguments are not valid JSON: {e}")])?;
    let object = match value {
        Value::Object(object) => object,
        _ => return Err(vec!["arguments must be a JSON object".to_string()]),
    };

    let mut violations = Vec::new();
    for name in &params.required {
        if !object.contains_key(name.as_str()) {
            violations.push(format!("missing required property '{name}'"));
        }
    }
    for (name, val) in &object {
        match params.properties.get(name) {
            None => violations.push(format!("unknown property '{name}'")),
            Some(def) => {
                if !value_matches_type(val, &def.prop_type) {
                    violations.push(format!(
                        "property '{name}' must be a {} but the model provided {val:?}",
                        def.prop_type
                    ));
                }
                if !def.r#enum.is_empty()
                    && !val
                        .as_str()
                        .is_some_and(|s| def.r#enum.iter().any(|e| e == s))
                {
                    violations.push(format!(
                        "property '{name}' must be one of {:?}",
                        def.r#enum
                    ));
                }
            }
        }
    }
    if violations.is_empty() {
        Ok(object)
    } else {
        Err(violations)
    }
}

fn value_matches_type(value: &Value, prop_type: &str) -> bool {
    match prop_type {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some(),
        "boolean" => value.is_bool(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown declared type: accept anything rather than guessing.
        _ => true,
    }
}

/// Diagnostic printed for the operator when tool arguments are rejected:
/// provider, model, the raw model output, and why it does not satisfy the
/// tool definition.
fn log_rejected_tool_args(
    name: &str,
    raw: &str,
    violations: &[String],
    api_url: &str,
    model: &str,
) {
    let reasons = violations
        .iter()
        .map(|v| format!("- {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    log::warn!(
        "tool call '{name}' rejected: the arguments do not match the tool definition.\n\
         provider: {api_url}\n\
         model: {model}\n\
         model output: {raw}\n\
         reasons:\n{reasons}"
    );
}

/// The feedback handed back to the model: the full tool definition re-sent
/// verbatim, so it can correct its arguments. Operator diagnostics
/// (provider/model/raw output) are logged separately, never sent to the
/// model.
fn rejected_tool_args_feedback(name: &str, def: &ToolDef) -> String {
    let definition = serde_json::to_string_pretty(def).unwrap_or_default();
    format!(
        "The arguments you provided for tool '{name}' were rejected because they do not \
         match the tool definition. Use exactly these properties:\n{definition}"
    )
}

/// SQLite-backed session manager: one instance per storage path. The
/// conversation layer decides the scope (e.g. one manager per conversation
/// directory) and injects the tool set — including conversation-bound tools
/// such as `reply` — at construction.
pub struct SqliteSessionManager {
    persona_name: String,
    system: String,
    context: String,
    api_url: String,
    model: String,
    tools: Arc<ToolRegistry>,
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    llm: Arc<dyn ChatLlm>,
    store: Arc<SqliteSessionStore>,
    sessions: Mutex<HashMap<String, Arc<SqliteSession>>>,
}

impl SqliteSessionManager {
    /// Create the manager for one scope. `root` is the directory sessions
    /// are stored in, flat as `<root>/<uuid>.db`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: &Path,
        persona_name: String,
        system: String,
        context: String,
        tools: Arc<ToolRegistry>,
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        config: LlmConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating session root {}", root.display()))?;
        let llm: Arc<dyn ChatLlm> = Arc::new(OpenAiLlm::new(
            &config.api_url,
            &config.api_key,
            &config.model,
            config.web_search,
        ));
        let api_url = config.api_url;
        let model = config.model;
        Self::with_llm(
            root,
            persona_name,
            system,
            context,
            api_url,
            model,
            tools,
            manager,
            permissions,
            llm,
        )
    }

    /// Test seam: construct the manager with a substitute LLM client.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_llm(
        root: &Path,
        persona_name: String,
        system: String,
        context: String,
        api_url: String,
        model: String,
        tools: Arc<ToolRegistry>,
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        llm: Arc<dyn ChatLlm>,
    ) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating session root {}", root.display()))?;
        Ok(Self {
            persona_name,
            system,
            context,
            api_url,
            model,
            tools,
            manager,
            permissions,
            llm,
            store: Arc::new(SqliteSessionStore::new(root)?),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    async fn build_session(&self, session_uuid: &str) -> Result<Arc<SqliteSession>> {
        let session = Arc::new(
            SqliteSession::new(
                session_uuid.to_string(),
                self.persona_name.clone(),
                self.system.clone(),
                self.api_url.clone(),
                self.model.clone(),
                self.tools.clone(),
                self.manager.clone(),
                self.permissions.clone(),
                self.llm.clone(),
                self.store.clone(),
            )
            .await?,
        );
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }
}

#[async_trait]
impl SessionManager for SqliteSessionManager {
    async fn create(&self) -> Result<Arc<dyn Session>> {
        let uuid = self.store.create().await?;
        let session = self.build_session(&uuid).await?;
        if !self.context.is_empty() {
            self.store
                .append(
                    &uuid,
                    &[SessionItem::Message {
                        role: MessageRole::Context,
                        content: self.context.clone(),
                    }],
                )
                .await?;
        }
        Ok(session)
    }

    async fn load(&self, id: &str) -> Result<Option<Arc<dyn Session>>> {
        if let Some(session) = self.sessions.lock().unwrap().get(id).cloned() {
            return Ok(Some(session));
        }
        if !self.store.has(id) {
            return Ok(None);
        }
        Ok(Some(self.build_session(id).await?))
    }

    async fn archive(&self, id: &str) -> Result<()> {
        self.store.archive(id).await
    }

    async fn list(&self) -> Result<Vec<Arc<dyn Session>>> {
        let mut out = Vec::new();
        for (session_uuid, _seq, _created_at) in self.store.list().await? {
            if let Some(session) = self.load(&session_uuid).await? {
                out.push(session);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::session::{ToolCall, ToolCallKind};
    use nota_core::tool::{PropertyDef, Tool, ToolParams};
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nota_llm_session_{tag}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Mock LLM: pops the next canned response per chat call.
    struct MockLlm(Mutex<VecDeque<LlmResponse>>);

    impl MockLlm {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self(Mutex::new(responses.into()))
        }
    }

    #[async_trait]
    impl ChatLlm for MockLlm {
        async fn chat(
            &self,
            _system: &str,
            _items: &[SessionItem],
            _tools: &[ToolDef],
        ) -> Result<LlmResponse> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock llm exhausted"))
        }
    }

    fn manager_with(
        root: &Path,
        llm: Arc<dyn ChatLlm>,
    ) -> (Arc<SqliteSessionManager>, Arc<ConversationManager>) {
        let manager = Arc::new(ConversationManager::new());
        let sm = Arc::new(
            SqliteSessionManager::with_llm(
                root,
                "bob".to_string(),
                "system".to_string(),
                "persona context".to_string(),
                String::new(),
                String::new(),
                Arc::new(ToolRegistry::new()),
                manager.clone(),
                Arc::new(PermissionRegistry::new()),
                llm,
            )
            .unwrap(),
        );
        (sm, manager)
    }

    #[tokio::test]
    async fn create_and_load_roundtrip() {
        let root = temp_root("roundtrip");
        let (sm, _) = manager_with(&root, Arc::new(MockLlm::new(vec![])));

        let s1 = sm.create().await.unwrap();
        assert!(
            !s1.id().contains('/'),
            "session id must be a plain uuid: {}",
            s1.id()
        );

        let loaded = sm.load(s1.id()).await.unwrap().unwrap();
        assert_eq!(loaded.id(), s1.id());
        assert!(sm.load("missing-session").await.unwrap().is_none());

        let list = sm.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id(), s1.id());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn send_persists_user_and_assistant_items_and_response_id() {
        let root = temp_root("send");
        let llm = Arc::new(MockLlm::new(vec![LlmResponse {
            id: Some("resp_x".to_string()),
            content: Some("hi back".to_string()),
            reasoning: None,
            tool_calls: vec![],
        }]));
        let (sm, _) = manager_with(&root, llm);

        let s = sm.create().await.unwrap();
        s.send("hello".to_string(), Some("req_1".to_string()))
            .await
            .unwrap();

        let history = s.raw_history().await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(
            history[0].1,
            SessionItem::Message {
                role: MessageRole::Context,
                content: "persona context".to_string(),
            }
        );
        assert_eq!(
            history[1].1,
            SessionItem::Message {
                role: MessageRole::User,
                content: "hello".to_string(),
            }
        );
        assert_eq!(
            history[2].1,
            SessionItem::Message {
                role: MessageRole::Assistant,
                content: "hi back".to_string(),
            }
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn send_runs_tool_loop_with_live_registry() {
        let root = temp_root("tools");
        // First call: two tool calls; second call: final content.
        let llm = Arc::new(MockLlm::new(vec![
            LlmResponse {
                id: Some("r1".to_string()),
                content: None,
                reasoning: None,
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        kind: ToolCallKind::FunctionCall,
                        name: Some("echo".to_string()),
                        arguments: Some("{}".to_string()),
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        kind: ToolCallKind::FunctionCall,
                        name: Some("echo".to_string()),
                        arguments: Some("{}".to_string()),
                    },
                ],
            },
            LlmResponse {
                id: Some("r2".to_string()),
                content: Some("done".to_string()),
                reasoning: None,
                tool_calls: vec![],
            },
        ]));
        let manager = Arc::new(ConversationManager::new());
        let tools = Arc::new(ToolRegistry::new());
        // The tool is registered AFTER the manager is built: the loop must
        // resolve it live from the registry.
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct EchoTool {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echo tool"
            }
            fn parameters(&self) -> ToolParams {
                ToolParams::object(HashMap::new(), vec![])
            }
            async fn run(
                &self,
                _args: HashMap<String, Value>,
                ctx: ToolContext,
            ) -> Result<String> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(format!("{}|{:?}", ctx.persona_name, ctx.request_id));
                Ok("tool result".to_string())
            }
        }
        tools
            .register(Arc::new(EchoTool { seen: seen.clone() }))
            .unwrap();

        let sm = Arc::new(
            SqliteSessionManager::with_llm(
                &root,
                "bob".to_string(),
                "system".to_string(),
                "persona context".to_string(),
                String::new(),
                String::new(),
                tools,
                manager.clone(),
                Arc::new(PermissionRegistry::new()),
                llm,
            )
            .unwrap(),
        );

        let s = sm.create().await.unwrap();
        s.send("please".to_string(), Some("req_9".to_string()))
            .await
            .unwrap();

        let history = s.raw_history().await.unwrap();
        let kinds: Vec<&str> = history
            .iter()
            .map(|(_, i)| match i {
                SessionItem::Message { .. } => "message",
                SessionItem::Reasoning { .. } => "reasoning",
                SessionItem::ToolCall(_) => "tool_call",
                SessionItem::ToolCallOutput { .. } => "tool_call_output",
                SessionItem::Wait { .. } => "wait",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "tool_call",
                "tool_call",
                "tool_call_output",
                "tool_call_output",
                "message",
            ]
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("bob|Some(\"req_9\")"));
        assert!(seen[1].starts_with("bob|Some(\"req_9\")"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn web_search_call_with_text_completes_turn() {
        let root = temp_root("websearch");
        // DeepSeek answers a web_search call inside the SAME response: the
        // turn must not issue another request or drop the text.
        let llm = Arc::new(MockLlm::new(vec![LlmResponse {
            id: Some("r1".to_string()),
            content: Some("found it".to_string()),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "ws_1".to_string(),
                kind: ToolCallKind::WebSearchCall,
                name: Some("web_search".to_string()),
                arguments: Some(r#"{"query":"what's new"}"#.to_string()),
            }],
        }]));
        let (sm, _) = manager_with(&root, llm);

        let s = sm.create().await.unwrap();
        s.send("what's new?".to_string(), None).await.unwrap();

        let history = s.raw_history().await.unwrap();
        let kinds: Vec<&str> = history
            .iter()
            .map(|(_, i)| match i {
                SessionItem::Message { .. } => "message",
                SessionItem::Reasoning { .. } => "reasoning",
                SessionItem::ToolCall(_) => "tool_call",
                SessionItem::ToolCallOutput { .. } => "tool_call_output",
                SessionItem::Wait { .. } => "wait",
            })
            .collect();
        assert_eq!(kinds, vec!["message", "message", "tool_call", "message"]);

        // The web_search_call item is persisted with its query as arguments
        // and produces no tool_call_output.
        let SessionItem::ToolCall(web_call) = &history[2].1 else {
            panic!("expected a tool_call item");
        };
        assert_eq!(web_call.id, "ws_1");
        assert_eq!(web_call.kind, ToolCallKind::WebSearchCall);
        assert_eq!(web_call.name.as_deref(), Some("web_search"));
        assert_eq!(
            web_call.arguments.as_deref(),
            Some(r#"{"query":"what's new"}"#)
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_tool_args_validates_required_types_and_enum() {
        let params = ToolParams::object(
            HashMap::from([
                (
                    "name".to_string(),
                    PropertyDef {
                        prop_type: "string".to_string(),
                        description: "city name".to_string(),
                        r#enum: vec![],
                    },
                ),
                (
                    "unit".to_string(),
                    PropertyDef {
                        prop_type: "string".to_string(),
                        description: "temperature unit".to_string(),
                        r#enum: vec!["celsius".to_string(), "fahrenheit".to_string()],
                    },
                ),
            ]),
            vec!["name".to_string(), "unit".to_string()],
        );

        let ok = parse_tool_args(r#"{"name":"sh","unit":"celsius"}"#, &params).unwrap();
        assert_eq!(ok["name"], Value::String("sh".to_string()));

        let err = parse_tool_args(r#"{"name":1,"extra":true}"#, &params).unwrap_err();
        assert!(err
            .iter()
            .any(|v| v.contains("missing required property 'unit'")));
        assert!(err.iter().any(|v| v.contains("property 'name' must be a string")));
        assert!(err.iter().any(|v| v.contains("unknown property 'extra'")));

        let err = parse_tool_args(r#"{"name":"x","unit":"kelvin"}"#, &params).unwrap_err();
        assert!(err
            .iter()
            .any(|v| v.contains("property 'unit' must be one of")));
    }

    #[tokio::test]
    async fn invalid_tool_args_are_rejected_with_definition_feedback() {
        let root = temp_root("args");
        // First call: bad args (unknown property). Second call: valid args.
        // Third call: final content.
        let llm = Arc::new(MockLlm::new(vec![
            LlmResponse {
                id: Some("r1".to_string()),
                content: None,
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: ToolCallKind::FunctionCall,
                    name: Some("echo".to_string()),
                    arguments: Some(r#"{"extra":1}"#.to_string()),
                }],
            },
            LlmResponse {
                id: Some("r2".to_string()),
                content: None,
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call_2".to_string(),
                    kind: ToolCallKind::FunctionCall,
                    name: Some("echo".to_string()),
                    arguments: Some("{}".to_string()),
                }],
            },
            LlmResponse {
                id: Some("r3".to_string()),
                content: Some("done".to_string()),
                reasoning: None,
                tool_calls: vec![],
            },
        ]));
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct EchoTool {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echo tool"
            }
            fn parameters(&self) -> ToolParams {
                ToolParams::object(HashMap::new(), vec![])
            }
            async fn run(
                &self,
                _args: HashMap<String, Value>,
                _ctx: ToolContext,
            ) -> Result<String> {
                self.seen.lock().unwrap().push("ran".to_string());
                Ok("tool result".to_string())
            }
        }
        let tools = Arc::new(ToolRegistry::new());
        tools
            .register(Arc::new(EchoTool {
                seen: seen.clone(),
            }))
            .unwrap();

        let manager = Arc::new(ConversationManager::new());
        let sm = Arc::new(
            SqliteSessionManager::with_llm(
                &root,
                "bob".to_string(),
                "system".to_string(),
                "persona context".to_string(),
                "https://example.com/v1".to_string(),
                "test-model".to_string(),
                tools,
                manager.clone(),
                Arc::new(PermissionRegistry::new()),
                llm,
            )
            .unwrap(),
        );

        let s = sm.create().await.unwrap();
        s.send("please".to_string(), None).await.unwrap();

        let history = s.raw_history().await.unwrap();
        let outputs: Vec<&str> = history
            .iter()
            .filter_map(|(_, i)| match i {
                SessionItem::ToolCallOutput { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[0].contains("rejected"));
        assert!(outputs[0].contains("echo"));
        assert!(outputs[0].contains("tool definition"));
        // Operator diagnostics stay in the log, never reach the model.
        assert!(!outputs[0].contains("example.com"));
        assert!(!outputs[0].contains("unknown property"));
        assert_eq!(outputs[1], "tool result");
        assert_eq!(seen.lock().unwrap().len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn wait_call_rolls_back_turn_and_persists_marker() {
        let root = temp_root("wait");
        // The mock is called exactly once: a successful wait ends the turn
        // immediately, so no follow-up request may be issued.
        let llm = Arc::new(MockLlm::new(vec![LlmResponse {
            id: Some("r1".to_string()),
            content: None,
            reasoning: Some("is this message complete?".to_string()),
            tool_calls: vec![ToolCall {
                id: "call_w".to_string(),
                kind: ToolCallKind::FunctionCall,
                name: Some("wait".to_string()),
                arguments: Some(
                    r#"{"seconds":10,"reason":"message incomplete"}"#.to_string(),
                ),
            }],
        }]));
        let manager = Arc::new(ConversationManager::new());
        let tools = Arc::new(ToolRegistry::new());
        struct WaitStub;
        #[async_trait]
        impl Tool for WaitStub {
            fn name(&self) -> &str {
                "wait"
            }
            fn description(&self) -> &str {
                "wait tool"
            }
            fn parameters(&self) -> ToolParams {
                let mut props = HashMap::new();
                props.insert(
                    "seconds".to_string(),
                    PropertyDef {
                        prop_type: "integer".to_string(),
                        description: String::new(),
                        r#enum: vec![],
                    },
                );
                props.insert(
                    "reason".to_string(),
                    PropertyDef {
                        prop_type: "string".to_string(),
                        description: String::new(),
                        r#enum: vec![],
                    },
                );
                ToolParams::object(props, vec![])
            }
            async fn run(
                &self,
                _args: HashMap<String, Value>,
                _ctx: ToolContext,
            ) -> Result<String> {
                Ok("ok".to_string())
            }
        }
        tools.register(Arc::new(WaitStub)).unwrap();

        let sm = Arc::new(
            SqliteSessionManager::with_llm(
                &root,
                "bob".to_string(),
                "system".to_string(),
                "persona context".to_string(),
                String::new(),
                String::new(),
                tools,
                manager.clone(),
                Arc::new(PermissionRegistry::new()),
                llm,
            )
            .unwrap(),
        );

        let s = sm.create().await.unwrap();
        s.send("你".to_string(), None).await.unwrap();

        let history = s.raw_history().await.unwrap();
        // Context + user message + wait marker only: the turn's reasoning,
        // tool call and output were rolled back.
        assert_eq!(history.len(), 3);
        assert_eq!(
            history[1].1,
            SessionItem::Message {
                role: MessageRole::User,
                content: "你".to_string(),
            }
        );
        assert_eq!(
            history[2].1,
            SessionItem::Wait {
                arguments: r#"{"seconds":10,"reason":"message incomplete"}"#.to_string(),
            }
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sessions_persist_across_restart() {
        let root = temp_root("persist");
        let llm = || {
            Arc::new(MockLlm::new(vec![LlmResponse {
                id: Some("resp_x".to_string()),
                content: Some("hi".to_string()),
                reasoning: None,
                tool_calls: vec![],
            }])) as Arc<dyn ChatLlm>
        };
        let (sm, _) = manager_with(&root, llm());
        let s1 = sm.create().await.unwrap();
        s1.send("hello".to_string(), None).await.unwrap();
        let s1_id = s1.id().to_string();

        // A fresh manager over the same root resumes the session by id.
        let (sm2, _) = manager_with(&root, llm());
        let resumed = sm2.load(&s1_id).await.unwrap().unwrap();
        assert_eq!(resumed.id(), s1_id);
        assert_eq!(resumed.raw_history().await.unwrap().len(), 3);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn archive_keeps_session_readable() {
        let root = temp_root("archive");
        let (sm, _) = manager_with(&root, Arc::new(MockLlm::new(vec![])));

        let s1 = sm.create().await.unwrap();
        let s2 = sm.create().await.unwrap();
        assert_ne!(s1.id(), s2.id());

        sm.archive(s1.id()).await.unwrap();
        // Archived sessions stay readable via load / list.
        assert!(sm.load(s1.id()).await.unwrap().is_some());
        let list = sm.list().await.unwrap();
        assert_eq!(list.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }
}
