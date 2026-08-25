-- Migration 014: Document workspace (Drafts).
--
-- A draft is an editable COPY of a document. The Vault original is never
-- mutated: opening from the Vault copies the bytes, and every save produces a
-- new version, so the pristine original (version 1) is recoverable forever.
--
-- The file bytes live on disk under AppData/drafts/{draft_id}/v{n}.{ext};
-- only metadata lives here. That keeps the database small and lets the storage
-- governor measure drafts as a filesystem component like models and the Vault.

CREATE TABLE IF NOT EXISTS drafts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  format TEXT NOT NULL,                   -- 'docx' | 'xlsx' | 'pptx' | 'pdf' | 'txt'
  origin_kind TEXT NOT NULL,              -- 'vault' | 'upload' | 'blank'
  -- Provenance only. Deleting the source document or Vault file must NOT
  -- delete the draft (the draft owns its own byte copy), so these are
  -- deliberately NOT foreign keys - the same reasoning as documents.local_file_id.
  source_document_id INTEGER,
  source_local_file_id INTEGER,
  current_version INTEGER NOT NULL DEFAULT 1,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- One row per saved state. Version 1 is always the untouched original.
CREATE TABLE IF NOT EXISTS draft_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  draft_id INTEGER NOT NULL,
  version_no INTEGER NOT NULL,
  storage_path TEXT NOT NULL,             -- relative to AppData, e.g. drafts/7/v3.docx
  byte_size INTEGER NOT NULL DEFAULT 0,
  patch_json TEXT,                        -- the edits that produced this version
  label TEXT,                             -- user-supplied ("Before client review")
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (draft_id) REFERENCES drafts(id) ON DELETE CASCADE,
  UNIQUE(draft_id, version_no)
);

CREATE INDEX IF NOT EXISTS idx_draft_versions_draft ON draft_versions (draft_id);
CREATE INDEX IF NOT EXISTS idx_drafts_updated ON drafts (updated_at DESC);
