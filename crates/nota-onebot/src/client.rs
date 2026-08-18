//! Forward WebSocket client for OneBot 11.
//!
//! The bot connects to the OneBot implementation's WS server (e.g. NapCat /
//! LLOneBot at `ws://127.0.0.1:3001`), receives `post_type` events, and sends
//! action requests over the same connection. Reconnects with exponential
//! backoff; the backoff resets after a connection stays up for a while.

use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender}, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::config::OnebotConfig;
use crate::types::{ActionRequest, ActionResponse, PostEvent};

pub(crate) type PendingResponses =
    Arc<Mutex<HashMap<String, oneshot::Sender<ActionResponse>>>>;

/// Sets the shared connected flag back to `false` when the connection
/// attempt ends (any path), so tools can report live connection state.
struct ConnectedGuard<'a>(&'a AtomicBool);

impl Drop for ConnectedGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A connection surviving this long is considered healthy, so the reconnect
/// backoff resets.
const STABLE_CONNECTION: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How a connection attempt ended.
enum WsEnd {
    /// Connection closed or failed after being up for this long.
    Gone(Duration),
    /// The bridge shut down; the whole loop must stop.
    Shutdown,
}

/// Runs the WS connection loop forever, reconnecting on failure. Incoming
/// events are forwarded to `event_tx`; actions from `action_rx` are written to
/// the socket. Returns when `action_rx` is closed (bridge shutdown).
pub async fn run_ws_loop(
    cfg: OnebotConfig,
    event_tx: UnboundedSender<PostEvent>,
    mut action_rx: UnboundedReceiver<ActionRequest>,
    pending: PendingResponses,
    connected: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(1);

    loop {
        match connect_and_run(&cfg, &event_tx, &mut action_rx, &pending, &connected).await {
            Ok(WsEnd::Gone(up)) => {
                if up >= STABLE_CONNECTION {
                    backoff = Duration::from_secs(1);
                }
            }
            Ok(WsEnd::Shutdown) => return,
            Err(e) => log::warn!("OneBot WebSocket error: {e:#}"),
        }

        log::info!("OneBot disconnected; reconnecting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_and_run(
    cfg: &OnebotConfig,
    event_tx: &UnboundedSender<PostEvent>,
    action_rx: &mut UnboundedReceiver<ActionRequest>,
    pending: &PendingResponses,
    connected: &AtomicBool,
) -> Result<WsEnd> {
    let started = Instant::now();

    let mut request = cfg.ws_url.as_str().into_client_request()?;
    if !cfg.access_token.is_empty() {
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            format!("Bearer {}", cfg.access_token).parse()?,
        );
    }

    let (mut socket, response) = tokio_tungstenite::connect_async(request).await?;
    connected.store(true, Ordering::SeqCst);
    let _connected_guard = ConnectedGuard(connected);
    log::info!(
        "OneBot connected to {} (status {})",
        cfg.ws_url,
        response.status()
    );

    loop {
        tokio::select! {
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => handle_incoming(&text, event_tx, pending),
                    Some(Ok(Message::Ping(payload))) => {
                        log::debug!("OneBot server ping, replying pong");
                        if let Err(e) = socket.send(Message::Pong(payload)).await {
                            return Err(e.into());
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(WsEnd::Gone(started.elapsed())),
                }
            }
            action = action_rx.recv() => {
                match action {
                    Some(action) => {
                        let text = serde_json::to_string(&action)?;
                        if let Err(e) = socket.send(Message::Text(text.into())).await {
                            return Err(e.into());
                        }
                    }
                    None => return Ok(WsEnd::Shutdown),
                }
            }
        }
    }
}

fn handle_incoming(
    text: &str,
    event_tx: &UnboundedSender<PostEvent>,
    pending: &PendingResponses,
) {
    if let Ok(event) = serde_json::from_str::<PostEvent>(text) {
        let _ = event_tx.send(event);
        return;
    }
    if let Ok(resp) = serde_json::from_str::<ActionResponse>(text) {
        if let Some(echo) = &resp.echo
            && let Some(tx) = pending.lock().unwrap().remove(echo)
        {
            let _ = tx.send(resp);
            return;
        }
        log::debug!(
            "OneBot action response without waiter: status={:?} retcode={:?} echo={:?}",
            resp.status,
            resp.retcode,
            resp.echo
        );
        return;
    }
    log::warn!("Unrecognized OneBot WebSocket message: {text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::accept_async;

    /// Full client round trip: connect, receive an event, send an action.
    #[tokio::test]
    async fn ws_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            // Send a private message event the client should forward.
            let event = serde_json::json!({
                "post_type": "message",
                "message_type": "private",
                "message_id": 1,
                "user_id": 42,
                "self_id": 7,
                "time": 1700000000,
                "message": "ping",
                "sender": {"user_id": 42, "nickname": "T"}
            });
            ws.send(Message::Text(event.to_string().into())).await.unwrap();

            // Expect the client's action request, then answer it.
            let incoming = ws.next().await.unwrap().unwrap();
            let Message::Text(text) = incoming else {
                panic!("expected text action, got {incoming:?}");
            };
            let action: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(action["action"], "send_private_msg");
            assert_eq!(action["params"]["user_id"], 42);
            assert_eq!(action["params"]["message"][0]["data"]["text"], "pong");

            let resp = serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "echo": action["echo"],
                "data": {"message_id": 9}
            });
            ws.send(Message::Text(resp.to_string().into())).await.unwrap();
        });

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

        let cfg = OnebotConfig {
            enabled: true,
            mode: "ws".to_string(),
            ws_url: format!("ws://{addr}"),
            access_token: String::new(),
            persona: "t".to_string(),
            prefix: String::new(),
            friend_ids: Vec::new(),
            group_ids: Vec::new(),
        };
        let connected: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

        let client = tokio::spawn(run_ws_loop(
            cfg,
            event_tx,
            action_rx,
            pending.clone(),
            connected,
        ));

        // The event must arrive on the bridge side.
        let event = event_rx.recv().await.unwrap();
        let PostEvent::Message(msg) = event else {
            panic!("expected message event");
        };
        assert_eq!(msg.message.unwrap().to_text(), "ping");

        // Send an action; the server verifies it and answers. The response
        // must be correlated back through the pending map by echo.
        let action = ActionRequest::send_private_msg(42, "pong");
        let echo = action.echo.clone();
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        pending.lock().unwrap().insert(echo.clone(), resp_tx);
        action_tx.send(action).unwrap();

        let resp = resp_rx.await.unwrap();
        assert_eq!(resp.status.as_deref(), Some("ok"));
        assert_eq!(resp.retcode, Some(0));
        assert_eq!(resp.echo.as_deref(), Some(echo.as_str()));

        server.await.unwrap();
        // Give the loop a moment to process the server's response, then stop.
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(action_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), client).await;
    }
}
