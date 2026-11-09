-- Migration 015: Folders for the document workspace.
--
-- Drafts were a flat list. This gives them a tree, so the workspace can show a
-- real file hierarchy rather than one long column of everything ever opened.
--
-- # Why the tree lives here and the membership lives on `drafts`
--
-- A folder owns nothing. It is a label with a parent, and a draft points at the
-- folder it currently sits in (`drafts.folder_id`, added by the store — see
-- below). Modelling it the other way round, with a folder holding a list of
-- draft ids, would make "move a draft" a two-table edit and would let a draft
-- be in two folders at once.
--
-- # Deletion
--
-- Deleting a folder CASCADEs to its subfolders, because a subtree with no root
-- is unreachable. It deliberately does NOT cascade to drafts: a draft owns its
-- own bytes and its own version history, and losing a document because its
-- label was deleted is exactly the kind of surprise this codebase avoids
-- elsewhere (see the provenance note in 014). Drafts in a deleted folder are
-- returned to the root instead, which the store does explicitly.
--
-- # The ALTER that is not here
--
-- `drafts.folder_id` is added by `DraftsStore::initialize_schema` behind a
-- `pragma table_info` check, NOT by this file. This file is executed by the
-- migration chain (once, guarded by schema_version) AND by initialize_schema
-- (on every startup, so in-memory test databases are not missing it). Every
-- statement here therefore has to be idempotent, and SQLite has no
-- `ADD COLUMN IF NOT EXISTS`.

CREATE TABLE IF NOT EXISTS draft_folders (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  -- NULL is the root. A folder whose parent is deleted goes with it.
  parent_id INTEGER,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (parent_id) REFERENCES draft_folders(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_draft_folders_parent ON draft_folders (parent_id);

-- Two siblings cannot share a name, the same rule a filesystem enforces.
--
-- Two partial indexes rather than one UNIQUE(parent_id, name), because SQLite
-- treats NULLs as distinct in a unique index: at the root, where parent_id IS
-- NULL, a plain UNIQUE would happily accept "Contracts" twice.
CREATE UNIQUE INDEX IF NOT EXISTS idx_draft_folders_name_child
  ON draft_folders (parent_id, name) WHERE parent_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_draft_folders_name_root
  ON draft_folders (name) WHERE parent_id IS NULL;
