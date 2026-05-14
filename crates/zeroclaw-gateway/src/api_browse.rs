//! HTTP adapter over `zeroclaw_runtime::browse::list_directory`.
//!
//! `GET /api/browse?path=<relative-to-shared>` returns one level of
//! children. All walking, containment, and sorting lives in the runtime
//! browse module; this is request shape → service call → response shape.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use zeroclaw_runtime::browse::{
    BrowseEntry, BrowseError, list_directory, make_directory, remove_directory,
};

use super::AppState;
use super::api::require_auth;

#[derive(Debug, Deserialize, Default)]
pub struct BrowseQuery {
    /// Path relative to `<install>/shared/`. Empty / unset = shared/ root.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BrowseResponse {
    pub path: String,
    pub entries: Vec<BrowseEntry>,
}

/// `GET /api/browse?path=<relative-to-shared>`
pub async fn handle_browse(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<BrowseQuery>,
) -> Response {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }
    let config = state.config.read().clone();
    let raw = q.path.unwrap_or_default();
    match list_directory(&config, &raw) {
        Ok(result) => Json(BrowseResponse {
            path: result.path,
            entries: result.entries,
        })
        .into_response(),
        Err(err) => browse_error_response(err),
    }
}

fn browse_error_response(err: BrowseError) -> Response {
    let status = match &err {
        BrowseError::Escape(_) => StatusCode::BAD_REQUEST,
        BrowseError::NotFound(_) => StatusCode::NOT_FOUND,
        BrowseError::NotADirectory(_) => StatusCode::BAD_REQUEST,
        BrowseError::Protected(_) => StatusCode::FORBIDDEN,
        BrowseError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct BrowsePathBody {
    pub path: String,
}

/// `POST /api/browse/mkdir` — create a directory under `<install>/shared/`.
pub async fn handle_browse_mkdir(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BrowsePathBody>,
) -> Response {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }
    let config = state.config.read().clone();
    match make_directory(&config, &body.path) {
        Ok(()) => Json(serde_json::json!({ "created": body.path })).into_response(),
        Err(err) => browse_error_response(err),
    }
}

/// `DELETE /api/browse/rmdir` — recursively remove a directory under
/// `<install>/shared/`. Refuses protected top-level entries.
pub async fn handle_browse_rmdir(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BrowsePathBody>,
) -> Response {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }
    let config = state.config.read().clone();
    match remove_directory(&config, &body.path) {
        Ok(()) => Json(serde_json::json!({ "removed": body.path })).into_response(),
        Err(err) => browse_error_response(err),
    }
}
