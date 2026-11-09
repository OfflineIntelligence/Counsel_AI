//! Matters — the top level of the workspace.
//!
//! # The rule this store exists to enforce
//!
//! Deleting a matter must never delete a document. A matter is a label; the
//! drafts inside it own their own bytes and their own version history, and
//! losing a client's file because someone tidied up a matter list is the kind
//! of surprise this codebase refuses elsewhere (see `delete_folder`, and the
//! provenance note in migration 014). `delete_matter` therefore returns its
//! drafts and folders to Unfiled in the same transaction that removes it.
//!
//! # Unfiled
//!
//! `NULL` is a real, reachable state, not a migration artefact: a document
//! that has not been filed yet belongs somewhere, and forcing a matter to be
//! chosen before anything can be opened would put a wall in front of the most
//! common action in the product.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tracing::info;

const SCHEMA_SQL: &str = include_str!("migrations/016_matters.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatterRecord {
    pub id: i64,
    pub name: String,
    pub client: Option<String>,
    pub matter_number: Option<String>,
    /// Drives date format, citation format and bundle pagination.
    pub jurisdiction: Option<String>,
    /// BCP-47.
    pub language: Option<String>,
    pub practice_area: Option<String>,
    pub status: String,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields a caller may set. Each is optional so one PATCH can change one
/// thing without the client having to send back the whole record.
#[derive(Debug, Default)]
pub struct MatterFields {
    pub name: Option<String>,
    pub client: Option<Option<String>>,
    pub matter_number: Option<Option<String>>,
    pub jurisdiction: Option<Option<String>>,
    pub language: Option<Option<String>>,
    pub practice_area: Option<Option<String>>,
    pub status: Option<String>,
}

pub struct MattersStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl MattersStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Also called from `new_in_memory`, which never runs the migration chain.
    /// Schema DDL for this store. Only ever called from the migration
    /// chain (`migration::get_migrations`), the single schema route.
    pub(crate) fn create_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(())
    }

    /// Bring the database this store uses up to the current schema. Goes
    /// through the migration chain like every other caller.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        super::migration::ensure_schema(&self.pool)
    }

    pub fn create(&self, fields: MatterFields) -> anyhow::Result<MatterRecord> {
        let conn = self.pool.get()?;
        let name = fields.name.unwrap_or_default();

        conn.execute(
            "INSERT INTO matters (name, client, matter_number, jurisdiction, language,
                                  practice_area, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, 'open'))",
            rusqlite::params![
                name,
                fields.client.flatten(),
                fields.matter_number.flatten(),
                fields.jurisdiction.flatten(),
                fields.language.flatten(),
                fields.practice_area.flatten(),
                fields.status,
            ],
        )
        .map_err(|e| {
            // The partial unique index on matter_number surfaces here. A
            // duplicate reference is a filing mistake, not a server fault.
            if e.to_string().contains("UNIQUE") {
                anyhow::anyhow!("Another matter already uses that matter number.")
            } else {
                e.into()
            }
        })?;

        let id = conn.last_insert_rowid();
        drop(conn);
        info!("Created matter {} '{}'", id, name);
        self.get(id)?
            .ok_or_else(|| anyhow::anyhow!("matter vanished immediately after creation"))
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<MatterRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, name, client, matter_number, jurisdiction, language, practice_area,
                    status, opened_at, closed_at, created_at, updated_at
             FROM matters WHERE id = ?1",
            [id],
            Self::row_to_matter,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Open matters first, each group most recently touched first.
    ///
    /// Closed and archived matters are still returned: they remain searchable
    /// and openable, they simply stop competing for attention. Hiding them
    /// entirely would make "where did the Henderson file go" unanswerable.
    pub fn list(&self) -> anyhow::Result<Vec<MatterRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, client, matter_number, jurisdiction, language, practice_area,
                    status, opened_at, closed_at, created_at, updated_at
             FROM matters
             ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'closed' THEN 1 ELSE 2 END,
                      updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_matter)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Apply only the fields the caller actually sent.
    pub fn update(&self, id: i64, fields: MatterFields) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let mut sets: Vec<&str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(v) = fields.name {
            sets.push("name = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.client {
            sets.push("client = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.matter_number {
            sets.push("matter_number = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.jurisdiction {
            sets.push("jurisdiction = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.language {
            sets.push("language = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.practice_area {
            sets.push("practice_area = ?");
            params.push(Box::new(v));
        }
        if let Some(v) = fields.status {
            // Closing stamps the date; reopening clears it, so a reopened
            // matter does not keep claiming it was closed last March.
            sets.push("closed_at = CASE WHEN ? = 'open' THEN NULL ELSE CURRENT_TIMESTAMP END");
            params.push(Box::new(v.clone()));
            sets.push("status = ?");
            params.push(Box::new(v));
        }

        if sets.is_empty() {
            return Ok(());
        }

        sets.push("updated_at = CURRENT_TIMESTAMP");
        let sql = format!("UPDATE matters SET {} WHERE id = ?", sets.join(", "));
        params.push(Box::new(id));

        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let changed = conn.execute(&sql, refs.as_slice()).map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                anyhow::anyhow!("Another matter already uses that matter number.")
            } else {
                anyhow::Error::from(e)
            }
        })?;

        if changed == 0 {
            return Err(anyhow::anyhow!("matter {} does not exist", id));
        }
        Ok(())
    }

    /// Delete a matter, returning its drafts and folders to Unfiled.
    ///
    /// Returns how many drafts moved, so the caller can tell the user where
    /// their documents went instead of leaving them to discover it.
    pub fn delete(&self, id: i64) -> anyhow::Result<usize> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM matters WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(anyhow::anyhow!("matter {} does not exist", id));
        }

        // Both BEFORE the delete, so there is no window in which a draft points
        // at a matter that has stopped existing.
        let moved = tx.execute("UPDATE drafts SET matter_id = NULL WHERE matter_id = ?1", [id])?;
        tx.execute("UPDATE draft_folders SET matter_id = NULL WHERE matter_id = ?1", [id])?;
        tx.execute("DELETE FROM matters WHERE id = ?1", [id])?;
        tx.commit()?;

        info!("Deleted matter {} ({} drafts returned to Unfiled)", id, moved);
        Ok(moved)
    }

    /// File a draft under a matter, or return it to Unfiled with `None`.
    pub fn assign_draft(&self, draft_id: i64, matter_id: Option<i64>) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        if let Some(m) = matter_id {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM matters WHERE id = ?1)",
                [m],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(anyhow::anyhow!("matter {} does not exist", m));
            }
        }
        let changed = conn.execute(
            "UPDATE drafts SET matter_id = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            rusqlite::params![matter_id, draft_id],
        )?;
        if changed == 0 {
            return Err(anyhow::anyhow!("draft {} does not exist", draft_id));
        }
        Ok(())
    }

    /// Set a document's status.
    pub fn set_draft_status(&self, draft_id: i64, status: &str) -> anyhow::Result<()> {
        if !DRAFT_STATUSES.contains(&status) {
            return Err(anyhow::anyhow!(
                "'{}' is not a document status. Use one of: {}.",
                status,
                DRAFT_STATUSES.join(", ")
            ));
        }
        let conn = self.pool.get()?;
        let changed = conn.execute(
            "UPDATE drafts SET status = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            rusqlite::params![status, draft_id],
        )?;
        if changed == 0 {
            return Err(anyhow::anyhow!("draft {} does not exist", draft_id));
        }
        Ok(())
    }

    fn row_to_matter(row: &rusqlite::Row) -> rusqlite::Result<MatterRecord> {
        Ok(MatterRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            client: row.get(2)?,
            matter_number: row.get(3)?,
            jurisdiction: row.get(4)?,
            language: row.get(5)?,
            practice_area: row.get(6)?,
            status: row.get(7)?,
            opened_at: super::drafts_store::parse_ts(row.get::<_, String>(8)?),
            closed_at: row
                .get::<_, Option<String>>(9)?
                .map(super::drafts_store::parse_ts),
            created_at: super::drafts_store::parse_ts(row.get::<_, String>(10)?),
            updated_at: super::drafts_store::parse_ts(row.get::<_, String>(11)?),
        })
    }
}

