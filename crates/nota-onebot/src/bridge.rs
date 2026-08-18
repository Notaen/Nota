//! OneBot 11 adapter bridge.
//!
//! There is no global bus anymore: the bridge subscribes to the `onebot`
//! adapter channel of the [`ConversationManager`]. Inbound OneBot messages are
//! delivered straight to the target persona's inbox; outbound events
//! (explicit `reply` / `onebot_send_msg` calls) and permission requests come
//! back through the adapter channel. The bridge owns the OneBot allowlist
//! and the approve/deny round-trip for anything leaving the allowlist.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use nota_core::conversation::{
    AdapterEvent, Conversation, ConversationManager, PermissionEvent,
};
use nota_core::permissions::PermissionRegistry;
use nota_core::tool::ToolRegistry;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::api::OneBotApi;
use crate::client::{self, PendingResponses};
use crate::config::OnebotConfig;
use crate::tools::{
    OneBotGetContentTool, OneBotGetMsgHistoryTool, OneBotSendMsgTool, OneBotStatusTool,
    OneBotVoiceTextTool,
};
use crate::types::{
    ActionRequest, MessageEvent, PostEvent, ReplyRoute, chunk_text, identity,
};

/// Max characters per outbound OneBot message.
const MAX_MESSAGE_CHARS: usize = 4000;

pub struct OneBotBridge {
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    persona: String,
    cfg: OnebotConfig,
    api: OneBotApi,
    /// Per-source-conversation queue of pending approval ids, in order.
    pending_approvals: Mutex<HashMap<String, Vec<String>>>,
    action_rx: Option<UnboundedReceiver<ActionRequest>>,
}

