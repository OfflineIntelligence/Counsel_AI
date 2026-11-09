//! Full-text search across the workspace.
//!
//! # What owns what
//!
//! The FTS5 table owns the document text; `draft_text` records only which
//! version produced it. See migration 017 for why the text is not stored
//! twice. `draft_fts.rowid` is `drafts.id`, so re-indexing is a delete and an
//! insert by id with no join.
//!
//! # Why the query is not passed through to FTS5
//!
//! FTS5 `MATCH` is a query language: a bare `AND`, an unbalanced quote, a
//! leading `*` or a stray `^` are all either syntax errors or mean something
//! the person typing did not intend. Someone searching for `indemnity (cap)`
//! wants those words, not a subexpression. So the input is tokenised and
//! re-quoted here, and the language is never exposed.

use std::sync::Arc;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::Serialize;
use tracing::debug;

const SCHEMA_SQL: &str = include_str!("migrations/017_workspace_search.sql");

/// Wraps the matched run inside a snippet.
///
/// Control characters rather than `<mark>`: the snippet crosses a JSON
/// boundary into a React tree, and returning HTML there would mean rendering
/// it with `dangerouslySetInnerHTML` — turning every document body in the
/// workspace into an injection surface to achieve bold text. These two bytes
/// cannot occur in extracted document text, so the client splits on them and
/// builds real elements.
pub const MARK_OPEN: char = '\u{2}';
pub const MARK_CLOSE: char = '\u{3}';

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub draft_id: i64,
    pub title: String,
    /// Body excerpt with matched runs delimited by [`MARK_OPEN`]/[`MARK_CLOSE`].
    pub snippet: String,
    /// FTS5 rank. Lower is a better match.
    pub rank: f64,
}

