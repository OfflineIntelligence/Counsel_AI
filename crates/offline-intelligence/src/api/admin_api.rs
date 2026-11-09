//! Admin API endpoints
//! 
//! This module provides administrative functionality for system management.
//! Currently a placeholder for future implementation.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::{Arc, atomic::Ordering};
use serde::{Deserialize, Serialize};

use crate::shared_state::SharedState;

/// System health response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
}

/// Database statistics response
#[derive(Debug, Serialize)]
pub struct DbStatsResponse {
    pub total_sessions: usize,
    pub total_messages: usize,
    pub total_summaries: usize,
    pub database_size_bytes: u64,
}

/// Maintenance request
#[derive(Debug, Deserialize)]
pub struct MaintenanceRequest {
    pub operation: String,
    pub parameters: Option<serde_json::Value>,
}


// Store application start time as seconds since epoch
static START_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn init_start_time() {
    let now: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_secs();
    START_TIME.store(now, Ordering::SeqCst);
}

fn get_uptime_seconds() -> u64 {
    let start = START_TIME.load(Ordering::SeqCst);
    if start == 0 {
        // Initialize on first call
        init_start_time();
        return 0;
    }
    
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_secs();
    
    now.saturating_sub(start)
}

/// Health check endpoint
pub async fn health(
    State(_shared_state): State<Arc<SharedState>>,
) -> Result<impl IntoResponse, StatusCode> {
    // Initialize start time if not already set
    if START_TIME.load(Ordering::SeqCst) == 0 {
        init_start_time();
    }
    
    Ok((
        StatusCode::OK,
        Json(HealthResponse {
            status: "healthy".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: get_uptime_seconds(),
        }),
    ))
}

/// Database statistics endpoint
pub async fn db_stats(
    State(shared_state): State<Arc<SharedState>>,
) -> Result<impl IntoResponse, StatusCode> {
    // Access the database to get actual statistics
    let total_sessions = shared_state.conversations.counters.active_sessions.load(Ordering::Relaxed);
    
    let total_messages = shared_state.conversations.counters.processed_messages.load(Ordering::Relaxed);
    
    // Get cache statistics
    let cache_hits = shared_state.conversations.counters.cache_hits.load(Ordering::Relaxed);
    let cache_misses = shared_state.conversations.counters.cache_misses.load(Ordering::Relaxed);
    let total_summaries = cache_hits + cache_misses; // Approximation
    
    // The real database (memory.db), not a guessed relative path.
    let database_size_bytes = shared_state
        .database_pool
        .get_stats()
        .map(|s| s.database_size_bytes.max(0) as u64)
        .unwrap_or(0);
    
    Ok((
        StatusCode::OK,
        Json(DbStatsResponse {
            total_sessions,
            total_messages,
            total_summaries,
            database_size_bytes,
        }),
    ))
}

/// Maintenance endpoint - performs system maintenance operations
pub async fn maintenance(
    State(shared_state): State<Arc<SharedState>>,
    Json(payload): Json<MaintenanceRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Sessions not used for this long leave the in-memory mirror (the
    // database keeps them). parameters.idle_seconds overrides; default 1 hour.
    let idle = payload
        .parameters
        .as_ref()
        .and_then(|p| p.get("idle_seconds"))
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    match payload.operation.as_str() {
        "cleanup_expired_sessions" | "clear_inactive_sessions" => {
            let sessions = &shared_state.conversations.sessions;
            let initial_count = sessions.len();
            let limit = std::time::Duration::from_secs(idle);
            sessions.retain(|_, s| match s.read() {
                Ok(s) => s.pinned || s.last_accessed.elapsed() < limit,
                Err(_) => false,
            });
            let removed = initial_count - sessions.len();
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "message": format!("Removed {} session(s) idle for {} s from memory", removed, idle),
                    "operation": payload.operation,
                    "initial_session_count": initial_count,
                    "removed": removed,
                    "status": "completed"
                })),
            ))
        }
        "optimize_database" => {
            let db = Arc::clone(&shared_state.database_pool);
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let mut conn = db.conversations.get_conn_public()?;
                crate::memory_db::migration::run_maintenance(&mut conn)?;
                Ok(())
            })
            .await;
            match result {
                Ok(Ok(())) => Ok((
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "message": "Database optimized",
                        "operation": "optimize_database",
                        "status": "completed"
                    })),
                )),
                Ok(Err(e)) => Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "message": e.to_string(), "status": "error" })),
                )),
                Err(e) => Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "message": e.to_string(), "status": "error" })),
                )),
            }
        }
        _ => Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "message": format!("Unknown maintenance operation: {}", payload.operation),
                "supported_operations": ["cleanup_expired_sessions", "optimize_database", "clear_inactive_sessions"],
                "status": "error"
            })),
        )),
    }
}
