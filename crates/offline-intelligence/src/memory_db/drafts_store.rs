//! Draft metadata and version history for the document workspace.
//!
//! The bytes of each version live on disk (see `document_workspace`); this
//! store owns only the metadata that makes them findable and ordered.
//!
//! # Why versions are append-only
//!
//! Version 1 is always the untouched original, and `restore` writes a NEW
//! version rather than deleting later ones. There is therefore no operation in
//! this store that can lose a state the user once saved — the only removal is
//! deleting the whole draft, which the user asks for explicitly.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const SCHEMA_SQL: &str = include_str!("migrations/014_drafts.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftRecord {
    pub id: i64,
    pub title: String,
    pub format: String,
    pub origin_kind: String,
    pub source_document_id: Option<i64>,
    pub source_local_file_id: Option<i64>,
    pub current_version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftVersionRecord {
    pub id: i64,
    pub draft_id: i64,
    pub version_no: i64,
    pub storage_path: String,
    pub byte_size: i64,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub struct NewDraft<'a> {
    pub title: &'a str,
    pub format: &'a str,
    pub origin_kind: &'a str,
    pub source_document_id: Option<i64>,
    pub source_local_file_id: Option<i64>,
}

pub struct DraftsStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl DraftsStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Create the tables.
    ///
    /// Called from BOTH `MemoryDatabase::new` and `new_in_memory`. In-memory
    /// test databases never run the migration chain, so a store that relied on
    /// migrations alone would exist in production and be missing in tests —
    /// the exact drift that forced migration 010 to special-case itself.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(())
    }

    /// Create a draft along with its version 1 row in one transaction, so a
    /// draft can never exist without the original it was made from.
    pub fn create_draft(
        &self,
        draft: NewDraft,
        version_path: &str,
        byte_size: i64,
    ) -> anyhow::Result<DraftRecord> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO drafts (title, format, origin_kind, source_document_id,
                                 source_local_file_id, current_version)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            rusqlite::params![
                draft.title,
                draft.format,
                draft.origin_kind,
                draft.source_document_id,
                draft.source_local_file_id,
            ],
        )?;
        let draft_id = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO draft_versions (draft_id, version_no, storage_path, byte_size, label)
             VALUES (?1, 1, ?2, ?3, 'Original')",
            rusqlite::params![draft_id, version_path, byte_size],
        )?;
        tx.commit()?;
        // Release the pooled connection BEFORE the read below. A single-
        // connection pool (which every in-memory test uses, because a second
        // connection would open a different database) deadlocks against itself
        // otherwise, and in production it needlessly holds a connection while
        // waiting for another.
        drop(conn);

        info!(
            "Created draft {} '{}' ({}, from {})",
            draft_id, draft.title, draft.format, draft.origin_kind
        );
        self.get_draft(draft_id)?
            .ok_or_else(|| anyhow::anyhow!("draft vanished immediately after creation"))
    }

    /// Correct a version's stored path.
    ///
    /// Exists for one specific ordering problem: a version's path contains the
    /// draft id, but the id is only assigned when the row is inserted. So
    /// `create_draft` writes a placeholder and the caller fixes it the moment
    /// the bytes land on disk. Doing it the other way — writing the file first
    /// and then inserting — would leave orphaned bytes behind whenever the
    /// insert failed.
    pub fn fix_version_path(&self, draft_id: i64, version_no: i64, path: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE draft_versions SET storage_path = ?1 WHERE draft_id = ?2 AND version_no = ?3",
            rusqlite::params![path, draft_id, version_no],
        )?;
        if changed == 0 {
            return Err(anyhow::anyhow!(
                "draft {} has no version {} to point at '{}'",
                draft_id,
                version_no,
                path
            ));
        }
        Ok(())
    }

    pub fn get_draft(&self, id: i64) -> anyhow::Result<Option<DraftRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, title, format, origin_kind, source_document_id, source_local_file_id,
                    current_version, created_at, updated_at
             FROM drafts WHERE id = ?1",
            [id],
            Self::row_to_draft,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_drafts(&self) -> anyhow::Result<Vec<DraftRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, format, origin_kind, source_document_id, source_local_file_id,
                    current_version, created_at, updated_at
             FROM drafts ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_draft)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append a version and make it current, in one transaction.
    pub fn add_version(
        &self,
        draft_id: i64,
        storage_path: &str,
        byte_size: i64,
        patch_json: Option<&str>,
        label: Option<&str>,
    ) -> anyhow::Result<i64> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version_no), 0) + 1 FROM draft_versions WHERE draft_id = ?1",
            [draft_id],
            |r| r.get(0),
        )?;

        tx.execute(
            "INSERT INTO draft_versions (draft_id, version_no, storage_path, byte_size, patch_json, label)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![draft_id, next, storage_path, byte_size, patch_json, label],
        )?;
        tx.execute(
            "UPDATE drafts SET current_version = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![next, Utc::now().to_rfc3339(), draft_id],
        )?;
        tx.commit()?;

        debug!("Draft {} advanced to version {}", draft_id, next);
        Ok(next)
    }

    /// Replace the CURRENT version's bytes in place, without cutting a new one.
    ///
    /// This is what keeps autosave from producing hundreds of versions: edits
    /// inside the amend window update the current version, and a new version is
    /// cut only on an explicit save or when the window expires. Version 1 is
    /// never amended — the pristine original must stay pristine.
    pub fn amend_current_version(
        &self,
        draft_id: i64,
        byte_size: i64,
        patch_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE draft_versions SET byte_size = ?1, patch_json = ?2, created_at = ?3
             WHERE draft_id = ?4
               AND version_no = (SELECT current_version FROM drafts WHERE id = ?4)
               AND version_no > 1",
            rusqlite::params![byte_size, patch_json, Utc::now().to_rfc3339(), draft_id],
        )?;
        conn.execute(
            "UPDATE drafts SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![Utc::now().to_rfc3339(), draft_id],
        )?;
        Ok(())
    }

    pub fn get_version(&self, draft_id: i64, version_no: i64) -> anyhow::Result<Option<DraftVersionRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, draft_id, version_no, storage_path, byte_size, label, created_at
             FROM draft_versions WHERE draft_id = ?1 AND version_no = ?2",
            rusqlite::params![draft_id, version_no],
            Self::row_to_version,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The current version's row. One query, not `get_draft` + `get_version`:
    /// two sequential pool checkouts would deadlock a single-connection pool.
    pub fn current_version(&self, draft_id: i64) -> anyhow::Result<Option<DraftVersionRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT v.id, v.draft_id, v.version_no, v.storage_path, v.byte_size, v.label, v.created_at
             FROM draft_versions v
             JOIN drafts d ON d.id = v.draft_id AND d.current_version = v.version_no
             WHERE v.draft_id = ?1",
            [draft_id],
            Self::row_to_version,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_versions(&self, draft_id: i64) -> anyhow::Result<Vec<DraftVersionRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, draft_id, version_no, storage_path, byte_size, label, created_at
             FROM draft_versions WHERE draft_id = ?1 ORDER BY version_no DESC",
        )?;
        let rows = stmt
            .query_map([draft_id], Self::row_to_version)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn rename_draft(&self, id: i64, title: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE drafts SET title = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![title, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Delete the draft row; `draft_versions` follows by cascade. The caller
    /// deletes the files on disk.
    pub fn delete_draft(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        info!("Deleted draft {}", id);
        Ok(())
    }

    /// Versions that are safe to evict under disk pressure: everything except
    /// the current version and version 1 (the pristine original), oldest first.
    pub fn evictable_versions(&self) -> anyhow::Result<Vec<DraftVersionRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT v.id, v.draft_id, v.version_no, v.storage_path, v.byte_size, v.label, v.created_at
             FROM draft_versions v
             JOIN drafts d ON d.id = v.draft_id
             WHERE v.version_no > 1 AND v.version_no <> d.current_version
             ORDER BY v.created_at ASC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_version)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_version(&self, version_id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM draft_versions WHERE id = ?1", [version_id])?;
        Ok(())
    }

    fn row_to_draft(row: &rusqlite::Row) -> rusqlite::Result<DraftRecord> {
        Ok(DraftRecord {
            id: row.get(0)?,
            title: row.get(1)?,
            format: row.get(2)?,
            origin_kind: row.get(3)?,
            source_document_id: row.get(4)?,
            source_local_file_id: row.get(5)?,
            current_version: row.get(6)?,
            created_at: parse_ts(row.get::<_, String>(7)?),
            updated_at: parse_ts(row.get::<_, String>(8)?),
        })
    }

    fn row_to_version(row: &rusqlite::Row) -> rusqlite::Result<DraftVersionRecord> {
        Ok(DraftVersionRecord {
            id: row.get(0)?,
            draft_id: row.get(1)?,
            version_no: row.get(2)?,
            storage_path: row.get(3)?,
            byte_size: row.get(4)?,
            label: row.get(5)?,
            created_at: parse_ts(row.get::<_, String>(6)?),
        })
    }
}

