//! Correlated action API over the OneBot WebSocket connection.
//!
//! Sending a message is fire-and-forget, but tools (e.g. reading group
//! history) need the implementation's response. `OneBotApi::call` sends an
//! action with a fresh `echo`, registers a oneshot under that echo, and
//! resolves it when the WS client sees the matching action response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::types::{ActionRequest, ActionResponse};

const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct OneBotApi {
    actions: UnboundedSender<ActionRequest>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ActionResponse>>>>,
}

impl OneBotApi {
    pub fn new(
        actions: UnboundedSender<ActionRequest>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<ActionResponse>>>>,
    ) -> Self {
        Self { actions, pending }
    }

    pub(crate) fn pending(&self) -> Arc<Mutex<HashMap<String, oneshot::Sender<ActionResponse>>>> {
        self.pending.clone()
    }

    pub(crate) fn sender(&self) -> UnboundedSender<ActionRequest> {
        self.actions.clone()
    }

    /// Send an action and await the implementation's response, matched by echo.
    pub async fn call(&self, action: ActionRequest) -> Result<ActionResponse> {
        let echo = action.echo.clone();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(echo.clone(), tx);

        if self.actions.send(action).is_err() {
            self.pending.lock().unwrap().remove(&echo);
            bail!("OneBot action channel closed");
        }

        match tokio::time::timeout(ACTION_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.pending.lock().unwrap().remove(&echo);
                bail!("OneBot action '{echo}' sender dropped")
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&echo);
                bail!("OneBot action '{echo}' timed out after {ACTION_TIMEOUT:?}")
            }
        }
    }
}
