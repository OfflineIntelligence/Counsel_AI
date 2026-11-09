//! The library — precedents, templates and clauses.
//!
//! Metadata only; the bytes of a library item live on disk under
//! `AppData/library/`, the same split the drafts store uses. See migration 018
//! for why items are files and clauses are text.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tracing::info;

const SCHEMA_SQL: &str = include_str!("migrations/018_library.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryItemRecord {
    pub id: i64,
    pub title: String,
    /// `template` | `precedent`
    pub kind: String,
    pub format: String,
    pub storage_path: String,
    pub byte_size: i64,
    pub source_draft_id: Option<i64>,
    pub jurisdiction: Option<String>,
    pub practice_area: Option<String>,
    pub language: Option<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set for documents shipped with the app (the knowledge collection): the
    /// stable key from `knowledge.json`. `None` for the firm's own items.
    pub bundled_key: Option<String>,
    /// The bundled document's catalogue version, used to refresh it after an update.
    pub bundled_version: Option<String>,
    /// The chat-searchable document this item was indexed into, if any.
    pub document_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClauseRecord {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub jurisdiction: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewLibraryItem<'a> {
    pub title: &'a str,
    pub kind: &'a str,
    pub format: &'a str,
    pub byte_size: i64,
    pub source_draft_id: Option<i64>,
    pub jurisdiction: Option<&'a str>,
    pub practice_area: Option<&'a str>,
    pub language: Option<&'a str>,
    pub tags: Vec<String>,
}

pub const LIBRARY_KINDS: [&str; 2] = ["template", "precedent"];

pub struct LibraryStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl LibraryStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Schema DDL for this store. Only ever called from the migration
    /// chain (`migration::get_migrations`), the single schema route.
    pub(crate) fn create_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
        conn.execute_batch(SCHEMA_SQL)?;
        // Bundled knowledge documents (see knowledge.rs). Added in place,
        // tolerating "already exists", like the drafts store's later columns.
        for stmt in [
            "ALTER TABLE library_items ADD COLUMN bundled_key TEXT",
            "ALTER TABLE library_items ADD COLUMN bundled_version TEXT",
            "ALTER TABLE library_items ADD COLUMN document_id INTEGER",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_library_bundled_key
               ON library_items (bundled_key) WHERE bundled_key IS NOT NULL;
             -- A bundled document the user deleted stays deleted: app updates
             -- must not quietly put it back.
             CREATE TABLE IF NOT EXISTS knowledge_removed (
               bundled_key TEXT PRIMARY KEY,
               removed_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
             );",
        )?;
        Ok(())
    }

    /// Bring the database this store uses up to the current schema. Goes
    /// through the migration chain like every other caller.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        super::migration::ensure_schema(&self.pool)
    }

    /// Insert an item and its tags in one transaction.
    ///
    /// The storage path contains the row id, which only exists after the
    /// insert, so a placeholder goes in first and `fix_storage_path` corrects
    /// it once the bytes are on disk — the same ordering `create_draft` uses,
    /// and for the same reason: writing the file first would strand bytes on
    /// disk whenever the insert failed.
    pub fn create(&self, item: NewLibraryItem) -> anyhow::Result<LibraryItemRecord> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO library_items (title, kind, format, storage_path, byte_size,
                                        source_draft_id, jurisdiction, practice_area, language)
             VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                item.title,
                item.kind,
                item.format,
                item.byte_size,
                item.source_draft_id,
                item.jurisdiction,
                item.practice_area,
                item.language,
            ],
        )?;
        let id = tx.last_insert_rowid();

        for tag in &item.tags {
            let tag = tag.trim().to_lowercase();
            if tag.is_empty() {
                continue;
            }
            // OR IGNORE: the same tag twice on one item is a duplicate, not an
            // error worth failing the whole save over.
            tx.execute(
                "INSERT OR IGNORE INTO library_tags (item_id, tag) VALUES (?1, ?2)",
                rusqlite::params![id, tag],
            )?;
        }
        tx.commit()?;
        drop(conn);

        info!("Added library item {} '{}' ({})", id, item.title, item.kind);
        self.get(id)?
            .ok_or_else(|| anyhow::anyhow!("library item vanished immediately after creation"))
    }

    pub fn fix_storage_path(&self, id: i64, path: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE library_items SET storage_path = ?1 WHERE id = ?2",
            rusqlite::params![path, id],
        )?;
        if changed == 0 {
            return Err(anyhow::anyhow!("library item {} does not exist", id));
        }
        Ok(())
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<LibraryItemRecord>> {
        let conn = self.pool.get()?;
        let item = conn
            .query_row(
                "SELECT id, title, kind, format, storage_path, byte_size, source_draft_id,
                        jurisdiction, practice_area, language, created_at, updated_at,
                        bundled_key, bundled_version, document_id
                 FROM library_items WHERE id = ?1",
                [id],
                Self::row_to_item,
            )
            .optional()?;
        // Released before tags_for asks the pool for its own connection:
        // holding both at once deadlocks a single-connection pool.
        drop(conn);

        match item {
            Some(mut i) => {
                i.tags = self.tags_for(id)?;
                Ok(Some(i))
            }
            None => Ok(None),
        }
    }

    pub fn list(&self) -> anyhow::Result<Vec<LibraryItemRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, kind, format, storage_path, byte_size, source_draft_id,
                    jurisdiction, practice_area, language, created_at, updated_at,
                    bundled_key, bundled_version, document_id
             FROM library_items ORDER BY (bundled_key IS NULL), updated_at DESC, id DESC",
        )?;
        let mut items = stmt
            .query_map([], Self::row_to_item)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        for item in &mut items {
            item.tags = self.tags_for(item.id)?;
        }
        Ok(items)
    }

    fn tags_for(&self, id: i64) -> anyhow::Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt =
            conn.prepare("SELECT tag FROM library_tags WHERE item_id = ?1 ORDER BY tag")?;
        // Bound to a local before returning: wrapping the expression in Ok(..)
        // keeps `stmt`'s temporaries alive past the end of the borrow of
        // `conn`, which the compiler rejects.
        let tags = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tags)
    }

    pub fn delete(&self, id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.pool.get()?;
        // The path comes back so the caller can remove the bytes; the row
        // going without the file would silently misreport disk usage.
        let path: Option<String> = conn
            .query_row("SELECT storage_path FROM library_items WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        if path.is_none() {
            return Err(anyhow::anyhow!("library item {} does not exist", id));
        }
        conn.execute("DELETE FROM library_items WHERE id = ?1", [id])?;
        Ok(path)
    }

    // ------------------------------------------------------------ knowledge

    /// The library item holding a bundled document, if it is still present.
    pub fn get_bundled(&self, key: &str) -> anyhow::Result<Option<LibraryItemRecord>> {
        let conn = self.pool.get()?;
        let id: Option<i64> = conn
            .query_row("SELECT id FROM library_items WHERE bundled_key = ?1", [key], |r| r.get(0))
            .optional()?;
        drop(conn);
        match id {
            Some(id) => self.get(id),
            None => Ok(None),
        }
    }

    /// Record a bundled document as a library item, or refresh its details.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_bundled(
        &self,
        key: &str,
        version: &str,
        title: &str,
        kind: &str,
        format: &str,
        storage_path: &str,
        byte_size: i64,
        jurisdiction: Option<&str>,
        practice_area: Option<&str>,
        language: Option<&str>,
        tags: &[String],
    ) -> anyhow::Result<LibraryItemRecord> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM library_items WHERE bundled_key = ?1", [key], |r| r.get(0))
            .optional()?;
        let id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE library_items
                        SET title = ?1, kind = ?2, format = ?3, storage_path = ?4, byte_size = ?5,
                            jurisdiction = ?6, practice_area = ?7, language = ?8,
                            bundled_version = ?9, updated_at = CURRENT_TIMESTAMP
                      WHERE id = ?10",
                    rusqlite::params![
                        title, kind, format, storage_path, byte_size, jurisdiction, practice_area,
                        language, version, id
                    ],
                )?;
                tx.execute("DELETE FROM library_tags WHERE item_id = ?1", [id])?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO library_items (title, kind, format, storage_path, byte_size,
                                                jurisdiction, practice_area, language,
                                                bundled_key, bundled_version)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        title, kind, format, storage_path, byte_size, jurisdiction, practice_area,
                        language, key, version
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };
        for tag in tags {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() {
                tx.execute(
                    "INSERT OR IGNORE INTO library_tags (item_id, tag) VALUES (?1, ?2)",
                    rusqlite::params![id, tag],
                )?;
            }
        }
        tx.commit()?;
        drop(conn);
        self.get(id)?
            .ok_or_else(|| anyhow::anyhow!("bundled library item vanished after being recorded"))
    }

    pub fn set_document_id(&self, id: i64, document_id: Option<i64>) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE library_items SET document_id = ?1 WHERE id = ?2",
            rusqlite::params![document_id, id],
        )?;
        Ok(())
    }

    pub fn is_knowledge_removed(&self, key: &str) -> anyhow::Result<bool> {
        let conn = self.pool.get()?;
        let found: Option<i64> = conn
            .query_row("SELECT 1 FROM knowledge_removed WHERE bundled_key = ?1", [key], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    pub fn mark_knowledge_removed(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("INSERT OR IGNORE INTO knowledge_removed (bundled_key) VALUES (?1)", [key])?;
        Ok(())
    }

    // ------------------------------------------------------------- clauses

    pub fn create_clause(
        &self,
        title: &str,
        body: &str,
        jurisdiction: Option<&str>,
    ) -> anyhow::Result<ClauseRecord> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO clauses (title, body, jurisdiction) VALUES (?1, ?2, ?3)",
            rusqlite::params![title, body, jurisdiction],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_clause(id)?
            .ok_or_else(|| anyhow::anyhow!("clause vanished immediately after creation"))
    }

    pub fn get_clause(&self, id: i64) -> anyhow::Result<Option<ClauseRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, title, body, jurisdiction, created_at, updated_at
             FROM clauses WHERE id = ?1",
            [id],
            Self::row_to_clause,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_clauses(&self) -> anyhow::Result<Vec<ClauseRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, jurisdiction, created_at, updated_at
             FROM clauses ORDER BY updated_at DESC, id DESC",
        )?;
        let clauses = stmt
            .query_map([], Self::row_to_clause)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(clauses)
    }

    pub fn delete_clause(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let changed = conn.execute("DELETE FROM clauses WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(anyhow::anyhow!("clause {} does not exist", id));
        }
        Ok(())
    }

    fn row_to_item(row: &rusqlite::Row) -> rusqlite::Result<LibraryItemRecord> {
        Ok(LibraryItemRecord {
            id: row.get(0)?,
            title: row.get(1)?,
            kind: row.get(2)?,
            format: row.get(3)?,
            storage_path: row.get(4)?,
            byte_size: row.get(5)?,
            source_draft_id: row.get(6)?,
            jurisdiction: row.get(7)?,
            practice_area: row.get(8)?,
            language: row.get(9)?,
            tags: Vec::new(),
            created_at: super::drafts_store::parse_ts(row.get::<_, String>(10)?),
            updated_at: super::drafts_store::parse_ts(row.get::<_, String>(11)?),
            bundled_key: row.get(12)?,
            bundled_version: row.get(13)?,
            document_id: row.get(14)?,
        })
    }

    fn row_to_clause(row: &rusqlite::Row) -> rusqlite::Result<ClauseRecord> {
        Ok(ClauseRecord {
            id: row.get(0)?,
            title: row.get(1)?,
            body: row.get(2)?,
            jurisdiction: row.get(3)?,
            created_at: super::drafts_store::parse_ts(row.get::<_, String>(4)?),
            updated_at: super::drafts_store::parse_ts(row.get::<_, String>(5)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> LibraryStore {
        let manager = SqliteConnectionManager::memory();
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        let s = LibraryStore::new(pool);
        s.initialize_schema().unwrap();
        s
    }

    fn item(s: &LibraryStore, title: &str, tags: &[&str]) -> LibraryItemRecord {
        s.create(NewLibraryItem {
            title,
            kind: "precedent",
            format: "docx",
            byte_size: 2048,
            source_draft_id: Some(7),
            jurisdiction: Some("GB-EAW"),
            practice_area: Some("corporate"),
            language: Some("en-GB"),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        })
        .unwrap()
    }

    #[test]
    fn an_item_keeps_its_provenance_and_its_tags() {
        let s = store();
        let created = item(&s, "Mutual NDA", &["nda", "confidentiality"]);

        assert_eq!(created.kind, "precedent");
        assert_eq!(created.source_draft_id, Some(7), "where it came from is recorded");
        assert_eq!(created.jurisdiction.as_deref(), Some("GB-EAW"));
        assert_eq!(created.tags, vec!["confidentiality", "nda"], "tags come back sorted");
    }

    #[test]
    fn tags_are_folded_to_lower_case_and_deduplicated() {
        let s = store();
        // Otherwise "NDA" and "nda" become two different tags and neither
        // filter finds both halves of the library.
        let created = item(&s, "Mutual NDA", &["NDA", "nda", "  Confidentiality  ", ""]);
        assert_eq!(created.tags, vec!["confidentiality", "nda"]);
    }

    #[test]
    fn the_storage_path_is_corrected_once_the_id_exists() {
        let s = store();
        let created = item(&s, "Engagement Letter", &[]);
        assert_eq!(created.storage_path, "", "a placeholder until the id is known");

        s.fix_storage_path(created.id, &format!("library/{}.docx", created.id)).unwrap();
        let after = s.get(created.id).unwrap().unwrap();
        assert_eq!(after.storage_path, format!("library/{}.docx", created.id));
    }

    #[test]
    fn deleting_an_item_reports_its_path_so_the_bytes_can_go_too() {
        let s = store();
        let created = item(&s, "Old form", &["nda"]);
        s.fix_storage_path(created.id, "library/1.docx").unwrap();

        let path = s.delete(created.id).unwrap();

        assert_eq!(path.as_deref(), Some("library/1.docx"));
        assert!(s.get(created.id).unwrap().is_none());
        // Tags cascade, or the next item to take this id inherits them.
        assert!(s.tags_for(created.id).unwrap().is_empty());
    }

    #[test]
    fn a_clause_stores_its_body_rather_than_a_file() {
        let s = store();
        let body = "This Agreement is governed by the laws of England and Wales.";
        let c = s.create_clause("Governing law (E&W)", body, Some("GB-EAW")).unwrap();

        // The whole point: a clause is dropped into a document being written,
        // so its text has to be available without opening anything.
        assert_eq!(c.body, body);
        assert_eq!(s.list_clauses().unwrap().len(), 1);

        s.delete_clause(c.id).unwrap();
        assert!(s.list_clauses().unwrap().is_empty());
        s.delete_clause(c.id).unwrap_err();
    }

    #[test]
    fn the_library_lists_most_recently_touched_first() {
        let s = store();
        let first = item(&s, "Older", &[]);
        let second = item(&s, "Newer", &[]);
        let ids: Vec<i64> = s.list().unwrap().iter().map(|i| i.id).collect();
        assert_eq!(ids.first(), Some(&second.id));
        assert!(ids.contains(&first.id));
    }
}
