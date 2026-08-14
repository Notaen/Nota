//! Chat sessions and the session-scoped message routing layer.
//!
//! There is no global broadcast bus: every message is routed by session.
//! - **Inbound**: an adapter delivers a chat message to the *target persona's
//!   inbox* (carrying the `Session`), so the persona always receives it.
//! - **Outbound**: the persona replies to a session (routed to that session's
//!   adapter) or sends to a channel-agnostic target (broadcast to adapters,
//!   each adapter claims what it understands).
//! - **Permissions** are routed to the session's adapter for user approval.
//! - **Slash commands** (`//clear` etc.) are handled here, before anything
//!   reaches the persona.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::history::HistoryStore;
use crate::permissions::PathPolicy;

/// Identifies one conversation: a persona plus an adapter-assigned session id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Session {
    pub persona: String,
    pub session_id: String,
}

impl Session {
    pub fn new(persona: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            persona: persona.into(),
            session_id: session_id.into(),
        }
    }
}

/// Adapter prefix of a session id (up to the first `_`): `onebot`, `web`, …
pub fn adapter_prefix(session_id: &str) -> &str {
    session_id.split('_').next().unwrap_or(session_id)
}

/// An inbound chat message delivered to a persona's inbox.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub session: Session,
    pub sender: String,
    /// Identity header shown before the content (e.g. `[好友 昵称(QQ)] `).
    pub prefix: String,
    /// The user's real message text, without any header.
    pub content: String,
    pub timestamp: i64,
    pub request_id: Option<String>,
}

/// An outbound event routed to adapters. Either `session_id` (replies and
/// session-targeted sends) or `target` (channel-agnostic, e.g.
/// `group:551947633`) is set.
#[derive(Debug, Clone)]
pub struct OutboundEvent {
    pub session_id: Option<String>,
    pub target: Option<String>,
    pub content: String,
    pub request_id: Option<String>,
}

/// A permission request routed to the session's adapter for user approval.
#[derive(Debug, Clone)]
pub struct PermissionEvent {
    pub session_id: String,
    pub permission_id: String,
    pub prompt: String,
    pub parent_request_id: Option<String>,
}

/// Everything an adapter can receive from its sessions.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    Outbound(OutboundEvent),
    Permission(PermissionEvent),
}

/// Routes messages by session: persona inboxes for inbound, adapter outboxes
/// for outbound and permissions. Owns slash-command handling (`//clear`).
pub struct SessionManager {
    persona_inboxes: Mutex<HashMap<String, UnboundedSender<InboundMessage>>>,
    adapter_outboxes: Mutex<HashMap<String, Vec<UnboundedSender<AdapterEvent>>>>,
    history: Arc<dyn HistoryStore>,
    policy: Arc<PathPolicy>,
}

impl SessionManager {
    pub fn new(history: Arc<dyn HistoryStore>, policy: Arc<PathPolicy>) -> Self {
        Self {
            persona_inboxes: Mutex::new(HashMap::new()),
            adapter_outboxes: Mutex::new(HashMap::new()),
            history,
            policy,
        }
    }

    /// Register a persona's inbox; the persona loop consumes from it.
    pub fn subscribe_persona(&self, persona: &str) -> UnboundedReceiver<InboundMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.persona_inboxes
            .lock()
            .unwrap()
            .insert(persona.to_string(), tx);
        rx
    }

    /// Register an adapter outbox (keyed by adapter prefix, e.g. `onebot`).
    pub fn subscribe_adapter(
        &self,
        prefix: &str,
    ) -> UnboundedReceiver<AdapterEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.adapter_outboxes
            .lock()
            .unwrap()
            .entry(prefix.to_string())
            .or_default()
            .push(tx);
        rx
    }

    /// Deliver an inbound chat message to the target persona. Slash commands
    /// (`//…`) are handled here and never reach the persona.
    pub async fn deliver(
        &self,
        session: &Session,
        sender: &str,
        prefix: &str,
        content: &str,
        request_id: Option<String>,
    ) {
        if let Some(cmd) = parse_command(content) {
            self.run_command(session, cmd).await;
            return;
        }

        let msg = InboundMessage {
            session: session.clone(),
            sender: sender.to_string(),
            prefix: prefix.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            request_id,
        };
        // DEBUG：记录流入 persona 的消息内容，方便排查会话里到底发生了什么
        log::debug!(
            "[in] session '{}' -> persona '{}' from '{}': {}{}",
            session.session_id,
            session.persona,
            sender,
            prefix,
            content
        );
        let inbox = self
            .persona_inboxes
            .lock()
            .unwrap()
            .get(&session.persona)
            .cloned();
        if let Some(tx) = inbox {
            let _ = tx.send(msg);
        } else {
            log::warn!(
                "no inbox for persona '{}'; message dropped",
                session.persona
            );
        }
    }

    /// Persona replies / sends: route to the session's adapter, or broadcast
    /// to every adapter when only a channel-agnostic target is known.
    pub async fn route_outbound(
        &self,
        session_id: Option<&str>,
        target: Option<&str>,
        content: &str,
        request_id: Option<String>,
    ) {
        let event = AdapterEvent::Outbound(OutboundEvent {
            session_id: session_id.map(str::to_string),
            target: target.map(str::to_string),
            content: content.to_string(),
            request_id,
        });
        // DEBUG：记录 persona 发出的消息内容（回复 / send_message / 命令回执）
        log::debug!(
            "[out] session '{}' target '{}': {}",
            session_id.unwrap_or("-"),
            target.unwrap_or("-"),
            content
        );
        let outboxes = self.adapter_outboxes.lock().unwrap();
        match session_id {
            Some(sid) => {
                if let Some(channels) = outboxes.get(adapter_prefix(sid)) {
                    for tx in channels {
                        let _ = tx.send(event.clone());
                    }
                }
            }
            None => {
                for channels in outboxes.values() {
                    for tx in channels {
                        let _ = tx.send(event.clone());
                    }
                }
            }
        }
    }

    /// Route a permission request to the session's adapter.
    pub async fn send_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        prompt: &str,
        parent_request_id: Option<String>,
    ) {
        let event = AdapterEvent::Permission(PermissionEvent {
            session_id: session_id.to_string(),
            permission_id: permission_id.to_string(),
            prompt: prompt.to_string(),
            parent_request_id,
        });
        let outboxes = self.adapter_outboxes.lock().unwrap();
        if let Some(channels) = outboxes.get(adapter_prefix(session_id)) {
            for tx in channels {
                let _ = tx.send(event.clone());
            }
        }
    }

    /// Execute a slash command against the session and ack through the
    /// adapter.
    async fn run_command(&self, session: &Session, cmd: SlashCommand) {
        match cmd {
            SlashCommand::Clear => {
                if let Err(e) = self.history.add_clear_boundary(session).await {
                    log::warn!("//clear boundary failed: {e:#}");
                }
                self.route_outbound(
                    Some(&session.session_id),
                    None,
                    "已清除当前会话的历史记录",
                    None,
                )
                .await;
            }
            SlashCommand::AllowRead(path) => {
                self.policy.allow_read(path.clone()).await;
                self.route_outbound(
                    Some(&session.session_id),
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
