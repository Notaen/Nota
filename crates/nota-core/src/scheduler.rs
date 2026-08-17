//! Scheduling port: fire a reminder into a conversation at a future time.

use anyhow::Result;
use async_trait::async_trait;

use crate::conversation::Conversation;

/// Schedules a reminder to be delivered into a conversation at a future time.
/// The reminder is delivered as a chat message (`sender = "scheduler"`), so
/// the persona can react to it like any other message.
#[async_trait]
pub trait Scheduler: Send + Sync {
    async fn schedule_reminder(
        &self,
        at_unix: i64,
        conversation: Conversation,
        message: String,
    ) -> Result<()>;
}
