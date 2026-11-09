-- Migration 019: retrieval index (chunks as byte ranges, contentless keyword
-- index, int8 vectors, defined terms) and matter <-> document scoping.

CREATE TABLE IF NOT EXISTS rag_documents (
  document_id     INTEGER PRIMARY KEY,
  chunker_version INTEGER NOT NULL DEFAULT 0,
  embed_model     TEXT,
  status          TEXT NOT NULL DEFAULT 'pending',   -- pending | chunked | embedding | embedded | failed
  chunk_count     INTEGER NOT NULL DEFAULT 0,
  embedded_count  INTEGER NOT NULL DEFAULT 0,
  error           TEXT,
  updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS rag_chunks (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  document_id    INTEGER NOT NULL,
  ord            INTEGER NOT NULL,
  byte_start     INTEGER NOT NULL,
  byte_end       INTEGER NOT NULL,
  page_start     INTEGER,
  page_end       INTEGER,
  location_label TEXT,
  section_label  TEXT,
  section_path   TEXT,
  para_start     INTEGER,
  para_end       INTEGER,
  prefix_start   INTEGER,
  prefix_end     INTEGER,
  is_toc         INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  UNIQUE (document_id, ord)
);

CREATE INDEX IF NOT EXISTS idx_rag_chunks_page ON rag_chunks (document_id, page_start);

CREATE VIRTUAL TABLE IF NOT EXISTS rag_chunks_fts USING fts5(
  heading,
  body,
  content = '',
  contentless_delete = 1,
  tokenize = 'porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS rag_chunks_fts_ad AFTER DELETE ON rag_chunks BEGIN
  DELETE FROM rag_chunks_fts WHERE rowid = old.id;
END;

CREATE TABLE IF NOT EXISTS rag_vectors (
  chunk_id INTEGER PRIMARY KEY,
  vec      BLOB NOT NULL,                              -- f32 LE scale + i8[dims]
  FOREIGN KEY (chunk_id) REFERENCES rag_chunks(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS rag_terms (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  document_id INTEGER NOT NULL,
  term_norm   TEXT NOT NULL,
  term        TEXT NOT NULL,
  chunk_id    INTEGER,
  byte_start  INTEGER NOT NULL,
  byte_end    INTEGER NOT NULL,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_rag_terms_norm ON rag_terms (term_norm);
CREATE INDEX IF NOT EXISTS idx_rag_terms_document ON rag_terms (document_id);

CREATE TABLE IF NOT EXISTS matter_documents (
  matter_id   INTEGER NOT NULL,
  document_id INTEGER NOT NULL,
  linked_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (matter_id, document_id),
  FOREIGN KEY (matter_id) REFERENCES matters(id) ON DELETE CASCADE,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_matter_documents_document ON matter_documents (document_id);

-- Documents produced by a Workspace draft: 'saved' is the searchable copy of
-- the current version (replaced on each save); 'published' is a copy the user
-- added to the Vault (kept when the draft is deleted).
CREATE TABLE IF NOT EXISTS draft_documents (
  draft_id    INTEGER NOT NULL,
  document_id INTEGER NOT NULL,
  kind        TEXT NOT NULL DEFAULT 'saved',
  linked_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (draft_id, document_id),
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_draft_documents_document ON draft_documents (document_id);

-- Embeddings of chat messages, so earlier conversations can be recalled by
-- meaning as well as by keyword.
CREATE TABLE IF NOT EXISTS rag_message_vectors (
  message_id INTEGER PRIMARY KEY,
  vec        BLOB NOT NULL,
  FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

-- Further parts of a message too long for one embedding input. Part 0 is
-- the row in rag_message_vectors; parts here start at 1.
CREATE TABLE IF NOT EXISTS rag_message_part_vectors (
  message_id INTEGER NOT NULL,
  part       INTEGER NOT NULL,
  vec        BLOB NOT NULL,
  PRIMARY KEY (message_id, part),
  FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

-- Index of KV states saved to disk by the engine (model_runtime::kv_store).
-- `key` names the exact message prefix a state holds; the file lives in the
-- engine's --slot-save-path directory.
CREATE TABLE IF NOT EXISTS kv_store_entries (
  key        TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  n_messages INTEGER NOT NULL,
  model_id   TEXT NOT NULL,
  engine_id  TEXT NOT NULL,
  ctx_size   INTEGER NOT NULL,
  n_tokens   INTEGER NOT NULL,
  bytes      INTEGER NOT NULL,
  file       TEXT NOT NULL,
  last_used  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_kv_store_entries_session ON kv_store_entries(session_id);
