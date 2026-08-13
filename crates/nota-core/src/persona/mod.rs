use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::agent::AgentRunner;
use crate::history::{HistoryEntry, HistoryKind, HistoryStore};
use crate::llm::{ChatMessage, LlmClient};
use crate::permissions::PermissionRegistry;
use crate::session::{InboundMessage, Session, SessionManager};
use crate::tool::{ToolContext, ToolRegistry};

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";
const PERSONA_FILES: &[&str] = &[SOLO_FILENAME, MEMORY_FILENAME];

#[derive(Debug, Clone)]
pub struct Persona {
    pub name: String,
}

#[async_trait]
pub trait PersonaStore: Send + Sync {
    async fn read_persona_file(&self, name: &str, filename: &str) -> Result<String>;

    async fn write_persona_file(&self, name: &str, filename: &str, content: &str)
        -> Result<()>;

    async fn create_persona(&self, name: &str) -> Result<()>;

    async fn delete_persona(&self, name: &str) -> Result<()>;

    async fn list_personas(&self) -> Result<Vec<String>>;
}

pub struct PersonaRuntime {
    persona: Persona,
    store: Arc<dyn PersonaStore>,
    history: Arc<dyn HistoryStore>,
    llm: Arc<dyn LlmClient>,
    registry: Arc<dyn ToolRegistry>,
    permissions: Arc<PermissionRegistry>,
}

impl PersonaRuntime {
    pub fn new(
        persona: Persona,
        store: Arc<dyn PersonaStore>,
        history: Arc<dyn HistoryStore>,
        llm: Arc<dyn LlmClient>,
        registry: Arc<dyn ToolRegistry>,
        permissions: Arc<PermissionRegistry>,
    ) -> Self {
        Self {
            persona,
            store,
            history,
            llm,
            registry,
            permissions,
        }
    }

    pub fn name(&self) -> &str {
        &self.persona.name
    }

    pub async fn run(self: Arc<Self>, manager: Arc<SessionManager>) {
        let mut rx = manager.subscribe_persona(&self.persona.name);
        let agent = AgentRunner::new(self.llm.clone(), self.registry.clone());
        let name = self.persona.name.clone();

        loop {
            let msg: InboundMessage = match rx.recv().await {
                Some(e) => e,
                None => break,
            };
            let session = msg.session;

            let system = self.build_system_prompt().await;

            let history = self
                .load_chatlog_context(&session)
                .await
                .unwrap_or_default();

            let mut messages = history;
            let display = format!("{}{}", msg.prefix, msg.content);
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(display.clone()),
                tool_calls: None,
                tool_call_id: None,
            });

            let suppress_reply = Arc::new(AtomicBool::new(false));
            let tool_ctx = ToolContext {
                persona_name: name.clone(),
                manager: manager.clone(),
                request_id: msg.request_id.clone(),
                permissions: self.permissions.clone(),
                session_id: Some(session.session_id.clone()),
                suppress_reply: suppress_reply.clone(),
            };

            let _ = self
                .history
                .append(
                    &session,
                    &[HistoryEntry {
                        kind: HistoryKind::User,
                        content: display,
                        timestamp: msg.timestamp,
                    }],
                )
                .await;

            match agent.run(&system, &messages, tool_ctx).await {
                Ok(new_msgs) => {
                    let mut history_entries: Vec<HistoryEntry> = Vec::new();
                    let now = chrono::Utc::now().timestamp();

                    for msg in &new_msgs {
                        let role_str = &msg.role;
                        let entry_content = match &msg.content {
                            Some(c) => c.clone(),
                            // Tool calls are stored with their raw payload,
                            // rendered by the llm module.
                            None if msg
                                .tool_calls
                                .as_ref()
                                .is_some_and(|t| !t.is_empty()) =>
                            {
                                msg.raw_json()
                            }
                            None => format!("[{role_str}]"),
                        };
                        let sender = if msg.role == "tool"
                            || msg
                                .tool_calls
                                .as_ref()
                                .is_some_and(|t| !t.is_empty())
                        {
                            HistoryKind::Tool
                        } else {
                            HistoryKind::Assistant
                        };
                        history_entries.push(HistoryEntry {
                            kind: sender,
                            content: entry_content.clone(),
                            timestamp: now,
                        });
                    }

                    let _ = self
                        .history
                        .append(&session, &history_entries)
                        .await;

                    if !suppress_reply.load(Ordering::SeqCst)
                        && let Some(last) = new_msgs.last()
                        && let Some(content) = &last.content
                        && last.role == "assistant"
                        && !content.trim().is_empty()
                    {
                        manager
                            .route_outbound(
                                Some(&session.session_id),
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

    async fn load_chatlog_context(&self, session: &Session) -> Result<Vec<ChatMessage>> {
        let entries = self.history.read_context(session).await?;

        let mut messages = Vec::new();
        for entry in entries {
            let role = match entry.kind {
                // Tool results are replayed as assistant context (they carry
                // no `tool_call_id`, which OpenAI-compatible APIs require for
                // a real `tool` role message).
                HistoryKind::Assistant | HistoryKind::Tool => "assistant",
                HistoryKind::User => "user",
                HistoryKind::ClearBoundary => continue,
            };
            messages.push(ChatMessage {
                role: role.to_string(),
                content: Some(entry.content),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        Ok(messages)
    }
}
