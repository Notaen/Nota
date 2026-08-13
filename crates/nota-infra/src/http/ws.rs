use std::sync::Arc;

use axum::{
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
};
use nota_core::permissions::PermissionRegistry;
use nota_core::session::{AdapterEvent, Session, SessionManager};
use serde::{Deserialize, Serialize};
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
    pub manager: Arc<SessionManager>,
    pub permissions: Arc<PermissionRegistry>,
}

async fn handle_socket(mut socket: WebSocket, state: Arc<WsState>) {
    // Each web connection is its own conversation session, so multiple
    // clients (or tabs) never see each other's messages or history.
    let session_id = format!("web_{}", Uuid::new_v4());
    let mut rx = state.manager.subscribe_adapter("web");

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
            state
                .manager
                .deliver(
                    &Session::new(persona, session_id),
                    "user",
                    "",
                    &content,
                    Some(request_id),
                )
                .await;
        }
        ClientCommand::Permission { permission_id, approved } => {
            state.permissions.resolve(&permission_id, approved).await;
        }
    }
    Ok(())
}

async fn forward_event(
    event: AdapterEvent,
    socket: &mut WebSocket,
    session_id: &str,
) {
    match event {
        AdapterEvent::Outbound(e) if e.session_id.as_deref() == Some(session_id) => {
            let payload = serde_json::to_string(&ServerEvent::Message {
                content: e.content,
                request_id: e.request_id.unwrap_or_default(),
            })
            .unwrap();
            let _ = socket.send(Message::Text(payload.into())).await;
        }
        AdapterEvent::Permission(p) if p.session_id == session_id => {
            let payload = serde_json::to_string(&ServerEvent::PermissionNeeded {
                permission_id: p.permission_id,
                prompt: p.prompt,
                request_id: p.parent_request_id.unwrap_or_default(),
            })
            .unwrap();
            let _ = socket.send(Message::Text(payload.into())).await;
        }
        _ => {}
    }
}
