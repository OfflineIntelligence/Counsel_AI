-- Migration 006: All files table for unlimited storage with all file formats
-- Stores user files with no size limit, supports all formats and folder uploads
-- Accessible via @filename in chat like curated_files
-- Stored in AppData/Offline_Intelligence/all_files directory

-- All files table (unlimited storage with folder support)
CREATE TABLE IF NOT EXISTS all_files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  parent_id INTEGER,
  is_directory BOOLEAN NOT NULL DEFAULT FALSE,
  file_path TEXT,
  size_bytes INTEGER NOT NULL DEFAULT 0,
  mime_type TEXT,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  modified_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_accessed TIMESTAMP,
  access_count INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (parent_id) REFERENCES all_files(id) ON DELETE CASCADE,
  UNIQUE(parent_id, name)
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_all_files_parent ON all_files (parent_id);
CREATE INDEX IF NOT EXISTS idx_all_files_path ON all_files (path);
CREATE INDEX IF NOT EXISTS idx_all_files_name ON all_files (name);
CREATE INDEX IF NOT EXISTS idx_all_files_is_directory ON all_files (is_directory);
CREATE INDEX IF NOT EXISTS idx_all_files_last_accessed ON all_files (last_accessed);
