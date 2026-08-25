//! API Keys Management API Endpoints
//!
//! Provides REST endpoints for managing API keys:
//! - Save/update HuggingFace keys
//! - Retrieve keys (decrypted)
//! - Delete keys
//! - Mark keys as used

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::{
    memory_db::{ApiKeyType, SimpleEncryption},
    shared_state::UnifiedAppState,
};

/// Request to save/update an API key
#[derive(Debug, Deserialize)]
pub struct SaveApiKeyRequest {
    pub key_type: String,  // "huggingface"
    pub value: String,     // Plain-text key value
}

/// Response after saving API key
#[derive(Debug, Serialize)]
pub struct SaveApiKeyResponse {
    pub success: bool,
    pub message: String,
}

/// Response with API key value (decrypted)
#[derive(Debug, Serialize)]
pub struct GetApiKeyResponse {
    pub key_type: String,
    pub value: Option<String>,  // Decrypted value
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_mode: Option<String>,
    pub usage_count: Option<i64>,
}

/// Response with all API keys
#[derive(Debug, Serialize)]
pub struct GetAllApiKeysResponse {
    pub keys: Vec<GetApiKeyResponse>,
}

/// Save or update an API key
pub async fn save_api_key(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<SaveApiKeyRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let key_type = ApiKeyType::from_str(&payload.key_type)
        .ok_or(StatusCode::BAD_REQUEST)?;

    info!("Saving API key for: {}", key_type.as_str());

    // Encrypt the value before storing
    let encrypted = SimpleEncryption::encrypt(&payload.value);

    match state.shared_state.database_pool.api_keys.save_key(key_type, encrypted) {
        Ok(_) => {
            Ok(Json(SaveApiKeyResponse {
                success: true,
                message: format!("API key saved for {}", payload.key_type),
            }))
        }
        Err(e) => {
            error!("Failed to save API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get an API key by type
pub async fn get_api_key(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let key_type_str = params.get("key_type")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let key_type = ApiKeyType::from_str(key_type_str)
        .ok_or(StatusCode::BAD_REQUEST)?;

    match state.shared_state.database_pool.api_keys.get_key(key_type) {
        Ok(Some(record)) => {
            // Decrypt the value
            let decrypted = SimpleEncryption::decrypt(&record.encrypted_value)
                .unwrap_or_else(|_| String::from("[decryption_failed]"));

            Ok(Json(GetApiKeyResponse {
                key_type: record.key_type,
                value: Some(decrypted),
                created_at: Some(record.created_at.to_rfc3339()),
                last_used_at: record.last_used_at.map(|dt| dt.to_rfc3339()),
                last_mode: record.last_mode,
                usage_count: Some(record.usage_count),
            }))
        }
        Ok(None) => {
            Ok(Json(GetApiKeyResponse {
                key_type: key_type_str.clone(),
                value: None,
                created_at: None,
                last_used_at: None,
                last_mode: None,
                usage_count: None,
            }))
        }
        Err(e) => {
            error!("Failed to get API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get all API keys (with values decrypted)
pub async fn get_all_api_keys(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    match state.shared_state.database_pool.api_keys.get_all_keys() {
        Ok(records) => {
            let keys: Vec<GetApiKeyResponse> = records.into_iter().map(|record| {
                let decrypted = SimpleEncryption::decrypt(&record.encrypted_value)
                    .unwrap_or_else(|_| String::from("[decryption_failed]"));

                GetApiKeyResponse {
                    key_type: record.key_type,
                    value: Some(decrypted),
                    created_at: Some(record.created_at.to_rfc3339()),
                    last_used_at: record.last_used_at.map(|dt| dt.to_rfc3339()),
                    last_mode: record.last_mode,
                    usage_count: Some(record.usage_count),
                }
            }).collect();

            Ok(Json(GetAllApiKeysResponse { keys }))
        }
        Err(e) => {
            error!("Failed to get all API keys: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete an API key
pub async fn delete_api_key(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let key_type_str = params.get("key_type")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let key_type = ApiKeyType::from_str(key_type_str)
        .ok_or(StatusCode::BAD_REQUEST)?;

    match state.shared_state.database_pool.api_keys.delete_key(key_type) {
        Ok(true) => {
            info!("API key deleted for: {}", key_type_str);
            Ok(Json(serde_json::json!({
                "success": true,
                "message": format!("API key deleted for {}", key_type_str)
            })))
        }
        Ok(false) => {
            warn!("API key not found for deletion: {}", key_type_str);
            Ok(Json(serde_json::json!({
                "success": false,
                "message": format!("API key not found for {}", key_type_str)
            })))
        }
        Err(e) => {
            error!("Failed to delete API key: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Mark an API key as used with the current mode
#[derive(Debug, Deserialize)]
pub struct MarkKeyUsedRequest {
    pub key_type: String,
    pub mode: String,  // "offline" or "online"
}

pub async fn mark_key_used(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<MarkKeyUsedRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let key_type = ApiKeyType::from_str(&payload.key_type)
        .ok_or(StatusCode::BAD_REQUEST)?;

    match state.shared_state.database_pool.api_keys.mark_used(key_type, &payload.mode) {
        Ok(_) => {
            Ok(Json(serde_json::json!({
                "success": true,
                "message": "Key usage recorded"
            })))
        }
        Err(e) => {
            error!("Failed to mark key as used: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
