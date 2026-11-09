//! Cloud providers: which exist, which are connected, and their models.
//!
//! Keys are saved through the existing /api-keys endpoints under the
//! provider id; this module never returns a key.

use axum::{extract::{Query, State}, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

use crate::{
    cloud_providers::{self, PROVIDERS},
    memory_db::{ApiKeyType, KeyEncryption},
    shared_state::UnifiedAppState,
};

fn saved_key(state: &UnifiedAppState, id: &'static str) -> Option<String> {
    state.shared_state.database_pool.api_keys
        .get_key(ApiKeyType::Cloud(id))
        .ok()
        .flatten()
        .and_then(|r| KeyEncryption::decrypt(&r.encrypted_value).ok())
}

/// GET /cloud/providers: every provider, and whether a key is saved for it.
pub async fn list_providers(State(state): State<UnifiedAppState>) -> impl IntoResponse {
    let list: Vec<serde_json::Value> = PROVIDERS
        .iter()
        .map(|p| serde_json::json!({
            "id": p.id,
            "name": p.name,
            "connected": saved_key(&state, p.id).is_some(),
        }))
        .collect();
    Json(list)
}

#[derive(Debug, Deserialize)]
pub struct ModelsQuery {
    pub provider: String,
}

/// GET /cloud/models?provider=id: the provider's own model list, fetched
/// with the saved key. Doubles as the "is this key valid" check.
pub async fn list_models(
    State(state): State<UnifiedAppState>,
    Query(q): Query<ModelsQuery>,
) -> impl IntoResponse {
    let Some(provider) = cloud_providers::find(&q.provider) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Unknown provider" }))).into_response();
    };
    let Some(key) = saved_key(&state, provider.id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Not connected" }))).into_response();
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let mut req = client
        .get(format!("{}/models", provider.base_url))
        .header("Authorization", format!("Bearer {}", key));
    for (k, v) in cloud_providers::extra_headers(provider, &key) {
        req = req.header(k, v);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": format!("Could not reach {}: {}", provider.name, e) }))).into_response();
        }
    };
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": format!("{} rejected the API key.", provider.name) }))).into_response();
    }
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    // OpenAI shape: { data: [{ id }] }. Gemini prefixes ids with "models/".
    let mut models: Vec<String> = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| a.iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
            .map(|id| id.trim_start_matches("models/").to_string())
            .collect())
        .unwrap_or_default();
    models.sort();
    models.dedup();
    Json(serde_json::json!({ "models": models, "ok": status.is_success() || !models.is_empty() })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ActiveRequest {
    /// Absent (or null) means On Computer.
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// GET /cloud/active: what answers right now.
pub async fn get_active(State(state): State<UnifiedAppState>) -> impl IntoResponse {
    match state.llm_worker.cloud() {
        Some(c) => Json(serde_json::json!({ "mode": "cloud", "provider": c.provider.id, "model": c.model })),
        None => Json(serde_json::json!({ "mode": "local" })),
    }
}

/// POST /cloud/active: the app-wide On Computer / On Cloud switch. Every
/// model call in the backend (chat, agents, drafting, document intelligence)
/// goes through the LLM worker, which this sets.
pub async fn set_active(
    State(state): State<UnifiedAppState>,
    Json(req): Json<ActiveRequest>,
) -> impl IntoResponse {
    let (Some(provider_id), Some(model)) = (req.provider.as_deref(), req.model.as_deref()) else {
        state.llm_worker.set_cloud(None);
        return Json(serde_json::json!({ "mode": "local" })).into_response();
    };
    let Some(provider) = cloud_providers::find(provider_id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Unknown provider" }))).into_response();
    };
    let Some(key) = saved_key(&state, provider.id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("{} is not connected.", provider.name) }))).into_response();
    };
    state.llm_worker.set_cloud(Some(crate::worker_threads::llm_worker::ActiveCloud {
        provider,
        key,
        model: model.to_string(),
    }));
    Json(serde_json::json!({ "mode": "cloud", "provider": provider.id, "model": model })).into_response()
}
