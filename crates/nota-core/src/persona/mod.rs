use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::AgentRunner;
use crate::bus::{BusEvent, EventBus};
use crate::llm::{ChatMessage, LlmClient};
use crate::permissions::PermissionRegistry;
use crate::tool::{ToolContext, ToolRegistry};

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";
const PERSONA_FILES: &[&str] = &[SOLO_FILENAME, MEMORY_FILENAME];

#[derive(Debug, Clone)]
pub struct Persona {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatLogEntry {
    pub sender: String,
    pub content: String,
    pub timestamp: i64,
    pub context: String,
}

#[async_trait]
pub trait PersonaStore: Send + Sync {
    async fn read_persona_file(&self, name: &str, filename: &str) -> Result<String>;

    async fn write_persona_file(&self, name: &str, filename: &str, content: &str)
        -> Result<()>;

    async fn create_persona(&self, name: &str) -> Result<()>;

    async fn delete_persona(&self, name: &str) -> Result<()>;

    async fn append_chatlog(&self, name: &str, entries: &[ChatLogEntry]) -> Result<()>;

    async fn read_chatlog(&self, name: &str, since: Option<i64>) -> Result<Vec<ChatLogEntry>>;

    async fn list_personas(&self) -> Result<Vec<String>>;
}

pub struct PersonaRuntime {
    persona: Persona,
    store: Arc<dyn PersonaStore>,
    llm: Arc<dyn LlmClient>,
    registry: Arc<dyn ToolRegistry>,
    permissions: Arc<PermissionRegistry>,
}

impl PersonaRuntime {
    pub fn new(
        persona: Persona,
        store: Arc<dyn PersonaStore>,
        llm: Arc<dyn LlmClient>,
        registry: Arc<dyn ToolRegistry>,
        permissions: Arc<PermissionRegistry>,
    ) -> Self {
        Self {
            persona,
            store,
            llm,
            registry,
            permissions,
        }
    }

    pub fn name(&self) -> &str {
        &self.persona.name
    }

    pub async fn run(self: Arc<Self>, bus: Arc<EventBus>) {
        let mut rx = bus.subscribe();
        let agent = AgentRunner::new(self.llm.clone(), self.registry.clone());
        let name = self.persona.name.clone();

        loop {
            let event = match rx.recv().await {
                Some(e) => e,
                None => break,
            };

            if event.sender == name {
                continue;
            }
            if let Some(ref t) = event.target
                && t != &name
            {
                continue;
            }

            let system = self.build_system_prompt().await;

            let history = self
                .load_chatlog_context()
                .await
                .unwrap_or_default();

            let mut messages = history;
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(event.content.clone()),
                tool_calls: None,
                tool_call_id: None,
            });

            let tool_ctx = ToolContext {
                persona_name: name.clone(),
                bus: bus.clone(),
                request_id: event.request_id.clone(),
                permissions: self.permissions.clone(),
            };

            let _ = self
                .store
                .append_chatlog(
                    &name,
                    &[ChatLogEntry {
                        sender: event.sender.clone(),
                        content: event.content.clone(),
                        timestamp: event.timestamp,
                        context: event.context.clone(),
                    }],
                )
                .await;

            match agent.run(&system, &messages, tool_ctx).await {
                Ok(new_msgs) => {
                    let mut chatlog_entries: Vec<ChatLogEntry> = Vec::new();
                    let now = chrono::Utc::now().timestamp();

                    for msg in &new_msgs {
                        let role_str = &msg.role;
                        let entry_content = msg
                            .content
                            .clone()
                            .unwrap_or_else(|| format!("[{role_str}]"));
                        chatlog_entries.push(ChatLogEntry {
                            sender: name.clone(),
                            content: entry_content.clone(),
                            timestamp: now,
                            context: String::new(),
                        });
                    }

                    let _ = self
                        .store
                        .append_chatlog(&name, &chatlog_entries)
                        .await;

                    if let Some(last) = new_msgs.last()
                        && let Some(content) = &last.content
                        && last.role == "assistant"
                    {
                        bus.send(BusEvent::message_with_context(
                            name.clone(),
                            content.clone(),
                            event.request_id.clone(),
                            event.context.clone(),
                        ));
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

    async fn load_chatlog_context(&self) -> Result<Vec<ChatMessage>> {
        let entries = self
            .store
            .read_chatlog(&self.persona.name, None)
            .await?;

        let mut messages = Vec::new();
        for entry in entries {
            let role = if entry.sender == self.persona.name {
                "assistant"
            } else {
                "user"
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
