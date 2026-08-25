-- Migration 004: Local files table for persistent file storage
-- Stores file metadata in database, actual file content in app data folder

-- Local files table (persistent file metadata with nested folder support)
CREATE TABLE IF NOT EXISTS local_files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  path TEXT NOT NULL,                    -- relative path within app data folder (e.g., "folder1/file.txt")
  parent_id INTEGER,                     -- NULL for root level, references local_files.id for nested items
  is_directory BOOLEAN NOT NULL DEFAULT FALSE,
  file_path TEXT,                        -- actual filesystem path relative to app data (NULL for directories)
  size_bytes INTEGER NOT NULL DEFAULT 0,
  mime_type TEXT,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  modified_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY (parent_id) REFERENCES local_files(id) ON DELETE CASCADE,
  UNIQUE(parent_id, name)
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_local_files_parent ON local_files (parent_id);
CREATE INDEX IF NOT EXISTS idx_local_files_path ON local_files (path);
CREATE INDEX IF NOT EXISTS idx_local_files_name ON local_files (name);
CREATE INDEX IF NOT EXISTS idx_local_files_is_directory ON local_files (is_directory);
