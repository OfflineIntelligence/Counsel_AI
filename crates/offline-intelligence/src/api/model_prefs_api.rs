//! Per-user model marks: which models are starred.
//!
//! A small JSON file in the app data directory rather than a table: it is a
//! handful of ids, read whole and written whole, and it must survive the model
//! catalogue being refreshed or a model being removed and reinstalled — so it is
//! keyed by catalogue id and never cleaned up automatically.

use std::path::PathBuf;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::shared_state::UnifiedAppState;

const FILE_NAME: &str = "model_marks.json";

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMarks {
    #[serde(default)]
    pub starred: Vec<String>,
}

fn path() -> PathBuf {
    crate::api::drafts_api::manager().app_data_dir().join(FILE_NAME)
}

fn read_marks() -> ModelMarks {
    std::fs::read(path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_marks(marks: &ModelMarks) -> std::io::Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename, so a crash mid-write never leaves a truncated file
    // that would silently unstar everything.
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(marks).unwrap_or_default())?;
    std::fs::rename(tmp, p)
}

/// GET /models/marks
pub async fn get_marks(State(_state): State<UnifiedAppState>) -> impl IntoResponse {
    Json(read_marks())
}

#[derive(Debug, Deserialize)]
pub struct StarBody {
    pub model_id: String,
    pub starred: bool,
}

/// POST /models/marks/star — star or unstar one model; returns every mark.
pub async fn set_star(State(_state): State<UnifiedAppState>, Json(body): Json<StarBody>) -> impl IntoResponse {
    let id = body.model_id.trim().to_string();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "model_id is required" }))).into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut marks = read_marks();
        marks.starred.retain(|s| s != &id);
        if body.starred {
            marks.starred.push(id);
        }
        write_marks(&marks).map(|_| marks)
    })
    .await;

    match result {
        Ok(Ok(marks)) => Json(marks).into_response(),
        Ok(Err(e)) => {
            warn!("Could not save model marks: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "model_marks_unwritable", "detail": e.to_string() })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}
