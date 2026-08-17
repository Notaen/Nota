//! Persona runtime: the loop that turns inbound conversation messages into
//! LLM turns.
//!
//! This is the only place that combines the persona store, the LLM session
//! manager (`nota-llm`), the agent loop, and the conversation router. Slash
//! commands (`//clear`, `//allow_read`) are handled here, before anything
//! reaches the LLM: `//clear` starts a fresh LLM session for the current
//! conversation, so it lives next to the session owner.
//!
//! The llm crate is conversation-agnostic and has no default store path:
//! this runtime gives each conversation its own directory under
//! `conversation/<conversation_id>/` and persists a `current.json` pointer to
//! the current session id there, so "which session is current" is a plain
//! caller-side read — the llm crate never knows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use nota_core::conversation::{Conversation, ConversationManager, InboundMessage};
use nota_core::permissions::{PathPolicy, PermissionRegistry};
use nota_core::persona::{Persona, PersonaStore};
use nota_llm::tool::{ToolContext, ToolRegistry};
use nota_llm::{AgentRunner, LlmClient, LlmItem, LlmSession, LlmSessionManager, MessageRole};
use serde::{Deserialize, Serialize};

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";
const PERSONA_FILES: &[&str] = &[SOLO_FILENAME, MEMORY_FILENAME];
/// File inside each conversation directory holding the current session id.
const CURRENT_SESSION_FILE: &str = "current.json";

#[derive(Serialize, Deserialize)]
struct CurrentSession {
    session_id: String,
}

pub struct PersonaRuntime {
    persona: Persona,
    store: Arc<dyn PersonaStore>,
    conversation_dir: PathBuf,
    llm: Arc<dyn LlmClient>,
    registry: Arc<dyn ToolRegistry>,
    permissions: Arc<PermissionRegistry>,
    policy: Arc<PathPolicy>,
    /// Per-conversation session managers (one directory per conversation).
    session_managers: Mutex<HashMap<String, Arc<LlmSessionManager>>>,
}

