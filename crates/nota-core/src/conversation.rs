//! User-visible conversations and the conversation-scoped message routing
//! layer.
//!
//! Terminology: **conversation** is the user-visible chat (OneBot private/group,
//! web) that adapters own; **session** is the LLM-level context managed by
//! `nota-llm`.
//!
//! There is no global broadcast bus: every message is routed by conversation.
//! - **Inbound**: an adapter delivers a chat message to the *target
//!   persona's inbox* (carrying the `Conversation`), so the persona always
//!   receives it.
//! - **Outbound**: the persona replies to a conversation (routed to that
//!   conversation's adapter) or sends to a channel-agnostic target
//!   (broadcast to adapters, each adapter claims what it understands).
//! - **Permissions** are routed to the conversation's adapter for user
//!   approval.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Identifies one conversation: a persona plus an adapter-assigned id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Conversation {
    pub persona: String,
    pub conversation_id: String,
}

impl Conversation {
    pub fn new(persona: impl Into<String>, conversation_id: impl Into<String>) -> Self {
        Self {
            persona: persona.into(),
            conversation_id: conversation_id.into(),
        }
    }
}

/// Adapter prefix of a conversation id (up to the first `_`): `onebot`,
/// `web`, …
pub fn adapter_prefix(conversation_id: &str) -> &str {
    conversation_id.split('_').next().unwrap_or(conversation_id)
}

/// An inbound chat message delivered to a persona's inbox.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub conversation: Conversation,
    pub sender: String,
    /// Identity header shown before the content (e.g. `[好友 昵称(id)] `).
    pub prefix: String,
    /// The user's real message text, without any header.
    pub content: String,
    pub timestamp: i64,
    pub request_id: Option<String>,
}

/// An outbound event routed to adapters. Either `conversation_id` (replies
/// and conversation-targeted sends) or `target` (channel-agnostic, e.g.
/// `group:551947633`) is set.
#[derive(Debug, Clone)]
pub struct OutboundEvent {
    pub conversation_id: Option<String>,
    pub target: Option<String>,
    pub content: String,
    pub request_id: Option<String>,
}

/// A permission request routed to the conversation's adapter for user
/// approval.
#[derive(Debug, Clone)]
pub struct PermissionEvent {
    pub conversation_id: String,
    pub permission_id: String,
    pub prompt: String,
    pub parent_request_id: Option<String>,
}

/// Everything an adapter can receive from its conversations.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    Outbound(OutboundEvent),
    Permission(PermissionEvent),
}

/// Routes messages by conversation: persona inboxes for inbound, adapter
/// outboxes for outbound and permissions.
pub struct ConversationManager {
    persona_inboxes: Mutex<HashMap<String, UnboundedSender<InboundMessage>>>,
    adapter_outboxes: Mutex<HashMap<String, Vec<UnboundedSender<AdapterEvent>>>>,
}

impl ConversationManager {
    pub fn new() -> Self {
        Self {
            persona_inboxes: Mutex::new(HashMap::new()),
            adapter_outboxes: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for ConversationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationManager {
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

    /// Deliver an inbound chat message to the target persona.
    pub async fn deliver(
        &self,
        conversation: &Conversation,
        sender: &str,
        prefix: &str,
        content: &str,
        request_id: Option<String>,
    ) {
        let msg = InboundMessage {
            conversation: conversation.clone(),
            sender: sender.to_string(),
            prefix: prefix.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            request_id,
        };
        // DEBUG：记录流入 persona 的消息内容，方便排查对话里到底发生了什么
        log::debug!(
            "[in] conversation '{}' -> persona '{}' from '{}': {}{}",
            conversation.conversation_id,
            conversation.persona,
            sender,
            prefix,
            content
        );
        let inbox = self
            .persona_inboxes
            .lock()
            .unwrap()
            .get(&conversation.persona)
            .cloned();
        if let Some(tx) = inbox {
            let _ = tx.send(msg);
        } else {
            log::warn!(
                "no inbox for persona '{}'; message dropped",
                conversation.persona
            );
        }
    }

    /// Persona replies / sends: route to the conversation's adapter, or
    /// broadcast to every adapter when only a channel-agnostic target is
    /// known.
    pub async fn route_outbound(
        &self,
        conversation_id: Option<&str>,
        target: Option<&str>,
        content: &str,
        request_id: Option<String>,
    ) {
        let event = AdapterEvent::Outbound(OutboundEvent {
            conversation_id: conversation_id.map(str::to_string),
            target: target.map(str::to_string),
            content: content.to_string(),
            request_id,
        });
        // DEBUG：记录 persona 发出的消息内容（回复 / send_message / 命令回执）
        log::debug!(
            "[out] conversation '{}' target '{}': {}",
            conversation_id.unwrap_or("-"),
            target.unwrap_or("-"),
            content
        );
        let outboxes = self.adapter_outboxes.lock().unwrap();
        match conversation_id {
            Some(cid) => {
                if let Some(channels) = outboxes.get(adapter_prefix(cid)) {
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

    /// Route a permission request to the conversation's adapter.
    pub async fn send_permission(
        &self,
        conversation_id: &str,
        permission_id: &str,
        prompt: &str,
        parent_request_id: Option<String>,
    ) {
        let event = AdapterEvent::Permission(PermissionEvent {
            conversation_id: conversation_id.to_string(),
            permission_id: permission_id.to_string(),
            prompt: prompt.to_string(),
            parent_request_id,
        });
        let outboxes = self.adapter_outboxes.lock().unwrap();
        if let Some(channels) = outboxes.get(adapter_prefix(conversation_id)) {
            for tx in channels {
                let _ = tx.send(event.clone());
            }
        }
    }
}
