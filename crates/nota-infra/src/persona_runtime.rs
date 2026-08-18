//! Persona runtime: the conversation layer that turns inbound chat messages
//! into session turns.
//!
//! It holds the core [`SessionManager`] abstraction (the concrete SQLite
//! manager lives in `nota-llm` and is injected by the composition root) and
//! knows nothing about the LLM client or the turn loop: for every inbound
//! message it resolves the conversation's current session (or creates one)
//! and feeds the content in. Slash commands (`//clear`, `//allow_read`) are
//! handled here, before anything reaches the session: `//clear` archives the
//! current session and starts a fresh one.

use std::path::PathBuf;
use std::sync::Arc;

use nota_core::conversation::{Conversation, ConversationManager};
use nota_core::permissions::PathPolicy;
use nota_core::persona::Persona;
use nota_core::session::SessionManager;

pub struct PersonaRuntime {
    persona: Persona,
    session_manager: Arc<dyn SessionManager>,
    manager: Arc<ConversationManager>,
    policy: Arc<PathPolicy>,
}

impl PersonaRuntime {
    pub fn new(
        persona: Persona,
        session_manager: Arc<dyn SessionManager>,
        manager: Arc<ConversationManager>,
        policy: Arc<PathPolicy>,
    ) -> Self {
        Self {
            persona,
            session_manager,
            manager,
            policy,
        }
    }

    pub fn name(&self) -> &str {
        &self.persona.name
    }

    pub async fn run(self: Arc<Self>) {
        let mut rx = self.manager.subscribe_persona(&self.persona.name);
        let name = self.persona.name.clone();

        loop {
            let msg = match rx.recv().await {
                Some(msg) => msg,
                None => break,
            };
            let conversation_id = msg.conversation.conversation_id.clone();

            // Slash commands are handled here, before anything reaches the
            // session: `//clear` archives the current session and starts a
            // fresh one, `//allow_read` grants a workspace-external read path.
            if let Some(cmd) = parse_command(&msg.content) {
                self.run_command(&msg.conversation, cmd).await;
                continue;
            }

            let session = match self.session_manager.current(&conversation_id).await {
                Ok(Some(session)) => session,
                Ok(None) => match self.session_manager.create(&conversation_id).await {
                    Ok(session) => session,
                    Err(e) => {
                        log::error!(
                            "failed to create session for conversation '{conversation_id}': {e:#}"
                        );
                        continue;
                    }
                },
                Err(e) => {
                    log::error!(
                        "failed to resolve session for conversation '{conversation_id}': {e:#}"
                    );
                    continue;
                }
            };

            let display = format!("{}{}", msg.prefix, msg.content);
            if let Err(e) = session.send(display, msg.request_id.clone()).await {
                log::error!("Persona {} session turn failed: {e}", name);
            }
        }
    }

    async fn run_command(&self, conversation: &Conversation, cmd: SlashCommand) {
        let conversation_id = conversation.conversation_id.clone();
        match cmd {
            SlashCommand::Clear => {
                if let Ok(Some(current)) = self.session_manager.current(&conversation_id).await
                    && let Err(e) = self.session_manager.archive(current.id()).await
                {
                    log::warn!("//clear archive failed: {e:#}");
                }
                match self.session_manager.create(&conversation_id).await {
                    Ok(_) => {}
                    Err(e) => log::warn!("//clear session creation failed: {e:#}"),
                }
                self.manager
                    .route_outbound(
                        Some(&conversation_id),
                        None,
                        "已开启新的会话，旧记录已存档",
                        None,
                    )
                    .await;
            }
            SlashCommand::AllowRead(path) => {
                self.policy.allow_read(path.clone()).await;
                self.manager
                    .route_outbound(
                        Some(&conversation_id),
                        None,
                        &format!("已允许读取：{}", path.display()),
                        None,
                    )
                    .await;
            }
        }
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
