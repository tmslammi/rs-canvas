//! Workspace management for the browser picker. Drawing stays on the bridge;
//! these routes only change a workspace's name or remove its saved document.
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::state::{AppState, WorkspaceManagementError};

#[derive(Deserialize)]
pub struct RenameRequest {
    name: String,
}

pub async fn rename(
    State(state): State<Arc<AppState>>,
    Path(source): Path<String>,
    Json(request): Json<RenameRequest>,
) -> Response {
    result(state.manage_workspace(&source, Some(&request.name)).await)
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    result(state.manage_workspace(&name, None).await)
}

fn result(result: Result<(), WorkspaceManagementError>) -> Response {
    match result {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(WorkspaceManagementError::Invalid(message)) => {
            failure(StatusCode::BAD_REQUEST, message)
        }
        Err(WorkspaceManagementError::Conflict(message)) => failure(StatusCode::CONFLICT, message),
        Err(WorkspaceManagementError::NotFound) => {
            failure(StatusCode::NOT_FOUND, "Workspace not found.".into())
        }
        Err(WorkspaceManagementError::Io(error)) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not update workspace: {error}"),
        ),
    }
}

fn failure(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}
