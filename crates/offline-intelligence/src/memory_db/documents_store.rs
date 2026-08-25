//! Unified document intelligence store.
//!
//! The backend, not the model, is the source of truth for document
//! knowledge. Every attached document (paperclip or Local Storage) is
//! recorded exactly once here, identified by a content hash - attaching the
//! same file anywhere, anytime, is recognized as the same document, never
//! re-extracted, and made available to every session that references it,
//! indefinitely.

use std::sync::Arc;
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const SCHEMA_SQL: &str = include_str!("migrations/007_documents.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub id: i64,
    pub content_hash: String,
    pub original_filename: String,
    pub source_path: Option<String>,
    pub source_kind: String,
    pub mime_type: Option<String>,
    pub size_bytes: i64,
    pub extracted_text: String,
    pub extraction_status: String,
    pub extraction_error: Option<String>,
    pub char_count: i64,
    pub local_file_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub last_referenced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunkRecord {
    pub id: i64,
    pub document_id: i64,
    pub chunk_index: i32,
    pub content: String,
    /// First/last PDF page this chunk was drawn from, when the source
    /// extraction tagged pages (native or OCR'd PDF text). None for
    /// formats without a page concept.
    pub page_start: Option<i32>,
    pub page_end: Option<i32>,
    /// Free-form physical location for formats without numeric pages, e.g.
    /// "Slide 3" or "Sheet: Q1 Data".
    pub location_label: Option<String>,
    /// Nearest legal heading/clause label in effect for this chunk (e.g.
    /// "Section 4.2(b): Indemnification"), when the source text exhibits
    /// recognizable structure. None for unstructured text.
    pub section_label: Option<String>,
    /// 0-based paragraph range this chunk spans - the guaranteed last-resort
    /// citation anchor when there is neither a page/location marker nor a
    /// recognizable heading. None only for chunks written before migration
    /// 009.
    pub paragraph_start: Option<i32>,
    pub paragraph_end: Option<i32>,
}

impl DocumentChunkRecord {
    /// Human-readable citation prefix for this chunk, e.g. "[p.12]",
    /// "[Slide 3]", "[P12-18]", "[p.12, Section 4.2(b)]". Mirrors
    /// utils::doc_context::DocumentChunk::citation_label - this is the
    /// version used once a chunk round-trips through storage.
    pub fn citation_label(&self) -> String {
        let location = match (self.page_start, self.page_end) {
            (Some(s), Some(e)) if s == e => Some(format!("p.{}", s)),
            (Some(s), Some(e)) => Some(format!("pp.{}-{}", s, e)),
            (Some(s), None) => Some(format!("p.{}", s)),
            _ => match &self.location_label {
                Some(l) => Some(l.clone()),
                None => match (self.paragraph_start, self.paragraph_end) {
                    (Some(s), Some(e)) if s == e => Some(format!("P{}", s + 1)),
                    (Some(s), Some(e)) => Some(format!("P{}-{}", s + 1, e + 1)),
                    _ => None,
                },
            },
        };
        match (location, &self.section_label) {
            (Some(l), Some(s)) => format!("[{}, {}]", l, s),
            (Some(l), None) => format!("[{}]", l),
            (None, Some(s)) => format!("[{}]", s),
            (None, None) => String::new(),
        }
    }
}

/// Input for recording a newly attached document. `original_bytes` is used
/// ONLY to compute the identity hash - callers decide independently whether
/// to persist the bytes themselves (Local Storage does, via local_file_id;
/// paperclip does not, per product design - source_path is the record of
/// where it came from instead).
pub struct NewDocument<'a> {
    pub original_bytes: &'a [u8],
    pub original_filename: &'a str,
    pub source_path: Option<String>,
    pub source_kind: &'a str, // "paperclip" | "local_storage"
    pub mime_type: Option<String>,
    pub size_bytes: i64,
    pub extracted_text: String,
    pub extraction_status: &'a str, // "ok" | "failed" | "partial"
    pub extraction_error: Option<String>,
    pub local_file_id: Option<i64>,
}