pub struct SearchStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl SearchStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Create the tables. Also called from `new_in_memory`, which never runs
    /// the migration chain — the same reason `DraftsStore` does this.
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

    /// Index (or re-index) one draft.
    pub fn index_draft(
        &self,
        draft_id: i64,
        title: &str,
        body: &str,
        version_no: i64,
    ) -> anyhow::Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        // Delete first, unconditionally: an FTS5 insert on an existing rowid
        // does not replace, it duplicates, and the document would then match
        // twice and rank oddly.
        tx.execute("DELETE FROM draft_fts WHERE rowid = ?1", [draft_id])?;
        tx.execute(
            "INSERT INTO draft_fts (rowid, title, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![draft_id, title, body],
        )?;
        tx.execute(
            "INSERT INTO draft_text (draft_id, version_no, char_count, indexed_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(draft_id) DO UPDATE SET
               version_no = excluded.version_no,
               char_count = excluded.char_count,
               indexed_at = CURRENT_TIMESTAMP",
            rusqlite::params![draft_id, version_no, body.chars().count() as i64],
        )?;
        tx.commit()?;
        debug!("Indexed draft {} at version {}", draft_id, version_no);
        Ok(())
    }

    /// Drop a draft from the index.
    ///
    /// `draft_text` cascades with the draft row, but `draft_fts` is a virtual
    /// table with no foreign keys — without this an FTS entry outlives the
    /// document, surfacing a result that cannot be opened and keeping the text
    /// of a deleted document searchable.
    pub fn remove_draft(&self, draft_id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM draft_fts WHERE rowid = ?1", [draft_id])?;
        conn.execute("DELETE FROM draft_text WHERE draft_id = ?1", [draft_id])?;
        Ok(())
    }

    /// Drafts whose index is missing or behind their current version.
    ///
    /// Drives the backfill: everything created before this migration existed,
    /// plus anything whose indexing failed.
    pub fn stale_draft_ids(&self, limit: usize) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT d.id FROM drafts d
             LEFT JOIN draft_text t ON t.draft_id = d.id
             WHERE t.draft_id IS NULL OR t.version_no < d.current_version
             ORDER BY d.updated_at DESC
             LIMIT ?1",
        )?;
        let ids = stmt
            .query_map([limit as i64], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub fn indexed_count(&self) -> anyhow::Result<i64> {
        let conn = self.pool.get()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM draft_text", [], |r| r.get(0))?)
    }

    /// Search titles and bodies.
    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        let match_expr = match to_match_expression(query) {
            Some(e) => e,
            // Nothing searchable survived tokenising (punctuation only). An
            // empty result is the honest answer; passing it to FTS5 would be
            // a syntax error.
            None => return Ok(Vec::new()),
        };

        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT f.rowid, d.title,
                    snippet(draft_fts, 1, char(2), char(3), '…', 14),
                    rank
             FROM draft_fts f
             JOIN drafts d ON d.id = f.rowid
             WHERE draft_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(rusqlite::params![match_expr, limit as i64], |row| {
                Ok(SearchHit {
                    draft_id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                    rank: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }
}

/// Turn what someone typed into a safe FTS5 MATCH expression.
///
/// Every token becomes a quoted string, so FTS5 operators inside the input are
/// data rather than syntax. The final token gets a `*` so search behaves as
/// people expect while still typing — "indemn" finds "indemnity".
pub fn to_match_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| c.is_whitespace())
        .map(|t| {
            // Trim surrounding punctuation. Apostrophes and hyphens survive
            // because they occur inside real words a lawyer searches for
            // ("client's", "non-compete").
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return None;
    }

    let last = tokens.len() - 1;
    let expr = tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            // Doubling is how a quote is escaped inside an FTS5 string.
            let escaped = t.replace('"', "\"\"");
            if i == last {
                format!("\"{}\"*", escaped)
            } else {
                format!("\"{}\"", escaped)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SearchStore, Arc<Pool<SqliteConnectionManager>>) {
        let manager = SqliteConnectionManager::memory();
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        let conn = pool.get().unwrap();
        conn.execute_batch(
            "CREATE TABLE drafts (id INTEGER PRIMARY KEY, title TEXT NOT NULL,
                                  current_version INTEGER NOT NULL DEFAULT 1,
                                  updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);",
        )
        .unwrap();
        drop(conn);
        let s = SearchStore::new(Arc::clone(&pool));
        s.initialize_schema().unwrap();
        (s, pool)
    }

    fn add_draft(pool: &Pool<SqliteConnectionManager>, id: i64, title: &str, version: i64) {
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO drafts (id, title, current_version) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, title, version],
            )
            .unwrap();
    }

    #[test]
    fn a_document_is_found_by_words_in_its_body() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Engagement Letter", 1);
        s.index_draft(1, "Engagement Letter", "The indemnity cap is limited to fees paid.", 1)
            .unwrap();

        let hits = s.search("indemnity cap", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].draft_id, 1);
        assert!(
            hits[0].snippet.contains(MARK_OPEN),
            "the snippet must mark the match: {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn a_partial_last_word_still_matches() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Engagement Letter", 1);
        s.index_draft(1, "Engagement Letter", "The indemnity cap is limited.", 1).unwrap();

        // Search as you type: "indemn" has to find "indemnity", or the field
        // looks broken until the whole word is typed.
        assert_eq!(s.search("indemn", 10).unwrap().len(), 1);
    }

    #[test]
    fn accents_are_folded_so_french_documents_are_searchable() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Contrat", 1);
        s.index_draft(1, "Contrat", "Le contrat est résilié de plein droit.", 1).unwrap();

        // The point of remove_diacritics 2, and why it is not optional for a
        // product used outside English-speaking jurisdictions.
        assert_eq!(s.search("resilie", 10).unwrap().len(), 1, "unaccented query");
        assert_eq!(s.search("résilié", 10).unwrap().len(), 1, "accented query");
    }

    #[test]
    fn fts_operators_typed_by_a_user_are_data_not_syntax() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Notes", 1);
        s.index_draft(1, "Notes", "The indemnity cap and the liability cap differ.", 1)
            .unwrap();

        // Each of these is either a syntax error or a different query if the
        // input reaches FTS5 unescaped. None may error or panic.
        for q in ["indemnity (cap)", "cap AND", "\"unbalanced", "* cap", "NEAR(a b)"] {
            s.search(q, 10)
                .unwrap_or_else(|e| panic!("query {q:?} failed: {e}"));
        }
    }

    #[test]
    fn punctuation_only_input_returns_nothing_rather_than_erroring() {
        let (s, _p) = store();
        assert!(s.search("()*\"", 10).unwrap().is_empty());
        assert!(to_match_expression("  ").is_none());
    }

    #[test]
    fn reindexing_replaces_rather_than_duplicating() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Letter", 2);
        s.index_draft(1, "Letter", "first draft mentions arbitration", 1).unwrap();
        s.index_draft(1, "Letter", "second draft mentions mediation", 2).unwrap();

        assert!(s.search("arbitration", 10).unwrap().is_empty(), "old text is gone");
        assert_eq!(s.search("mediation", 10).unwrap().len(), 1, "and not duplicated");
        assert_eq!(s.indexed_count().unwrap(), 1);
    }

    #[test]
    fn a_deleted_draft_leaves_nothing_behind_in_the_index() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Letter", 1);
        s.index_draft(1, "Letter", "confidential settlement terms", 1).unwrap();

        s.remove_draft(1).unwrap();

        // An orphaned FTS row would surface a result that cannot be opened,
        // and would keep a deleted document's text searchable.
        assert!(s.search("settlement", 10).unwrap().is_empty());
        assert_eq!(s.indexed_count().unwrap(), 0);
    }

    #[test]
    fn stale_and_unindexed_drafts_are_both_reported_for_backfill() {
        let (s, pool) = store();
        add_draft(&pool, 1, "Never indexed", 1);
        add_draft(&pool, 2, "Indexed and current", 1);
        add_draft(&pool, 3, "Indexed but behind", 4);
        s.index_draft(2, "Indexed and current", "text", 1).unwrap();
        s.index_draft(3, "Indexed but behind", "text", 2).unwrap();

        let stale = s.stale_draft_ids(10).unwrap();
        assert!(stale.contains(&1), "never indexed");
        assert!(stale.contains(&3), "indexed at v2 but now on v4");
        assert!(!stale.contains(&2), "already current");
    }
}