/// SQLite writes CURRENT_TIMESTAMP as "YYYY-MM-DD HH:MM:SS" while our own
/// inserts use RFC-3339; accept both rather than losing a timestamp.
fn parse_ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
                .map_err(|e| e.into())
        })
        .unwrap_or_else(|_: anyhow::Error| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use r2d2_sqlite::SqliteConnectionManager;

    fn store() -> (DraftsStore, Arc<Pool<SqliteConnectionManager>>) {
        // FK enforcement ON, matching production. An earlier test helper in this
        // codebase omitted this pragma and hid a real FK failure for weeks.
        let manager = SqliteConnectionManager::memory().with_init(|c| {
            c.execute_batch("PRAGMA foreign_keys=ON;")
        });
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        let s = DraftsStore::new(Arc::clone(&pool));
        s.initialize_schema().unwrap();
        (s, pool)
    }

    fn sample(s: &DraftsStore) -> DraftRecord {
        s.create_draft(
            NewDraft {
                title: "Engagement Letter",
                format: "docx",
                origin_kind: "vault",
                source_document_id: Some(3),
                source_local_file_id: Some(9),
            },
            "drafts/1/v1.docx",
            2048,
        )
        .unwrap()
    }

    #[test]
    fn a_new_draft_starts_at_version_one_labelled_original() {
        let (s, _p) = store();
        let d = sample(&s);
        assert_eq!(d.current_version, 1);
        let v = s.current_version(d.id).unwrap().unwrap();
        assert_eq!(v.version_no, 1);
        assert_eq!(v.label.as_deref(), Some("Original"));
        assert_eq!(v.byte_size, 2048);
    }

    #[test]
    fn versions_append_and_advance_the_current_pointer() {
        let (s, _p) = store();
        let d = sample(&s);
        let v2 = s.add_version(d.id, "drafts/1/v2.docx", 2100, Some("[]"), None).unwrap();
        let v3 = s.add_version(d.id, "drafts/1/v3.docx", 2200, Some("[]"), Some("Reviewed")).unwrap();
        assert_eq!((v2, v3), (2, 3));
        assert_eq!(s.get_draft(d.id).unwrap().unwrap().current_version, 3);
        assert_eq!(s.list_versions(d.id).unwrap().len(), 3);
    }

    /// The pristine original must never be amended, or "restore the file I
    /// started from" stops being true.
    #[test]
    fn amending_never_touches_version_one() {
        let (s, _p) = store();
        let d = sample(&s);
        s.amend_current_version(d.id, 999, Some("[]")).unwrap();
        let v1 = s.get_version(d.id, 1).unwrap().unwrap();
        assert_eq!(v1.byte_size, 2048, "version 1 must be untouched");

        s.add_version(d.id, "drafts/1/v2.docx", 100, None, None).unwrap();
        s.amend_current_version(d.id, 777, Some("[{}]")).unwrap();
        assert_eq!(s.get_version(d.id, 2).unwrap().unwrap().byte_size, 777);
        assert_eq!(s.list_versions(d.id).unwrap().len(), 2, "amend must not add a version");
    }

    #[test]
    fn evictable_versions_exclude_the_original_and_the_current() {
        let (s, _p) = store();
        let d = sample(&s);
        s.add_version(d.id, "drafts/1/v2.docx", 10, None, None).unwrap();
        s.add_version(d.id, "drafts/1/v3.docx", 10, None, None).unwrap();
        s.add_version(d.id, "drafts/1/v4.docx", 10, None, None).unwrap();

        let evictable: Vec<i64> = s.evictable_versions().unwrap().iter().map(|v| v.version_no).collect();
        assert_eq!(evictable, vec![2, 3], "v1 (original) and v4 (current) are protected");
    }

    #[test]
    fn deleting_a_draft_cascades_to_its_versions() {
        let (s, pool) = store();
        let d = sample(&s);
        s.add_version(d.id, "drafts/1/v2.docx", 10, None, None).unwrap();
        s.delete_draft(d.id).unwrap();

        assert!(s.get_draft(d.id).unwrap().is_none());
        let remaining: i64 = pool
            .get()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM draft_versions WHERE draft_id = ?1", [d.id], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "versions must cascade");
    }

    #[test]
    fn listing_orders_by_most_recently_updated() {
        let (s, _p) = store();
        let a = sample(&s);
        let b = s
            .create_draft(
                NewDraft { title: "NDA", format: "docx", origin_kind: "blank",
                           source_document_id: None, source_local_file_id: None },
                "drafts/2/v1.docx",
                10,
            )
            .unwrap();
        s.add_version(a.id, "drafts/1/v2.docx", 10, None, None).unwrap();
        let ids: Vec<i64> = s.list_drafts().unwrap().iter().map(|d| d.id).collect();
        assert_eq!(ids.first(), Some(&a.id), "most recently updated first");
        assert!(ids.contains(&b.id));
    }
}
