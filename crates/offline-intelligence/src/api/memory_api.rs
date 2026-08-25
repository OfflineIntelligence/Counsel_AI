//! Memory management API endpoints

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn, error};
use serde::{Deserialize, Serialize};

use crate::shared_state::SharedState;
use crate::metrics;

// --- Error Handling ---

/// Custom error type for API validation and processing failures
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
                "code": self.status.as_u16(),
            })),
        )
            .into_response()
    }
}

// --- Validation Helpers ---

/// Validate session ID format (alphanumeric, dashes, underscores)
fn validate_session_id(session_id: &str) -> Result<(), ApiError> {
    if session_id.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Session ID cannot be empty".to_string(),
        });
    }

    if session_id.len() > 256 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Session ID too long (max 256 chars)".to_string(),
        });
    }

    if !session_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Session ID contains invalid characters".to_string(),
        });
    }

    Ok(())
}

/// Validate the integrity and safety of the message list
fn validate_messages(messages: &[crate::memory::Message]) -> Result<(), ApiError> {
    if messages.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "At least one message is required".to_string(),
        });
    }

    if messages.len() > 1000 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Too many messages (max 1000)".to_string(),
        });
    }

    for (idx, msg) in messages.iter().enumerate() {
        if msg.role.is_empty() || msg.content.is_empty() {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("Message {} has empty role or content", idx + 1),
            });
        }

        if msg.content.len() > 65_536 {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "Message {} content exceeds 64KB limit",
                    idx + 1
                ),
            });
        }

        if msg.content.contains('\0') {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "Message {} contains illegal null bytes",
                    idx + 1
                ),
            });
        }
    }

    Ok(())
}

// --- Response Types (since we can't import from context_engine yet) ---

#[derive(Debug, Serialize)]
pub struct SessionStats {
    pub total_messages: usize,
    pub optimized_messages: usize,
    pub compression_ratio: f32,
    pub last_accessed: Option<String>,
    pub memory_size_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CleanupStats {
    pub messages_removed: usize,
    pub final_count: usize,
    pub memory_freed_bytes: Option<usize>,
}

// --- API Handlers ---

/// Optimize conversation context with input validation
pub async fn memory_optimize(
    State(shared_state): State<Arc<SharedState>>,
    Json(payload): Json<MemoryOptimizeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // 1. Validation
    validate_session_id(&payload.session_id)?;
    validate_messages(&payload.messages)?;

    if let Some(ref query) = payload.user_query {
        if query.len() > 8_192 {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "User query too long (max 8KB)".to_string(),
            });
        }
    }

    // 2. Process
    let ctx_budget = shared_state.current_context_budget().await;
    let mut orchestrator_guard = shared_state.context_orchestrator.write().await;

    if let Some(orchestrator) = &mut *orchestrator_guard {
        match orchestrator
            .process_conversation(
                &payload.session_id,
                &payload.messages,
                payload.user_query.as_deref(),
                ctx_budget,
            )
            .await
        {
            Ok(optimized) => {
                metrics::inc_request("memory_optimize", "ok");

                let original_len: usize = payload.messages.len();
                let optimized_len: usize = optimized.len();

                let response = json!({
                    "optimized_messages": optimized,
                    "original_count": original_len,
                    "optimized_count": optimized_len,
                    "compression_ratio": if original_len > 0 {
                        (original_len as f32 - optimized_len as f32) / original_len as f32
                    } else {
                        0.0
                    }
                });

                Ok((StatusCode::OK, Json(response)))
            }
            Err(e) => {
                metrics::inc_request("memory_optimize", "error");
                warn!(
                    "Optimization failed for session {}: {}",
                    payload.session_id,
                    e
                );
                Err(ApiError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("Optimization failed: {}", e),
                })
            }
        }
    } else {
        metrics::inc_request("memory_optimize", "disabled");
        Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Memory system not available".to_string(),
        })
    }
}

/// Get memory statistics for a specific session
pub async fn memory_stats(
    State(shared_state): State<Arc<SharedState>>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    validate_session_id(&session_id)?;

    let orchestrator_guard = shared_state.context_orchestrator.read().await;

    if let Some(orchestrator) = &*orchestrator_guard {
        match orchestrator.get_session_stats(&session_id).await {
            Ok(session_stats) => {
                let stats = SessionStats {
                    total_messages: session_stats.tier_stats.tier1_count + 
                                   session_stats.tier_stats.tier3_count,
                    optimized_messages: session_stats.tier_stats.tier1_count,
                    compression_ratio: if session_stats.tier_stats.tier1_count > 0 {
                        session_stats.tier_stats.tier3_count as f32
                        / session_stats.tier_stats.tier1_count as f32
                    } else {
                        0.0
                    },
                    last_accessed: None, // Not available in current TierStats
                    memory_size_bytes: Some((session_stats.tier_stats.tier1_count + 
                                           session_stats.tier_stats.tier3_count) * 1024), // Estimate 1KB per message
                };
        
                metrics::inc_request("memory_stats", "ok");
                Ok((StatusCode::OK, Json(stats)))
            }
            Err(e) => {
                metrics::inc_request("memory_stats", "error");
                warn!("Failed to get session stats for {}: {}", session_id, e);
                Err(ApiError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("Failed to retrieve session statistics: {}", e),
                })
            }
        }
    } else {
        metrics::inc_request("memory_stats", "disabled");
        Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Memory system not available".to_string(),
        })
    }
}

/// Clean up old memory data within specified time bounds
pub async fn memory_cleanup(
    State(shared_state): State<Arc<SharedState>>,
    Json(payload): Json<MemoryCleanupRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Range validation: 1 hour to 1 year
    if !(3_600..=31_536_000).contains(&payload.older_than_seconds) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Cleanup threshold must be between 1 hour and 1 year".to_string(),
        });
    }

    let mut orchestrator_guard = shared_state.context_orchestrator.write().await;

    if let Some(orchestrator) = &mut *orchestrator_guard {
        match orchestrator.cleanup(payload.older_than_seconds).await {
            Ok(cleanup_stats) => {
                let stats = CleanupStats {
                    messages_removed: cleanup_stats.sessions_cleaned + cleanup_stats.cache_entries_cleaned,
                    final_count: cleanup_stats.sessions_cleaned, // Approximate remaining count
                    memory_freed_bytes: Some(cleanup_stats.cache_entries_cleaned * 1024), // Estimate 1KB per entry
                };
        
        info!("Memory cleanup completed: {:?}", stats);
                metrics::inc_request("memory_cleanup", "ok");
                Ok((StatusCode::OK, Json(stats)))
            }
            Err(e) => {
                metrics::inc_request("memory_cleanup", "error");
                error!("Memory cleanup failed: {}", e);
                Err(ApiError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("Memory cleanup failed: {}", e),
                })
            }
        }
    } else {
        metrics::inc_request("memory_cleanup", "disabled");
        Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Memory system not available".to_string(),
        })
    }
}

// --- Data Structures ---

#[derive(Debug, Deserialize)]
pub struct MemoryOptimizeRequest {
    pub session_id: String,
    pub messages: Vec<crate::memory::Message>,
    pub user_query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MemoryCleanupRequest {
    pub older_than_seconds: u64,
}