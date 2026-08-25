//! Local files store - manages persistent file metadata in database
//!
//! Files are stored with metadata in SQLite and actual content in the app data folder.
//! Supports nested folder hierarchy.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use tracing::info;

/// Represents a local file or directory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFile {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub parent_id: Option<i64>,
    pub is_directory: bool,
    pub file_path: Option<String>,
    pub size_bytes: i64,
    pub mime_type: Option<String>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

/// Represents a file tree node with children (for nested display)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFileTree {
    #[serde(flatten)]
    pub file: LocalFile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<LocalFileTree>>,
}

/// Store for managing local files in database
pub struct LocalFilesStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
    app_data_dir: PathBuf,
}

const SCHEMA_SQL: &str = include_str!("migrations/004_local_files.sql");

impl LocalFilesStore {
    /// Create a new local files store
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>, app_data_dir: PathBuf) -> Self {
        Self { pool, app_data_dir }
    }

    /// Idempotent (CREATE TABLE/INDEX IF NOT EXISTS) - safe to call whenever
    /// a caller cannot rely on MigrationManager having already run (e.g. an
    /// in-memory test database), mirroring the pattern every other store
    /// (documents_store, api_keys_store, users_store) already follows.
    /// Production's real database is unaffected: MigrationManager applies
    /// this same table (migration 004) before this ever runs there.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(())
    }

    /// Get the app data directory path
    pub fn get_app_data_dir(&self) -> &Path {
        &self.app_data_dir
    }

    /// Create a folder
    pub fn create_folder(&self, parent_id: Option<i64>, name: &str) -> anyhow::Result<LocalFile> {
        let conn = self.pool.get()?;
        
        // Build the path
        let path = if let Some(pid) = parent_id {
            let parent_path: String = conn.query_row(
                "SELECT path FROM local_files WHERE id = ?1",
                [pid],
                |row| row.get(0),
            )?;
            format!("{}/{}", parent_path, name)
        } else {
            name.to_string()
        };

        let now = chrono::Utc::now().to_rfc3339();
        
        conn.execute(
            "INSERT INTO local_files (name, path, parent_id, is_directory, created_at, modified_at)
             VALUES (?1, ?2, ?3, TRUE, ?4, ?4)",
            rusqlite::params![name, path, parent_id, now],
        )?;

        let id = conn.last_insert_rowid();
        
        // Create actual directory in filesystem
        let fs_path = self.app_data_dir.join(&path);
        std::fs::create_dir_all(&fs_path)?;
        
        info!("Created folder: {} (id: {})", path, id);
        
        self.get_file(id)
    }

    /// Upload a file
    pub fn upload_file(
        &self,
        parent_id: Option<i64>,
        name: &str,
        content: &[u8],
        mime_type: Option<&str>,
    ) -> anyhow::Result<LocalFile> {
        let conn = self.pool.get()?;
        
        // Build the path
        let path = if let Some(pid) = parent_id {
            let parent_path: String = conn.query_row(
                "SELECT path FROM local_files WHERE id = ?1",
                [pid],
                |row| row.get(0),
            )?;
            format!("{}/{}", parent_path, name)
        } else {
            name.to_string()
        };

        // Save file to filesystem
        let fs_path = self.app_data_dir.join(&path);
        if let Some(parent) = fs_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&fs_path, content)?;

        let now = chrono::Utc::now().to_rfc3339();
        let size = content.len() as i64;
        
        // Check if file already exists (update) or create new
        let existing_id: Option<i64> = conn.query_row(
            "SELECT id FROM local_files WHERE parent_id IS ?1 AND name = ?2",
            rusqlite::params![parent_id, name],
            |row| row.get(0),
        ).ok();

        let id = if let Some(existing) = existing_id {
            // Update existing file
            conn.execute(
                "UPDATE local_files SET file_path = ?1, size_bytes = ?2, mime_type = ?3, modified_at = ?4
                 WHERE id = ?5",
                rusqlite::params![path, size, mime_type, now, existing],
            )?;
            existing
        } else {
            // Insert new file
            conn.execute(
                "INSERT INTO local_files (name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at)
                 VALUES (?1, ?2, ?3, FALSE, ?2, ?4, ?5, ?6, ?6)",
                rusqlite::params![name, path, parent_id, size, mime_type, now],
            )?;
            conn.last_insert_rowid()
        };
        
        info!("Uploaded file: {} ({} bytes, id: {})", path, size, id);
        
        self.get_file(id)
    }

    /// List files in a directory (or root if parent_id is None)
    pub fn list_files(&self, parent_id: Option<i64>) -> anyhow::Result<Vec<LocalFile>> {
        let conn = self.pool.get()?;
        
        let files: Vec<LocalFile> = if let Some(pid) = parent_id {
            let mut stmt = conn.prepare(
                "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
                 FROM local_files WHERE parent_id = ?1
                 ORDER BY is_directory DESC, name ASC"
            )?;
            let rows = stmt.query_map([pid], |row| self.row_to_local_file(row))?;
            rows.filter_map(|r| r.ok()).collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
                 FROM local_files WHERE parent_id IS NULL
                 ORDER BY is_directory DESC, name ASC"
            )?;
            let rows = stmt.query_map([], |row| self.row_to_local_file(row))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        Ok(files)
    }

    /// Get a single file by ID
    pub fn get_file(&self, id: i64) -> anyhow::Result<LocalFile> {
        let conn = self.pool.get()?;
        
        let file = conn.query_row(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
             FROM local_files WHERE id = ?1",
            [id],
            |row| self.row_to_local_file(row),
        )?;
        
        Ok(file)
    }

    /// Get file by path
    pub fn get_file_by_path(&self, path: &str) -> anyhow::Result<LocalFile> {
        let conn = self.pool.get()?;
        
        let file = conn.query_row(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
             FROM local_files WHERE path = ?1",
            [path],
            |row| self.row_to_local_file(row),
        )?;
        
        Ok(file)
    }

    /// Get file by name (searches all files, returns first match)
    pub fn get_file_by_name(&self, name: &str) -> anyhow::Result<LocalFile> {
        let conn = self.pool.get()?;
        
        let file = conn.query_row(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
             FROM local_files WHERE name = ?1 AND is_directory = FALSE
             ORDER BY modified_at DESC LIMIT 1",
            [name],
            |row| self.row_to_local_file(row),
        )?;
        
        Ok(file)
    }

    /// Check if an entry (file or folder) exists with given name and parent
    fn entry_exists(&self, parent_id: Option<i64>, name: &str) -> anyhow::Result<Option<LocalFile>> {
        let conn = self.pool.get()?;
        
        let result = if let Some(pid) = parent_id {
            conn.query_row(
                "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
                 FROM local_files WHERE parent_id = ?1 AND name = ?2",
                rusqlite::params![pid, name],
                |row| self.row_to_local_file(row),
            )
        } else {
            conn.query_row(
                "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
                 FROM local_files WHERE parent_id IS NULL AND name = ?1",
                [name],
                |row| self.row_to_local_file(row),
            )
        };
        
        match result {
            Ok(file) => Ok(Some(file)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get file content by ID
    pub fn get_file_content(&self, id: i64) -> anyhow::Result<Vec<u8>> {
        let file = self.get_file(id)?;
        
        if file.is_directory {
            anyhow::bail!("Cannot read content of a directory");
        }
        
        let file_path = file.file_path.ok_or_else(|| anyhow::anyhow!("File has no path"))?;
        let fs_path = self.app_data_dir.join(&file_path);
        
        let content = std::fs::read(&fs_path)?;
        Ok(content)
    }

    /// Get file content as string
    pub fn get_file_content_string(&self, id: i64) -> anyhow::Result<String> {
        let content = self.get_file_content(id)?;
        let text = String::from_utf8(content)?;
        Ok(text)
    }

    /// Delete a file or folder (recursively)
    pub fn delete_file(&self, id: i64) -> anyhow::Result<()> {
        let file = self.get_file(id)?;
        let conn = self.pool.get()?;
        
        // Delete from filesystem
        let fs_path = self.app_data_dir.join(&file.path);
        if file.is_directory {
            if fs_path.exists() {
                std::fs::remove_dir_all(&fs_path)?;
            }
        } else if fs_path.exists() {
            std::fs::remove_file(&fs_path)?;
        }
        
        // Delete from database (CASCADE will handle children)
        conn.execute("DELETE FROM local_files WHERE id = ?1", [id])?;
        
        info!("Deleted: {} (id: {})", file.path, id);
        
        Ok(())
    }

    /// Search files by name (partial match)
    pub fn search_files(&self, query: &str) -> anyhow::Result<Vec<LocalFile>> {
        let conn = self.pool.get()?;
        let pattern = format!("%{}%", query);
        
        let mut stmt = conn.prepare(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
             FROM local_files WHERE name LIKE ?1
             ORDER BY is_directory DESC, name ASC
             LIMIT 50"
        )?;

        let rows = stmt.query_map([pattern], |row| self.row_to_local_file(row))?;
        let files: Vec<LocalFile> = rows.filter_map(|r| r.ok()).collect();
        
        Ok(files)
    }

    /// Get the complete file tree (nested structure)
    pub fn get_file_tree(&self) -> anyhow::Result<Vec<LocalFileTree>> {
        let root_files = self.list_files(None)?;
        let tree = self.build_tree(root_files)?;
        Ok(tree)
    }

    /// Build nested tree recursively
    fn build_tree(&self, files: Vec<LocalFile>) -> anyhow::Result<Vec<LocalFileTree>> {
        let mut tree = Vec::new();
        
        for file in files {
            let children = if file.is_directory {
                let child_files = self.list_files(Some(file.id))?;
                if child_files.is_empty() {
                    None
                } else {
                    Some(self.build_tree(child_files)?)
                }
            } else {
                None
            };
            
            tree.push(LocalFileTree { file, children });
        }
        
        Ok(tree)
    }

    /// Get all files (flat list)
    pub fn get_all_files(&self) -> anyhow::Result<Vec<LocalFile>> {
        let conn = self.pool.get()?;
        
        let mut stmt = conn.prepare(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at
             FROM local_files WHERE is_directory = FALSE
             ORDER BY name ASC"
        )?;

        let rows = stmt.query_map([], |row| self.row_to_local_file(row))?;
        let files: Vec<LocalFile> = rows.filter_map(|r| r.ok()).collect();
        
        Ok(files)
    }

    /// Helper to convert a row to LocalFile
    fn row_to_local_file(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<LocalFile> {
        let created_at_str: String = row.get(8)?;
        let modified_at_str: String = row.get(9)?;
        
        let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        
        let modified_at = chrono::DateTime::parse_from_rfc3339(&modified_at_str)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        
        Ok(LocalFile {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            parent_id: row.get(3)?,
            is_directory: row.get(4)?,
            file_path: row.get(5)?,
            size_bytes: row.get(6)?,
            mime_type: row.get(7)?,
            created_at,
            modified_at,
        })
    }

    /// Sync filesystem with database (import existing files)
    pub fn sync_from_filesystem(&self) -> anyhow::Result<usize> {
        let mut imported = 0;
        
        if !self.app_data_dir.exists() {
            std::fs::create_dir_all(&self.app_data_dir)?;
            return Ok(0);
        }
        
        imported += self.import_directory(None, &self.app_data_dir)?;
        
        info!("Synced {} files/folders from filesystem", imported);
        Ok(imported)
    }

    /// Clear all entries from local_files table (for cleanup/reset)
    pub fn clear_all(&self) -> anyhow::Result<usize> {
        let conn = self.pool.get()?;
        let deleted = conn.execute("DELETE FROM local_files", [])?;
        info!("Cleared {} entries from local_files", deleted);
        Ok(deleted)
    }

    /// Import a directory recursively
    fn import_directory(&self, parent_id: Option<i64>, dir_path: &Path) -> anyhow::Result<usize> {
        let mut count = 0;
        
        // System directories to exclude from local files browser
        const EXCLUDED_DIRS: &[&str] = &["models", "registry"];
        
        for entry in std::fs::read_dir(dir_path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata()?;
            
            // Skip system files and excluded directories
            if name.starts_with('.') || name == "conversations.db" || name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }
            
            // Skip system directories at root level
            if parent_id.is_none() && EXCLUDED_DIRS.contains(&name.as_str()) {
                continue;
            }
            
            if metadata.is_dir() {
                // Check if folder exists in DB (by parent_id and name)
                let folder_id = if let Some(existing) = self.entry_exists(parent_id, &name)? {
                    existing.id
                } else {
                    let folder = self.create_folder(parent_id, &name)?;
                    count += 1;
                    folder.id
                };
                
                // Recurse into subdirectory
                count += self.import_directory(Some(folder_id), &entry.path())?;
            } else {
                // Check if file exists in DB (by parent_id and name)
                if self.entry_exists(parent_id, &name)?.is_none() {
                    let content = std::fs::read(entry.path())?;
                    let mime = mime_guess::from_path(&entry.path())
                        .first()
                        .map(|m| m.to_string());
                    
                    self.upload_file(parent_id, &name, &content, mime.as_deref())?;
                    count += 1;
                }
            }
        }
        
        Ok(count)
    }
}

