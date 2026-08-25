//! All Files store - unlimited storage for all file formats with folder support
//!
//! Files are stored with metadata in SQLite and actual content in the all_files folder.

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

/// Represents a file or directory in all_files storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllFile {
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
    pub last_accessed: Option<DateTime<Utc>>,
    pub access_count: i64,
}

/// Represents a file tree node with children (for nested display)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllFileTree {
    #[serde(flatten)]
    pub file: AllFile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<AllFileTree>>,
}

/// Store for managing all files in database
pub struct AllFilesStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
    all_files_dir: PathBuf,
}

impl AllFilesStore {
    /// Create a new all files store
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>, all_files_dir: PathBuf) -> Self {
        if !all_files_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&all_files_dir) {
                tracing::warn!("Failed to create all files directory: {}", e);
            }
        }
        Self {
            pool,
            all_files_dir,
        }
    }

    /// Get the all files directory path
    pub fn get_all_files_dir(&self) -> &Path {
        &self.all_files_dir
    }

    /// Create a folder
    pub fn create_folder(&self, parent_id: Option<i64>, name: &str) -> anyhow::Result<AllFile> {
        let conn = self.pool.get()?;

        let path = if let Some(pid) = parent_id {
            let parent_path: String =
                conn.query_row("SELECT path FROM all_files WHERE id = ?1", [pid], |row| {
                    row.get(0)
                })?;
            format!("{}/{}", parent_path, name)
        } else {
            name.to_string()
        };

        let now = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO all_files (name, path, parent_id, is_directory, created_at, modified_at)
             VALUES (?1, ?2, ?3, TRUE, ?4, ?4)",
            rusqlite::params![name, path, parent_id, now],
        )?;

        let id = conn.last_insert_rowid();

        let fs_path = self.all_files_dir.join(&path);
        std::fs::create_dir_all(&fs_path)?;

        info!("Created all_files folder: {} (id: {})", path, id);

        self.get_file(id)
    }

    /// Upload a file
    pub fn upload_file(
        &self,
        parent_id: Option<i64>,
        name: &str,
        content: &[u8],
        mime_type: Option<&str>,
    ) -> anyhow::Result<AllFile> {
        let conn = self.pool.get()?;

        let path = if let Some(pid) = parent_id {
            let parent_path: String =
                conn.query_row("SELECT path FROM all_files WHERE id = ?1", [pid], |row| {
                    row.get(0)
                })?;
            format!("{}/{}", parent_path, name)
        } else {
            name.to_string()
        };

        let fs_path = self.all_files_dir.join(&path);
        if let Some(parent) = fs_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&fs_path, content)?;

        let now = chrono::Utc::now().to_rfc3339();
        let size = content.len() as i64;

        // Handle NULL parent_id specially for SQLite
        let existing_id: Option<i64> = if parent_id.is_none() {
            conn.query_row(
                "SELECT id FROM all_files WHERE parent_id IS NULL AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .ok()
        } else {
            conn.query_row(
                "SELECT id FROM all_files WHERE parent_id = ?1 AND name = ?2",
                rusqlite::params![parent_id, name],
                |row| row.get(0),
            )
            .ok()
        };

        let id = if let Some(existing) = existing_id {
            conn.execute(
                "UPDATE all_files SET file_path = ?1, size_bytes = ?2, mime_type = ?3, modified_at = ?4
                 WHERE id = ?5",
                rusqlite::params![path, size, mime_type, now, existing],
            )?;
            existing
        } else {
            conn.execute(
                "INSERT INTO all_files (name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at)
                 VALUES (?1, ?2, ?3, FALSE, ?2, ?4, ?5, ?6, ?6)",
                rusqlite::params![name, path, parent_id, size, mime_type, now],
            )?;
            conn.last_insert_rowid()
        };

        info!("Uploaded all_files: {} ({} bytes)", path, size);

        self.get_file(id)
    }

    /// Upload multiple files with their directory structure
    pub fn upload_files_with_structure(
        &self,
        files: Vec<(Option<String>, String, Vec<u8>)>,
        parent_id: Option<i64>,
    ) -> anyhow::Result<Vec<AllFile>>
    where
        Option<String>: std::fmt::Debug,
    {
        let mut uploaded = Vec::new();

        for (relative_path, filename, content) in files {
            let path = if let Some(parent_path) = relative_path {
                if parent_path.is_empty() {
                    filename.clone()
                } else {
                    format!("{}/{}", parent_path, filename)
                }
            } else {
                filename.clone()
            };

            // Split by both / and \ to handle Windows and Unix paths
            let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
            let mut current_parent_id: Option<i64> = parent_id;

            for (i, part) in parts.iter().enumerate() {
                let is_last = i == parts.len() - 1;
                let is_dir = !is_last;

                if is_dir {
                    // Handle NULL parent_id specially for SQLite
                    let existing_dir_id: Option<i64> = if current_parent_id.is_none() {
                        self.pool.get()?.query_row(
                            "SELECT id FROM all_files WHERE parent_id IS NULL AND name = ?1 AND is_directory = TRUE",
                            [part],
                            |row| row.get(0),
                        ).ok()
                    } else {
                        self.pool.get()?.query_row(
                            "SELECT id FROM all_files WHERE parent_id = ?1 AND name = ?2 AND is_directory = TRUE",
                            rusqlite::params![current_parent_id, part],
                            |row| row.get(0),
                        ).ok()
                    };

                    current_parent_id = if let Some(dir_id) = existing_dir_id {
                        Some(dir_id)
                    } else {
                        let new_dir = self.create_folder(current_parent_id, part)?;
                        Some(new_dir.id)
                    };
                } else {
                    let mime_type = mime_guess::from_path(&path).first().map(|m| m.to_string());

                    let file =
                        self.upload_file(current_parent_id, part, &content, mime_type.as_deref())?;
                    uploaded.push(file);
                    break;
                }
            }
        }

        Ok(uploaded)
    }

    /// Get file tree (nested)
    pub fn get_file_tree(&self) -> anyhow::Result<Vec<AllFileTree>> {
        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
             FROM all_files WHERE parent_id IS NULL 
             ORDER BY is_directory DESC, name ASC"
        )?;

        let root_files: Vec<AllFile> = stmt
            .query_map([], |row| {
                Ok(AllFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: row.get(3)?,
                    is_directory: row.get(4)?,
                    file_path: row.get(5)?,
                    size_bytes: row.get(6)?,
                    mime_type: row.get(7)?,
                    created_at: row
                        .get::<_, String>(8)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    modified_at: row
                        .get::<_, String>(9)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    last_accessed: row
                        .get::<_, Option<String>>(10)?
                        .and_then(|s| s.parse().ok()),
                    access_count: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default();

        let tree: Vec<AllFileTree> = root_files
            .into_iter()
            .map(|f| self.build_tree(&f, &conn))
            .collect();

        Ok(tree)
    }

    fn build_tree(
        &self,
        file: &AllFile,
        conn: &r2d2::PooledConnection<SqliteConnectionManager>,
    ) -> AllFileTree {
        let children = if file.is_directory {
            let child_files: Vec<AllFile> = conn
                .prepare(
                    "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
                     FROM all_files WHERE parent_id = ?1 
                     ORDER BY is_directory DESC, name ASC",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([file.id], |row| {
                        Ok(AllFile {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            path: row.get(2)?,
                            parent_id: row.get(3)?,
                            is_directory: row.get(4)?,
                            file_path: row.get(5)?,
                            size_bytes: row.get(6)?,
                            mime_type: row.get(7)?,
                            created_at: row.get::<_, String>(8)?.parse().unwrap_or_else(|_| Utc::now()),
                            modified_at: row.get::<_, String>(9)?.parse().unwrap_or_else(|_| Utc::now()),
                            last_accessed: row.get::<_, Option<String>>(10)?.and_then(|s| s.parse().ok()),
                            access_count: row.get(11)?,
                        })
                    })
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                })
                .unwrap_or_default();

            if child_files.is_empty() {
                None
            } else {
                Some(
                    child_files
                        .into_iter()
                        .map(|f| self.build_tree(&f, conn))
                        .collect(),
                )
            }
        } else {
            None
        };

        AllFileTree {
            file: file.clone(),
            children,
        }
    }

    /// Get all files (flat list)
    pub fn get_all_files(&self) -> anyhow::Result<Vec<AllFile>> {
        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
             FROM all_files WHERE is_directory = FALSE 
             ORDER BY name ASC"
        )?;

        let files = stmt
            .query_map([], |row| {
                Ok(AllFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: row.get(3)?,
                    is_directory: row.get(4)?,
                    file_path: row.get(5)?,
                    size_bytes: row.get(6)?,
                    mime_type: row.get(7)?,
                    created_at: row
                        .get::<_, String>(8)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    modified_at: row
                        .get::<_, String>(9)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    last_accessed: row
                        .get::<_, Option<String>>(10)?
                        .and_then(|s| s.parse().ok()),
                    access_count: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(files)
    }

    /// Get single file by ID
    pub fn get_file(&self, id: i64) -> anyhow::Result<AllFile> {
        let conn = self.pool.get()?;

        conn.query_row(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
             FROM all_files WHERE id = ?1",
            [id],
            |row| {
                Ok(AllFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: row.get(3)?,
                    is_directory: row.get(4)?,
                    file_path: row.get(5)?,
                    size_bytes: row.get(6)?,
                    mime_type: row.get(7)?,
                    created_at: row.get::<_, String>(8)?.parse().unwrap_or_else(|_| Utc::now()),
                    modified_at: row.get::<_, String>(9)?.parse().unwrap_or_else(|_| Utc::now()),
                    last_accessed: row.get::<_, Option<String>>(10)?.and_then(|s| s.parse().ok()),
                    access_count: row.get(11)?,
                })
            },
        ).map_err(|e| anyhow::anyhow!("File not found: {}", e))
    }

    /// Get file by path
    pub fn get_file_by_path(&self, path: &str) -> anyhow::Result<AllFile> {
        let conn = self.pool.get()?;

        conn.query_row(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
             FROM all_files WHERE path = ?1",
            [path],
            |row| {
                Ok(AllFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: row.get(3)?,
                    is_directory: row.get(4)?,
                    file_path: row.get(5)?,
                    size_bytes: row.get(6)?,
                    mime_type: row.get(7)?,
                    created_at: row.get::<_, String>(8)?.parse().unwrap_or_else(|_| Utc::now()),
                    modified_at: row.get::<_, String>(9)?.parse().unwrap_or_else(|_| Utc::now()),
                    last_accessed: row.get::<_, Option<String>>(10)?.and_then(|s| s.parse().ok()),
                    access_count: row.get(11)?,
                })
            },
        ).map_err(|e| anyhow::anyhow!("File not found: {}", e))
    }

    /// Get file content as string
    pub fn get_file_content_string(&self, id: i64) -> anyhow::Result<String> {
        let file = self.get_file(id)?;

        if file.is_directory {
            return Err(anyhow::anyhow!("Cannot read content of directory"));
        }

        let file_path = file
            .file_path
            .ok_or_else(|| anyhow::anyhow!("File path not stored"))?;
        let fs_path = self.all_files_dir.join(&file_path);

        std::fs::read_to_string(&fs_path)
            .or_else(|_| {
                std::fs::read(&fs_path).map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            })
            .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))
    }

    /// Update access count
    pub fn record_access(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let now = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "UPDATE all_files SET last_accessed = ?1, access_count = access_count + 1 WHERE id = ?2",
            rusqlite::params![now, id],
        )?;

        Ok(())
    }

    /// Delete file or folder by ID
    pub fn delete_file(&self, id: i64) -> anyhow::Result<()> {
        let file = self.get_file(id)?;

        if file.is_directory {
            self.delete_recursive(id)?;
        } else {
            if let Some(file_path) = file.file_path {
                let fs_path = self.all_files_dir.join(&file_path);
                let _ = std::fs::remove_file(fs_path);
            }
        }

        let conn = self.pool.get()?;
        conn.execute("DELETE FROM all_files WHERE id = ?1", [id])?;

        Ok(())
    }

    fn delete_recursive(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;

        let children: Vec<i64> = conn
            .prepare("SELECT id FROM all_files WHERE parent_id = ?1")?
            .query_map([id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        for child_id in children {
            self.delete_file(child_id)?;
        }

        let file = self.get_file(id)?;
        if let Some(file_path) = file.file_path {
            let fs_path = self.all_files_dir.join(&file_path);
            let _ = std::fs::remove_dir_all(fs_path);
        }

        Ok(())
    }

    /// Search files by name
    pub fn search_files(&self, query: &str) -> anyhow::Result<Vec<AllFile>> {
        let conn = self.pool.get()?;

        let pattern = format!("%{}%", query);

        let mut stmt = conn.prepare(
            "SELECT id, name, path, parent_id, is_directory, file_path, size_bytes, mime_type, created_at, modified_at, last_accessed, access_count 
             FROM all_files WHERE name LIKE ?1 AND is_directory = FALSE
             ORDER BY name ASC"
        )?;

        let files = stmt
            .query_map([pattern], |row| {
                Ok(AllFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: row.get(3)?,
                    is_directory: row.get(4)?,
                    file_path: row.get(5)?,
                    size_bytes: row.get(6)?,
                    mime_type: row.get(7)?,
                    created_at: row
                        .get::<_, String>(8)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    modified_at: row
                        .get::<_, String>(9)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    last_accessed: row
                        .get::<_, Option<String>>(10)?
                        .and_then(|s| s.parse().ok()),
                    access_count: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(files)
    }
}
