use std::sync::Arc;

use axum::{
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
};
use nota_core::bus::{BusEvent, EventBus, EventKind};
use nota_core::permissions::PermissionRegistry;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientCommand {
    Send {
        persona: String,
        content: String,
        request_id: String,
    },
    Permission {
        permission_id: String,
        approved: bool,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerEvent {
    Message {
        content: String,
        request_id: String,
    },
    PermissionNeeded {
        permission_id: String,
        prompt: String,
        request_id: String,
    },
    Error {
        content: String,
    },
}

pub async fn ws_chat_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<WsState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

pub struct WsState {
    pub bus: Arc<EventBus>,
    pub permissions: Arc<PermissionRegistry>,
}

async fn handle_socket(mut socket: WebSocket, state: Arc<WsState>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    state.bus.subscribe_with_sender(tx);

    // Each web connection is its own conversation session, so multiple
    // clients (or tabs) never see each other's messages or history.
    let session_id = format!("web_{}", Uuid::new_v4());

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_command(&text, &state, &session_id).await {
                            let _ = socket.send(Message::Text(
                                serde_json::to_string(&ServerEvent::Error {
                                    content: e.to_string(),
                                })
                                .unwrap()
                                .into(),
                            )).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            event = rx.recv() => {
                if let Some(event) = event {
                    forward_event(event, &mut socket, &session_id).await;
                }
            }
        }
    }
}

async fn handle_command(
    text: &str,
    state: &Arc<WsState>,
    session_id: &str,
) -> anyhow::Result<()> {
    let cmd: ClientCommand = serde_json::from_str(text)?;
    match cmd {
        ClientCommand::Send { persona, content, request_id } => {
            state.bus.send(
                BusEvent::targeted_message(
                    "user".to_string(),
                    content,
                    Some(request_id),
                    persona,
                )
                .with_session(Some(session_id.to_string())),
            );
        }
        ClientCommand::Permission { permission_id, approved } => {
            state.permissions.resolve(&permission_id, approved).await;
        }
    }
    Ok(())
}

async fn forward_event(
    event: BusEvent,
    socket: &mut WebSocket,
    session_id: &str,
) {
    // Only events for this connection's session may be forwarded.
    if event.session_id.as_deref() != Some(session_id) {
        return;
    }
    match event.kind {
        EventKind::Message => {
            if let Some(ref rid) = event.request_id {
                let payload = serde_json::to_string(&ServerEvent::Message {
                    content: event.content,
                    request_id: rid.clone(),
                })
                .unwrap();
                let _ = socket.send(Message::Text(payload.into())).await;
            } else {
                // System notices (e.g. outbound approval requests) carry no
                // request id but must still reach the client.
                let payload = serde_json::to_string(&ServerEvent::Message {
                    content: event.content,
                    request_id: String::new(),
                })
                .unwrap();
                let _ = socket.send(Message::Text(payload.into())).await;
            }
        }
        EventKind::PermissionRequest => {
            if let Some(ref parent) = event.parent_request_id {
                let payload = serde_json::to_string(&ServerEvent::PermissionNeeded {
                    permission_id: event.request_id.unwrap_or_default(),
                    prompt: event.content,
                    request_id: parent.clone(),
                })
                .unwrap();
                let _ = socket.send(Message::Text(payload.into())).await;
            }
        }
        // Persona-initiated outbound messages are forwarded by the channel
        // (e.g. OneBot), not by the web client.
        EventKind::OutboundMessage => {}
    }
}