pub struct DocumentsStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl DocumentsStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(SCHEMA_SQL)?;
        // Provenance-anchor columns (migration 008). Normally added once by
        // MigrationManager against the shared production database before
        // this runs, but this store is also initialized standalone (its own
        // test suite, and any pool that skips MigrationManager), so adding
        // them here too - tolerating "already exists" - keeps this store
        // correct on its own rather than depending on initialization order.
        for stmt in [
            "ALTER TABLE document_chunks ADD COLUMN page_start INTEGER",
            "ALTER TABLE document_chunks ADD COLUMN page_end INTEGER",
            "ALTER TABLE document_chunks ADD COLUMN section_label TEXT",
            "ALTER TABLE document_chunks ADD COLUMN location_label TEXT",
            "ALTER TABLE document_chunks ADD COLUMN paragraph_start INTEGER",
            "ALTER TABLE document_chunks ADD COLUMN paragraph_end INTEGER",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    /// Content-identity hash. Same bytes anywhere = the same document.
    pub fn hash_bytes(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    /// Record a document, or recognize it as one already known. Extraction
    /// and chunking are done by the caller / here respectively - this
    /// function never re-extracts an existing document's text.
    pub fn upsert_document(&self, doc: NewDocument) -> anyhow::Result<DocumentRecord> {
        let hash = Self::hash_bytes(doc.original_bytes);
        let conn = self.pool.get()?;

        if let Some(existing) = Self::find_by_hash(&conn, &hash)? {
            // Same content, seen before: bump recency and fill in any
            // metadata this attachment newly provides, never re-extract.
            //
            // mime_type is COALESCE'd here for the same reason as the other
            // two: the first path to store a document does not always know it.
            // The @filename / [Attached: x] path resolves a file by NAME and
            // has no client-supplied MIME, so without this backfill a document
            // first seen that way would carry a NULL mime_type permanently,
            // even after later being attached by a paperclip that did know it.
            conn.execute(
                "UPDATE documents SET last_referenced_at = ?1,
                    source_path = COALESCE(source_path, ?2),
                    local_file_id = COALESCE(local_file_id, ?3),
                    mime_type = COALESCE(mime_type, ?4)
                 WHERE id = ?5",
                rusqlite::params![
                    Utc::now().to_rfc3339(),
                    doc.source_path,
                    doc.local_file_id,
                    doc.mime_type,
                    existing.id
                ],
            )?;
            debug!(
                "Document '{}' already known (hash {}), reusing id {}",
                doc.original_filename, &hash[..12], existing.id
            );
            return Self::find_by_hash(&conn, &hash)?
                .ok_or_else(|| anyhow::anyhow!("document vanished immediately after update"));
        }

        let now = Utc::now().to_rfc3339();
        let char_count = doc.extracted_text.chars().count() as i64;
        let insert = conn.execute(
            "INSERT INTO documents (
                content_hash, original_filename, source_path, source_kind,
                mime_type, size_bytes, extracted_text, extraction_status,
                extraction_error, char_count, local_file_id, created_at, last_referenced_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
            rusqlite::params![
                hash,
                doc.original_filename,
                doc.source_path,
                doc.source_kind,
                doc.mime_type,
                doc.size_bytes,
                doc.extracted_text,
                doc.extraction_status,
                doc.extraction_error,
                char_count,
                doc.local_file_id,
                now,
            ],
        );

        // Lost an insert race for the same content: another task passed the
        // find_by_hash check above at the same time we did, and got its INSERT
        // in first. `content_hash` is UNIQUE, so ours fails.
        //
        // The winner stored the identical bytes, so its row is exactly what this
        // caller wanted - returning it is correct, not a consolation. Treating
        // the collision as an error instead is what made attaching two copies of
        // the same file wedge the UI: `attach_document` reported nothing for the
        // losing file, and the frontend left its chip in "processing" forever
        // with the send button disabled.
        //
        // Only a UNIQUE violation is swallowed. Any other failure is a real
        // error and is propagated.
        if let Err(e) = insert {
            let is_unique_violation = matches!(
                e,
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _
                )
            );
            if is_unique_violation {
                if let Some(winner) = Self::find_by_hash(&conn, &hash)? {
                    debug!(
                        "Document '{}' was inserted concurrently by another task \
                         (hash {}) - reusing id {}",
                        doc.original_filename, &hash[..12], winner.id
                    );
                    return Ok(winner);
                }
            }
            return Err(e.into());
        }
        let document_id = conn.last_insert_rowid();

        // Chunk immediately (pure Rust, cheap). Chunks carry the structural
        // provenance (page/slide/sheet, heading, paragraph range) computed by
        // doc_context::chunk_text.
        if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() {
            let chunks = crate::utils::doc_context::chunk_text(&doc.extracted_text);
            for (idx, chunk) in chunks.iter().enumerate() {
                conn.execute(
                    "INSERT INTO document_chunks
                        (document_id, chunk_index, content, page_start, page_end,
                         section_label, location_label, paragraph_start, paragraph_end)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    rusqlite::params![
                        document_id,
                        idx as i32,
                        chunk.content,
                        chunk.page_start,
                        chunk.page_end,
                        chunk.section_label,
                        chunk.location_label,
                        chunk.paragraph_start,
                        chunk.paragraph_end,
                    ],
                )?;
            }
            info!(
                "Document '{}' recorded (id {}, {} chars, {} chunks)",
                doc.original_filename, document_id, char_count, chunks.len()
            );
        } else {
            warn!(
                "Document '{}' recorded with extraction_status='{}' ({}); no chunks created",
                doc.original_filename,
                doc.extraction_status,
                doc.extraction_error.as_deref().unwrap_or("no error detail")
            );
        }

        Self::find_by_hash(&conn, &hash)?
            .ok_or_else(|| anyhow::anyhow!("document vanished immediately after insert"))
    }

    /// Re-record an EXISTING document's extraction and rebuild its chunks.
    ///
    /// `upsert_document` deliberately never re-extracts a document it already
    /// knows by hash - that is what makes re-attaching a file free. The flip
    /// side is that a row written with a WRONG extraction stays wrong
    /// forever, and no amount of re-attaching can heal it. That is not
    /// hypothetical: files uploaded to Local Storage while api::files_api
    /// used a naive "any '[' prefix is a failure" classifier recorded every
    /// image and scanned PDF as extraction_status='failed' with empty text,
    /// so the model was shown "[Could not read 'X']" on every turn even
    /// though OCR had actually succeeded.
    ///
    /// This is the deliberate, explicit repair path for that case. It is
    /// never called speculatively - only when a caller has real bytes in hand
    /// and has found the stored extraction to be unusable. Existing chunks
    /// Existing chunks are deleted and rebuilt, because they describe the
    /// old text.
    pub fn repair_extraction(
        &self,
        document_id: i64,
        extracted_text: &str,
        extraction_status: &str,
        extraction_error: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let char_count = extracted_text.chars().count() as i64;

        conn.execute(
            "UPDATE documents
                SET extracted_text = ?1, extraction_status = ?2,
                    extraction_error = ?3, char_count = ?4,
                    last_referenced_at = ?5
              WHERE id = ?6",
            rusqlite::params![
                extracted_text,
                extraction_status,
                extraction_error,
                char_count,
                Utc::now().to_rfc3339(),
                document_id,
            ],
        )?;

        conn.execute("DELETE FROM document_chunks WHERE document_id = ?1", [document_id])?;

        if extraction_status == "ok" && !extracted_text.trim().is_empty() {
            let chunks = crate::utils::doc_context::chunk_text(extracted_text);
            for (idx, chunk) in chunks.iter().enumerate() {
                conn.execute(
                    "INSERT INTO document_chunks
                        (document_id, chunk_index, content, page_start, page_end,
                         section_label, location_label, paragraph_start, paragraph_end)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    rusqlite::params![
                        document_id,
                        idx as i32,
                        chunk.content,
                        chunk.page_start,
                        chunk.page_end,
                        chunk.section_label,
                        chunk.location_label,
                        chunk.paragraph_start,
                        chunk.paragraph_end,
                    ],
                )?;
            }
            info!(
                "Document {} extraction repaired: {} chars, {} chunks rebuilt",
                document_id, char_count, chunks.len()
            );
        } else {
            warn!(
                "Document {} re-extraction still unusable (status='{}'): {}",
                document_id,
                extraction_status,
                extraction_error.unwrap_or("no error detail")
            );
        }
        Ok(())
    }

    fn find_by_hash(
        conn: &rusqlite::Connection,
        hash: &str,
    ) -> anyhow::Result<Option<DocumentRecord>> {
        conn.query_row(
            "SELECT id, content_hash, original_filename, source_path, source_kind,
                    mime_type, size_bytes, extracted_text, extraction_status,
                    extraction_error, char_count, local_file_id, created_at, last_referenced_at
             FROM documents WHERE content_hash = ?1",
            [hash],
            Self::row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<DocumentRecord> {
        let created_str: String = row.get(12)?;
        let referenced_str: String = row.get(13)?;
        Ok(DocumentRecord {
            id: row.get(0)?,
            content_hash: row.get(1)?,
            original_filename: row.get(2)?,
            source_path: row.get(3)?,
            source_kind: row.get(4)?,
            mime_type: row.get(5)?,
            size_bytes: row.get(6)?,
            extracted_text: row.get(7)?,
            extraction_status: row.get(8)?,
            extraction_error: row.get(9)?,
            char_count: row.get(10)?,
            local_file_id: row.get(11)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            last_referenced_at: DateTime::parse_from_rfc3339(&referenced_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    /// Link a document to a session (idempotent). This is what makes a
    /// document attached in one chat browsable/known from any other.
    pub fn link_session(
        &self,
        session_id: &str,
        document_id: i64,
        attachment_source: &str,
    ) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR IGNORE INTO session_documents (session_id, document_id, attached_at, attachment_source)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, document_id, Utc::now().to_rfc3339(), attachment_source],
        )?;
        Ok(())
    }

    /// All documents ever linked to this session, most recently attached first.
    pub fn get_session_documents(&self, session_id: &str) -> anyhow::Result<Vec<DocumentRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT d.id, d.content_hash, d.original_filename, d.source_path, d.source_kind,
                    d.mime_type, d.size_bytes, d.extracted_text, d.extraction_status,
                    d.extraction_error, d.char_count, d.local_file_id, d.created_at, d.last_referenced_at
             FROM documents d
             JOIN session_documents sd ON sd.document_id = d.id
             WHERE sd.session_id = ?1
             ORDER BY sd.attached_at DESC",
        )?;
        let rows = stmt
            .query_map([session_id], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Look up a document by the blake3 hash of its original bytes.
    ///
    /// Exists so callers can answer "do we already have this file's text?"
    /// BEFORE paying to extract it. Hashing is microseconds; extraction is
    /// pdfium plus up to 50 pages of OCR. Every path that holds real bytes
    /// should check this first - see api::stream_api::upsert_and_link.
    pub fn get_document_by_hash(&self, content_hash: &str) -> anyhow::Result<Option<DocumentRecord>> {
        let conn = self.pool.get()?;
        Self::find_by_hash(&conn, content_hash)
    }

    /// Record a fresh reference to a document we already hold, without
    /// re-extracting: bump recency and fill in metadata this reference newly
    /// provides. `COALESCE` means an existing value is never overwritten with
    /// a null one, so a later reference can only ever ADD provenance.
    pub fn touch_document(
        &self,
        document_id: i64,
        source_path: Option<&str>,
        local_file_id: Option<i64>,
        mime_type: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE documents SET last_referenced_at = ?1,
                source_path = COALESCE(source_path, ?2),
                local_file_id = COALESCE(local_file_id, ?3),
                mime_type = COALESCE(mime_type, ?4)
             WHERE id = ?5",
            rusqlite::params![
                Utc::now().to_rfc3339(),
                source_path,
                local_file_id,
                mime_type,
                document_id
            ],
        )?;
        Ok(())
    }

    /// Local Storage files whose text has never been successfully extracted.
    ///
    /// Local Storage uploads extract in a background task, and a background
    /// task does not survive process exit. Uploading a file and closing the app
    /// moments later therefore leaves bytes in the vault with no `documents`
    /// row - permanently invisible to retrieval, because nothing would ever
    /// revisit it unless the user happened to attach it to a chat. This query
    /// is what lets startup find and finish that work.
    ///
    /// Also catches: extractions that failed transiently (OCR unavailable at
    /// the time), and files uploaded before the unified document store existed.
    ///
    /// Folders are excluded (nothing to extract). Rows whose type is outside
    /// the supported set are excluded too: legacy uploads predate the format
    /// gate, and re-extracting a `.zip` would only produce the mojibake the
    /// gate exists to prevent.
    ///
    /// Returns an empty vec (not an error) when `local_files` does not exist -
    /// this store is initialized standalone in tests and must not depend on
    /// another store's migration order, the same reasoning as
    /// `documents.local_file_id` carrying no FK constraint.
    pub fn local_files_needing_extraction(&self, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
        let conn = self.pool.get()?;
        let has_local_files: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_files'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if !has_local_files {
            return Ok(Vec::new());
        }

        let mut stmt = conn.prepare(
            "SELECT lf.id, lf.name
             FROM local_files lf
             LEFT JOIN documents d ON d.local_file_id = lf.id
             WHERE lf.is_directory = 0
               AND (d.id IS NULL
                    OR d.extraction_status != 'ok'
                    OR TRIM(d.extracted_text) = '')
             ORDER BY lf.id DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|(_, name)| crate::utils::is_supported_attachment(name))
            .collect())
    }

    /// Resolve a document by the Local Storage file it was created from.
    /// Local Storage files uploaded before this store existed have no
    /// document row yet - callers should upsert one lazily on a miss.
    pub fn get_document_by_local_file_id(&self, local_file_id: i64) -> anyhow::Result<Option<DocumentRecord>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, content_hash, original_filename, source_path, source_kind,
                    mime_type, size_bytes, extracted_text, extraction_status,
                    extraction_error, char_count, local_file_id, created_at, last_referenced_at
             FROM documents WHERE local_file_id = ?1",
            [local_file_id],
            Self::row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_document(&self, id: i64) -> anyhow::Result<DocumentRecord> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT id, content_hash, original_filename, source_path, source_kind,
                    mime_type, size_bytes, extracted_text, extraction_status,
                    extraction_error, char_count, local_file_id, created_at, last_referenced_at
             FROM documents WHERE id = ?1",
            [id],
            Self::row_to_record,
        )
        .map_err(|e| anyhow::anyhow!("document {} not found: {}", id, e))
    }

    /// Documents across ALL sessions matching a filename substring - the
    /// backbone of cross-session document awareness/browsing.
    pub fn search_documents(&self, query: &str, limit: i64) -> anyhow::Result<Vec<DocumentRecord>> {
        let conn = self.pool.get()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT id, content_hash, original_filename, source_path, source_kind,
                    mime_type, size_bytes, extracted_text, extraction_status,
                    extraction_error, char_count, local_file_id, created_at, last_referenced_at
             FROM documents WHERE original_filename LIKE ?1 ESCAPE '\\'
             ORDER BY last_referenced_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![pattern, limit], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rank documents against free-form user text, best match first.
    ///
    /// This is the retrieval primitive behind "which document said that" — the
    /// job the deleted embedding model used to do. It searches BOTH the filename
    /// and the extracted text, with filenames weighted higher so naming a file
    /// finds that file (see fts::documents_bm25_weights).
    ///
    /// Distinct from `search_documents`, which matches filenames with LIKE only
    /// and is for browsing, not retrieval. This one reads content.
    ///
    /// Returns an empty vec when the query has nothing searchable in it (all
    /// stop-words or punctuation) — never "everything", which as a retrieval
    /// result would flood the prompt with unrelated documents.
    ///
    /// Each result carries its BM25 score. SQLite returns these NEGATIVE with
    /// more negative meaning a better match; the sign is normalised here so
    /// callers can apply an intuitive "score >= threshold" cutoff.
    /// `time_range` narrows results to documents last referenced inside a
    /// window, for queries that name a date ("the contract I sent last week").
    /// Filtering in SQL rather than post-hoc in Rust matters: a post-filter
    /// applied after `LIMIT` would silently drop the best in-window match
    /// whenever more recent out-of-window documents outranked it.
    pub fn search_documents_fulltext(
        &self,
        user_text: &str,
        limit: i64,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> anyhow::Result<Vec<(DocumentRecord, f64)>> {
        let Some(query) = crate::memory_db::fts::fts_query_from_user_text(user_text) else {
            return Ok(Vec::new());
        };
        let conn = self.pool.get()?;
        let (filename_weight, body_weight) = crate::memory_db::fts::documents_bm25_weights();
        // Bound the window to strings even when absent, so one statement serves
        // both cases and the parameter positions never shift.
        let (range_start, range_end) = match time_range {
            Some((s, e)) => (Some(s.to_rfc3339()), Some(e.to_rfc3339())),
            None => (None, None),
        };

        // Only documents with usable content are candidates: a failed
        // extraction has nothing to retrieve, and surfacing it as a "match"
        // would put a "[Could not read ...]" marker in front of the model as
        // though it were an answer.
        let mut stmt = conn.prepare(
            "SELECT d.id, d.content_hash, d.original_filename, d.source_path, d.source_kind,
                    d.mime_type, d.size_bytes, d.extracted_text, d.extraction_status,
                    d.extraction_error, d.char_count, d.local_file_id, d.created_at,
                    d.last_referenced_at,
                    bm25(documents_fts, ?2, ?3) AS rank
             FROM documents_fts
             JOIN documents d ON d.id = documents_fts.rowid
             WHERE documents_fts MATCH ?1
               AND d.extraction_status = 'ok'
               AND TRIM(d.extracted_text) != ''
               AND (?4 IS NULL OR d.last_referenced_at >= ?4)
               AND (?5 IS NULL OR d.last_referenced_at <= ?5)
             ORDER BY rank ASC
             LIMIT ?6",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    query,
                    filename_weight,
                    body_weight,
                    range_start,
                    range_end,
                    limit
                ],
                |row| {
                    let record = Self::row_to_record(row)?;
                    let rank: f64 = row.get(14)?;
                    Ok((record, -rank))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Filenames only, most recently referenced first.
    ///
    /// Exists because the reference detector needs to know WHICH documents are
    /// stored (so "the merger agreement" can resolve to merger-agreement.pdf)
    /// and nothing more — and it needs that on EVERY turn.
    ///
    /// Using `all_documents` for this was a real cost, not a style point: that
    /// query pulls `extracted_text` for every row it returns, so asking it for
    /// 500 filenames loads 500 whole contracts into memory and throws the text
    /// away, before the model has produced a single token. This returns a few KB
    /// for the same library.
    pub fn all_document_filenames(&self, limit: i64) -> anyhow::Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT original_filename FROM documents
             ORDER BY last_referenced_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_documents(&self, limit: i64) -> anyhow::Result<Vec<DocumentRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, content_hash, original_filename, source_path, source_kind,
                    mime_type, size_bytes, extracted_text, extraction_status,
                    extraction_error, char_count, local_file_id, created_at, last_referenced_at
             FROM documents ORDER BY last_referenced_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All chunks for a document, in chunk_index order, with their structural
    /// provenance (page/slide/sheet, heading, paragraph range).
    pub fn get_chunks(&self, document_id: i64) -> anyhow::Result<Vec<DocumentChunkRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, document_id, chunk_index, content, page_start, page_end,
                    section_label, location_label, paragraph_start, paragraph_end
             FROM document_chunks WHERE document_id = ?1 ORDER BY chunk_index ASC",
        )?;
        let rows = stmt
            .query_map([document_id], |row| {
                Ok(DocumentChunkRecord {
                    id: row.get(0)?,
                    document_id: row.get(1)?,
                    chunk_index: row.get(2)?,
                    content: row.get(3)?,
                    page_start: row.get(4)?,
                    page_end: row.get(5)?,
                    section_label: row.get(6)?,
                    location_label: row.get(7)?,
                    paragraph_start: row.get(8)?,
                    paragraph_end: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use r2d2_sqlite::SqliteConnectionManager;

    fn test_store() -> (DocumentsStore, Arc<Pool<SqliteConnectionManager>>) {
        // max_size(1): a bare `:memory:` SQLite connection is an isolated
        // database per-connection - a pool that opens more than one physical
        // connection would silently scatter state across separate databases.
        // A single shared connection guarantees every pool.get() sees the
        // same in-memory database (production always uses a file-backed DB,
        // where this distinction does not apply).
        let manager = SqliteConnectionManager::memory();
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        {
            let conn = pool.get().unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at TIMESTAMP NOT NULL, last_accessed TIMESTAMP NOT NULL, metadata TEXT NOT NULL);
                 INSERT INTO sessions (id, created_at, last_accessed, metadata) VALUES ('s1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z','{}');
                 INSERT INTO sessions (id, created_at, last_accessed, metadata) VALUES ('s2','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z','{}');",
            )
            .unwrap();
        }
        let store = DocumentsStore::new(Arc::clone(&pool));
        store.initialize_schema().unwrap();
        (store, pool)
    }

    fn sample_doc<'a>(bytes: &'a [u8], filename: &'a str) -> NewDocument<'a> {
        NewDocument {
            original_bytes: bytes,
            original_filename: filename,
            source_path: Some("C:/docs/contract.pdf".to_string()),
            source_kind: "paperclip",
            mime_type: Some("application/pdf".to_string()),
            size_bytes: bytes.len() as i64,
            extracted_text: "This is the extracted contract text about indemnification.".to_string(),
            extraction_status: "ok",
            extraction_error: None,
            local_file_id: None,
        }
    }

    /// Regression test for a real production bug: link_session's INSERT
    /// into session_documents carries a foreign key on session_id. If the
    /// caller (stream_api::generate_stream) processes attachments BEFORE
    /// creating the session row - the exact ordering that shipped
    /// originally - this insert fails silently under FK enforcement (which
    /// production always has ON), leaving the document extracted and stored
    /// but never linked. build_document_context then finds nothing for the
    /// session, and the model reports it was never given the document.
    /// This proves the contract link_session depends on, and that the fix
    /// (create the session row first) is required, not optional.
    #[test]
    fn link_session_fails_under_fk_enforcement_when_session_row_missing() {
        let manager = SqliteConnectionManager::memory();
        let pool = Arc::new(Pool::builder().max_size(1).build(manager).unwrap());
        {
            let conn = pool.get().unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at TIMESTAMP NOT NULL, last_accessed TIMESTAMP NOT NULL, metadata TEXT NOT NULL);",
            )
            .unwrap();
        }
        let store = DocumentsStore::new(Arc::clone(&pool));
        store.initialize_schema().unwrap();

        let doc = store.upsert_document(sample_doc(b"contract bytes", "Agreement.pdf")).unwrap();

        // The bug: attaching to a session whose DB row does not exist yet.
        let result = store.link_session("brand-new-not-yet-created", doc.id, "paperclip");
        assert!(
            result.is_err(),
            "link_session must fail (loudly, via FK) when the session row doesn't exist yet - \
             this is exactly the silent-failure mode that shipped: stream_api must create the \
             session row BEFORE processing attachments, never after"
        );

        // The fix: create the session row FIRST, then linking succeeds and
        // the document becomes visible to that session.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "INSERT INTO sessions (id, created_at, last_accessed, metadata) VALUES (?1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '{}')",
                rusqlite::params!["brand-new-not-yet-created"],
            )
            .unwrap();
        }
        store.link_session("brand-new-not-yet-created", doc.id, "paperclip").unwrap();
        let docs = store.get_session_documents("brand-new-not-yet-created").unwrap();
        assert_eq!(docs.len(), 1, "document must be visible to the session once properly linked");
        assert_eq!(docs[0].original_filename, "Agreement.pdf");
    }

    #[test]
    fn same_bytes_dedup_across_sessions() {
        let (store, _pool) = test_store();
        let bytes = b"identical file contents";

        let doc1 = store.upsert_document(sample_doc(bytes, "contract.pdf")).unwrap();
        store.link_session("s1", doc1.id, "paperclip").unwrap();

        // Attached again in a DIFFERENT session with the same bytes
        let doc2 = store.upsert_document(sample_doc(bytes, "contract.pdf")).unwrap();
        store.link_session("s2", doc2.id, "paperclip").unwrap();

        assert_eq!(doc1.id, doc2.id, "identical bytes must resolve to ONE document");

        let s1_docs = store.get_session_documents("s1").unwrap();
        let s2_docs = store.get_session_documents("s2").unwrap();
        assert_eq!(s1_docs.len(), 1);
        assert_eq!(s2_docs.len(), 1);
        assert_eq!(s1_docs[0].id, s2_docs[0].id, "same document must be findable from both sessions");
    }

    #[test]
    fn different_bytes_are_different_documents() {
        let (store, _pool) = test_store();
        let d1 = store.upsert_document(sample_doc(b"file one", "a.pdf")).unwrap();
        let d2 = store.upsert_document(sample_doc(b"file two", "b.pdf")).unwrap();
        assert_ne!(d1.id, d2.id);
    }

    #[test]
    fn chunks_are_created_at_document_creation() {
        let (store, _pool) = test_store();
        let doc = store.upsert_document(sample_doc(b"some bytes", "notes.txt")).unwrap();
        let chunks = store.get_chunks(doc.id).unwrap();
        assert!(!chunks.is_empty(), "extracted text must be chunked immediately");
        assert!(chunks[0].content.contains("indemnification"));
    }

    #[test]
    fn failed_extraction_creates_no_chunks_but_document_exists() {
        let (store, _pool) = test_store();
        let mut doc_input = sample_doc(b"broken pdf bytes", "broken.pdf");
        doc_input.extraction_status = "failed";
        doc_input.extraction_error = Some("PDF is corrupt".to_string());
        doc_input.extracted_text = String::new();

        let doc = store.upsert_document(doc_input).unwrap();
        assert_eq!(doc.extraction_status, "failed");
        assert_eq!(doc.extraction_error.as_deref(), Some("PDF is corrupt"));
        let chunks = store.get_chunks(doc.id).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn reattachment_fills_in_missing_metadata_without_reextracting() {
        let (store, _pool) = test_store();
        let bytes = b"same content twice";

        let mut first = sample_doc(bytes, "report.docx");
        first.source_path = None; // first attach: no path known
        let d1 = store.upsert_document(first).unwrap();
        assert!(d1.source_path.is_none());

        // Re-attached later WITH a captured path
        let second = sample_doc(bytes, "report.docx"); // has source_path set
        let d2 = store.upsert_document(second).unwrap();
        assert_eq!(d1.id, d2.id);
        assert_eq!(d2.source_path.as_deref(), Some("C:/docs/contract.pdf"));
    }

    #[test]
    fn search_documents_by_filename() {
        let (store, _pool) = test_store();
        store.upsert_document(sample_doc(b"a", "MergerAgreement.pdf")).unwrap();
        store.upsert_document(sample_doc(b"b", "NDA.docx")).unwrap();
        let results = store.search_documents("agreement", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].original_filename, "MergerAgreement.pdf");
    }

    /// Product invariant: "no matter how many documents are attached, they
    /// must ALL be processed." get_session_documents is the load-bearing
    /// query for that promise - it is what document_memory reloads on every
    /// single turn - so it must have no cap, hidden or otherwise.
    ///
    /// This replaces an earlier test that asserted the same invariant through
    /// the cross-session candidate list. That path is gone with the embedding
    /// model, but the invariant it protected is not, so the coverage moves
    /// here rather than disappearing with the code it used to exercise.
    #[test]
    fn every_document_linked_to_a_session_is_returned_with_no_hidden_cap() {
        let (store, pool) = test_store();
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, created_at TIMESTAMP, \
                 last_accessed TIMESTAMP, metadata TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (id, created_at, last_accessed, metadata) \
                 VALUES ('bulk', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, '{}')",
                [],
            )
            .unwrap();
        }

        // 60 documents - more than any cap this codebase has ever carried
        // (the historical ones were 16, 30 and 500).
        for i in 0..60 {
            let doc = store
                .upsert_document(sample_doc(
                    format!("content {i}").as_bytes(),
                    &format!("file{i}.txt"),
                ))
                .unwrap();
            store.link_session("bulk", doc.id, "paperclip").unwrap();
        }

        let docs = store.get_session_documents("bulk").unwrap();
        assert_eq!(
            docs.len(),
            60,
            "all 60 session documents must be returned - a cap here would make \
             documents silently invisible to the model"
        );
    }
}
