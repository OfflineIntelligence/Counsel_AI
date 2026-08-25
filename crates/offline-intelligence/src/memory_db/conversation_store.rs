//! Conversation storage and retrieval operations with batch support and safe parsing

use crate::memory_db::schema::*;
use rusqlite::{params, Result, Row, Connection};
use chrono::{DateTime, Utc, NaiveDateTime};
use uuid::Uuid;
use tracing::{info, debug, warn};
use std::sync::Arc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// Parameters for storing a message
pub struct MessageParams<'a> {
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub message_index: i32,
    pub tokens: i32,
    pub importance_score: f32,
}

/// Manages conversation storage and retrieval using a connection pool
pub struct ConversationStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl ConversationStore {
    /// Create a new conversation store with a shared connection pool
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Internal helper to get a connection from the pool
    fn get_conn(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.pool.get().map_err(|e| anyhow::anyhow!("Failed to get connection from pool: {}", e))
    }

    /// Public connection accessor for cross-module queries (e.g., search_api)
    pub fn get_conn_public(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.get_conn()
    }

    // --- Transactional & Internal Helpers ---

    /// Internal helper to update session access using an existing connection or transaction
    fn update_session_access_with_conn(&self, conn: &Connection, session_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE sessions SET last_accessed = ?1 WHERE id = ?2",
            params![now, session_id],
        )?;
        Ok(())
    }

    /// Store a message using an external transaction
    pub fn store_message_with_tx(
        &self,
        tx: &mut Connection,
        params: MessageParams,
    ) -> anyhow::Result<StoredMessage> {
        // Update session access time
        self.update_session_access_with_conn(tx, params.session_id)?;
        
        let now = Utc::now();
        
        tx.execute(
            "INSERT INTO messages 
             (session_id, message_index, role, content, tokens, timestamp, importance_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                params.session_id,
                params.message_index,
                params.role,
                params.content,
                params.tokens,
                now.to_rfc3339(),
                params.importance_score,
            ],
        )?;
        
        let id = tx.last_insert_rowid();
        
        Ok(StoredMessage {
            id,
            session_id: params.session_id.to_string(),
            message_index: params.message_index,
            role: params.role.to_string(),
            content: params.content.to_string(),
            tokens: params.tokens,
            timestamp: now,
            importance_score: params.importance_score,
        })
    }

    // --- Batch Operations ---

    /// Store multiple messages in batch
    pub fn store_messages_batch(
        &self,
        session_id: &str,
        messages: &[(String, String, i32, i32, f32)], // (role, content, index, tokens, importance)
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let mut conn = self.get_conn()?;
        
        self.update_session_access_with_conn(&conn, session_id)?;
        
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let mut stored_messages = Vec::new();
        
        let tx = conn.transaction()?;
        {
            for (role, content, message_index, tokens, importance_score) in messages.iter() {
                tx.execute(
                    "INSERT INTO messages 
                     (session_id, message_index, role, content, tokens, timestamp, importance_score)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![session_id, message_index, role, content, tokens, &now_str, importance_score],
                )?;
                
                let id = tx.last_insert_rowid();
                
                stored_messages.push(StoredMessage {
                    id,
                    session_id: session_id.to_string(),
                    message_index: *message_index,
                    role: role.clone(),
                    content: content.clone(),
                    tokens: *tokens,
                    timestamp: now,
                    importance_score: *importance_score,
                });

                // Periodic commit check can be handled by outer logic or left to the full transaction
                // Note: manual "COMMIT; BEGIN;" inside a rusqlite Transaction is generally discouraged.
            }
        }
        tx.commit()?;
        
        debug!("Stored {} messages in batch for session {}", messages.len(), session_id);
        Ok(stored_messages)
    }

    /// Store details in batch
    pub fn store_details_batch(
        &self,
        details: &[(&str, i64, &str, &str, &str, f32)], // (session_id, message_id, type, content, context, importance)
    ) -> anyhow::Result<()> {
        if details.is_empty() { return Ok(()); }
        
        let mut conn = self.get_conn()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        
        for (session_id, message_id, detail_type, content, context, importance_score) in details {
            tx.execute(
                "INSERT INTO details 
                 (session_id, message_id, detail_type, content, context, importance_score, accessed_count, last_accessed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![session_id, message_id, detail_type, content, context, importance_score, 0, &now],
            )?;
        }
        
        tx.commit()?;
        debug!("Stored {} details in batch", details.len());
        Ok(())
    }

    // --- Session & Message Management ---

    pub fn create_session(&self, metadata: Option<SessionMetadata>) -> anyhow::Result<Session> {
        let session_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let metadata = metadata.unwrap_or_default();
        let metadata_json = serde_json::to_string(&metadata)?;
        
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO sessions (id, created_at, last_accessed, metadata) VALUES (?1, ?2, ?3, ?4)",
            params![&session_id, now.to_rfc3339(), now.to_rfc3339(), metadata_json],
        )?;
        
        Ok(Session { id: session_id, created_at: now, last_accessed: now, metadata })
    }

    /// Chat persistence: Create session with frontend-provided ID to maintain ID consistency across frontend and backend
    pub fn create_session_with_id(&self, session_id: &str, metadata: Option<SessionMetadata>) -> anyhow::Result<Session> {
        let now = Utc::now();
        let metadata = metadata.unwrap_or_default();
        let metadata_json = serde_json::to_string(&metadata)?;
        
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO sessions (id, created_at, last_accessed, metadata) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, now.to_rfc3339(), now.to_rfc3339(), metadata_json],
        )?;
        
        info!("Created session with ID: {}", session_id);
        Ok(Session { id: session_id.to_string(), created_at: now, last_accessed: now, metadata })
    }

    /// Chat persistence: Update session title after auto-generation, also refresh last_accessed.
    /// Handles the race condition where title update arrives before the session is created
    /// by the streaming endpoint, using INSERT OR IGNORE to avoid UNIQUE constraint failures.
    pub fn update_session_title(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        let conn = self.get_conn()?;
        let now = Utc::now();

        // Ensure the session exists first (INSERT OR IGNORE handles concurrent creation)
        let default_metadata = SessionMetadata {
            title: Some(title.to_string()),
            ..Default::default()
        };
        let default_metadata_json = serde_json::to_string(&default_metadata)?;

        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, created_at, last_accessed, metadata) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, now.to_rfc3339(), now.to_rfc3339(), default_metadata_json],
        )?;

        // Now fetch and update metadata (session is guaranteed to exist)
        let mut stmt = conn.prepare("SELECT metadata FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query([session_id])?;

        if let Some(row) = rows.next()? {
            let metadata_json: String = row.get(0)?;
            let mut metadata: SessionMetadata = serde_json::from_str(&metadata_json)
                .unwrap_or_default();

            metadata.title = Some(title.to_string());
            let updated_metadata_json = serde_json::to_string(&metadata)?;

            conn.execute(
                "UPDATE sessions SET metadata = ?1, last_accessed = ?2 WHERE id = ?3",
                params![updated_metadata_json, now.to_rfc3339(), session_id],
            )?;

            info!("Updated session {} title to: {}", session_id, title);
        }

        Ok(())
    }

    pub fn update_session_pinned(&self, session_id: &str, pinned: bool) -> anyhow::Result<()> {
        let conn = self.get_conn()?;
        
        // Fetch current metadata
        let mut stmt = conn.prepare("SELECT metadata FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query([session_id])?;
        
        if let Some(row) = rows.next()? {
            let metadata_json: String = row.get(0)?;
            let mut metadata: SessionMetadata = serde_json::from_str(&metadata_json)
                .unwrap_or_default();
            
            // Update pinned status
            metadata.pinned = pinned;
            let updated_metadata_json = serde_json::to_string(&metadata)?;
            
            // Update session with new metadata only; pinning is a UI organization action
            // and does not constitute accessing the conversation content, so don't update last_accessed
            conn.execute(
                "UPDATE sessions SET metadata = ?1 WHERE id = ?2",
                params![updated_metadata_json, session_id],
            )?;
            
            info!("Updated session {} pinned status to: {}", session_id, pinned);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session {} not found", session_id))
        }
    }

    pub fn get_session(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare("SELECT id, created_at, last_accessed, metadata FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query([session_id])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some(self.row_to_session(row)?))
        } else {
            Ok(None)
        }
    }

    /// Chat persistence: Retrieve all sessions for sidebar display, ordered by recency
    pub fn get_all_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, created_at, last_accessed, metadata FROM sessions ORDER BY last_accessed DESC"
        )?;
        let mut rows = stmt.query([])?;
        let mut sessions = Vec::new();
        
        while let Some(row) = rows.next()? {
            sessions.push(self.row_to_session(row)?);
        }
        
        Ok(sessions)
    }

    // --- Parsing Logic ---

    fn parse_datetime_safe(datetime_str: &str) -> Option<DateTime<Utc>> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(datetime_str) {
            return Some(dt.with_timezone(&Utc));
        }
        if let Ok(dt) = DateTime::parse_from_str(datetime_str, "%+") {
            return Some(dt.with_timezone(&Utc));
        }
        if let Ok(dt) = NaiveDateTime::parse_from_str(datetime_str, "%Y-%m-%d %H:%M:%S") {
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }
        if let Ok(dt) = NaiveDateTime::parse_from_str(datetime_str, "%Y-%m-%d %H:%M:%S%.f") {
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }
        None
    }

    fn row_to_session(&self, row: &Row) -> anyhow::Result<Session> {
        let metadata_json: String = row.get(3)?;
        let metadata: SessionMetadata = serde_json::from_str(&metadata_json)
            .map_err(|e| anyhow::anyhow!("Metadata JSON error: {}", e))?;
        
        let created_at = Self::parse_datetime_safe(&row.get::<_, String>(1)?)
            .unwrap_or_else(|| { warn!("Failed parse created_at"); Utc::now() });
            
        let last_accessed = Self::parse_datetime_safe(&row.get::<_, String>(2)?)
            .unwrap_or_else(|| { warn!("Failed parse last_accessed"); Utc::now() });
        
        Ok(Session { id: row.get(0)?, created_at, last_accessed, metadata })
    }

    fn row_to_stored_message(&self, row: &Row) -> anyhow::Result<StoredMessage> {
        let timestamp = Self::parse_datetime_safe(&row.get::<_, String>(6)?)
            .unwrap_or_else(|| { warn!("Failed parse message timestamp"); Utc::now() });
        
        Ok(StoredMessage {
            id: row.get(0)?,
            session_id: row.get(1)?,
            message_index: row.get(2)?,
            role: row.get(3)?,
            content: row.get(4)?,
            tokens: row.get(5)?,
            timestamp,
            importance_score: row.get(7)?,
        })
    }

    // --- Standard Operations (Existing) ---

    pub fn get_session_messages(&self, session_id: &str, limit: Option<i32>, offset: Option<i32>) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, message_index, role, content, tokens, timestamp, importance_score
             FROM messages WHERE session_id = ?1 ORDER BY message_index LIMIT ?2 OFFSET ?3"
        )?;
        let mut rows = stmt.query(params![session_id, limit.unwrap_or(1000), offset.unwrap_or(0)])?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next()? { messages.push(self.row_to_stored_message(row)?); }
        Ok(messages)
    }

    pub fn get_session_message_count(&self, session_id: &str) -> anyhow::Result<usize> {
        let conn = self.get_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            [session_id],
            |row| row.get(0)
        )?;
        Ok(count as usize)
    }

    pub fn delete_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let conn = self.get_conn()?;
        let deleted = conn.execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        info!("Deleted session {}", session_id);
        Ok(deleted)
    }

    // --- New Method for Cache Management ---

    /// Search messages by keywords
    pub async fn search_messages_by_keywords(
        &self,
        session_id: &str,
        keywords: &[String],
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.get_conn()?;
        
        // Build search patterns
        let patterns: Vec<String> = keywords.iter()
            .map(|k| format!("%{}%", k.to_lowercase()))
            .collect();
        
        // Build query
        let mut query = String::from(
            "SELECT id, session_id, message_index, role, content, tokens, 
                    timestamp, importance_score
             FROM messages 
             WHERE session_id = ?1"
        );
        
        for i in 0..patterns.len() {
            query.push_str(&format!(" AND LOWER(content) LIKE ?{}", i + 2));
        }
        
        query.push_str(" ORDER BY timestamp DESC LIMIT ?");
        
        let mut stmt = conn.prepare(&query)?;
        
        // Build parameters
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        params.push(&session_id);
        for pattern in &patterns {
            params.push(pattern);
        }
        // FIX: Store in variable to avoid temporary reference
        let limit_i64 = limit as i64;
        params.push(&limit_i64);
        
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut messages = Vec::new();
        
        while let Some(row) = rows.next()? {
            messages.push(self.row_to_stored_message(row)?);
        }
        
        Ok(messages)
    }

    /// Rank past messages against free-form user text, best match first.
    ///
    /// The retrieval primitive behind "what did we discuss about X". Unlike
    /// `search_messages_by_topic_across_sessions`, which builds `LIKE '%kw%'`
    /// patterns, this uses the FTS5 index: it stems (a query for "termination"
    /// finds a message saying "terminate") and it RANKS, so the most relevant
    /// message comes first instead of the most recent one that happened to
    /// contain a substring.
    ///
    /// `exclude_session` omits the current conversation, whose recent messages
    /// the context engine already supplies through tier 1. Pass `None` to search
    /// everything including the current chat.
    ///
    /// Returns an empty vec when the query has nothing searchable in it, never
    /// "everything" - a retrieval result of every message would flood the prompt.
    ///
    /// Each result carries its BM25 score, sign-normalised so higher is better.
    ///
    /// `time_range` narrows results to messages sent inside a window, for
    /// queries naming a date ("what did we decide on Monday"). Applied in SQL,
    /// not after `LIMIT`, so a highly-ranked out-of-window message cannot
    /// displace the in-window match the user actually asked for.
    pub fn search_messages_fulltext(
        &self,
        user_text: &str,
        limit: i64,
        exclude_session: Option<&str>,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> anyhow::Result<Vec<(StoredMessage, f64)>> {
        let Some(query) = crate::memory_db::fts::fts_query_from_user_text(user_text) else {
            return Ok(Vec::new());
        };
        let conn = self.get_conn()?;
        let (range_start, range_end) = match time_range {
            Some((s, e)) => (Some(s.to_rfc3339()), Some(e.to_rfc3339())),
            None => (None, None),
        };

        // Two near-identical statements rather than a dynamically built WHERE:
        // the parameter positions stay fixed and obvious, which matters because
        // a mis-numbered bind here would silently search for the wrong thing.
        let sql = if exclude_session.is_some() {
            "SELECT m.id, m.session_id, m.message_index, m.role, m.content,
                    m.tokens, m.timestamp, m.importance_score,
                    bm25(messages_fts) AS rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND m.session_id != ?2
               AND (?4 IS NULL OR m.timestamp >= ?4)
               AND (?5 IS NULL OR m.timestamp <= ?5)
             ORDER BY rank ASC
             LIMIT ?3"
        } else {
            "SELECT m.id, m.session_id, m.message_index, m.role, m.content,
                    m.tokens, m.timestamp, m.importance_score,
                    bm25(messages_fts) AS rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND ?2 IS NULL
               AND (?4 IS NULL OR m.timestamp >= ?4)
               AND (?5 IS NULL OR m.timestamp <= ?5)
             ORDER BY rank ASC
             LIMIT ?3"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![query, exclude_session, limit, range_start, range_end],
                |row| {
                    let ts_str: String = row.get(6)?;
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&ts_str)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                    let rank: f64 = row.get(8)?;
                    Ok((
                        StoredMessage {
                            id: row.get(0)?,
                            session_id: row.get(1)?,
                            message_index: row.get(2)?,
                            role: row.get(3)?,
                            content: row.get(4)?,
                            tokens: row.get(5)?,
                            timestamp,
                            importance_score: row.get(7)?,
                        },
                        -rank,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Search messages by topic keywords across sessions
    pub async fn search_messages_by_topic_across_sessions(
        &self,
        topic_keywords: &[String],
        limit: usize,
        session_id_filter: Option<&str>, // Optional: exclude or include specific sessions
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let conn = self.get_conn()?;
        
        // Build search patterns
        let patterns: Vec<String> = topic_keywords.iter()
            .map(|k| format!("%{}%", k.to_lowercase()))
            .collect();
        
        // Build query with session filtering
        let mut query = String::from(
            "SELECT m.id, m.session_id, m.message_index, m.role, m.content, 
                    m.tokens, m.timestamp, m.importance_score
             FROM messages m
             JOIN sessions s ON m.session_id = s.id
             WHERE 1=1"
        );
        
        // Add session filter if provided - use Box<dyn ToSql> to store owned values
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(session_id) = session_id_filter {
            query.push_str(" AND m.session_id != ?");
            params.push(Box::new(session_id.to_string())); // Store owned string
        }
        
        // Add keyword search
        for pattern in &patterns {
            query.push_str(" AND LOWER(m.content) LIKE ?");
            params.push(Box::new(pattern.clone())); // Clone the pattern
        }
        
        // Order by relevance (keyword matches + recency + importance)
        query.push_str(" ORDER BY 
            m.importance_score DESC,
            CASE WHEN m.role = 'assistant' THEN 1 ELSE 0 END, -- Prioritize assistant responses
            s.last_accessed DESC,
            m.timestamp DESC
            LIMIT ?");
        
        // Store limit in variable
        let limit_i64 = limit as i64;
        params.push(Box::new(limit_i64));
        
        let mut stmt = conn.prepare(&query)?;
        
        // Convert params to references for the query
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter()
            .map(|p| p.as_ref())
            .collect();
        
        let mut rows = stmt.query(rusqlite::params_from_iter(param_refs))?;
        let mut messages = Vec::new();
        
        while let Some(row) = rows.next()? {
            let timestamp_str: String = row.get(6)?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| anyhow::anyhow!("Failed to parse timestamp: {}", e))?
                .with_timezone(&chrono::Utc);
            
            messages.push(StoredMessage {
                id: row.get(0)?,
                session_id: row.get(1)?,
                message_index: row.get(2)?,
                role: row.get(3)?,
                content: row.get(4)?,
                tokens: row.get(5)?,
                timestamp,
                importance_score: row.get(7)?,
            });
        }
        
        Ok(messages)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::MemoryDatabase;

    /// Regression: persisting a message must actually work.
    ///
    /// This file had NO tests, and that gap let a real bug ship through a fully
    /// green suite. Removing the `embedding_generated` column dropped it from
    /// the INSERT's column list but left the `?8` placeholder and its bound
    /// value in place, so every message insert failed with
    /// "8 values for 7 columns".
    ///
    /// `store_messages_batch` is on the chat hot path - every user message and
    /// every assistant reply persists through it - so the practical effect was
    /// that no conversation could be saved. 176 other tests passed regardless,
    /// because they exercised documents and extraction, never message writes.
    #[test]
    fn storing_messages_persists_them_and_returns_their_ids() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("s1", None).ok();

        let stored = db
            .conversations
            .store_messages_batch(
                "s1",
                &[
                    ("user".to_string(), "what is the notice period?".to_string(), 0, 6, 0.5),
                    ("assistant".to_string(), "Thirty days.".to_string(), 1, 3, 0.5),
                ],
            )
            .expect("storing messages must succeed");

        assert_eq!(stored.len(), 2, "both messages must be stored");
        assert!(stored[0].id > 0, "a real row id must come back");
        assert_ne!(stored[0].id, stored[1].id);

        // And they must read back out with their fields intact.
        let read = db
            .conversations
            .get_session_messages("s1", None, None)
            .expect("reading messages must succeed");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].role, "user");
        assert_eq!(read[0].content, "what is the notice period?");
        assert_eq!(read[0].tokens, 6);
        assert_eq!(read[1].role, "assistant");
        assert_eq!(read[1].content, "Thirty days.");
    }

    /// Same column/placeholder correctness for the single-message insert.
    ///
    /// `store_message_with_tx` has no callers in production code today, but it
    /// was broken by the same edit and still compiles - so it is covered here
    /// rather than left as a trap for whoever wires it up next.
    #[test]
    fn single_message_insert_has_matching_columns_and_values() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("s2", None).ok();

        let mut conn = db.conversations.pool.get().unwrap();
        let stored = db
            .conversations
            .store_message_with_tx(
                &mut conn,
                MessageParams {
                    session_id: "s2",
                    role: "user",
                    content: "does the indemnity survive termination?",
                    message_index: 0,
                    tokens: 6,
                    importance_score: 0.5,
                },
            )
            .expect("single-message insert must succeed");

        assert!(stored.id > 0);
        assert_eq!(stored.content, "does the indemnity survive termination?");
        drop(conn);

        let read = db.conversations.get_session_messages("s2", None, None).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].content, "does the indemnity survive termination?");
    }
}
