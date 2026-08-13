//! In-memory scheduler backed by tokio timers.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::scheduler::Scheduler;
use nota_core::session::{Session, SessionManager};

/// Spawns a tokio task per reminder and delivers the message into the target
/// session when it is due. Tasks are lost on restart (no persistence yet).
pub struct TokioScheduler {
    manager: Arc<SessionManager>,
}

impl TokioScheduler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Scheduler for TokioScheduler {
    async fn schedule_reminder(
        &self,
        at_unix: i64,
        session: Session,
        message: String,
    ) -> Result<()> {
        let manager = self.manager.clone();
        tokio::spawn(async move {
            let now = chrono::Utc::now().timestamp();
            let delay = (at_unix - now).max(0) as u64;
            if delay > 0 {
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            log::info!("scheduler reminder -> {}", session.session_id);
            manager
                .deliver(&session, "scheduler", "", &message, None)
                .await;
        });
        Ok(())
    }
}