/// The lifecycle a document moves through. Ordered as it is lived.
pub const DRAFT_STATUSES: [&str; 3] = ["draft", "in_review", "completed"];

/// The states a matter can be in.
pub const MATTER_STATUSES: [&str; 3] = ["open", "closed", "archived"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::drafts_store::{DraftsStore, NewDraft};

    fn world() -> (MattersStore, DraftsStore) {
        let manager = SqliteConnectionManager::memory();
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        let drafts = DraftsStore::new(Arc::clone(&pool));
        let matters = MattersStore::new(Arc::clone(&pool));
        matters.initialize_schema().unwrap();
        // After matters, because the drafts schema adds the matter_id column.
        drafts.initialize_schema().unwrap();
        (matters, drafts)
    }

    fn a_draft(d: &DraftsStore) -> i64 {
        d.create_draft(
            NewDraft {
                title: "Engagement Letter",
                format: "docx",
                origin_kind: "upload",
                source_document_id: None,
                source_local_file_id: None,
            },
            "drafts/1/v1.docx",
            10,
        )
        .unwrap()
        .id
    }

    fn named(m: &MattersStore, name: &str) -> MatterRecord {
        m.create(MatterFields { name: Some(name.into()), ..Default::default() })
            .unwrap()
    }

    #[test]
    fn a_matter_carries_the_jurisdiction_and_language_its_documents_need() {
        let (m, _d) = world();
        let created = m
            .create(MatterFields {
                name: Some("Acme v. Initech".into()),
                client: Some(Some("Acme Corp".into())),
                matter_number: Some(Some("2026-0417".into())),
                jurisdiction: Some(Some("GB-EAW".into())),
                language: Some(Some("en-GB".into())),
                practice_area: Some(Some("litigation".into())),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(created.status, "open", "a new matter is open");
        assert_eq!(created.jurisdiction.as_deref(), Some("GB-EAW"));
        assert_eq!(created.language.as_deref(), Some("en-GB"));
        assert_eq!(created.closed_at, None);
    }

    #[test]
    fn two_matters_cannot_share_a_matter_number_but_may_both_omit_one() {
        let (m, _d) = world();
        m.create(MatterFields {
            name: Some("First".into()),
            matter_number: Some(Some("2026-0001".into())),
            ..Default::default()
        })
        .unwrap();

        m.create(MatterFields {
            name: Some("Second".into()),
            matter_number: Some(Some("2026-0001".into())),
            ..Default::default()
        })
        .unwrap_err();

        // The partial index is what allows this: a plain UNIQUE would let only
        // one matter go without a reference.
        named(&m, "No reference A");
        named(&m, "No reference B");
        assert_eq!(m.list().unwrap().len(), 3);
    }

    #[test]
    fn a_draft_starts_unfiled_and_can_be_filed_and_unfiled_again() {
        let (m, d) = world();
        let draft = a_draft(&d);
        assert_eq!(d.get_draft(draft).unwrap().unwrap().matter_id, None);

        let matter = named(&m, "Acme");
        m.assign_draft(draft, Some(matter.id)).unwrap();
        assert_eq!(d.get_draft(draft).unwrap().unwrap().matter_id, Some(matter.id));

        m.assign_draft(draft, None).unwrap();
        assert_eq!(d.get_draft(draft).unwrap().unwrap().matter_id, None);
    }

    #[test]
    fn deleting_a_matter_keeps_its_documents_and_returns_them_to_unfiled() {
        let (m, d) = world();
        let draft = a_draft(&d);
        let matter = named(&m, "Acme");
        m.assign_draft(draft, Some(matter.id)).unwrap();

        let moved = m.delete(matter.id).unwrap();

        assert_eq!(moved, 1);
        let survivor = d
            .get_draft(draft)
            .unwrap()
            .expect("the document must outlive the matter it was filed under");
        assert_eq!(survivor.matter_id, None, "and be reachable in Unfiled");
    }

    #[test]
    fn closing_a_matter_stamps_the_date_and_reopening_clears_it() {
        let (m, _d) = world();
        let matter = named(&m, "Acme");

        m.update(matter.id, MatterFields { status: Some("closed".into()), ..Default::default() })
            .unwrap();
        let closed = m.get(matter.id).unwrap().unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some(), "closing records when");

        m.update(matter.id, MatterFields { status: Some("open".into()), ..Default::default() })
            .unwrap();
        let reopened = m.get(matter.id).unwrap().unwrap();
        // Otherwise a reopened matter keeps claiming it was closed last March.
        assert_eq!(reopened.closed_at, None, "reopening clears the closing date");
    }

    #[test]
    fn an_update_touches_only_the_fields_that_were_sent() {
        let (m, _d) = world();
        let matter = m
            .create(MatterFields {
                name: Some("Acme".into()),
                client: Some(Some("Acme Corp".into())),
                jurisdiction: Some(Some("GB-EAW".into())),
                ..Default::default()
            })
            .unwrap();

        m.update(matter.id, MatterFields { name: Some("Acme v. Initech".into()), ..Default::default() })
            .unwrap();

        let after = m.get(matter.id).unwrap().unwrap();
        assert_eq!(after.name, "Acme v. Initech");
        assert_eq!(after.client.as_deref(), Some("Acme Corp"), "untouched");
        assert_eq!(after.jurisdiction.as_deref(), Some("GB-EAW"), "untouched");
    }

    #[test]
    fn open_matters_sort_ahead_of_closed_ones() {
        let (m, _d) = world();
        let closed = named(&m, "Old matter");
        let open = named(&m, "Live matter");
        m.update(closed.id, MatterFields { status: Some("closed".into()), ..Default::default() })
            .unwrap();

        let ids: Vec<i64> = m.list().unwrap().iter().map(|x| x.id).collect();
        assert_eq!(ids.first(), Some(&open.id), "open work comes first");
        assert!(ids.contains(&closed.id), "closed matters are still listed, not hidden");
    }

    #[test]
    fn document_status_moves_through_its_lifecycle_and_refuses_nonsense() {
        let (m, d) = world();
        let draft = a_draft(&d);
        assert_eq!(d.get_draft(draft).unwrap().unwrap().status, "draft");

        for s in ["in_review", "completed"] {
            m.set_draft_status(draft, s).unwrap();
            assert_eq!(d.get_draft(draft).unwrap().unwrap().status, s);
        }

        m.set_draft_status(draft, "sent").unwrap_err();
        m.set_draft_status(draft, "shredded").unwrap_err();
        assert_eq!(
            d.get_draft(draft).unwrap().unwrap().status,
            "completed",
            "a refused status leaves the document where it was"
        );
    }

    #[test]
    fn filing_under_a_matter_that_does_not_exist_is_refused() {
        let (m, d) = world();
        let draft = a_draft(&d);
        m.assign_draft(draft, Some(9999)).unwrap_err();
        assert_eq!(d.get_draft(draft).unwrap().unwrap().matter_id, None);
    }
}
