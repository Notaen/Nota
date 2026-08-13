//! Bridges OneBot events onto the persona event bus.
//!
//! Each chat endpoint (a friend, a group) maps to one conversation
//! **session** (`onebot_private_<qq>` / `onebot_group_<qq>`). Incoming
//! messages carry the session id on the bus; the persona's automatic reply
//! and its explicit `reply`/`send_*` tool messages are routed back through
//! the same session id. Permission requests cannot be approved over OneBot
//! yet, so they are auto-denied with a notice instead of leaving the persona
//! hanging.

use std::collections::HashMap;
use std::sync::Arc;

use nota_core::bus::{BusEvent, EventBus, EventKind};
use nota_core::permissions::PermissionRegistry;
use nota_core::tool::ToolRegistry;
use tokio::sync::mpsc::{self, UnboundedSender};
use uuid::Uuid;

use crate::config::OnebotConfig;
use crate::api::OneBotApi;
use crate::client::{self, PendingResponses};
use crate::tools::{
    GetLoginInfoTool, GetMsgTool, ReadGroupChatTool, ReplyTool, SendGroupMsgTool,
    SendPrivateMsgTool, SkipReplyTool,
};
use crate::types::{
    ActionRequest, MessageEvent, PostEvent, ReplyRoute, chunk_text, identity,
};

/// Max characters per outbound QQ message.
const MAX_MESSAGE_CHARS: usize = 4000;

pub struct OneBotBridge {
    bus: Arc<EventBus>,
    permissions: Arc<PermissionRegistry>,
    persona: String,
    cfg: OnebotConfig,
    api: OneBotApi,
    action_rx: Option<mpsc::UnboundedReceiver<ActionRequest>>,
}

/// Shared handle for persona-initiated sends (replies and proactive
/// messages). Enforces the allowlist before any action reaches the socket.
#[derive(Clone)]
pub struct Outbound {
    pub(crate) action_tx: UnboundedSender<ActionRequest>,
    pub(crate) cfg: OnebotConfig,
}

impl Outbound {
    pub fn send_private(&self, user_id: i64, text: &str) -> anyhow::Result<()> {
        if !self.cfg.friend_ids.contains(&user_id) {
            anyhow::bail!("target user {user_id} is not in the friend allowlist");
        }
        for chunk in chunk_text(text, MAX_MESSAGE_CHARS) {
            self.action_tx
                .send(ActionRequest::send_private_msg(user_id, &chunk))?;
        }
        Ok(())
    }

    pub fn send_group(&self, group_id: i64, text: &str) -> anyhow::Result<()> {
        if !self.cfg.group_ids.contains(&group_id) {
            anyhow::bail!("target group {group_id} is not in the group allowlist");
        }
        for chunk in chunk_text(text, MAX_MESSAGE_CHARS) {
            self.action_tx
                .send(ActionRequest::send_group_msg(group_id, &chunk))?;
        }
        Ok(())
    }
}

impl OneBotBridge {
    pub fn new(
        bus: Arc<EventBus>,
        permissions: Arc<PermissionRegistry>,
        persona: String,
        cfg: OnebotConfig,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let pending: PendingResponses = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let api = OneBotApi::new(action_tx, pending);
        Self {
            bus,
            permissions,
            persona,
            cfg,
            api,
            action_rx: Some(action_rx),
        }
    }

    /// Correlated action API, used to register OneBot tools (e.g. reading
    /// group history) before the bridge is spawned.
    pub fn api(&self) -> OneBotApi {
        self.api.clone()
    }

    /// Shared handle for persona-initiated sends (reply / proactive tools).
    pub fn outbound(&self) -> Outbound {
        Outbound {
            action_tx: self.api.sender(),
            cfg: self.cfg.clone(),
        }
    }

    /// Register every OneBot tool into `registry`. The CLI never touches the
    /// individual tool types.
    pub fn register_tools(&self, registry: &dyn ToolRegistry) {
        registry.register(Arc::new(ReadGroupChatTool::new(self.api())));
        registry.register(Arc::new(GetLoginInfoTool::new(self.api())));
        registry.register(Arc::new(GetMsgTool::new(self.api())));
        registry.register(Arc::new(ReplyTool));
        registry.register(Arc::new(SendPrivateMsgTool));
        registry.register(Arc::new(SendGroupMsgTool));
        registry.register(Arc::new(SkipReplyTool));
    }

