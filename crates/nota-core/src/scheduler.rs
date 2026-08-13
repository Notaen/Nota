//! Scheduling port: fire a reminder into a session at a future time.

use anyhow::Result;
use async_trait::async_trait;

use crate::session::Session;

/// Schedules a reminder to be delivered into a session at a future time.
/// The reminder is delivered as a chat message (`sender = "scheduler"`), so
/// the persona can react to it like any other message.
#[async_trait]
pub trait Scheduler: Send + Sync {
    async fn schedule_reminder(
        &self,
        at_unix: i64,
        session: Session,
        message: String,
    ) -> Result<()>;
}
