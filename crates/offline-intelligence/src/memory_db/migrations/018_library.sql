-- Migration 018: The library — precedents, templates and clauses.
--
-- Lawyers do not draft from nothing. They start from the last one that worked,
-- which is why "new from precedent" is the most common way a document begins
-- and why firms keep a house set of templates at all. Until now the workspace
-- could only start a document blank or by importing one, so that starting
-- point lived in somebody's personal folder.
--
-- # Two shapes, because they are used differently
--
-- A `library_item` is a FILE — a whole engagement letter, an NDA, a claim
-- form. You open it, and it becomes a new draft.
--
-- A `clause` is TEXT — one governing-law provision, one liability cap. You do
-- not open it; you drop it into a document you are already writing. Storing a
-- clause as a file would mean opening a second document to copy three lines
-- out of it, so its body lives in the row.
--
-- # Copies, not references
--
-- Promoting a draft to the library copies its bytes to
-- `AppData/library/{id}.{ext}`. `source_draft_id` records where it came from
-- and is deliberately NOT a foreign key: deleting the matter it was drawn from
-- must never reach into the firm's precedent bank and remove it. Same
-- reasoning as `drafts.source_document_id` in migration 014.

CREATE TABLE IF NOT EXISTS library_items (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  title           TEXT NOT NULL,
  -- 'template' is a blank house form; 'precedent' is a real document that
  -- worked once. The distinction matters when choosing what to start from.
  kind            TEXT NOT NULL DEFAULT 'precedent',
  format          TEXT NOT NULL,
  storage_path    TEXT NOT NULL,        -- relative to AppData
  byte_size       INTEGER NOT NULL DEFAULT 0,
  -- Provenance only. No FK: see the note above.
  source_draft_id INTEGER,
  jurisdiction    TEXT,
  practice_area   TEXT,
  language        TEXT,
  created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_library_kind ON library_items (kind, updated_at DESC);

CREATE TABLE IF NOT EXISTS library_tags (
  item_id INTEGER NOT NULL,
  tag     TEXT NOT NULL,
  PRIMARY KEY (item_id, tag),
  FOREIGN KEY (item_id) REFERENCES library_items(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS clauses (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  title        TEXT NOT NULL,
  body         TEXT NOT NULL,
  jurisdiction TEXT,
  created_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_clauses_updated ON clauses (updated_at DESC);
