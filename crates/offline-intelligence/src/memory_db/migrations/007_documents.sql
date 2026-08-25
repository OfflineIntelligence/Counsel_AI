-- Migration 007: Unified document intelligence store.
--
-- The model never persists document knowledge - the backend does. Any
-- attached document (paperclip or Local Storage) is recorded ONCE here,
-- identified by content hash, and made available to every session that
-- references it, forever, regardless of what model is loaded or how long
-- the conversation runs.

-- One row per UNIQUE document (by content hash). "location" (source_path)
-- lives here even when we do not keep a permanent byte copy (paperclip);
-- local_file_id links to the existing local_files row when the bytes ARE
-- kept on disk (Local Storage), for the document viewer.
CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  content_hash TEXT NOT NULL UNIQUE,      -- blake3 hex of the original bytes
  original_filename TEXT NOT NULL,
  source_path TEXT,                       -- absolute path on the user's machine, when captured
  source_kind TEXT NOT NULL,              -- 'paperclip' | 'local_storage'
  mime_type TEXT,
  size_bytes INTEGER NOT NULL DEFAULT 0,
  extracted_text TEXT NOT NULL DEFAULT '',
  extraction_status TEXT NOT NULL DEFAULT 'ok',   -- 'ok' | 'failed' | 'partial'
  extraction_error TEXT,
  char_count INTEGER NOT NULL DEFAULT 0,
  local_file_id INTEGER,                  -- references local_files.id when source_kind='local_storage'
                                           -- (no FK constraint: documents_store must not depend on
                                           -- local_files' migration/creation order; nulled out in
                                           -- application code if the local_files row is ever deleted)
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_referenced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Which sessions have ever referenced which documents (many-to-many). This
-- is what makes a document attached in Chat A findable and fully known when
-- asked about in Chat B, while still recording true provenance.
CREATE TABLE IF NOT EXISTS session_documents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  document_id INTEGER NOT NULL,
  attached_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  attachment_source TEXT NOT NULL,        -- 'paperclip' | 'local_storage' | 'at_reference'
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  UNIQUE(session_id, document_id)
);

-- Chunked text for retrieval when a document (or the session's total
-- document set) exceeds the active model's real context budget. Chunk text
-- is written immediately at document-creation time (cheap, pure Rust);
-- embedding/embedding_model are filled in LAZILY on first retrieval need
-- (no model may be loaded yet when a document is attached).
CREATE TABLE IF NOT EXISTS document_chunks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  document_id INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  content TEXT NOT NULL,
  embedding BLOB,
  embedding_model TEXT,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  UNIQUE(document_id, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_session_documents_session ON session_documents (session_id);
CREATE INDEX IF NOT EXISTS idx_session_documents_document ON session_documents (document_id);
CREATE INDEX IF NOT EXISTS idx_document_chunks_document ON document_chunks (document_id);
CREATE INDEX IF NOT EXISTS idx_documents_last_referenced ON documents (last_referenced_at);
