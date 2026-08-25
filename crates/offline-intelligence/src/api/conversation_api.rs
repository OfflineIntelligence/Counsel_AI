//! API endpoints for conversation/session management

use axum::{
    extract::{State, Path},
    response::{IntoResponse, Response},
    Json,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, error};

use crate::shared_state::UnifiedAppState;

/// Response for fetching all conversations
#[derive(Debug, Serialize)]
pub struct ConversationsResponse {
    pub conversations: Vec<ConversationSummary>,
}

/// Summary of a conversation for the sidebar
#[derive(Debug, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub last_accessed: String,
    pub message_count: usize,
    pub pinned: bool,
}

/// Response for fetching a specific conversation's messages
#[derive(Debug, Serialize)]
pub struct ConversationDetailResponse {
    pub id: String,
    pub title: String,
    pub messages: Vec<MessageResponse>,
}

/// Message format for API response
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub role: String,
    pub content: String,
}

/// Create a new conversation and return its ID + title
pub async fn create_conversation(
    State(state): State<UnifiedAppState>,
) -> Result<Json<Value>, Response> {
    info!("Creating new conversation");

    let orchestrator_lock = state.context_orchestrator.read().await;

    if let Some(ref orchestrator) = *orchestrator_lock {
        match orchestrator.database().conversations.create_session(None) {
            Ok(session) => {
                let title = session
                    .metadata
                    .title
                    .unwrap_or_else(|| "New Chat".to_string());
                info!("Created conversation: {}", session.id);
                Ok(Json(serde_json::json!({
                    "id": session.id,
                    "title": title
                })))
            }
            Err(e) => {
                error!("Failed to create conversation: {}", e);
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Database error: {}", e),
                )
                    .into_response())
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}

/// Fetch all conversations/sessions from the database
pub async fn get_conversations(
    State(state): State<UnifiedAppState>,
) -> Result<Json<ConversationsResponse>, Response> {
    info!("Fetching all conversations");
    
    let orchestrator_lock = state.context_orchestrator.read().await;
    
    if let Some(ref orchestrator) = *orchestrator_lock {
        match orchestrator.database().conversations.get_all_sessions() {
            Ok(sessions) => {
                let mut conversations = Vec::new();
                
                for session in sessions {
                    // Get message count for this session
                    let message_count = orchestrator.database().conversations
                        .get_session_message_count(&session.id)
                        .unwrap_or(0);
                    
                    // Include sessions that have a title OR have messages (even if title not generated yet)
                    if let Some(ref title) = session.metadata.title {
                        conversations.push(ConversationSummary {
                            id: session.id.clone(),
                            title: title.clone(),
                            created_at: session.created_at.to_rfc3339(),
                            last_accessed: session.last_accessed.to_rfc3339(),
                            message_count,
                            pinned: session.metadata.pinned,
                        });
                    } else if message_count > 0 {
                        // Get first user message for title
                        let first_message = orchestrator.database().conversations
                            .get_session_messages(&session.id, Some(1), Some(0))
                            .ok()
                            .and_then(|msgs| msgs.into_iter().find(|m| m.role == "user"));
                        
                        let title = first_message
                            .map(|m| {
                                let content = m.content.chars().take(50).collect::<String>();
                                if m.content.len() > 50 {
                                    format!("{}...", content)
                                } else {
                                    content
                                }
                            })
                            .unwrap_or_else(|| format!("Chat {}", session.created_at.format("%b %d, %Y")));
                        
                        conversations.push(ConversationSummary {
                            id: session.id.clone(),
                            title,
                            created_at: session.created_at.to_rfc3339(),
                            last_accessed: session.last_accessed.to_rfc3339(),
                            message_count,
                            pinned: session.metadata.pinned,
                        });
                    }
                }
                
                info!("Found {} conversations", conversations.len());
                Ok(Json(ConversationsResponse { conversations }))
            }
            Err(e) => {
                error!("Failed to fetch conversations: {}", e);
                Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response())
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}

/// Fetch a specific conversation's messages
pub async fn get_conversation(
    State(state): State<UnifiedAppState>,
    Path(session_id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, Response> {
    info!("Fetching conversation: {}", session_id);
    
    let orchestrator_lock = state.context_orchestrator.read().await;
    
    if let Some(ref orchestrator) = *orchestrator_lock {
        // Get session metadata
        let session = match orchestrator.database().conversations.get_session(&session_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Err((StatusCode::NOT_FOUND, "Conversation not found").into_response());
            }
            Err(e) => {
                error!("Failed to fetch session: {}", e);
                return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response());
            }
        };
        
        // Get messages
        let messages = match orchestrator.database().conversations.get_session_messages(&session_id, None, None) {
            Ok(msgs) => msgs.into_iter()
                .map(|msg| MessageResponse {
                    role: msg.role,
                    content: msg.content,
                })
                .collect(),
            Err(e) => {
                error!("Failed to fetch messages: {}", e);
                return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response());
            }
        };
        
        Ok(Json(ConversationDetailResponse {
            id: session.id,
            title: session.metadata.title.unwrap_or_else(|| "New Chat".to_string()),
            messages,
        }))
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}

/// Request to update a conversation's title
#[derive(Debug, Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

/// Update a conversation's title
pub async fn update_conversation_title(
    State(state): State<UnifiedAppState>,
    Path(session_id): Path<String>,
    Json(req): Json<UpdateTitleRequest>,
) -> Result<Json<Value>, Response> {
    info!("Updating title for conversation: {}", session_id);
    
    if req.title.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Title cannot be empty").into_response());
    }
    
    let orchestrator_lock = state.context_orchestrator.read().await;
    
    if let Some(ref orchestrator) = *orchestrator_lock {
        match orchestrator.database().conversations.update_session_title(&session_id, &req.title) {
            Ok(_) => {
                info!("Successfully updated title for conversation: {}", session_id);
                Ok(Json(serde_json::json!({
                    "success": true,
                    "id": session_id,
                    "title": req.title
                })))
            }
            Err(e) => {
                // Standardize on 500 so the frontend handles all DB failures uniformly
                error!("Failed to update conversation title for session {}: {}", session_id, e);
                Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response())
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}

/// Delete a conversation permanently from the database
/// Called via DELETE /conversations/:id from frontend
/// Returns success JSON or error status code with message
pub async fn delete_conversation(
    State(state): State<UnifiedAppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, Response> {
    info!("Deleting conversation: {}", session_id);
    
    let orchestrator_lock = state.context_orchestrator.read().await;
    
    if let Some(ref orchestrator) = *orchestrator_lock {
        match orchestrator.database().conversations.delete_session(&session_id) {
            Ok(deleted_count) => {
                if deleted_count == 0 {
                    info!("Conversation not found for deletion: {}", session_id);
                    Err((StatusCode::NOT_FOUND, format!("Conversation not found: {}", session_id)).into_response())
                } else {
                    info!("Successfully deleted conversation: {}", session_id);
                    Ok(Json(serde_json::json!({
                        "success": true,
                        "id": session_id
                    })))
                }
            }
            Err(e) => {
                error!("Failed to delete conversation: {}", e);
                // Return detailed error to help with debugging
                Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response())
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}

/// DB-level statistics response for the Conversations (memory.db) Tier 4 view
#[derive(Debug, Serialize)]
pub struct ConversationsDbStatsResponse {
    /// Absolute path to memory.db on disk
    pub db_path: String,
    /// File size in bytes (0 if in-memory DB)
    pub db_size_bytes: u64,
    /// Human-readable file size
    pub db_size_human: String,
    /// Total number of chat sessions ever recorded
    pub total_sessions: i64,
    /// Total messages stored across all sessions
    pub total_messages: i64,
    /// Total detail records (structured facts extracted from conversations)
    pub total_details: i64,
    /// Most recently accessed session timestamp (RFC-3339) or null
    pub last_accessed: Option<String>,
}

fn format_bytes_local(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 { return "0 B".to_string(); }
    let i = (bytes as f64).log(1024.0).floor() as usize;
    let i = i.min(UNITS.len() - 1);
    let val = bytes as f64 / 1024_f64.powi(i as i32);
    format!("{:.1} {}", val, UNITS[i])
}

/// GET /conversations/db-stats — returns memory.db statistics for the Tier 4 view
pub async fn get_conversations_db_stats(
    State(state): State<UnifiedAppState>,
) -> Result<Json<ConversationsDbStatsResponse>, Response> {
    info!("Fetching conversations DB stats");

    let orchestrator_lock = state.context_orchestrator.read().await;

    let (
        total_sessions,
        total_messages,
        total_details,
        last_accessed,
    ) = if let Some(ref orchestrator) = *orchestrator_lock {
        let db = orchestrator.database();
        let conn_result = db.conversations.get_conn_public();
        match conn_result {
            Ok(conn) => {
                let count = |sql: &str| -> i64 {
                    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
                };
                let sessions    = count("SELECT COUNT(*) FROM sessions");
                let messages    = count("SELECT COUNT(*) FROM messages");
                let details     = count("SELECT COUNT(*) FROM details");
                let last: Option<String> = conn
                    .query_row(
                        "SELECT MAX(last_accessed) FROM sessions",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(None);
                (sessions, messages, details, last)
            }
            Err(e) => {
                error!("Failed to get DB connection for stats: {}", e);
                (0, 0, 0, None)
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response());
    };

    // Resolve the DB file path (same logic as thread_server)
    let db_path = crate::config::get_app_data_dir()
        .join("data")
        .join("memory.db");

    let db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    Ok(Json(ConversationsDbStatsResponse {
        db_path: db_path.to_string_lossy().to_string(),
        db_size_bytes,
        db_size_human: format_bytes_local(db_size_bytes),
        total_sessions,
        total_messages,
        total_details,
        last_accessed,
    }))
}

/// Request to update a conversation's pinned status
#[derive(Debug, Deserialize)]
pub struct UpdatePinnedRequest {
    pub pinned: bool,
}

/// Update a conversation's pinned status
pub async fn update_conversation_pinned(
    State(state): State<UnifiedAppState>,
    Path(session_id): Path<String>,
    Json(req): Json<UpdatePinnedRequest>,
) -> Result<Json<Value>, Response> {
    info!("Updating pinned status for conversation: {} to {}", session_id, req.pinned);
    
    let orchestrator_lock = state.context_orchestrator.read().await;
    
    if let Some(ref orchestrator) = *orchestrator_lock {
        match orchestrator.database().conversations.update_session_pinned(&session_id, req.pinned) {
            Ok(_) => {
                info!("Successfully updated pinned status for conversation: {}", session_id);
                Ok(Json(serde_json::json!({
                    "success": true,
                    "id": session_id,
                    "pinned": req.pinned
                })))
            }
            Err(e) => {
                let error_msg = e.to_string();
                // Check if the error is due to session not found
                if error_msg.contains("not found") {
                    error!("Conversation not found: {}", session_id);
                    Err((StatusCode::NOT_FOUND, format!("Conversation not found: {}", session_id)).into_response())
                } else {
                    error!("Failed to update conversation pinned status: {}", e);
                    Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response())
                }
            }
        }
    } else {
        error!("Context orchestrator not initialized");
        Err((StatusCode::SERVICE_UNAVAILABLE, "Memory system not available").into_response())
    }
}
