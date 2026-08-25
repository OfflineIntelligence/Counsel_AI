// "D:\_ProjectWorks\AUDIO_Interface\Server\src\memory_db\schema.rs"
//! Database schema definitions for the memory system

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Represents a conversation session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub metadata: SessionMetadata,
}

// Chat persistence: Session metadata with serde defaults for backward compatibility with existing database records
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMetadata {
    pub title: Option<String>,
    #[serde(default)]  // Handle old records missing this field
    pub tags: Vec<String>,
    #[serde(default)]  // Handle old records missing this field
    pub user_defined: HashMap<String, String>,
    #[serde(default)]  // Handle old records missing this field
    pub pinned: bool,
}

/// Represents a single message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub session_id: String,
    pub message_index: i32,
    pub role: String,
    pub content: String,
    pub tokens: i32,
    pub timestamp: DateTime<Utc>,
    pub importance_score: f32,
}

/// Represents a preserved detail from a message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detail {
    pub id: i64,
    pub session_id: String,
    pub message_id: i64,
    pub detail_type: String,
    pub content: String,
    pub context: String,
    pub importance_score: f32,
    pub accessed_count: i32,
    pub last_accessed: DateTime<Utc>,
}

/// Database statistics
#[derive(Debug, Clone)]
pub struct DatabaseStats {
    pub total_sessions: i64,
    pub total_messages: i64,
    pub total_details: i64,
    pub database_size_bytes: i64,
}

/// SQL statements for table creation
pub const SCHEMA_SQL: &str = "
-- Sessions table
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at TIMESTAMP NOT NULL,
    last_accessed TIMESTAMP NOT NULL,
    metadata TEXT NOT NULL
);

-- Messages table
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    message_index INTEGER NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    tokens INTEGER NOT NULL,
    timestamp TIMESTAMP NOT NULL,
    importance_score REAL NOT NULL DEFAULT 0.5,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    UNIQUE(session_id, message_index)
);

-- Details table
CREATE TABLE IF NOT EXISTS details (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    message_id INTEGER NOT NULL,
    detail_type TEXT NOT NULL,
    content TEXT NOT NULL,
    context TEXT NOT NULL,
    importance_score REAL NOT NULL,
    accessed_count INTEGER NOT NULL DEFAULT 0,
    last_accessed TIMESTAMP NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages (session_id);
CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages (timestamp);
CREATE INDEX IF NOT EXISTS idx_details_session ON details (session_id);
CREATE INDEX IF NOT EXISTS idx_details_type ON details (detail_type);
";