    /// Run the bridge forever: forward OneBot events to the bus and route
    /// persona replies / permission results back to OneBot.
    pub async fn run(mut self) {
        let mut bus_rx = self.bus.subscribe();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let action_rx = self
            .action_rx
            .take()
            .expect("bridge action receiver present");
        let action_tx = self.api.sender();
        let pending = self.api.pending();

        let ws_cfg = self.cfg.clone();
        tokio::spawn(async move {
            client::run_ws_loop(ws_cfg, event_tx, action_rx, pending).await;
        });

        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    self.handle_onebot_event(event).await;
                }
                Some(bus_event) = bus_rx.recv() => {
                    self.handle_bus_event(bus_event, &action_tx).await;
                }
                else => break,
            }
        }
        log::info!("OneBot bridge stopped");
    }

    async fn handle_onebot_event(&self, event: PostEvent) {
        let PostEvent::Message(msg) = event else {
            return;
        };

        // Never answer our own messages.
        if msg.user_id == msg.self_id {
            return;
        }

        // Entry gate: content from non-allowlisted chats must never reach the
        // persona, the LLM, or the bus. Nothing below runs for them.
        if !self.is_allowed(&msg) {
            log::debug!(
                "OneBot ignored message from non-allowlisted chat (type={}, user_id={})",
                msg.message_type,
                msg.user_id
            );
            return;
        }

        let Some(content) = msg.message else { return };
        let mut text = content.to_text();
        if text.trim().is_empty() {
            return;
        }

        if !self.cfg.prefix.is_empty() {
            if !text.starts_with(&self.cfg.prefix) {
                return;
            }
            text = text[self.cfg.prefix.len()..].trim_start().to_string();
        }

        let session_id = match msg.message_type.as_str() {
            "private" => {
                text = format!(
                    "[私聊 {} → bot({})] {text}",
                    identity(msg.sender.as_ref(), msg.user_id),
                    msg.self_id
                );
                format!("onebot_private_{}", msg.user_id)
            }
            "group" => {
                let Some(group_id) = msg.group_id else { return };
                text = format!(
                    "[群 {group_id} {} → bot({})] {text}",
                    identity(msg.sender.as_ref(), msg.user_id),
                    msg.self_id
                );
                format!("onebot_group_{group_id}")
            }
            _ => return,
        };

        let request_id = Uuid::new_v4().to_string();
        log::info!("OneBot -> {}: {text}", self.persona);
        self.bus.send(
            BusEvent::targeted_message(
                "user".to_string(),
                text,
                Some(request_id),
                self.persona.clone(),
            )
            .with_session(Some(session_id)),
        );
    }

    /// Whether a message event comes from an allowlisted chat. Private
    /// messages are matched against `friend_ids`, group messages against
    /// `group_ids`; empty allowlist = nobody is allowed.
    fn is_allowed(&self, msg: &MessageEvent) -> bool {
        match msg.message_type.as_str() {
            "private" => self.cfg.friend_ids.contains(&msg.user_id),
            "group" => msg
                .group_id
                .is_some_and(|g| self.cfg.group_ids.contains(&g)),
            _ => false,
        }
    }

    async fn handle_bus_event(
        &self,
        event: BusEvent,
        action_tx: &UnboundedSender<ActionRequest>,
    ) {
        if event.sender != self.persona {
            return;
        }

        // Only events addressed to a OneBot session are handled here; other
        // channels (web, future adapters) route their own sessions.
        let Some(session_id) = event.session_id.as_deref() else { return };
        let Some(route) = session_to_route(session_id) else { return };

        match event.kind {
            EventKind::Message => {
                // Automatic reply: the persona's final text is delivered back
                // to the originating session. The allowlist is enforced in
                // `Outbound`.
                let outbound = self.outbound_for(action_tx);
                if let Err(e) = send_via(&outbound, &route, &event.content) {
                    log::warn!("OneBot reply suppressed: {e:#}");
                }
            }
            EventKind::PermissionRequest => {
                let Some(_parent) = event.parent_request_id else { return };
                // OneBot has no interactive approval channel yet: deny so the
                // tool call fails fast instead of hanging the persona loop.
                self.permissions
                    .resolve(&event.request_id.unwrap_or_default(), false)
                    .await;
                let notice = format!(
                    "需要你授权：{}\nOneBot 通道暂不支持在线授权，已自动拒绝。",
                    event.content
                );
                self.send_reply(action_tx, &route, &notice);
            }
            EventKind::OutboundMessage => {
                // The persona asked to send a message explicitly; the target
                // rides in the session id and the allowlist is enforced in
                // `Outbound`.
                let outbound = self.outbound_for(action_tx);
                if let Err(e) = send_via(&outbound, &route, &event.content) {
                    log::warn!("OneBot outbound send failed: {e:#}");
                }
            }
        }
    }

    /// Send `text` to `route` if the chat is allowlisted. Returns whether the
    /// message was actually sent.
    fn send_reply(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        route: &ReplyRoute,
        text: &str,
    ) -> bool {
        let outbound = self.outbound_for(action_tx);
        match send_via(&outbound, route, text) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("OneBot reply suppressed: {e}");
                false
            }
        }
    }
}

