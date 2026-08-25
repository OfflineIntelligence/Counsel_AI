-- Migration 003: Add KV Snapshot and Granular Cache Support

-- 1. Snapshots Table: Stores the bulk state of KV pairs
CREATE TABLE IF NOT EXISTS kv_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    message_id INTEGER NOT NULL,
    snapshot_type TEXT NOT NULL DEFAULT 'full' CHECK(snapshot_type IN ('full', 'incremental', 'checkpoint')),
    kv_state BLOB NOT NULL,
    kv_state_hash TEXT NOT NULL,  -- For deduplication and integrity
    access_pattern TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE,
    UNIQUE(session_id, message_id, kv_state_hash)
);

-- 2. KV Metadata: Stats for snapshot analysis and optimization
CREATE TABLE IF NOT EXISTS kv_metadata (
    snapshot_id INTEGER PRIMARY KEY,
    key_count INTEGER NOT NULL,
    avg_key_size INTEGER NOT NULL,
    avg_value_size INTEGER NOT NULL,
    compression_ratio REAL,
    analyzed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (snapshot_id) REFERENCES kv_snapshots(id) ON DELETE CASCADE
);

-- 3. KV Cache Entries: Granular storage of individual KV pairs
CREATE TABLE IF NOT EXISTS kv_cache_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_id INTEGER NOT NULL,
    key_hash TEXT NOT NULL,
    key_data BLOB,
    value_data BLOB NOT NULL,
    key_type TEXT NOT NULL DEFAULT 'attention_key' CHECK(key_type IN ('attention_key', 'attention_value', 'ffn_key', 'ffn_value')),
    layer_index INTEGER NOT NULL,
    head_index INTEGER,  -- NULL for FFN keys/values
    importance_score REAL DEFAULT 0.5,
    access_count INTEGER DEFAULT 0,
    last_accessed TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (snapshot_id) REFERENCES kv_snapshots(id) ON DELETE CASCADE,
    UNIQUE(snapshot_id, key_hash, layer_index, head_index)
);

-- 4. KV Cache Metadata: Global session-level cache statistics
CREATE TABLE IF NOT EXISTS kv_cache_metadata (
    session_id TEXT PRIMARY KEY,
    total_entries INTEGER DEFAULT 0,
    total_size_bytes INTEGER DEFAULT 0,
    avg_importance_score REAL DEFAULT 0.5,
    last_cleared_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    conversation_count INTEGER DEFAULT 0,  -- Tracks 16-conversation cycles for cleanup
    metadata TEXT DEFAULT '{}',
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

---
-- Indexes for Performance Optimization
---

-- Snapshot Retrieval Indexes
CREATE INDEX IF NOT EXISTS idx_kv_snapshots_session 
ON kv_snapshots (session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_kv_snapshots_message 
ON kv_snapshots (message_id);

CREATE INDEX IF NOT EXISTS idx_kv_snapshots_type 
ON kv_snapshots (snapshot_type);

CREATE INDEX IF NOT EXISTS idx_kv_snapshots_hash 
ON kv_snapshots (kv_state_hash);

-- Cache Entry Retrieval Indexes
CREATE INDEX IF NOT EXISTS idx_kv_cache_snapshot 
ON kv_cache_entries (snapshot_id, layer_index, head_index);

CREATE INDEX IF NOT EXISTS idx_kv_cache_key_hash 
ON kv_cache_entries (key_hash);

CREATE INDEX IF NOT EXISTS idx_kv_cache_importance 
ON kv_cache_entries (importance_score DESC);

CREATE INDEX IF NOT EXISTS idx_kv_cache_access 
ON kv_cache_entries (last_accessed DESC);