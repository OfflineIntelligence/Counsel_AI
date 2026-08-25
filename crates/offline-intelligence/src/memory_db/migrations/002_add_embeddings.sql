-- Migration 002: Enhanced embeddings support
-- Embedding metadata table (for tracking different models)
CREATE TABLE IF NOT EXISTS embedding_metadata (
    model_name TEXT PRIMARY KEY,
    vector_size INTEGER NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Embedding similarity cache (optional optimization)
CREATE TABLE IF NOT EXISTS embedding_similarities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    source_embedding_id INTEGER NOT NULL,
    target_embedding_id INTEGER NOT NULL,
    similarity_score REAL NOT NULL,
    calculated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (source_embedding_id) REFERENCES embeddings(id) ON DELETE CASCADE,
    FOREIGN KEY (target_embedding_id) REFERENCES embeddings(id) ON DELETE CASCADE,
    UNIQUE(source_embedding_id, target_embedding_id)
);

-- Indexes for faster similarity search
CREATE INDEX IF NOT EXISTS idx_embeddings_session_model 
ON embeddings (message_id, embedding_model);

CREATE INDEX IF NOT EXISTS idx_embedding_similarities_session 
ON embedding_similarities (session_id);

CREATE INDEX IF NOT EXISTS idx_embedding_similarities_score 
ON embedding_similarities (similarity_score DESC);