-- Migration 001: Initial schema - Create all base tables (CLEAN - No PRAGMA)
-- Sessions table
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_accessed TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  metadata TEXT DEFAULT '{}' NOT NULL
);
-- Messages table
CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  message_index INTEGER NOT NULL,
  role TEXT NOT NULL CHECK(
    role IN ('system', 'user', 'assistant', 'function')
  ),
  content TEXT NOT NULL,
  tokens INTEGER NOT NULL DEFAULT 0,
  timestamp TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  importance_score REAL NOT NULL DEFAULT 0.5,
  embedding_generated BOOLEAN NOT NULL DEFAULT FALSE,
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  UNIQUE(session_id, message_index)
);
-- Summaries table (Tier 2)
CREATE TABLE IF NOT EXISTS summaries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  message_range_start INTEGER NOT NULL,
  message_range_end INTEGER NOT NULL,
  summary_text TEXT NOT NULL,
  compression_ratio REAL NOT NULL,
  key_topics TEXT NOT NULL,
  generated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  UNIQUE(
    session_id,
    message_range_start,
    message_range_end
  )
);
-- Details table (Context details)
CREATE TABLE IF NOT EXISTS details (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  message_id INTEGER NOT NULL,
  detail_type TEXT NOT NULL,
  content TEXT NOT NULL,
  context TEXT NOT NULL,
  importance_score REAL NOT NULL DEFAULT 0.5,
  accessed_count INTEGER NOT NULL DEFAULT 0,
  last_accessed TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);
-- Embeddings table
CREATE TABLE IF NOT EXISTS embeddings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id INTEGER NOT NULL,
  embedding BLOB NOT NULL,
  embedding_model TEXT NOT NULL,
  generated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE,
  UNIQUE(message_id, embedding_model)
);
-- Tier 3 Content table (LONG-TERM MEMORY - WAS MISSING!)
CREATE TABLE IF NOT EXISTS tier3_content (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  content_type TEXT NOT NULL CHECK(
    content_type IN (
      'fact',
      'detail',
      'concept',
      'preference',
      'rule'
    )
  ),
  content_text TEXT NOT NULL,
  source_message_ids TEXT,
  -- JSON array: [1, 2, 3]
  importance_score REAL NOT NULL DEFAULT 0.0,
  access_count INTEGER NOT NULL DEFAULT 0,
  last_accessed TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  metadata TEXT DEFAULT '{}',
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  UNIQUE(session_id, content_hash, content_type)
);
-- Initial indexes for performance
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages (session_id);
CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages (timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_role ON messages (role);
CREATE INDEX IF NOT EXISTS idx_summaries_session ON summaries (session_id);
CREATE INDEX IF NOT EXISTS idx_details_session ON details (session_id);
CREATE INDEX IF NOT EXISTS idx_details_type ON details (detail_type);
CREATE INDEX IF NOT EXISTS idx_embeddings_message ON embeddings (message_id);
CREATE INDEX IF NOT EXISTS idx_tier3_session ON tier3_content (session_id);
CREATE INDEX IF NOT EXISTS idx_tier3_content_type ON tier3_content (content_type);
CREATE INDEX IF NOT EXISTS idx_tier3_importance ON tier3_content (importance_score DESC);
CREATE INDEX IF NOT EXISTS idx_tier3_access ON tier3_content (last_accessed DESC);