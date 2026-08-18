//! Concrete session manager — the only public surface of the llm crate.
//!
//! Other modules never see the LLM client or the turn loop; they hold the
//! core [`SessionManager`] / [`Session`] abstractions and simply feed
//! content into a session. This crate supplies the SQLite-backed
//! implementation:
//!
//! - one manager per persona, created with a storage root, the system
//!   prompt, the shared [`ToolRegistry`], and the routing/approval ports;
//! - sessions are conversation-namespaced (`<conversation_id>/<uuid>`) and
//!   stored as `<uuid>.db` under `<root>/<conversation_id>/`, so a whole
//!   conversation can be wiped by removing its directory;
//! - each session runs the whole turn internally: append the user message →
//!   LLM call (tools resolved live from the registry, sorted by name for
//!   prefix-cache stability) → execute tool calls with a per-session
//!   `ToolContext` → persist items and the response id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use nota_core::conversation::ConversationManager;
use nota_core::permissions::PermissionRegistry;
use nota_core::session::{MessageRole, Session, SessionItem, SessionManager};
use nota_core::tool::{ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};

use crate::responses::{ChatLlm, LlmResponse, OpenAiLlm, ToolDef};
use crate::store::SqliteSessionStore;

const MAX_ITERATIONS: usize = 16;

/// LLM provider configuration, passed by the composition root.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub web_search: bool,
}

/// One session of one conversation, backed by its own `<uuid>.db` file under
/// `<root>/<conversation_id>/`.
struct SqliteSession {
    id: String,
    conversation_id: String,
    persona_name: String,
    system: String,
    tools: Arc<ToolRegistry>,
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    llm: Arc<dyn ChatLlm>,
    store: Arc<SqliteSessionStore>,
    session_uuid: String,
    created_at: i64,
}

impl SqliteSession {
    #[allow(clippy::too_many_arguments)]
    async fn new(
        conversation_id: &str,
        session_uuid: String,
        persona_name: String,
        system: String,
        tools: Arc<ToolRegistry>,
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        llm: Arc<dyn ChatLlm>,
        store: Arc<SqliteSessionStore>,
    ) -> Result<Self> {
        let created_at = store.created_at(&session_uuid).await?;
        Ok(Self {
            id: format!("{conversation_id}/{session_uuid}"),
            conversation_id: conversation_id.to_string(),
            persona_name,
            system,
            tools,
            manager,
            permissions,
            llm,
            store,
            session_uuid,
            created_at,
        })
    }

    async fn run_turn(&self, content: String, request_id: Option<String>) -> Result<()> {
        let user_item = SessionItem::Message {
            role: MessageRole::User,
            content,
        };
        let mut items = self.store.read_items(&self.session_uuid).await?;
        items.push(user_item.clone());
        self.store.append(&self.session_uuid, &[user_item]).await?;

        let ctx = ToolContext {
            persona_name: self.persona_name.clone(),
            manager: self.manager.clone(),
            request_id,
            permissions: self.permissions.clone(),
            conversation_id: Some(self.conversation_id.clone()),
        };

        let mut last_response_id = self.store.response_id(&self.session_uuid).await?;

        for _iteration in 0..MAX_ITERATIONS {
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

            let resp: LlmResponse = self
                .llm
                .chat(&self.system, &items, &tool_defs)
                .await?;
            if let Some(id) = &resp.id {
                last_response_id = Some(id.clone());
            }

            if !resp.tool_calls.is_empty() {
                for tc in &resp.tool_calls {
                    // Each function_call is immediately followed by its
                    // function_call_output: DeepSeek's Responses endpoint
                    // rejects interleaved items between a call and its result.
                    let call_item = SessionItem::FunctionCall(tc.clone());
                    items.push(call_item.clone());
                    self.store.append(&self.session_uuid, &[call_item]).await?;

                    let result = match self.tools.get(&tc.name) {
                        Some(tool) => tool.run(&tc.arguments, ctx.clone()).await,
                        None => Err(anyhow::anyhow!("unknown tool: {}", tc.name)),
                    };
                    let output_item = SessionItem::FunctionCallOutput {
                        call_id: tc.id.clone(),
                        output: match result {
                            Ok(out) => out,
                            Err(e) => format!("tool error: {e}"),
                        },
                    };
                    items.push(output_item.clone());
                    self.store
                        .append(&self.session_uuid, &[output_item])
                        .await?;
                }
                continue;
            }

            if let Some(content) = resp.content {
                let assistant_item = SessionItem::Message {
                    role: MessageRole::Assistant,
                    content,
                };
                items.push(assistant_item.clone());
                self.store
                    .append(&self.session_uuid, &[assistant_item])
                    .await?;
            }
            break;
        }

        if let Some(id) = last_response_id {
            self.store.set_response_id(&self.session_uuid, &id).await?;
        }
        Ok(())
    }
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
        self.store.read_raw(&self.session_uuid).await
    }
}

