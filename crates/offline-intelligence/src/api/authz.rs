//! Authorization: once this computer has an account, the routes that hold
//! secrets or spend money (API keys, cloud models) need a signed-in session.
//!
//! A request proves it with `Authorization: Bearer <token>`, the JWT issued at
//! sign-in (signed with this install's secret, with an expiry). The account it
//! names must still exist and be verified. Before the first account is made,
//! the app is single-user and open, so these routes stay usable.

use crate::api::auth_api::{decode_jwt_token, get_auth_state};
use crate::shared_state::UnifiedAppState;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Whether these request headers carry a valid signed-in session — or no
/// account exists yet (single-user and open). The same rule as
/// `require_session`, for handlers that only need it on some requests.
pub fn session_ok(state: &UnifiedAppState, headers: &axum::http::HeaderMap) -> bool {
    let auth = get_auth_state(state);
    let has_accounts = auth.users.count_users().map(|n| n > 0).unwrap_or(false);
    if !has_accounts {
        return true;
    }
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    !token.is_empty()
        && decode_jwt_token(token, &auth.jwt_secret)
            .ok()
            .and_then(|t| auth.users.get_user_by_login(&t.claims.email).ok().flatten())
            .map(|u| u.email_verified)
            .unwrap_or(false)
}

pub async fn require_session(State(state): State<UnifiedAppState>, req: Request, next: Next) -> Response {
    if session_ok(&state, req.headers()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "success": false, "error": "sign_in_required", "message": "Sign in to continue." })),
        )
            .into_response()
    }
}
