use std::collections::HashSet;
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

    let mut active_requests: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_command(&text, &state, &mut active_requests).await {
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
                    forward_event(event, &mut socket, &mut active_requests).await;
                }
            }
        }
    }
}

async fn handle_command(
    text: &str,
    state: &Arc<WsState>,
    active: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let cmd: ClientCommand = serde_json::from_str(text)?;
    match cmd {
        ClientCommand::Send { persona, content, request_id } => {
            active.insert(request_id.clone());
            state.bus.send(BusEvent::targeted_message(
                "user".to_string(),
                content,
                Some(request_id),
                persona,
            ));
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
    active: &mut HashSet<String>,
) {
    match event.kind {
        EventKind::Message => {
            if let Some(ref rid) = event.request_id
                && active.contains(rid)
            {
                let payload = serde_json::to_string(&ServerEvent::Message {
                    content: event.content,
                    request_id: rid.clone(),
                })
                .unwrap();
                let _ = socket.send(Message::Text(payload.into())).await;
                active.remove(rid);
            }
        }
        EventKind::PermissionRequest => {
            if let Some(ref parent) = event.parent_request_id
                && active.contains(parent)
            {
                let payload = serde_json::to_string(&ServerEvent::PermissionNeeded {
                    permission_id: event.request_id.unwrap_or_default(),
                    prompt: event.content,
                    request_id: parent.clone(),
                })
                .unwrap();
                let _ = socket.send(Message::Text(payload.into())).await;
            }
        }
    }
}