/// Shared handle for persona-initiated sends. Enforces the allowlist before
/// any action reaches the socket.
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
        manager: Arc<ConversationManager>,
        permissions: Arc<PermissionRegistry>,
        persona: String,
        cfg: OnebotConfig,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let pending: PendingResponses = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let connected: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let api = OneBotApi::new(action_tx, pending, connected);
        Self {
            manager,
            permissions,
            persona,
            cfg,
            api,
            pending_approvals: Mutex::new(HashMap::new()),
            action_rx: Some(action_rx),
        }
    }

    /// Correlated action API, used to register OneBot tools before the
    /// bridge is spawned.
    pub fn api(&self) -> OneBotApi {
        self.api.clone()
    }

    /// Register every OneBot tool into `registry`. Fails on a name
    /// collision so startup aborts instead of shadowing an existing tool.
    pub fn register_tools(&self, registry: &ToolRegistry) -> anyhow::Result<()> {
        registry.register(Arc::new(OneBotSendMsgTool))?;
        registry.register(Arc::new(OneBotGetMsgHistoryTool::new(self.api())))?;
        registry.register(Arc::new(OneBotGetContentTool::new(self.api())))?;
        registry.register(Arc::new(OneBotStatusTool::new(
            self.api(),
            self.cfg.ws_url.clone(),
        )))?;
        registry.register(Arc::new(OneBotVoiceTextTool::new(self.api())))?;
        Ok(())
    }

    /// Run the bridge forever: deliver inbound events to the persona inbox
    /// and consume outbound / permission events for this adapter.
    pub async fn run(mut self) {
        let mut adapter_rx = self.manager.subscribe_adapter("onebot");
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let action_rx = self
            .action_rx
            .take()
            .expect("bridge action receiver present");
        let action_tx = self.api.sender();
        let pending = self.api.pending();
        let connected = self.api.connected();

        let ws_cfg = self.cfg.clone();
        tokio::spawn(async move {
            client::run_ws_loop(ws_cfg, event_tx, action_rx, pending, connected).await;
        });

        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    self.handle_onebot_event(event).await;
                }
                Some(adapter_event) = adapter_rx.recv() => {
                    self.handle_adapter_event(adapter_event, &action_tx).await;
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

        // Approve / deny commands for pending approvals are handled here and
        // never reach the persona.
        let raw_text = msg
            .message
            .as_ref()
            .map(|m| m.to_text())
            .unwrap_or_default();
        if let Some((approved, seq)) = parse_approval(&raw_text) {
        if let Some(conversation_id) = conversation_id_for(&msg)
            && let Some(permission_id) =
                self.take_pending_approval(&conversation_id, seq)
            {
                log::info!(
                    "OneBot approval {permission_id} (approved={approved}) from {conversation_id}"
                );
                self.permissions.resolve(&permission_id, approved).await;
            }
            return;
        }

        let Some(content) = msg.message else { return };
        // 非文本段统一渲染为 [{segment_type} msg id:<id>]，persona 需要时
        // 自己调用工具（如 onebot_voice_text / onebot_get_content）获取内容，
        // 不在桥接层自动处理。
        let mut text = content.to_text_with_id(&msg.message_id.to_string());
        if text.trim().is_empty() {
            return;
        }

        if !self.cfg.prefix.is_empty() {
            if !text.starts_with(&self.cfg.prefix) {
                return;
            }
            text = text[self.cfg.prefix.len()..].trim_start().to_string();
        }

        // The identity header is carried separately as `prefix`; `text` stays
        // the user's real content (so slash commands reach the conversation
        // manager verbatim).
        let (conversation_id, prefix) = match msg.message_type.as_str() {
            "private" => {
                let prefix = format!(
                    "[好友 {}] ",
                    identity(msg.sender.as_ref(), msg.user_id)
                );
                (format!("onebot_private_{}", msg.user_id), prefix)
            }
            "group" => {
                let Some(group_id) = msg.group_id else { return };
                let prefix = format!(
                    "[群 {group_id} {}] ",
                    identity(msg.sender.as_ref(), msg.user_id)
                );
                (format!("onebot_group_{group_id}"), prefix)
            }
            _ => return,
        };

        let request_id = Uuid::new_v4().to_string();
        log::info!("OneBot -> {}: {text}", self.persona);
        self.manager
            .deliver(
                &Conversation::new(self.persona.clone(), conversation_id),
                "user",
                &prefix,
                &text,
                Some(request_id),
            )
            .await;
    }

    /// Whether a message event comes from an allowlisted chat.
    fn is_allowed(&self, msg: &MessageEvent) -> bool {
        match msg.message_type.as_str() {
            "private" => self.cfg.friend_ids.contains(&msg.user_id),
            "group" => msg
                .group_id
                .is_some_and(|g| self.cfg.group_ids.contains(&g)),
            _ => false,
        }
    }

    async fn handle_adapter_event(
        &self,
        event: AdapterEvent,
        action_tx: &UnboundedSender<ActionRequest>,
    ) {
        match event {
            AdapterEvent::Outbound(e) => {
                if let Some(target) = e.target.as_deref() {
                    // onebot_send_msg: `target` is the destination,
                    // `conversation_id` is the source for the approval notice.
                    let Some(route) = target_to_route(target) else {
                        log::warn!("OneBot outbound event has no usable target: '{target}'");
                        return;
                    };
                    if self.route_allowed(&route) {
                        if let Err(err) = self.send_route(action_tx, &route, &e.content) {
                            log::warn!("OneBot outbound send failed: {err:#}");
                        }
                    } else {
                        self.request_outbound_approval(
                            action_tx,
                            e.conversation_id.as_deref(),
                            &route,
                            &e.content,
                        )
                        .await;
                    }
                } else if let Some(cid) = e.conversation_id.as_deref() {
                    // Explicit reply to the current conversation
                    // (`reply`): the conversation is the destination.
                    let Some(route) = conversation_to_route(cid) else { return };
                    if self.route_allowed(&route) {
                        if let Err(err) = self.send_route(action_tx, &route, &e.content) {
                            log::warn!("OneBot reply suppressed: {err:#}");
                        }
                    } else {
                        self.request_outbound_approval(action_tx, Some(cid), &route, &e.content)
                            .await;
                    }
                }
            }
            AdapterEvent::Permission(p) => {
                self.request_permission_approval(action_tx, &p).await;
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

    fn send_route(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        route: &ReplyRoute,
        text: &str,
    ) -> anyhow::Result<()> {
        let outbound = self.outbound_for(action_tx);
        send_via(&outbound, route, text)
    }

    /// Ask the user to approve an outbound message to a non-allowlisted
    /// target. Only OneBot sources can be asked; other channels handle their
    /// own approvals (or drop).
    async fn request_outbound_approval(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        source_conversation: Option<&str>,
        route: &ReplyRoute,
        content: &str,
    ) {
        let Some(source_route) = source_conversation.and_then(conversation_to_route) else {
            log::warn!("outbound approval request dropped: source is not an OneBot conversation");
            return;
        };

        let (permission_id, decision) = self.permissions.register().await;
        let source_conversation = source_conversation.unwrap_or_default().to_string();
        let seq = self.push_pending(&source_conversation, permission_id.clone());
        let notice = if seq == 1 {
            format!(
                "persona 想向{}发送消息：{}\n回复「同意」批准，或「拒绝」拒绝\n（权限ID：{permission_id}）",
                describe_target(route),
                content
            )
        } else {
            format!(
                "persona 想向{}发送消息：{}\n这是第 {seq} 个待处理请求，回复「同意{seq}」或「拒绝{seq}」\n（权限ID：{permission_id}）",
                describe_target(route),
                content
            )
        };
        self.send_reply(action_tx, &source_route, &notice);

        // Wait for the decision; approved sends bypass the allowlist (the
        // user's explicit approval is the gate).
        let action_tx = action_tx.clone();
        let cfg = self.cfg.clone();
        let route = route.clone();
        let content = content.to_string();
        tokio::spawn(async move {
            if !decision.await.unwrap_or(false) {
                log::info!("OneBot outbound message denied by user");
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
            }
        });
    }

    /// Ask the user to approve a tool permission request routed to a OneBot
    /// conversation (e.g. file access outside the workspace).
    async fn request_permission_approval(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        p: &PermissionEvent,
    ) {
        let Some(route) = conversation_to_route(&p.conversation_id) else {
            log::warn!("permission request dropped: not an OneBot conversation");
            return;
        };
        let seq = self.push_pending(&p.conversation_id, p.permission_id.clone());
        let notice = if seq == 1 {
            format!(
                "需要你授权：{}\n回复「同意」批准，或「拒绝」拒绝\n（权限ID：{}）",
                p.prompt, p.permission_id
            )
        } else {
            format!(
                "需要你授权：{}\n这是第 {seq} 个待处理请求，回复「同意{seq}」或「拒绝{seq}」\n（权限ID：{}）",
                p.prompt, p.permission_id
            )
        };
        self.send_reply(action_tx, &route, &notice);
    }

    fn push_pending(&self, conversation_id: &str, permission_id: String) -> usize {
        let mut queue = self.pending_approvals.lock().unwrap();
        let list = queue.entry(conversation_id.to_string()).or_default();
        list.push(permission_id);
        list.len()
    }

    /// Take the next pending approval id for a source conversation. `seq` is the
    /// 1-based position in the queue (`None` or `1` = the first/only one).
    fn take_pending_approval(
        &self,
        conversation_id: &str,
        seq: Option<usize>,
    ) -> Option<String> {
        let mut queue = self.pending_approvals.lock().unwrap();
        let list = queue.get_mut(conversation_id)?;
        let index = match seq {
            None | Some(1) if !list.is_empty() => 0,
            Some(n) if n >= 1 && n <= list.len() => n - 1,
            _ => return None,
        };
        let id = list.remove(index);
        if list.is_empty() {
            queue.remove(conversation_id);
        }
        Some(id)
    }

    /// Send `text` to `route` if the chat is allowlisted.
    fn send_reply(
        &self,
        action_tx: &UnboundedSender<ActionRequest>,
        route: &ReplyRoute,
        text: &str,
    ) -> bool {
        match self.send_route(action_tx, route, text) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("OneBot reply suppressed: {e}");
                false
            }
        }
    }
}