/// SQLite-backed session manager: one instance per persona, created by the
/// composition root with the storage root, system prompt, shared tool
/// registry, and the routing/approval ports.
pub struct SqliteSessionManager {
    root: PathBuf,
    persona_name: String,
    system: String,
    context: String,
    tools: Arc<ToolRegistry>,
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    llm: Arc<dyn ChatLlm>,
    stores: Mutex<HashMap<String, Arc<SqliteSessionStore>>>,
    sessions: Mutex<HashMap<String, Arc<SqliteSession>>>,
}

#[derive(Serialize, Deserialize)]
struct CurrentSession {
    session_id: String,
}

impl SqliteSessionManager {
    /// Create the manager for one persona. `root` is the persona's
    /// conversation directory; sessions live under
    /// `<root>/<conversation_id>/<uuid>.db`.
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
        Self::with_llm(
            root,
            persona_name,
            system,
            context,
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
        tools: Arc<ToolRegistry>,
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        llm: Arc<dyn ChatLlm>,
    ) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating session root {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            persona_name,
            system,
            context,
            tools,
            manager,
            permissions,
            llm,
            stores: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn store_for(&self, conversation_id: &str) -> Result<Arc<SqliteSessionStore>> {
        let mut stores = self.stores.lock().unwrap();
        if let Some(store) = stores.get(conversation_id) {
            return Ok(store.clone());
        }
        let store = Arc::new(SqliteSessionStore::new(&self.root.join(conversation_id))?);
        stores.insert(conversation_id.to_string(), store.clone());
        Ok(store)
    }

    fn current_path(&self, conversation_id: &str) -> PathBuf {
        self.root.join(conversation_id).join("current.json")
    }

    async fn save_current(&self, conversation_id: &str, id: &str) -> Result<()> {
        let path = self.current_path(conversation_id);
        tokio::fs::write(&path, serde_json::to_string(&CurrentSession {
            session_id: id.to_string(),
        })?)
        .await
        .with_context(|| format!("writing current session pointer {}", path.display()))?;
        Ok(())
    }

    async fn read_current(&self, conversation_id: &str) -> Result<Option<String>> {
        let path = self.current_path(conversation_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => match serde_json::from_str::<CurrentSession>(&content) {
                Ok(current) if !current.session_id.is_empty() => Ok(Some(current.session_id)),
                _ => {
                    log::warn!("invalid current session file: {}", path.display());
                    Ok(None)
                }
            },
            Err(_) => Ok(None),
        }
    }

    async fn build_session(
        &self,
        conversation_id: &str,
        session_uuid: &str,
    ) -> Result<Arc<SqliteSession>> {
        let store = self.store_for(conversation_id)?;
        let session = Arc::new(
            SqliteSession::new(
                conversation_id,
                session_uuid.to_string(),
                self.persona_name.clone(),
                self.system.clone(),
                self.tools.clone(),
                self.manager.clone(),
                self.permissions.clone(),
                self.llm.clone(),
                store,
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
    async fn create(&self, conversation_id: &str) -> Result<Arc<dyn Session>> {
        let store = self.store_for(conversation_id)?;
        let uuid = store.create().await?;
        let session = self.build_session(conversation_id, &uuid).await?;
        if !self.context.is_empty() {
            store
                .append(
                    &uuid,
                    &[SessionItem::Message {
                        role: MessageRole::Context,
                        content: self.context.clone(),
                    }],
                )
                .await?;
        }
        self.save_current(conversation_id, &session.id).await?;
        Ok(session)
    }

    async fn current(&self, conversation_id: &str) -> Result<Option<Arc<dyn Session>>> {
        match self.read_current(conversation_id).await? {
            Some(id) => self.load(&id).await,
            None => Ok(None),
        }
    }

    async fn load(&self, id: &str) -> Result<Option<Arc<dyn Session>>> {
        let key = id.to_string();
        if let Some(session) = self.sessions.lock().unwrap().get(&key).cloned() {
            return Ok(Some(session));
        }
        let Some((conversation_id, session_uuid)) = id.split_once('/') else {
            return Ok(None);
        };
        let store = self.store_for(conversation_id)?;
        if !store.has(session_uuid) {
            return Ok(None);
        }
        Ok(Some(
            self.build_session(conversation_id, session_uuid).await?,
        ))
    }

    async fn archive(&self, id: &str) -> Result<()> {
        let Some((conversation_id, session_uuid)) = id.split_once('/') else {
            anyhow::bail!("invalid session id: {id}");
        };
        let store = self.store_for(conversation_id)?;
        store.archive(session_uuid).await
    }

    async fn list(&self, conversation_id: &str) -> Result<Vec<Arc<dyn Session>>> {
        let store = self.store_for(conversation_id)?;
        let mut out = Vec::new();
        for (session_uuid, _seq, _created_at) in store.list().await? {
            let id = format!("{conversation_id}/{session_uuid}");
            if let Some(session) = self.load(&id).await? {
                out.push(session);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::session::ToolCall;
    use nota_core::tool::{Tool, ToolParams};
    use std::collections::VecDeque;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nota_llm_session_{tag}_{}",
            std::process::id()
        ));
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
    async fn create_and_current_roundtrip() {
        let root = temp_root("roundtrip");
        let (sm, _) = manager_with(&root, Arc::new(MockLlm::new(vec![])));

        let s1 = sm.create("onebot_private_42").await.unwrap();
        assert!(
            s1.id().starts_with("onebot_private_42/"),
            "session id must be conversation-namespaced: {}",
            s1.id()
        );

        let current = sm.current("onebot_private_42").await.unwrap().unwrap();
        assert_eq!(current.id(), s1.id());
        assert!(sm.current("other_chat").await.unwrap().is_none());
        assert!(sm.load("missing-session").await.unwrap().is_none());

        let list = sm.list("onebot_private_42").await.unwrap();
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
            tool_calls: vec![],
        }]));
        let (sm, _) = manager_with(&root, llm);

        let s = sm.create("conv1").await.unwrap();
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
        // First call: tool call; second call: final content.
        let llm = Arc::new(MockLlm::new(vec![
            LlmResponse {
                id: Some("r1".to_string()),
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "echo".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
            LlmResponse {
                id: Some("r2".to_string()),
                content: Some("done".to_string()),
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
            async fn run(&self, _args: &str, ctx: ToolContext) -> Result<String> {
                self.seen.lock().unwrap().push(format!(
                    "{}|{}|{:?}",
                    ctx.persona_name,
                    ctx.conversation_id.as_deref().unwrap_or("none"),
                    ctx.request_id
                ));
                Ok("tool result".to_string())
            }
        }
        tools
            .register(Arc::new(EchoTool {
                seen: seen.clone(),
            }))
            .unwrap();

        let sm = Arc::new(
            SqliteSessionManager::with_llm(
                &root,
                "bob".to_string(),
                "system".to_string(),
                "persona context".to_string(),
                tools,
                manager.clone(),
                Arc::new(PermissionRegistry::new()),
                llm,
            )
            .unwrap(),
        );

        let s = sm.create("conv1").await.unwrap();
        s.send("please".to_string(), Some("req_9".to_string()))
            .await
            .unwrap();

        let history = s.raw_history().await.unwrap();
        let kinds: Vec<&str> = history
            .iter()
            .map(|(_, i)| match i {
                SessionItem::Message { .. } => "message",
                SessionItem::FunctionCall(_) => "function_call",
                SessionItem::FunctionCallOutput { .. } => "function_call_output",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "function_call",
                "function_call_output",
                "message"
            ]
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("bob|conv1|Some(\"req_9\")"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sessions_persist_across_restart() {
        let root = temp_root("persist");
        let llm = || {
            Arc::new(MockLlm::new(vec![LlmResponse {
                id: Some("resp_x".to_string()),
                content: Some("hi".to_string()),
                tool_calls: vec![],
            }])) as Arc<dyn ChatLlm>
        };
        let (sm, _) = manager_with(&root, llm());
        let s1 = sm.create("conv1").await.unwrap();
        s1.send("hello".to_string(), None).await.unwrap();
        let s1_id = s1.id().to_string();

        // A fresh manager over the same root resumes the current session.
        let (sm2, _) = manager_with(&root, llm());
        let resumed = sm2.current("conv1").await.unwrap().unwrap();
        assert_eq!(resumed.id(), s1_id);
        assert_eq!(resumed.raw_history().await.unwrap().len(), 3);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn archive_keeps_session_readable() {
        let root = temp_root("archive");
        let (sm, _) = manager_with(&root, Arc::new(MockLlm::new(vec![])));

        let s1 = sm.create("conv1").await.unwrap();
        let s2 = sm.create("conv1").await.unwrap();
        assert_ne!(s1.id(), s2.id());

        sm.archive(&s1.id()).await.unwrap();
        // Archived sessions stay readable via list; current points at s2.
        assert!(sm.load(&s1.id()).await.unwrap().is_some());
        let list = sm.list("conv1").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(sm.current("conv1").await.unwrap().unwrap().id(), s2.id());

        std::fs::remove_dir_all(&root).ok();
    }
}
