-- Migration 005: Curated files table for RAG-enabled persistent file storage
-- Stores user-selected files/folders for context inclusion via @filename or folder icon
-- Separate from local_files (which is for synced app data directory)
-- Stored in dedicated curated_files directory for isolation

-- Curated files table (user-managed RAG files with nested folder support)
CREATE TABLE IF NOT EXISTS curated_files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  path TEXT NOT NULL,                    -- relative path within curated_files folder (e.g., "docs/readme.txt")
  parent_id INTEGER,                     -- NULL for root level, references curated_files.id for nested items
  is_directory BOOLEAN NOT NULL DEFAULT FALSE,
  file_path TEXT,                        -- actual filesystem path relative to curated_files dir (NULL for directories)
  size_bytes INTEGER NOT NULL DEFAULT 0,
  mime_type TEXT,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  modified_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_accessed TIMESTAMP,               -- track when file was last included in context
  access_count INTEGER NOT NULL DEFAULT 0,  -- how many times file was used in context
  FOREIGN KEY (parent_id) REFERENCES curated_files(id) ON DELETE CASCADE,
  UNIQUE(parent_id, name)
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_curated_files_parent ON curated_files (parent_id);
CREATE INDEX IF NOT EXISTS idx_curated_files_path ON curated_files (path);
CREATE INDEX IF NOT EXISTS idx_curated_files_name ON curated_files (name);
CREATE INDEX IF NOT EXISTS idx_curated_files_is_directory ON curated_files (is_directory);
CREATE INDEX IF NOT EXISTS idx_curated_files_last_accessed ON curated_files (last_accessed);
