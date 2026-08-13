use std::sync::RwLock;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// An inbound chat message for a persona to answer.
    Message,
    /// A permission decision is required for a tool call.
    PermissionRequest,
    /// A persona explicitly asks to send a message outbound (channel target
    /// encoded in `context`). Channels subscribe and forward it.
    OutboundMessage,
}

#[derive(Debug, Clone)]
pub struct BusEvent {
    pub kind: EventKind,
    pub sender: String,
    pub content: String,
    pub timestamp: i64,
    pub context: String,
    /// The conversation session this event belongs to (adapter-assigned,
    /// e.g. `onebot_private_2961354039`); `None` for channel-agnostic events.
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub parent_request_id: Option<String>,
    pub target: Option<String>,
}

impl BusEvent {
    /// Attach the conversation session this event belongs to.
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn message(
        sender: String,
        content: String,
        request_id: Option<String>,
    ) -> Self {
        Self::message_with_context(sender, content, request_id, String::new())
    }

    /// Same as [`BusEvent::message`] but carries the inbound routing context
    /// (e.g. the chat identifier of the originating channel) so the reply can
    /// be delivered back through the same adapter.
    pub fn message_with_context(
        sender: String,
        content: String,
        request_id: Option<String>,
        context: String,
    ) -> Self {
        Self {
            kind: EventKind::Message,
            sender,
            content,
            timestamp: chrono::Utc::now().timestamp(),
            context,
            session_id: None,
            request_id,
            parent_request_id: None,
            target: None,
        }
    }

    pub fn targeted_message(
        sender: String,
        content: String,
        request_id: Option<String>,
        target: String,
    ) -> Self {
        Self {
            kind: EventKind::Message,
            sender,
            content,
            timestamp: chrono::Utc::now().timestamp(),
            context: String::new(),
            session_id: None,
            request_id,
            parent_request_id: None,
            target: Some(target),
        }
    }

    /// Like [`BusEvent::targeted_message`] but also carries the inbound
    /// routing context (e.g. the originating OneBot chat) so the persona can
    /// reply through the same channel.
    pub fn targeted_message_with_context(
        sender: String,
        content: String,
        request_id: Option<String>,
        target: String,
        context: String,
    ) -> Self {
        Self {
            kind: EventKind::Message,
            sender,
            content,
            timestamp: chrono::Utc::now().timestamp(),
            context,
            session_id: None,
            request_id,
            parent_request_id: None,
            target: Some(target),
        }
    }

    pub fn permission_request(
        sender: String,
        prompt: String,
        permission_id: String,
        parent_request_id: Option<String>,
    ) -> Self {
        Self {
            kind: EventKind::PermissionRequest,
            sender,
            content: prompt,
            timestamp: chrono::Utc::now().timestamp(),
            context: String::new(),
            session_id: None,
            request_id: Some(permission_id),
            parent_request_id,
            target: None,
        }
    }

    /// A persona-initiated outbound message. `context` carries the channel
    /// target (e.g. `"private:2961354039"`), so the sending persona does not
    /// touch the transport directly.
    pub fn outbound_message(
        sender: String,
        content: String,
        request_id: Option<String>,
        context: String,
    ) -> Self {
        Self {
            kind: EventKind::OutboundMessage,
            sender,
            content,
            timestamp: chrono::Utc::now().timestamp(),
            context,
            session_id: None,
            request_id,
            parent_request_id: None,
            target: None,
        }
    }
}

pub struct EventBus {
    senders: RwLock<Vec<UnboundedSender<BusEvent>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> UnboundedReceiver<BusEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.write().unwrap().push(tx);
        rx
    }

    pub fn subscribe_with_sender(&self, tx: UnboundedSender<BusEvent>) {
        self.senders.write().unwrap().push(tx);
    }

    pub fn send(&self, event: BusEvent) {
        let senders = self.senders.read().unwrap();
        for tx in senders.iter() {
            let _ = tx.send(event.clone());
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