impl OneBotBridge {
    fn outbound_for(&self, action_tx: &UnboundedSender<ActionRequest>) -> Outbound {
        Outbound {
            action_tx: action_tx.clone(),
            cfg: self.cfg.clone(),
        }
    }
}

/// Map a OneBot conversation id back to its reply route.
fn conversation_to_route(conversation_id: &str) -> Option<ReplyRoute> {
    let rest = conversation_id.strip_prefix("onebot_")?;
    let (kind, id) = rest.split_once('_')?;
    let id: i64 = id.parse().ok()?;
    match kind {
        "private" => Some(ReplyRoute::Private { user_id: id }),
        "group" => Some(ReplyRoute::Group { group_id: id }),
        _ => None,
    }
}

/// The conversation id for an incoming OneBot message.
fn conversation_id_for(msg: &MessageEvent) -> Option<String> {
    match msg.message_type.as_str() {
        "private" => Some(format!("onebot_private_{}", msg.user_id)),
        "group" => msg.group_id.map(|g| format!("onebot_group_{g}")),
        _ => None,
    }
}

/// Map an adapter-independent target (`private:<id>` / `group:<id>`) to a
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
        ActionParams, MessageContent, MessageEvent, MessageSegment, Sender,
    };
    use nota_core::conversation::OutboundEvent;

    fn test_bridge() -> (OneBotBridge, Arc<ConversationManager>) {
        let manager = Arc::new(ConversationManager::new());
        let bridge = OneBotBridge::new(
            manager.clone(),
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
        );
        (bridge, manager)
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

    fn voice_event(user_id: i64) -> PostEvent {
        PostEvent::Message(MessageEvent {
            self_id: 7,
            time: 1700000000,
            message_id: 99,
            message_type: "private".to_string(),
            sub_type: Some("friend".to_string()),
            user_id,
            message: Some(MessageContent::Segments(vec![MessageSegment {
                segment_type: "record".to_string(),
                data: HashMap::from([("file".to_string(), serde_json::json!("voice.amr"))]),
            }])),
            group_id: None,
            sender: Some(Sender {
                user_id,
                nickname: "Alice".to_string(),
                card: None,
            }),
        })
    }

    fn outbound(
        conversation_id: Option<&str>,
        target: Option<&str>,
        content: &str,
    ) -> AdapterEvent {
        AdapterEvent::Outbound(OutboundEvent {
            conversation_id: conversation_id.map(str::to_string),
            target: target.map(str::to_string),
            content: content.to_string(),
            request_id: None,
        })
    }

    #[tokio::test]
    async fn delivers_private_message_to_persona_inbox() {
        let (mut bridge, manager) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut inbox = manager.subscribe_persona("bob");

        bridge.handle_onebot_event(private_event(42, "hello")).await;

        let msg = inbox.recv().await.unwrap();
        assert_eq!(msg.sender, "user");
        assert_eq!(msg.prefix, "[好友 Alice(42)] ");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.conversation.persona, "bob");
        assert_eq!(msg.conversation.conversation_id, "onebot_private_42");
    }

    #[tokio::test]
    async fn delivers_group_message_from_allowlisted_group() {
        let (mut bridge, manager) = test_bridge();
        bridge.cfg.group_ids = vec![30003];
        let mut inbox = manager.subscribe_persona("bob");
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

        let msg = inbox.recv().await.unwrap();
        assert_eq!(msg.prefix, "[群 30003 Alice(42)] ");
        assert_eq!(msg.content, "hi");
        assert_eq!(msg.conversation.conversation_id, "onebot_group_30003");
    }

    #[tokio::test]
    async fn delivers_voice_message_with_message_id() {
        let (mut bridge, manager) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut inbox = manager.subscribe_persona("bob");

        bridge.handle_onebot_event(voice_event(42)).await;

        let msg = inbox.recv().await.unwrap();
        assert_eq!(msg.content, "[record msg id:99 file=voice.amr]");
        assert_eq!(msg.conversation.conversation_id, "onebot_private_42");
    }

    #[tokio::test]
    async fn ignores_private_message_from_non_allowlisted_friend() {
        let (mut bridge, manager) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut inbox = manager.subscribe_persona("bob");

        bridge.handle_onebot_event(private_event(99, "hi")).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), inbox.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ignores_all_when_allowlists_are_empty() {
        let (bridge, manager) = test_bridge();
        let mut inbox = manager.subscribe_persona("bob");

        bridge.handle_onebot_event(private_event(42, "hi")).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), inbox.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn routes_explicit_reply_by_conversation() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(outbound(Some("onebot_private_42"), None, "hi"), &action_tx)
            .await;

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
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(outbound(Some("web_abc"), None, "hi"), &action_tx)
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn routes_onebot_send_msg_target() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.group_ids = vec![30003];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(
                outbound(Some("onebot_private_42"), Some("group:30003"), "yo"),
                &action_tx,
            )
            .await;

        let action = action_rx.recv().await.unwrap();
        assert_eq!(action.action, "send_group_msg");
    }

    fn extract_permission_id(notice: &str) -> String {
        let start = notice.find("权限ID：").unwrap() + "权限ID：".len();
        notice[start..start + 36].to_string()
    }

    #[tokio::test]
    async fn outbound_to_non_allowlisted_requires_approval() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        bridge.cfg.group_ids = vec![30003];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(
                outbound(Some("onebot_private_42"), Some("group:99999"), "hi"),
                &action_tx,
            )
            .await;

        let notice_action = action_rx.recv().await.unwrap();
        assert_eq!(notice_action.action, "send_private_msg");
        let ActionParams::Private { message, .. } = notice_action.params else {
            panic!("expected private action");
        };
        let notice = &message[0].data.text;
        assert!(notice.contains("群 99999"));
        assert!(notice.contains("同意"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err()
        );

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
    }

    #[tokio::test]
    async fn denied_outbound_not_sent() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(
                outbound(Some("onebot_private_42"), Some("group:99999"), "hi"),
                &action_tx,
            )
            .await;

        let notice_action = action_rx.recv().await.unwrap();
        let ActionParams::Private { message, .. } = notice_action.params else {
            panic!("expected private action");
        };
        let id = extract_permission_id(&message[0].data.text);
        bridge.permissions.resolve(&id, false).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), action_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn drops_approval_for_non_onebot_source() {
        let (bridge, _) = test_bridge();
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();

        bridge
            .handle_adapter_event(
                outbound(Some("web_abc"), Some("group:99999"), "hi"),
                &action_tx,
            )
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), action_rx.recv())
                .await
                .is_err()
        );
        let pending = bridge.pending_approvals.lock().unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn approval_command_resolves_and_is_not_forwarded() {
        let (mut bridge, manager) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let mut inbox = manager.subscribe_persona("bob");
        let (permission_id, decision) = bridge.permissions.register().await;
        bridge.push_pending("onebot_private_42", permission_id.clone());

        bridge.handle_onebot_event(private_event(42, "同意")).await;

        assert!(decision.await.unwrap());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), inbox.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deny_command_resolves_false() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (permission_id, decision) = bridge.permissions.register().await;
        bridge.push_pending("onebot_private_42", permission_id.clone());

        bridge.handle_onebot_event(private_event(42, "拒绝")).await;

        assert!(!decision.await.unwrap());
    }

    #[tokio::test]
    async fn approval_seq_targets_second_pending_request() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        bridge.push_pending("onebot_private_42", Uuid::new_v4().to_string());
        bridge.push_pending("onebot_private_42", Uuid::new_v4().to_string());

        bridge.handle_onebot_event(private_event(42, "同意2")).await;

        let remaining = {
            let queue = bridge.pending_approvals.lock().unwrap();
            queue.get("onebot_private_42").cloned().unwrap_or_default()
        };
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn permission_request_asks_approval() {
        let (mut bridge, _) = test_bridge();
        bridge.cfg.friend_ids = vec![42];
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let (permission_id, decision) = bridge.permissions.register().await;
        let event = AdapterEvent::Permission(PermissionEvent {
            conversation_id: "onebot_private_42".to_string(),
            permission_id: permission_id.clone(),
            prompt: "Allow file_read on /etc/passwd?".to_string(),
            parent_request_id: Some("p1".to_string()),
        });

        bridge.handle_adapter_event(event, &action_tx).await;

        let notice_action = action_rx.recv().await.unwrap();
        let ActionParams::Private { message, .. } = notice_action.params else {
            panic!("expected private action");
        };
        assert!(message[0].data.text.contains("需要你授权"));

        // The user's 同意 reply resolves the pending permission.
        bridge.handle_onebot_event(private_event(42, "同意")).await;
        assert!(decision.await.unwrap());
    }
}

