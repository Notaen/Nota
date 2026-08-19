use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
};
use nota_core::persona::PersonaStore;
use nota_core::session::SessionItem;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{Config, ConfigStore};
use crate::persona_runtime::PersonaRuntime;

pub struct ApiState {
    pub persona_store: Arc<dyn PersonaStore>,
    /// Per-persona runtimes (conversation layer), assembled by the
    /// composition root.
    pub persona_runtimes: HashMap<String, Arc<PersonaRuntime>>,
    pub config: Arc<RwLock<Config>>,
    pub config_path: std::path::PathBuf,
}

#[derive(Serialize)]
struct PersonaInfo {
    name: String,
    files: Vec<String>,
}

#[derive(Deserialize)]
struct CreatePersonaBody {
    name: String,
}

#[derive(Deserialize)]
struct FileWriteBody {
    content: String,
}

#[derive(Serialize)]
struct FileReadResponse {
    content: String,
}

#[derive(Serialize)]
struct SessionLog {
    session_id: String,
    created_at: i64,
    messages: Vec<(i64, SessionItem)>,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

async fn list_personas(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorBody>)> {
    let names = state
        .persona_store
        .list_personas()
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(names))
}

async fn create_persona(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<CreatePersonaBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name is required".into()));
    }
    state
        .persona_store
        .create_persona(name)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::CREATED)
}

async fn delete_persona(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state
        .persona_store
        .delete_persona(&name)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_persona_info(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<Json<PersonaInfo>, (StatusCode, Json<ErrorBody>)> {
    let names = state
        .persona_store
        .list_personas()
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !names.contains(&name) {
        return Err(err(StatusCode::NOT_FOUND, "persona not found".into()));
    }
    Ok(Json(PersonaInfo {
        name,
        files: vec!["solo.md".into(), "memory.md".into()],
    }))
}

async fn read_file(
    State(state): State<Arc<ApiState>>,
    Path((name, filename)): Path<(String, String)>,
) -> Result<Json<FileReadResponse>, (StatusCode, Json<ErrorBody>)> {
    match state.persona_store.read_persona_file(&name, &filename).await {
        Ok(content) => Ok(Json(FileReadResponse { content })),
        Err(_) => Err(err(
            StatusCode::NOT_FOUND,
            format!("file not found: {name}/{filename}"),
        )),
    }
}

async fn write_file(
    State(state): State<Arc<ApiState>>,
    Path((name, filename)): Path<(String, String)>,
    Json(body): Json<FileWriteBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state
        .persona_store
        .write_persona_file(&name, &filename, &body.content)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::OK)
}

async fn read_history(
    State(state): State<Arc<ApiState>>,
    Path((name, conversation_id)): Path<(String, String)>,
) -> Result<Json<Vec<SessionLog>>, (StatusCode, Json<ErrorBody>)> {
    let runtime = state
        .persona_runtimes
        .get(&name)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("persona not found: {name}")))?;
    let sessions = runtime
        .sessions(&conversation_id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut logs = Vec::new();
    for session in sessions {
        let messages = session
            .raw_history()
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let created_at = session
            .created_at()
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        logs.push(SessionLog {
            session_id: session.id().to_string(),
            created_at,
            messages,
        });
    }
    Ok(Json(logs))
}

async fn get_settings(State(state): State<Arc<ApiState>>) -> Json<Config> {
    Json(state.config.read().await.clone())
}

async fn put_settings(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<Config>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    {
        let mut cfg = state.config.write().await;
        *cfg = body.clone();
    }
    let store = ConfigStore::new(state.config_path.parent().unwrap());
    store
        .save(&body)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::OK)
}

fn err(status: StatusCode, msg: String) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { error: msg }))
}

// TODO: 这api有问题，和设计冲突。
pub fn router() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/personas", get(list_personas).post(create_persona))
        .route("/personas/{name}", delete(delete_persona).get(get_persona_info))
        .route("/personas/{name}/files/{filename}", get(read_file).put(write_file))
        .route(
            "/personas/{name}/chatlog/{conversation_id}",
            get(read_history),
        )
        .route("/settings", get(get_settings).put(put_settings))
}
