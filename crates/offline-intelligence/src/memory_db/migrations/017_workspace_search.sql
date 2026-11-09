-- Migration 017: Full-text search over the workspace.
--
-- Finding the right document is the most frequent thing anyone does here and
-- was the worst served: there was no search at all, and `drafts` stores only
-- metadata -- the bytes live on disk and the text lived nowhere.
--
-- # Where the text is kept, and why it is not kept twice
--
-- The FTS5 table owns the text. `draft_text` deliberately holds NO body
-- column: it records only which version was indexed and when, so a stale
-- index can be detected without a second copy of every document in the
-- database. Keeping the text in both places would double the storage cost of
-- the entire workspace to answer a question -- "is this current?" -- that one
-- integer answers.
--
-- `rowid` in `draft_fts` IS `drafts.id`. That is what lets a re-index be a
-- DELETE plus an INSERT by id, with no join and no trigger.
--
-- # Why a plain FTS5 table rather than external-content
--
-- An external-content table would avoid the duplication we have already
-- avoided above, at the price of triggers that must be kept in step with
-- every write path. At this scale -- hundreds to low thousands of documents,
-- not a web corpus -- that trade is not worth a class of silent sync bugs.
--
-- # remove_diacritics 2
--
-- Deliberate, and load-bearing for a product used across jurisdictions: it
-- makes "résilié" and "resilie" match each other. Option 2 rather than 1
-- because 1 only folds Latin-1, which would leave most of Central European
-- and Turkish text unmatched.

CREATE TABLE IF NOT EXISTS draft_text (
  draft_id   INTEGER PRIMARY KEY,
  -- The version the index was built from. If this is behind
  -- drafts.current_version the entry is stale and gets rebuilt.
  version_no INTEGER NOT NULL,
  char_count INTEGER NOT NULL DEFAULT 0,
  indexed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (draft_id) REFERENCES drafts(id) ON DELETE CASCADE
);

CREATE VIRTUAL TABLE IF NOT EXISTS draft_fts USING fts5(
  title,
  body,
  tokenize = 'unicode61 remove_diacritics 2'
);
