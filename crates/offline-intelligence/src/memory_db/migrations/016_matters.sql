-- Migration 016: Matters — the unit legal work is actually organised around.
--
-- Until now the workspace had folders and nothing above them, which made it a
-- file manager. A firm does not think in folders; it thinks in matters, and
-- everything else hangs off one: the documents, the deadlines, the bill.
--
-- # What a matter carries, and why each field is here
--
-- `jurisdiction` and `language` are not decoration. They are what let the same
-- product serve a practice spanning several countries without pretending to
-- know the law of any of them:
--
--   * date format — "03/04/2026" is 3 April in most of the world and
--     March 4th in the United States, and a chronology that guesses is worse
--     than one that refuses;
--   * citation format, collation order, and the default template set;
--   * bundle pagination — Bates numbering is a US artefact and does not exist
--     in German, French or Brazilian procedure.
--
-- `matter_number` is the firm's own reference, not ours. It is unique when
-- present because two matters sharing a number is a filing error someone wants
-- to hear about immediately, and NULL when the practice does not use one.
--
-- # Deleting a matter never deletes documents
--
-- The same rule migration 015 established for folders, for the same reason: a
-- matter is a LABEL. Its drafts and folders return to Unfiled, which the store
-- does explicitly in one transaction.
--
-- # The ALTERs that are not here
--
-- `drafts.matter_id`, `drafts.status` and `draft_folders.matter_id` are added
-- by `DraftsStore::initialize_schema` behind `pragma table_info` checks. This
-- file runs on every startup as well as once through the migration chain, and
-- SQLite has no `ADD COLUMN IF NOT EXISTS`.

CREATE TABLE IF NOT EXISTS matters (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL,
  client        TEXT,
  -- The firm's own reference (e.g. "2026-0417"), not an id of ours.
  matter_number TEXT,
  -- ISO 3166 where it is known, free text where it is not. Deliberately not
  -- constrained: a practice spanning many countries will meet jurisdictions
  -- no enum of ours anticipated.
  jurisdiction  TEXT,
  -- BCP-47. Drives date parsing and display, and collation.
  language      TEXT,
  practice_area TEXT,
  status        TEXT NOT NULL DEFAULT 'open',   -- open | closed | archived
  opened_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  closed_at     TIMESTAMP,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Partial, so the constraint applies only where a number was actually given.
-- A plain UNIQUE would let exactly one matter omit it.
CREATE UNIQUE INDEX IF NOT EXISTS idx_matters_number
  ON matters (matter_number) WHERE matter_number IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_matters_status ON matters (status, updated_at DESC);
