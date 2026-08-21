//! Persona runtime: the conversation layer that turns inbound chat messages
//! into session turns.
//!
//! It is the **caller** of the core [`SessionManager`] abstraction: sessions
//! are conversation-agnostic (flat uuid files), so this runtime owns the
//! conversation → session mapping. For every conversation it lazily creates
//! one session manager rooted at that conversation's directory and injects
//! the conversation's tool set (including a conversation-bound `reply` tool)
//! through the injected `make_manager` closure. The current session id per
//! conversation is persisted in `current.json` inside the conversation
//! directory; `//clear` archives the current session and starts a fresh one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use nota_core::conversation::{Conversation, ConversationManager};
use nota_core::permissions::PathPolicy;
use nota_core::persona::Persona;
use nota_core::session::{Session, SessionManager};
// `MessageRole` / `SessionItem` were only used by the auto-delivery feature;
// restore them with `deliver_assistant_reply` if it is ever re-enabled.
use serde::{Deserialize, Serialize};

/// Builds the session manager for one conversation (rooted at the
/// conversation's directory, with the conversation's tool set). Injected by
/// the composition root — `nota-infra` itself never references the llm crate.
pub type ManagerFactory =
    Arc<dyn Fn(&str) -> Result<Arc<dyn SessionManager>> + Send + Sync>;

#[derive(Serialize, Deserialize)]
struct CurrentSession {
    session_id: String,
}

pub struct PersonaRuntime {
    persona: Persona,
    make_manager: ManagerFactory,
    manager: Arc<ConversationManager>,
    policy: Arc<PathPolicy>,
    conversations_root: PathBuf,
    managers: Mutex<HashMap<String, Arc<dyn SessionManager>>>,
}

impl PersonaRuntime {
    pub fn new(
        persona: Persona,
        make_manager: ManagerFactory,
        manager: Arc<ConversationManager>,
        policy: Arc<PathPolicy>,
        conversations_root: PathBuf,
    ) -> Self {
        Self {
            persona,
            make_manager,
            manager,
            policy,
            conversations_root,
            managers: Mutex::new(HashMap::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.persona.name
    }

    fn current_path(&self, conversation_id: &str) -> PathBuf {
        self.conversations_root
            .join(conversation_id)
            .join("current.json")
    }

    fn manager_for(&self, conversation_id: &str) -> Result<Arc<dyn SessionManager>> {
        if let Some(manager) = self
            .managers
            .lock()
            .unwrap()
            .get(conversation_id)
            .cloned()
        {
            return Ok(manager);
        }
        let manager = (self.make_manager)(conversation_id)?;
        self.managers
            .lock()
            .unwrap()
            .insert(conversation_id.to_string(), manager.clone());
        Ok(manager)
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

    async fn save_current(&self, conversation_id: &str, id: &str) -> Result<()> {
        let path = self.current_path(conversation_id);
        tokio::fs::write(
            &path,
            serde_json::to_string(&CurrentSession {
                session_id: id.to_string(),
            })?,
        )
        .await?;
        Ok(())
    }

    /// The current session of a conversation, creating one on first contact
    /// and persisting the pointer.
    async fn resolve_current_session(
        &self,
        conversation_id: &str,
    ) -> Result<Arc<dyn Session>> {
        let manager = self.manager_for(conversation_id)?;
        if let Some(current) = self.read_current(conversation_id).await?
            && let Some(session) = manager.load(&current).await?
        {
            return Ok(session);
        }
        let session = manager.create().await?;
        self.save_current(conversation_id, session.id()).await?;
        Ok(session)
    }

    /// Every session of a conversation, oldest first (active + archived),
    /// for the chatlog API. Unknown conversations return an empty list.
    pub async fn sessions(&self, conversation_id: &str) -> Result<Vec<Arc<dyn Session>>> {
        let dir = self.conversations_root.join(conversation_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let manager = self.manager_for(conversation_id)?;
        manager.list().await
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

            let session = match self.resolve_current_session(&conversation_id).await {
                Ok(session) => session,
                Err(e) => {
                    log::error!(
                        "failed to resolve session for conversation '{conversation_id}': {e:#}"
                    );
                    continue;
                }
            };

            // Auto-delivery of the final assistant text is disabled
            // (2026-08-20): the turn's answer reaches the user only through
            // explicit send tools (`reply`, adapter sends). The snapshot and
            // the `deliver_assistant_reply` call below are kept in comments
            // and can be restored if the feature ever returns.
            //
            // // Snapshot the history length before the turn so the assistant
            // // text this turn produces can be delivered afterwards without
            // // re-delivering earlier messages.
            // let history_before = match session.raw_history().await {
            //     Ok(history) => Some(history.len()),
            //     Err(e) => {
            //         log::warn!(
            //             "failed to snapshot history before turn for conversation \
            //              '{conversation_id}': {e:#}"
            //         );
            //         None
            //     }
            // };

            let display = format!("{}{}", msg.prefix, msg.content);
            if let Err(e) = session.send(display, msg.request_id.clone()).await {
                log::error!("Persona {} session turn failed: {e}", name);
                continue;
            }

            // // Directly deliver the LLM's answer into the conversation. The
            // // explicit send tools (`reply`, adapter sends) are untouched;
            // // this only forwards the final assistant text of the turn.
            // if let Some(before) = history_before {
            //     self.deliver_assistant_reply(
            //         session.as_ref(),
            //         &conversation_id,
            //         before,
            //         msg.request_id,
            //     )
            //     .await;
            // }
        }
    }

    // /// Deliver the assistant text a turn produced directly into its
    // /// conversation. Only items appended after `before` (the history length
    // /// at turn start) are considered, so earlier assistant messages are
    // /// never re-delivered; empty text is skipped (staying silent means
    // /// producing no text at all).
    // async fn deliver_assistant_reply(
    //     &self,
    //     session: &dyn Session,
    //     conversation_id: &str,
    //     before: usize,
    //     request_id: Option<String>,
    // ) {
    //     let history = match session.raw_history().await {
    //         Ok(history) => history,
    //         Err(e) => {
    //             log::warn!(
    //                 "failed to read history after turn for conversation \
    //                  '{conversation_id}': {e:#}"
    //             );
    //             return;
    //         }
    //     };
    //     for (_, item) in history.into_iter().skip(before) {
    //         if let SessionItem::Message {
    //             role: MessageRole::Assistant,
    //             content,
    //         } = item
    //             && !content.trim().is_empty()
    //         {
    //             self.manager
    //                 .route_outbound(
    //                     Some(conversation_id),
    //                     None,
    //                     &content,
    //                     request_id.clone(),
    //                 )
    //                 .await;
    //         }
    //     }
    // }

    async fn run_command(&self, conversation: &Conversation, cmd: SlashCommand) {
        let conversation_id = conversation.conversation_id.clone();
        match cmd {
            SlashCommand::Clear => {
                let result = async {
                    let manager = self.manager_for(&conversation_id)?;
                    if let Some(current) = self.read_current(&conversation_id).await?
                        && let Some(session) = manager.load(&current).await?
                    {
                        manager.archive(session.id()).await?;
                    }
                    let session = manager.create().await?;
                    self.save_current(&conversation_id, session.id()).await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = result {
                    log::warn!("//clear failed: {e:#}");
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

#[cfg(test)]
mod tests {
    // Auto-delivery tests (and their MockSession / test_runtime scaffolding)
    // were commented out together with the feature on 2026-08-20; restore
    // them with `deliver_assistant_reply` if it is ever re-enabled.
}
