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
use std::sync::Mutex;
use std::sync::Arc;

use nota_core::bus::{BusEvent, EventBus, EventKind};
use nota_core::permissions::PermissionRegistry;
use nota_core::persona::ChatLogEntry;
use nota_core::session::{Session, SessionStore};
use nota_core::tool::ToolRegistry;
use tokio::sync::mpsc::{self, UnboundedSender};
use uuid::Uuid;

use crate::config::OnebotConfig;
use crate::api::OneBotApi;
use crate::client::{self, PendingResponses};
use crate::tools::{GetLoginInfoTool, GetMsgTool, ReadGroupChatTool};
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
    sessions: Arc<dyn SessionStore>,
    /// Per-source-session queue of pending outbound approval ids, in order.
    pending_approvals: Mutex<HashMap<String, Vec<String>>>,
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
        self.send_private_approved(user_id, text)
    }

    /// Send without the allowlist check — only used after the user approved
    /// an outbound message to a non-allowlisted target.
    pub(crate) fn send_private_approved(
        &self,
        user_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
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
        self.send_group_approved(group_id, text)
    }

    /// Send without the allowlist check — only used after the user approved
    /// an outbound message to a non-allowlisted target.
    pub(crate) fn send_group_approved(
        &self,
        group_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
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
        sessions: Arc<dyn SessionStore>,
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
            sessions,
            pending_approvals: Mutex::new(HashMap::new()),
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

        // Approve / deny commands for pending outbound messages are handled
        // here and never reach the persona.
        let raw_text = msg
            .message
            .as_ref()
            .map(|m| m.to_text())
            .unwrap_or_default();
        if let Some((approved, seq)) = parse_approval(&raw_text) {
            if let Some(session_id) = session_id_for(&msg)
                && let Some(permission_id) = self.take_pending_approval(&session_id, seq)
            {
                log::info!(
                    "OneBot approval {permission_id} (approved={approved}) from {session_id}"
                );
                self.permissions.resolve(&permission_id, approved).await;
            }
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

        match event.kind {
            EventKind::Message => {
                // Automatic reply: the persona's final text is delivered back
                // to the originating session. The allowlist is enforced in
                // `Outbound`.
                let Some(session_id) = event.session_id.as_deref() else { return };
                let Some(route) = session_to_route(session_id) else { return };
                let outbound = self.outbound_for(action_tx);
                match send_via(&outbound, &route, &event.content) {
                    Ok(()) => self.record_shallow(session_id, &event.content).await,
                    Err(e) => log::warn!("OneBot reply suppressed: {e:#}"),
                }
            }
            EventKind::PermissionRequest => {
                let Some(session_id) = event.session_id.as_deref() else { return };
                let Some(route) = session_to_route(session_id) else { return };
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
                // The persona asked to send a message explicitly. `context`
                // carries the adapter-independent target
                // (`private:<QQ>` / `group:<QQ>`); allowlisted targets are
                // delivered immediately, everything else goes through an
                // approval round-trip with the user.
                let Some(route) = target_to_route(&event.context) else {
                    log::warn!(
                        "OneBot outbound event has no usable target: '{}'",
                        event.context
                    );
                    return;
                };
                let outbound = self.outbound_for(action_tx);
                if self.route_allowed(&route) {
                    match send_via(&outbound, &route, &event.content) {
                        Ok(()) => {
                            self.record_shallow(
                                &route_to_session_id(&route),
                                &event.content,
                            )
                            .await;
                        }
                        Err(e) => log::warn!("OneBot outbound send failed: {e:#}"),
                    }
                    return;
                }
                self.request_outbound_approval(action_tx, &event, &route)
                    .await;
            }
        }
    }

    /// Whether the route is inside the configured allowlist.
    fn route_allowed(&self, route: &ReplyRoute) -> bool {
        match route {
            ReplyRoute::Private { user_id } => self.cfg.friend_ids.contains(user_id),
            ReplyRoute::Group { group_id } => self.cfg.group_ids.contains(group_id),
        }
    }

    /// Ask the user to approve an outbound message to a non-allowlisted
    /// target. The notice goes back to the session that started the turn;
    /// the message is sent only after approval.
    async fn request_outbound_approval(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        event: &BusEvent,
        route: &ReplyRoute,
    ) {
        let (permission_id, decision) = self.permissions.register().await;
        let source_session = event.session_id.clone().unwrap_or_default();
        let seq = {
            let mut queue = self.pending_approvals.lock().unwrap();
            let list = queue.entry(source_session.clone()).or_default();
            list.push(permission_id.clone());
            list.len()
        };
        let notice = if seq == 1 {
            format!(
                "persona 想向{}发送消息：{}\n回复「同意」批准，或「拒绝」拒绝\n（权限ID：{permission_id}）",
                describe_target(route),
                event.content
            )
        } else {
            format!(
                "persona 想向{}发送消息：{}\n这是第 {seq} 个待处理请求，回复「同意{seq}」或「拒绝{seq}」\n（权限ID：{permission_id}）",
                describe_target(route),
                event.content
            )
        };

        // Notify the session that started this turn (QQ via a reply, other
        // channels via a bus message event carrying the source session).
        match event.session_id.as_deref().and_then(session_to_route) {
            Some(source_route) => {
                self.send_reply(action_tx, &source_route, &notice);
            }
            None => {
                self.bus.send(
                    BusEvent::message("system".to_string(), notice, None)
                        .with_session(event.session_id.clone()),
                );
            }
        }

        // Wait for the decision; approved sends bypass the allowlist (the
        // user's explicit approval is the gate).
        let action_tx = action_tx.clone();
        let cfg = self.cfg.clone();
        let sessions = self.sessions.clone();
        let persona = self.persona.clone();
        let route = route.clone();
        let content = event.content.clone();
        tokio::spawn(async move {
            if !decision.await.unwrap_or(false) {
                log::info!(
                    "OneBot outbound message denied by user: {}",
                    route_to_session_id(&route)
                );
                return;
            }
            let outbound = Outbound { action_tx, cfg };
            let result = match route {
                ReplyRoute::Private { user_id } => {
                    outbound.send_private_approved(user_id, &content)
                }
                ReplyRoute::Group { group_id } => {
                    outbound.send_group_approved(group_id, &content)
                }
            };
            if let Err(e) = result {
                log::warn!("OneBot outbound send failed after approval: {e:#}");
                return;
            }
            if let Err(e) = sessions
                .append_shallow(
                    &Session::new(persona.clone(), route_to_session_id(&route)),
                    &[ChatLogEntry {
                        sender: persona,
                        content,
                        timestamp: chrono::Utc::now().timestamp(),
                        context: String::new(),
                    }],
                )
                .await
            {
                log::warn!("failed to record shallow message: {e:#}");
            }
        });
    }

    /// Take the next pending approval id for a source session. `seq` is the
    /// 1-based position in the queue (`None` or `1` = the first/only one).
    fn take_pending_approval(
        &self,
        session_id: &str,
        seq: Option<usize>,
    ) -> Option<String> {
        let mut queue = self.pending_approvals.lock().unwrap();
        let list = queue.get_mut(session_id)?;
        let index = match seq {
            None | Some(1) if !list.is_empty() => 0,
            Some(n) if n >= 1 && n <= list.len() => n - 1,
            _ => return None,
        };
        let id = list.remove(index);
        if list.is_empty() {
            queue.remove(session_id);
        }
        Some(id)
    }

    /// Record an actually-delivered message in the session's shallow layer
    /// (what the user saw).
    async fn record_shallow(&self, session_id: &str, content: &str) {
        let entry = ChatLogEntry {
            sender: self.persona.clone(),
            content: content.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            context: String::new(),
        };
        if let Err(e) = self
            .sessions
            .append_shallow(&Session::new(self.persona.clone(), session_id), &[entry])
            .await
        {
            log::warn!("failed to record shallow message: {e:#}");
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

/// The session id for an incoming OneBot message.
fn session_id_for(msg: &MessageEvent) -> Option<String> {
    match msg.message_type.as_str() {
        "private" => Some(format!("onebot_private_{}", msg.user_id)),
        "group" => msg.group_id.map(|g| format!("onebot_group_{g}")),
        _ => None,
    }
}

/// Map an adapter-independent target (`private:<QQ>` / `group:<QQ>`) to a
/// OneBot reply route.
fn target_to_route(target: &str) -> Option<ReplyRoute> {
    let (kind, id) = target.split_once(':')?;
    let id: i64 = id.parse().ok()?;
    match kind {
        "private" => Some(ReplyRoute::Private { user_id: id }),
        "group" => Some(ReplyRoute::Group { group_id: id }),
        _ => None,
    }
}

/// Human-readable description of an outbound target, e.g. `群 551947633`.
fn describe_target(route: &ReplyRoute) -> String {
    match route {
        ReplyRoute::Private { user_id } => format!("好友 {user_id}"),
        ReplyRoute::Group { group_id } => format!("群 {group_id}"),
    }
}

/// Parse an approve/deny command from a chat message:
/// `同意` / `拒绝` (optionally `同意N` / `拒绝N` for the N-th pending
/// request). `批准` is accepted as an alias of `同意`.
fn parse_approval(text: &str) -> Option<(bool, Option<usize>)> {
    for (prefix, approved) in [("同意", true), ("批准", true), ("拒绝", false)] {
        if let Some(rest) = text.trim().strip_prefix(prefix) {
            let rest = rest.trim();
            let seq = if rest.is_empty() {
                None
            } else {
                rest.parse::<usize>().ok()
            };
            if seq.is_some() || rest.is_empty() {
                return Some((approved, seq));
            }
        }
    }
    None
}

/// The OneBot session id for a reply route.
fn route_to_session_id(route: &ReplyRoute) -> String {
    match route {
        ReplyRoute::Private { user_id } => format!("onebot_private_{user_id}"),
        ReplyRoute::Group { group_id } => format!("onebot_group_{group_id}"),
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
    use anyhow::Result;
    use async_trait::async_trait;
    use crate::types::{
        ActionParams, MessageContent, MessageEvent, Sender,
    };
    use nota_core::bus::EventBus;
    use nota_core::persona::ChatLogEntry;
    use nota_core::session::{Session, SessionStore};
    use std::sync::Mutex;

    /// In-memory session store for tests.
    #[derive(Default)]
    struct MemSessionStore {
        deep: Mutex<Vec<ChatLogEntry>>,
        shallow: Mutex<Vec<ChatLogEntry>>,
    }

    #[async_trait]
    impl SessionStore for MemSessionStore {
        async fn append_deep(
            &self,
            _session: &Session,
            entries: &[ChatLogEntry],
        ) -> Result<()> {
            self.deep.lock().unwrap().extend(entries.iter().cloned());
            Ok(())
        }

        async fn read_deep(
            &self,
            _session: &Session,
            _since: Option<i64>,
        ) -> Result<Vec<ChatLogEntry>> {
            Ok(self.deep.lock().unwrap().clone())
        }

        async fn append_shallow(
            &self,
            _session: &Session,
            entries: &[ChatLogEntry],
        ) -> Result<()> {
            self.shallow.lock().unwrap().extend(entries.iter().cloned());
            Ok(())
        }

        async fn read_shallow(
            &self,
            _session: &Session,
            _since: Option<i64>,
        ) -> Result<Vec<ChatLogEntry>> {
            Ok(self.shallow.lock().unwrap().clone())
        }
    }

    fn test_bridge_with_store() -> (OneBotBridge, Arc<MemSessionStore>) {
        let store = Arc::new(MemSessionStore::default());
        let bridge = OneBotBridge::new(
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
            store.clone(),
        );
        (bridge, store)
    }

    fn test_bridge() -> OneBotBridge {
        test_bridge_with_store().0
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
        let (mut bridge, store) = test_bridge_with_store();
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

        // The delivered reply is recorded in the session's shallow layer.
        let shallow = store.shallow.lock().unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].sender, "bob");
        assert_eq!(shallow[0].content, "hi");
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
        let (mut bridge, store) = test_bridge_with_store();
        bridge.cfg.group_ids = vec![30003];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::outbound_message(
            "bob".to_string(),
            "hi".to_string(),
            None,
            "group:30003".to_string(),
        );

        bridge.handle_bus_event(event, &action_tx).await;

        let action = action_rx.recv().await.unwrap();
        assert_eq!(action.action, "send_group_msg");

        // The actual outbound message lands in the target session's shallow
        // layer even though this turn originated elsewhere.
        let shallow = store.shallow.lock().unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].content, "hi");
    }

    fn extract_permission_id(notice: &str) -> String {
        let start = notice.find("权限ID：").unwrap() + "权限ID：".len();
        notice[start..start + 36].to_string()
    }

    #[tokio::test]
    async fn outbound_to_non_allowlisted_requires_approval() {
        let (mut bridge, store) = test_bridge_with_store();
        bridge.cfg.friend_ids = vec![42];
        bridge.cfg.group_ids = vec![30003];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::outbound_message(
            "bob".to_string(),
            "hi".to_string(),
            None,
            "group:99999".to_string(),
        )
        .with_session(Some("onebot_private_42".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        // No direct send; a notice asking for approval goes to the source
        // session instead.
        let notice_action = action_rx.recv().await.unwrap();
        assert_eq!(notice_action.action, "send_private_msg");
        let ActionParams::Private { message, .. } = notice_action.params else {
            panic!("expected private action");
        };
        let notice = &message[0].data.text;
        assert!(notice.contains("群 99999"));
        assert!(notice.contains("同意"));
        assert!(notice.contains("权限ID"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err(),
            "nothing is sent before approval"
        );

        // Approve -> the message is sent (bypassing the allowlist) and
        // recorded in the target session's shallow layer.
        let id = extract_permission_id(notice);
        bridge.permissions.resolve(&id, true).await;
        let sent = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            action_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(sent.action, "send_group_msg");
        let ActionParams::Group { group_id, message } = sent.params else {
            panic!("expected group action");
        };
        assert_eq!(group_id, 99999);
        assert_eq!(message[0].data.text, "hi");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let shallow = store.shallow.lock().unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].content, "hi");
    }

    #[tokio::test]
    async fn denied_outbound_not_sent() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let event = BusEvent::outbound_message(
            "bob".to_string(),
            "hi".to_string(),
            None,
            "group:99999".to_string(),
        )
        .with_session(Some("onebot_private_42".to_string()));

        bridge.handle_bus_event(event, &action_tx).await;

        let notice_action = action_rx.recv().await.unwrap();
        let ActionParams::Private { message, .. } = notice_action.params else {
            panic!("expected private action");
        };
        let id = extract_permission_id(&message[0].data.text);
        bridge.permissions.resolve(&id, false).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), action_rx.recv())
                .await
                .is_err(),
            "denied outbound message must not be sent"
        );
    }

    #[tokio::test]
    async fn approval_command_resolves_and_is_not_forwarded() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut bus_rx = bridge.bus.subscribe();
        let (permission_id, decision) = bridge.permissions.register().await;
        {
            let mut queue = bridge.pending_approvals.lock().unwrap();
            queue
                .entry("onebot_private_42".to_string())
                .or_default()
                .push(permission_id.clone());
        }

        bridge.handle_onebot_event(private_event(42, "同意")).await;

        assert!(decision.await.unwrap());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), bus_rx.recv())
                .await
                .is_err(),
            "approval commands are consumed by the bridge, never the persona"
        );
    }

    #[tokio::test]
    async fn deny_command_resolves_false() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (permission_id, decision) = bridge.permissions.register().await;
        {
            let mut queue = bridge.pending_approvals.lock().unwrap();
            queue
                .entry("onebot_private_42".to_string())
                .or_default()
                .push(permission_id.clone());
        }

        bridge.handle_onebot_event(private_event(42, "拒绝")).await;

        assert!(!decision.await.unwrap());
    }

    #[tokio::test]
    async fn approval_seq_targets_second_pending_request() {
        let mut bridge = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        bridge
            .handle_bus_event(
                BusEvent::outbound_message(
                    "bob".to_string(),
                    "one".to_string(),
                    None,
                    "group:99999".to_string(),
                )
                .with_session(Some("onebot_private_42".to_string())),
                &action_tx,
            )
            .await;
        bridge
            .handle_bus_event(
                BusEvent::outbound_message(
                    "bob".to_string(),
                    "two".to_string(),
                    None,
                    "group:88888".to_string(),
                )
                .with_session(Some("onebot_private_42".to_string())),
                &action_tx,
            )
            .await;
        let _n1 = action_rx.recv().await.unwrap();
        let _n2 = action_rx.recv().await.unwrap();

        let ids: Vec<String> = {
            let queue = bridge.pending_approvals.lock().unwrap();
            queue.get("onebot_private_42").unwrap().clone()
        };
        assert_eq!(ids.len(), 2);

        // 同意2 approves the second pending request only.
        bridge.handle_onebot_event(private_event(42, "同意2")).await;

        assert!(
            !bridge.permissions.resolve(&ids[1], false).await,
            "second request was consumed and resolved"
        );
        assert!(
            bridge.permissions.resolve(&ids[0], false).await,
            "first request is still pending"
        );
        let remaining = {
            let queue = bridge.pending_approvals.lock().unwrap();
            queue
                .get("onebot_private_42")
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(remaining, vec![ids[0].clone()]);
    }
}