impl PersonaRuntime {
    pub fn new(
        persona: Persona,
        store: Arc<dyn PersonaStore>,
        conversation_dir: PathBuf,
        llm: Arc<dyn LlmClient>,
        registry: Arc<dyn ToolRegistry>,
        permissions: Arc<PermissionRegistry>,
        policy: Arc<PathPolicy>,
    ) -> Self {
        Self {
            persona,
            store,
            conversation_dir,
            llm,
            registry,
            permissions,
            policy,
            session_managers: Mutex::new(HashMap::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.persona.name
    }

    /// The session manager for one conversation: sessions live flat in
    /// `conversation_dir/<conversation_id>/`, created lazily on first use.
    fn session_manager(&self, conversation_id: &str) -> Result<Arc<LlmSessionManager>> {
        let mut managers = self.session_managers.lock().unwrap();
        if let Some(manager) = managers.get(conversation_id) {
            return Ok(manager.clone());
        }
        let manager = Arc::new(LlmSessionManager::new(&self.conversation_path(conversation_id))?);
        managers.insert(conversation_id.to_string(), manager.clone());
        Ok(manager)
    }

    fn conversation_path(&self, conversation_id: &str) -> PathBuf {
        self.conversation_dir.join(conversation_id)
    }

    /// Read the current session id of a conversation, if a pointer exists.
    async fn read_current_session(&self, conversation_id: &str) -> Option<String> {
        let pointer = self.conversation_path(conversation_id).join(CURRENT_SESSION_FILE);
        match tokio::fs::read_to_string(&pointer).await {
            Ok(content) => {
                match serde_json::from_str::<CurrentSession>(&content) {
                    Ok(current) if !current.session_id.is_empty() => {
                        Some(current.session_id)
                    }
                    _ => {
                        log::warn!("invalid current session file: {}", pointer.display());
                        None
                    }
                }
            }
            Err(_) => None,
        }
    }

    async fn save_current_session(
        &self,
        conversation_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let pointer = self.conversation_path(conversation_id).join(CURRENT_SESSION_FILE);
        let content = serde_json::to_string(&CurrentSession {
            session_id: session_id.to_string(),
        })?;
        tokio::fs::write(&pointer, content).await?;
        Ok(())
    }

    /// The current session of a conversation: whatever the `current.json`
    /// pointer says, or a fresh one (persisted) on first contact or when the
    /// pointer points at a missing session.
    async fn current_session(
        &self,
        manager: &LlmSessionManager,
        conversation_id: &str,
    ) -> Result<Arc<LlmSession>> {
        if let Some(id) = self.read_current_session(conversation_id).await {
            if let Some(session) = manager.session(&id).await? {
                return Ok(session);
            }
            log::warn!("current session '{id}' missing; creating a fresh one");
        }
        let session = manager.create().await?;
        self.save_current_session(conversation_id, &session.id).await?;
        Ok(session)
    }

    pub async fn run(self: Arc<Self>, manager: Arc<ConversationManager>) {
        let mut rx = manager.subscribe_persona(&self.persona.name);
        let agent = AgentRunner::new(self.llm.clone(), self.registry.clone());
        let name = self.persona.name.clone();

        loop {
            let msg: InboundMessage = match rx.recv().await {
                Some(e) => e,
                None => break,
            };
            let conversation = msg.conversation;
            let conversation_id = conversation.conversation_id.clone();

            // Slash commands are handled here, before anything reaches the
            // LLM: `//clear` starts a fresh LLM session for this conversation
            // (the old one stays archived), `//allow_read` grants a
            // workspace-external read path.
            if let Some(cmd) = parse_command(&msg.content) {
                self.run_command(&conversation, cmd, &manager).await;
                continue;
            }

            let Ok(session_manager) = self.session_manager(&conversation_id) else {
                log::error!(
                    "failed to open session store for conversation '{conversation_id}'"
                );
                continue;
            };
            let llm_session = match self
                .current_session(&session_manager, &conversation_id)
                .await
            {
                Ok(session) => session,
                Err(e) => {
                    log::error!(
                        "failed to resolve LLM session for conversation '{conversation_id}': {e:#}"
                    );
                    continue;
                }
            };

            let system = self.build_system_prompt().await;

            let display = format!("{}{}", msg.prefix, msg.content);
            let _ = llm_session
                .append(&[LlmItem::Message {
                    role: MessageRole::User,
                    content: display,
                }])
                .await;

            let context = llm_session.context().await.unwrap_or_default();

            let suppress_reply = Arc::new(AtomicBool::new(false));
            let tool_ctx = ToolContext {
                persona_name: name.clone(),
                manager: manager.clone(),
                request_id: msg.request_id.clone(),
                permissions: self.permissions.clone(),
                conversation_id: Some(conversation_id.clone()),
                suppress_reply: suppress_reply.clone(),
            };

            match agent.run(&system, &context, tool_ctx).await {
                Ok((new_items, response_id)) => {
                    let _ = llm_session.append(&new_items).await;
                    if let Some(id) = response_id {
                        let _ = llm_session.set_response_id(&id).await;
                    }

                    if !suppress_reply.load(Ordering::SeqCst)
                        && let Some(LlmItem::Message {
                            role: MessageRole::Assistant,
                            content,
                        }) = new_items.last()
                        && !content.trim().is_empty()
                    {
                        manager
                            .route_outbound(
                                Some(&conversation_id),
                                None,
                                content,
                                msg.request_id.clone(),
                            )
                            .await;
                    }
                }
                Err(e) => {
                    log::error!("Persona {} agent error: {e}", name);
                }
            }
        }
    }

    async fn run_command(
        &self,
        conversation: &Conversation,
        cmd: SlashCommand,
        manager: &ConversationManager,
    ) {
        match cmd {
            SlashCommand::Clear => {
                let conversation_id = conversation.conversation_id.clone();
                if let Ok(session_manager) = self.session_manager(&conversation_id) {
                    match session_manager.create().await {
                        Ok(session) => {
                            let _ = self
                                .save_current_session(&conversation_id, &session.id)
                                .await;
                        }
                        Err(e) => log::warn!("//clear session creation failed: {e:#}"),
                    }
                } else {
                    log::warn!("//clear session store unavailable");
                }
                manager
                    .route_outbound(
                        Some(&conversation.conversation_id),
                        None,
                        "已开启新的会话，旧记录已存档",
                        None,
                    )
                    .await;
            }
            SlashCommand::AllowRead(path) => {
                self.policy.allow_read(path.clone()).await;
                manager
                    .route_outbound(
                        Some(&conversation.conversation_id),
                        None,
                        &format!("已允许读取：{}", path.display()),
                        None,
                    )
                    .await;
            }
        }
    }

    async fn build_system_prompt(&self) -> String {
        let name = &self.persona.name;
        let mut parts = Vec::new();
        for filename in PERSONA_FILES {
            match self.store.read_persona_file(name, filename).await {
                Ok(content) if !content.is_empty() => {
                    parts.push(format!("# {filename}\n{content}"));
                }
                _ => {}
            }
        }
        parts.join("\n\n")
    }
}

enum SlashCommand {
    Clear,
    AllowRead(PathBuf),
}

fn parse_command(content: &str) -> Option<SlashCommand> {
    let trimmed = content.trim();
    let cmd = trimmed.strip_prefix("//")?.trim();
    if cmd == "clear" {
        return Some(SlashCommand::Clear);
    }
    if let Some(rest) = cmd.strip_prefix("allow_read") {
        let path = rest.trim();
        if !path.is_empty() {
            return Some(SlashCommand::AllowRead(PathBuf::from(path)));
        }
    }
    None
}
