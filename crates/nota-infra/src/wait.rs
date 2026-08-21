//! Per-conversation wait hub: backs the `wait` tool.
//!
//! The model asks to hold a conversation open when a message looks
//! semantically incomplete (e.g. a bare "你" with unrelated context). A wait
//! is per-conversation state with a **consecutive budget**:
//! - a real inbound message cancels the pending wait and resets the budget
//!   (the runtime calls [`WaitHub::cancel`]);
//! - a timeout delivers a `[等待超时]` notice into the persona inbox as an
//!   ordinary message (`sender = "wait_timeout"`), so the model decides what
//!   to do next — ask, wait again, or stay silent;
//! - more than [`MAX_CONSECUTIVE_WAITS`] waits in a row (with no real
//!   message in between) are rejected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use nota_core::conversation::{Conversation, ConversationManager};

/// Maximum consecutive `wait` calls per conversation before the model must
/// reply or stay silent. A real message or `//clear` resets the budget.
pub const MAX_CONSECUTIVE_WAITS: u32 = 3;

/// Default wait duration in seconds when the `wait` tool omits `seconds`.
pub const DEFAULT_WAIT_SECONDS: u64 = 10;

/// Sender identity of timeout notices. The runtime uses it to tell a wake
/// from a real inbound message: a wake must NOT cancel the wait budget.
pub const WAIT_TIMEOUT_SENDER: &str = "wait_timeout";

struct WaitState {
    /// Monotonic generation; a timer only fires while it is still the
    /// latest registration for its conversation.
    generation: u64,
    /// Whether a timer is currently armed (false after it fired).
    pending: bool,
    /// Consecutive wait calls since the last real message / `//clear`.
    count: u32,
}

#[derive(Clone)]
pub struct WaitHub {
    inner: Arc<WaitHubInner>,
}

struct WaitHubInner {
    manager: Arc<ConversationManager>,
    waits: Mutex<HashMap<(String, String), WaitState>>,
    counter: AtomicU64,
}

impl WaitHub {
    pub fn new(manager: Arc<ConversationManager>) -> Self {
        Self {
            inner: Arc::new(WaitHubInner {
                manager,
                waits: Mutex::new(HashMap::new()),
                counter: AtomicU64::new(0),
            }),
        }
    }

    /// Register a wait for one conversation, replacing any pending wait.
    /// `seconds == 0` means "until the next real message" (no timer).
    /// Rejected when the consecutive budget is exhausted.
    pub fn register(
        &self,
        persona: &str,
        conversation_id: &str,
        seconds: u64,
        reason: Option<String>,
    ) -> Result<()> {
        let key = (persona.to_string(), conversation_id.to_string());
        let mut waits = self.inner.waits.lock().unwrap();
        let state = waits.entry(key.clone()).or_insert(WaitState {
            generation: 0,
            pending: false,
            count: 0,
        });
        if state.count >= MAX_CONSECUTIVE_WAITS {
            bail!(
                "wait rejected: {MAX_CONSECUTIVE_WAITS} consecutive waits used up; \
                 reply now or end the turn silently"
            );
        }
        state.count += 1;
        state.pending = true;
        state.generation = self.inner.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let generation = state.generation;
        drop(waits);

        if seconds == 0 {
            return Ok(());
        }

        let inner = self.inner.clone();
        let manager = self.inner.manager.clone();
        let conversation = Conversation::new(persona.to_string(), conversation_id.to_string());
        let text = timeout_text(seconds, reason.as_deref());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let fire = {
                let mut waits = inner.waits.lock().unwrap();
                match waits.get_mut(&key) {
                    Some(state) if state.pending && state.generation == generation => {
                        state.pending = false;
                        true
                    }
                    _ => false,
                }
            };
            if fire {
                manager
                    .deliver(&conversation, WAIT_TIMEOUT_SENDER, "", &text, None)
                    .await;
            }
        });
        Ok(())
    }

    /// Cancel a pending wait for a conversation and reset its consecutive
    /// budget. Called on every real inbound message and on `//clear`.
    pub fn cancel(&self, persona: &str, conversation_id: &str) {
        self.inner
            .waits
            .lock()
            .unwrap()
            .remove(&(persona.to_string(), conversation_id.to_string()));
    }
}

fn timeout_text(seconds: u64, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!(
            "[等待超时] 你请求等待（原因：{reason}），{seconds} 秒内没有新的消息到达。\
             如果你还在等对方把话说完，可以主动发送一条消息询问（例如“？”），\
             也可以继续等待或就此结束。"
        ),
        None => format!(
            "[等待超时] 你请求等待，{seconds} 秒内没有新的消息到达。\
             如果你还在等对方把话说完，可以主动发送一条消息询问（例如“？”），\
             也可以继续等待或就此结束。"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::conversation::{Conversation, InboundMessage};

    fn test_hub() -> (WaitHub, Arc<ConversationManager>) {
        let manager = Arc::new(ConversationManager::new());
        (WaitHub::new(manager.clone()), manager)
    }

    #[tokio::test]
    async fn timeout_delivers_wake_message() {
        let (hub, manager) = test_hub();
        let mut inbox = manager.subscribe_persona("bob");

        hub.register("bob", "onebot_private_42", 1, Some("等对方把话说完".to_string()))
            .unwrap();

        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            inbox.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(msg.sender, WAIT_TIMEOUT_SENDER);
        assert_eq!(msg.conversation, Conversation::new("bob", "onebot_private_42"));
        assert!(msg.content.contains("[等待超时]"));
        assert!(msg.content.contains("等对方把话说完"));
    }

    #[tokio::test]
    async fn cancel_prevents_timeout_delivery() {
        let (hub, manager) = test_hub();
        let mut inbox = manager.subscribe_persona("bob");

        hub.register("bob", "onebot_private_42", 1, None).unwrap();
        hub.cancel("bob", "onebot_private_42");

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), inbox.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn budget_rejects_more_than_three_consecutive_waits() {
        let (hub, _) = test_hub();

        for _ in 0..MAX_CONSECUTIVE_WAITS {
            hub.register("bob", "onebot_private_42", 0, None).unwrap();
        }
        let err = hub.register("bob", "onebot_private_42", 0, None).unwrap_err();
        assert!(err.to_string().contains("consecutive waits"));

        // A real message (cancel) resets the budget.
        hub.cancel("bob", "onebot_private_42");
        hub.register("bob", "onebot_private_42", 0, None).unwrap();
    }

    #[tokio::test]
    async fn replacing_a_pending_wait_cancels_the_old_timer() {
        let (hub, manager) = test_hub();
        let mut inbox = manager.subscribe_persona("bob");

        hub.register("bob", "onebot_private_42", 2, None).unwrap();
        hub.register("bob", "onebot_private_42", 1, None).unwrap();

        // Only the second (1s) timer fires; the first must not deliver.
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            inbox.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(msg.content.contains("1 秒内"));

        // No second wake after the first one fired.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), inbox.recv())
                .await
                .is_err()
        );
    }

    // Keep the unused import warning-free in case InboundMessage usage
    // changes; the type is exercised through the manager channel above.
    #[allow(dead_code)]
    fn _inbound_type_probe(_: InboundMessage) {}
}
