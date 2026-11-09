//! Which in-app announcements the user has already dismissed.
//!
//! # Why this is server-side
//!
//! The obvious home for "has this user seen the what's-new dialog" is
//! `localStorage`. It is one line and needs no endpoint. It is also wrong
//! here for two reasons:
//!
//! 1. The webview's storage is cleared by things the user does not think of
//!    as "resetting the app" — a reinstall, a profile move, a cache wipe.
//!    An announcement re-appearing months later, on a product the user has
//!    been using daily, reads as a bug.
//! 2. Every other durable fact in this product lives in the database. A
//!    single preference hiding in the webview is the one nobody thinks to
//!    export, back up or migrate.
//!
//! # Why not a generic key/value endpoint
//!
//! `app_settings` is a generic store, and exposing it directly as
//! `PUT /app-settings/:key` would have been less code. But that table also
//! holds `storage.disk_limit_mb` and the per-model activation timestamps —
//! values with real consequences, validated carefully by their own handlers.
//! A generic writer would let any caller set them without passing any of that
//! validation. This endpoint can only ever touch keys under its own prefix.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::shared_state::UnifiedAppState;

/// Prefix owned by this module. Everything written here is namespaced under
/// it, which is what makes "this endpoint cannot touch the storage budget" a
/// property of the code rather than a convention.
const SEEN_PREFIX: &str = "announcement.seen.";

fn seen_key(id: &str) -> String {
    format!("{}{}", SEEN_PREFIX, id)
}

/// An announcement id is part of a database key, so it is constrained rather
/// than trusted: lowercase alphanumerics, dot, dash and underscore. This is
/// not about SQL injection — the queries are parameterised — but about a
/// caller being unable to write a key that collides with another module's
/// namespace by embedding a dot-path in the id.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[derive(Serialize)]
pub struct SeenResponse {
    /// True when this announcement has been dismissed. Absent from the store
    /// means "never seen" — the same fallback the settings store documents.
    pub seen: bool,
}

#[derive(Deserialize)]
pub struct MarkSeenRequest {
    pub id: String,
}

/// GET /announcements/:id/seen
pub async fn get_seen(
    State(state): State<UnifiedAppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if !is_valid_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_announcement_id" })),
        )
            .into_response();
    }

    let db = &state.shared_state.database_pool;
    match db.settings.get(&seen_key(&id)) {
        Ok(value) => Json(SeenResponse {
            seen: value.as_deref() == Some("1"),
        })
        .into_response(),
        Err(e) => {
            // A read failure must not block the UI. The dialog showing one
            // extra time is a far smaller harm than the app refusing to
            // render because a preference could not be read, so this reports
            // "not seen" and logs the real cause.
            warn!("Failed to read announcement state for '{}': {}", id, e);
            Json(SeenResponse { seen: false }).into_response()
        }
    }
}

/// POST /announcements/seen
pub async fn mark_seen(
    State(state): State<UnifiedAppState>,
    Json(req): Json<MarkSeenRequest>,
) -> axum::response::Response {
    if !is_valid_id(&req.id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_announcement_id" })),
        )
            .into_response();
    }

    let db = &state.shared_state.database_pool;
    if let Err(e) = db.settings.set(&seen_key(&req.id), "1") {
        warn!("Failed to persist announcement dismissal '{}': {}", req.id, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "settings_write_failed",
                "detail": format!("Could not record the dismissal: {}", e),
            })),
        )
            .into_response();
    }

    Json(SeenResponse { seen: true }).into_response()
}