/// Build an outbound handle bound to a given action channel (used by tests
/// and the bridge loop alike).
impl OneBotBridge {
    fn outbound_for(&self, action_tx: &UnboundedSender<ActionRequest>) -> Outbound {
        Outbound {
            action_tx: action_tx.clone(),
            cfg: self.cfg.clone(),
        }
    }
}

/// Map a OneBot session id back to its reply route.
fn session_to_route(session_id: &str) -> Option<ReplyRoute> {
    let rest = session_id.strip_prefix("onebot_")?;
    let (kind, id) = rest.split_once('_')?;
    let id: i64 = id.parse().ok()?;
    match kind {
        "private" => Some(ReplyRoute::Private { user_id: id }),
        "group" => Some(ReplyRoute::Group { group_id: id }),
        _ => None,
    }
}

/// Send `text` to `route` through `outbound` (allowlist + chunking applied
/// inside `Outbound`).
fn send_via(
    outbound: &Outbound,
    route: &ReplyRoute,
    text: &str,
) -> anyhow::Result<()> {
    match route {
        ReplyRoute::Private { user_id } => outbound.send_private(*user_id, text),
        ReplyRoute::Group { group_id } => outbound.send_group(*group_id, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ActionParams, MessageContent, MessageEvent, Sender,
    };
    use nota_core::bus::EventBus;

    fn test_bridge() -> OneBotBridge {
        OneBotBridge::new(
            Arc::new(EventBus::new()),
            Arc::new(PermissionRegistry::new()),
            "bob".to_string(),
            OnebotConfig {
                enabled: true,
                mode: "ws".to_string(),
                ws_url: "ws://127.0.0.1:3001".to_string(),
                access_token: String::new(),
                persona: "bob".to_string(),
                prefix: String::new(),
                friend_ids: Vec::new(),
                group_ids: Vec::new(),
            },
        )
    }

    fn private_event(user_id: i64, text: &str) -> PostEvent {
        PostEvent::Message(MessageEvent {
            self_id: 7,
            time: 1700000000,
            message_id: 1,
            message_type: "private".to_string(),
            sub_type: Some("friend".to_string()),
            user_id,
            message: Some(MessageContent::Text(text.to_string())),
            group_id: None,
            sender: Some(Sender {
                user_id,
                nickname: "Alice".to_string(),
                card: None,
            }),
        })
    }

    #[tokio::test]
    async fn forwards_private_message_to_persona() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut bus_rx = bridge.bus.subscribe();

        bridge.handle_onebot_event(private_event(42, "hello")).await;

        let event = bus_rx.recv().await.unwrap();
        assert_eq!(event.sender, "user");
        assert_eq!(event.content, "[私聊 Alice(42) → bot(7)] hello");
        assert_eq!(event.target.as_deref(), Some("bob"));
        assert_eq!(event.session_id.as_deref(), Some("onebot_private_42"));
        assert!(event.request_id.is_some());
    }

    #[tokio::test]
    async fn forwards_group_message_from_allowlisted_group() {
        let mut bridge = test_bridge();
        bridge.cfg.group_ids = vec![30003];
        let mut bus_rx = bridge.bus.subscribe();
        let event = PostEvent::Message(MessageEvent {
            self_id: 7,
            time: 1700000000,
            message_id: 4,
            message_type: "group".to_string(),
            sub_type: None,
            user_id: 42,
            message: Some(MessageContent::Text("hi".to_string())),
            group_id: Some(30003),
            sender: Some(Sender {
                user_id: 42,
                nickname: "Alice".to_string(),
                card: None,
            }),
        });

        bridge.handle_onebot_event(event).await;

        let event = bus_rx.recv().await.unwrap();
        assert_eq!(event.content, "[群 30003 Alice(42) → bot(7)] hi");
        assert_eq!(event.session_id.as_deref(), Some("onebot_group_30003"));
    }

    #[tokio::test]
    async fn ignores_private_message_from_non_allowlisted_friend() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut bus_rx = bridge.bus.subscribe();

        bridge.handle_onebot_event(private_event(99, "hi")).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv())
                .await
                .is_err(),
            "non-allowlisted message must not reach the persona (no LLM call)"
        );
    }

    #[tokio::test]
    async fn ignores_group_message_from_non_allowlisted_group() {
        let mut bridge = test_bridge();
        bridge.cfg.group_ids = vec![30003];
        let mut bus_rx = bridge.bus.subscribe();
        let event = PostEvent::Message(MessageEvent {
            self_id: 7,
            time: 1700000000,
            message_id: 2,
            message_type: "group".to_string(),
            sub_type: None,
            user_id: 42,
            message: Some(MessageContent::Text("hi".to_string())),
            group_id: Some(99999),
            sender: None,
        });

        bridge.handle_onebot_event(event).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ignores_all_when_allowlists_are_empty() {
        let bridge = test_bridge(); // friend_ids / group_ids are both empty
        let mut bus_rx = bridge.bus.subscribe();
        let event = PostEvent::Message(MessageEvent {
            self_id: 7,
            time: 1700000000,
            message_id: 3,
            message_type: "group".to_string(),
            sub_type: None,
            user_id: 42,
            message: Some(MessageContent::Text("hi".to_string())),
            group_id: Some(30003),
            sender: None,
        });

        bridge.handle_onebot_event(event).await;
        bridge.handle_onebot_event(private_event(42, "hi")).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv())
                .await
                .is_err(),
            "empty allowlists must ignore everyone"
        );
    }

    #[tokio::test]
    async fn auto_routes_persona_reply_by_session() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::message(
            "bob".to_string(),
            "hi".to_string(),
            Some("r1".to_string()),
        )
        .with_session(Some("onebot_private_42".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        let action = action_rx.recv().await.unwrap();
        assert_eq!(action.action, "send_private_msg");
        let ActionParams::Private { user_id, message } = action.params else {
            panic!("expected private action");
        };
        assert_eq!(user_id, 42);
        assert_eq!(message[0].data.text, "hi");
    }

    #[tokio::test]
    async fn ignores_replies_for_other_channels() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::message(
            "bob".to_string(),
            "hi".to_string(),
            Some("r1".to_string()),
        )
        .with_session(Some("web_abc".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err(),
            "non-OneBot sessions are routed by their own channel"
        );
    }

    #[tokio::test]
    async fn auto_denies_permission_request() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        let (permission_id, decision) = bridge.permissions.register().await;
        let event = BusEvent::permission_request(
                    "bob".to_string(),
                    "Allow file_read on /etc/passwd?".to_string(),
                    permission_id,
                    Some("p1".to_string()),
        )
        .with_session(Some("onebot_private_42".to_string()));
        bridge.handle_bus_event(event, &action_tx).await;

        // Denied, so the waiting tool resumes with false.
        assert!(!decision.await.unwrap());

        // The chat receives a notice explaining the denial.
        let action = action_rx.recv().await.unwrap();
        let ActionParams::Private { message, .. } = action.params else {
            panic!("expected private action");
        };
        assert!(message[0].data.text.contains("需要你授权"));
    }

    #[tokio::test]
    async fn suppresses_reply_to_non_allowlisted_friend() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::message(
            "bob".to_string(),
            "hi".to_string(),
            Some("r1".to_string()),
        )
        .with_session(Some("onebot_private_99".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err(),
            "no message may be sent to a non-allowlisted friend"
        );
    }

    #[tokio::test]
    async fn suppresses_reply_to_non_allowlisted_group() {
        let mut bridge = test_bridge();
        bridge.cfg.group_ids = vec![30003];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::message(
            "bob".to_string(),
            "hi".to_string(),
            Some("r2".to_string()),
        )
        .with_session(Some("onebot_group_99999".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn forwards_outbound_message_by_session() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::outbound_message(
            "bob".to_string(),
            "hi".to_string(),
            None,
            String::new(),
        )
        .with_session(Some("onebot_private_42".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        let action = action_rx.recv().await.unwrap();
        assert_eq!(action.action, "send_private_msg");
    }
}
