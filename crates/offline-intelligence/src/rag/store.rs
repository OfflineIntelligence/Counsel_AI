//! Persistent retrieval index: chunks (byte ranges), a contentless FTS5
//! index, int8 vectors, defined terms, per-document index state and the
//! matter <-> document links used for scoping.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tracing::{debug, info};

use super::chunker::{self, CHUNKER_VERSION};

pub const SCHEMA_SQL: &str = include_str!("../memory_db/migrations/019_rag.sql");

/// Part slots per message in the vector index: content parts first, the
/// last slot reserved for the exchange's round key ([`ROUND_PART`]).
pub const MAX_MESSAGE_PARTS: i64 = 8;
/// Content parts one message is embedded in; text past them is not indexed.
pub const MAX_CONTENT_PARTS: i64 = MAX_MESSAGE_PARTS - 1;
/// Part number of an assistant reply's round key (question + answer points).
pub const ROUND_PART: i64 = MAX_MESSAGE_PARTS - 1;

/// Index entry for part `part` (>= 1) of message `message_id`. Part 0 is the
/// message id itself; later parts are negative, so they never collide with a
/// message id.
pub fn message_part_entry(message_id: i64, part: i64) -> i64 {
    if part == 0 {
        message_id
    } else {
        -(message_id * MAX_MESSAGE_PARTS + part)
    }
}

/// The message an index entry belongs to.
pub fn entry_message(entry: i64) -> i64 {
    if entry >= 0 {
        entry
    } else {
        (-entry) / MAX_MESSAGE_PARTS
    }
}

/// (message id, part) for a later-part entry; None for part 0.
fn entry_part(entry: i64) -> Option<(i64, i64)> {
    (entry < 0).then(|| ((-entry) / MAX_MESSAGE_PARTS, (-entry) % MAX_MESSAGE_PARTS))
}

/// One KV state saved to disk: which message prefix it holds, for which
/// model, engine build and context size, and where its file is.
#[derive(Debug, Clone, PartialEq)]
pub struct KvStoreEntry {
    pub key: String,
    pub session_id: String,
    pub n_messages: i64,
    pub model_id: String,
    pub engine_id: String,
    pub ctx_size: u32,
    pub n_tokens: u64,
    pub bytes: u64,
    pub file: String,
    pub last_used: i64,
}

fn kv_row(r: &rusqlite::Row) -> rusqlite::Result<KvStoreEntry> {
    Ok(KvStoreEntry {
        key: r.get(0)?,
        session_id: r.get(1)?,
        n_messages: r.get(2)?,
        model_id: r.get(3)?,
        engine_id: r.get(4)?,
        ctx_size: r.get::<_, i64>(5)? as u32,
        n_tokens: r.get::<_, i64>(6)? as u64,
        bytes: r.get::<_, i64>(7)? as u64,
        file: r.get(8)?,
        last_used: r.get(9)?,
    })
}

const KV_COLUMNS: &str = "key, session_id, n_messages, model_id, engine_id, ctx_size, n_tokens, bytes, file, last_used";

#[derive(Debug, Clone, Serialize)]
pub struct ChunkRow {
    pub id: i64,
    pub document_id: i64,
    pub ord: i64,
    pub byte_start: i64,
    pub byte_end: i64,
    pub page_start: Option<i32>,
    pub page_end: Option<i32>,
    pub location_label: Option<String>,
    pub section_label: Option<String>,
    pub section_path: Option<String>,
    pub para_start: Option<i32>,
    pub para_end: Option<i32>,
    pub prefix_start: Option<i64>,
    pub prefix_end: Option<i64>,
}

impl ChunkRow {
    pub fn prefix(&self) -> Option<(usize, usize)> {
        match (self.prefix_start, self.prefix_end) {
            (Some(s), Some(e)) if s >= 0 && e > s => Some((s as usize, e as usize)),
            _ => None,
        }
    }

    /// The chunk's text as indexed and shown (header row first for sheets).
    pub fn text(&self, document_text: &str) -> String {
        chunker::chunk_text_with_prefix(document_text, self.byte_start as usize, self.byte_end as usize, self.prefix())
    }

    pub fn citation(&self) -> String {
        chunker::citation(
            self.page_start,
            self.page_end,
            self.location_label.as_deref(),
            self.section_label.as_deref(),
            self.para_start,
            self.para_end,
        )
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IndexTotals {
    pub documents: i64,
    pub documents_chunked: i64,
    pub documents_embedded: i64,
    pub documents_failed: i64,
    pub chunks: i64,
    pub chunks_embedded: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocumentIndexState {
    pub document_id: i64,
    pub status: String,
    pub chunk_count: i64,
    pub embedded_count: i64,
    pub embed_model: Option<String>,
    pub error: Option<String>,
}

/// RAG tables (019_rag.sql plus later columns). Only called from the
/// migration chain, the single schema route.
pub(crate) fn create_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    // Columns added after the table first shipped.
    for stmt in [
        "ALTER TABLE rag_chunks ADD COLUMN prefix_start INTEGER",
        "ALTER TABLE rag_chunks ADD COLUMN prefix_end INTEGER",
        "ALTER TABLE rag_chunks ADD COLUMN is_toc INTEGER NOT NULL DEFAULT 0",
    ] {
        if let Err(e) = conn.execute(stmt, []) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
    }
    Ok(())
}

/// Text fed to the keyword index for one chunk: a context header (file name
/// and section path) followed by the chunk's content without page markers.
pub fn index_header(filename: &str, section_path: Option<&str>) -> String {
    let stem = filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(filename);
    let readable = stem.replace(['_', '-'], " ");
    match section_path {
        Some(p) => format!("{} {}", readable, p),
        None => readable,
    }
}

/// Chunk + keyword-index + term-extract one document, replacing any previous
/// index for it. Runs in one transaction on the given connection.
pub fn index_document_text(
    conn: &Connection,
    document_id: i64,
    filename: &str,
    text: &str,
) -> anyhow::Result<usize> {
    let chunks = chunker::chunk_document(text);
    let terms = chunker::extract_defined_terms(text);

    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM rag_chunks WHERE document_id = ?1", [document_id])?;
    tx.execute("DELETE FROM rag_terms WHERE document_id = ?1", [document_id])?;
    // The legacy chunk table duplicated the text; it is superseded by
    // byte ranges, so reclaim that space.
    let _ = tx.execute("DELETE FROM document_chunks WHERE document_id = ?1", [document_id]);

    let mut chunk_ids: Vec<(i64, usize, usize)> = Vec::with_capacity(chunks.len());
    {
        let mut insert_chunk = tx.prepare_cached(
            "INSERT INTO rag_chunks
                (document_id, ord, byte_start, byte_end, page_start, page_end,
                 location_label, section_label, section_path, para_start, para_end,
                 prefix_start, prefix_end, is_toc)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        )?;
        let mut insert_fts = tx.prepare_cached(
            "INSERT INTO rag_chunks_fts(rowid, heading, body) VALUES (?1, ?2, ?3)",
        )?;
        for (ord, c) in chunks.iter().enumerate() {
            insert_chunk.execute(params![
                document_id,
                ord as i64,
                c.byte_start as i64,
                c.byte_end as i64,
                c.page_start,
                c.page_end,
                c.location_label,
                c.section_label,
                c.section_path,
                c.para_start,
                c.para_end,
                c.prefix_start.map(|v| v as i64),
                c.prefix_end.map(|v| v as i64),
                c.is_toc as i64,
            ])?;
            let id = tx.last_insert_rowid();
            if c.is_toc {
                // Contents pages are not searchable: they echo every heading.
                chunk_ids.push((id, c.byte_start, c.byte_end));
                continue;
            }
            let body = chunker::chunk_text_with_prefix(text, c.byte_start, c.byte_end, c.prefix_start.zip(c.prefix_end));
            insert_fts.execute(params![id, index_header(filename, c.section_path.as_deref()), body])?;
            chunk_ids.push((id, c.byte_start, c.byte_end));
        }

        let mut insert_term = tx.prepare_cached(
            "INSERT INTO rag_terms (document_id, term_norm, term, chunk_id, byte_start, byte_end)
             VALUES (?1,?2,?3,?4,?5,?6)",
        )?;
        for t in &terms {
            let chunk_id = chunk_ids
                .iter()
                .find(|(_, s, e)| t.byte_start >= *s && t.byte_start < *e)
                .map(|(id, _, _)| *id);
            insert_term.execute(params![
                document_id,
                t.term.to_lowercase(),
                t.term,
                chunk_id,
                t.byte_start as i64,
                t.byte_end as i64
            ])?;
        }
    }

    tx.execute(
        "INSERT INTO rag_documents (document_id, chunker_version, embed_model, status, chunk_count,
                                    embedded_count, error, updated_at)
         VALUES (?1, ?2, NULL, 'chunked', ?3, 0, NULL, CURRENT_TIMESTAMP)
         ON CONFLICT(document_id) DO UPDATE SET
            chunker_version = excluded.chunker_version, embed_model = NULL, status = 'chunked',
            chunk_count = excluded.chunk_count, embedded_count = 0, error = NULL,
            updated_at = CURRENT_TIMESTAMP",
        params![document_id, CHUNKER_VERSION, chunks.len() as i64],
    )?;
    tx.commit()?;

    debug!(
        "Indexed document {} ('{}'): {} chunks, {} defined terms",
        document_id, filename, chunks.len(), terms.len()
    );
    super::signal_index_work();
    Ok(chunks.len())
}

/// Remove a document's index (used when its extraction becomes unusable).
pub fn clear_document_index(conn: &Connection, document_id: i64, status: &str, error: Option<&str>) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM rag_chunks WHERE document_id = ?1", [document_id])?;
    tx.execute("DELETE FROM rag_terms WHERE document_id = ?1", [document_id])?;
    let _ = tx.execute("DELETE FROM document_chunks WHERE document_id = ?1", [document_id]);
    tx.execute(
        "INSERT INTO rag_documents (document_id, chunker_version, embed_model, status, chunk_count,
                                    embedded_count, error, updated_at)
         VALUES (?1, ?2, NULL, ?3, 0, 0, ?4, CURRENT_TIMESTAMP)
         ON CONFLICT(document_id) DO UPDATE SET status = excluded.status, chunk_count = 0,
            embedded_count = 0, error = excluded.error, chunker_version = excluded.chunker_version,
            updated_at = CURRENT_TIMESTAMP",
        params![document_id, CHUNKER_VERSION, status, error],
    )?;
    tx.commit()?;
    Ok(())
}

pub struct RagStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

fn row_to_chunk(row: &rusqlite::Row) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        document_id: row.get(1)?,
        ord: row.get(2)?,
        byte_start: row.get(3)?,
        byte_end: row.get(4)?,
        page_start: row.get(5)?,
        page_end: row.get(6)?,
        location_label: row.get(7)?,
        section_label: row.get(8)?,
        section_path: row.get(9)?,
        para_start: row.get(10)?,
        para_end: row.get(11)?,
        prefix_start: row.get(12)?,
        prefix_end: row.get(13)?,
    })
}

const CHUNK_COLUMNS: &str = "c.id, c.document_id, c.ord, c.byte_start, c.byte_end, c.page_start, c.page_end,
     c.location_label, c.section_label, c.section_path, c.para_start, c.para_end, c.prefix_start, c.prefix_end";

fn ids_json(ids: &HashSet<i64>) -> String {
    let mut v: Vec<i64> = ids.iter().copied().collect();
    v.sort_unstable();
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string())
}

impl RagStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Bring the database this store uses up to the current schema. Goes
    /// through the migration chain like every other caller.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        crate::memory_db::migration::ensure_schema(&self.pool)
    }

    pub fn conn(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        Ok(self.pool.get()?)
    }

    // ---- indexing state -------------------------------------------------

    /// Documents with usable text whose chunk index is missing or stale.
    pub fn documents_needing_chunking(&self, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT d.id, d.original_filename FROM documents d
             LEFT JOIN rag_documents r ON r.document_id = d.id
             WHERE d.extraction_status = 'ok' AND length(d.extracted_text) > 0
               AND (r.document_id IS NULL OR r.chunker_version < ?1)
             ORDER BY d.last_referenced_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![CHUNKER_VERSION, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn index_document(&self, document_id: i64) -> anyhow::Result<usize> {
        let conn = self.pool.get()?;
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT original_filename, extracted_text, extraction_status FROM documents WHERE id = ?1",
                [document_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((filename, text, status)) = row else {
            return Ok(0);
        };
        if status != "ok" || text.trim().is_empty() {
            clear_document_index(&conn, document_id, "failed", Some("no usable extracted text"))?;
            return Ok(0);
        }
        index_document_text(&conn, document_id, &filename, &text)
    }

    /// Make sure the given documents have a current chunk index (cheap when
    /// they already do). Used on the request path so a just-attached file is
    /// searchable before the background indexer reaches it.
    pub fn ensure_chunked(&self, document_ids: &[i64]) -> anyhow::Result<()> {
        if document_ids.is_empty() {
            return Ok(());
        }
        let stale: Vec<i64> = {
            let conn = self.pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT d.id FROM documents d LEFT JOIN rag_documents r ON r.document_id = d.id
                 WHERE d.id = ?1 AND d.extraction_status = 'ok' AND length(d.extracted_text) > 0
                   AND (r.document_id IS NULL OR r.chunker_version < ?2)",
            )?;
            let mut out = Vec::new();
            for id in document_ids {
                if stmt.exists(params![id, CHUNKER_VERSION])? {
                    out.push(*id);
                }
            }
            out
        };
        for id in stale {
            self.index_document(id)?;
        }
        Ok(())
    }

    pub fn document_state(&self, document_id: i64) -> anyhow::Result<Option<DocumentIndexState>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT document_id, status, chunk_count, embedded_count, embed_model, error
             FROM rag_documents WHERE document_id = ?1",
            [document_id],
            |r| {
                Ok(DocumentIndexState {
                    document_id: r.get(0)?,
                    status: r.get(1)?,
                    chunk_count: r.get(2)?,
                    embedded_count: r.get(3)?,
                    embed_model: r.get(4)?,
                    error: r.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn totals(&self) -> anyhow::Result<IndexTotals> {
        let conn = self.pool.get()?;
        let documents: i64 = conn.query_row(
            "SELECT count(*) FROM documents WHERE extraction_status = 'ok' AND length(extracted_text) > 0",
            [],
            |r| r.get(0),
        )?;
        let (chunked, embedded, failed): (i64, i64, i64) = conn.query_row(
            "SELECT
                coalesce(sum(CASE WHEN status IN ('chunked','embedding','embedded') AND chunker_version >= ?1 THEN 1 ELSE 0 END),0),
                coalesce(sum(CASE WHEN status = 'embedded' THEN 1 ELSE 0 END),0),
                coalesce(sum(CASE WHEN status = 'failed' THEN 1 ELSE 0 END),0)
             FROM rag_documents",
            [CHUNKER_VERSION],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let chunks: i64 = conn.query_row("SELECT count(*) FROM rag_chunks", [], |r| r.get(0))?;
        let chunks_embedded: i64 = conn.query_row("SELECT count(*) FROM rag_vectors", [], |r| r.get(0))?;
        Ok(IndexTotals {
            documents,
            documents_chunked: chunked,
            documents_embedded: embedded,
            documents_failed: failed,
            chunks,
            chunks_embedded,
        })
    }

    // ---- vectors ----------------------------------------------------------

    /// Next chunks lacking a vector, most recently used documents first.
    pub fn chunks_needing_vectors(&self, limit: i64) -> anyhow::Result<Vec<(ChunkRow, String)>> {
        let conn = self.pool.get()?;
        let sql = format!(
            "SELECT {}, d.original_filename FROM rag_chunks c
             JOIN documents d ON d.id = c.document_id
             LEFT JOIN rag_vectors v ON v.chunk_id = c.id
             WHERE v.chunk_id IS NULL AND c.is_toc = 0
             ORDER BY d.last_referenced_at DESC, c.document_id, c.ord
             LIMIT ?1",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([limit], |r| Ok((row_to_chunk(r)?, r.get::<_, String>(14)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn store_vectors(&self, model_key: &str, items: &[(i64, Vec<u8>)]) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.pool.get()?;
        let tx = conn.unchecked_transaction()?;
        let mut touched: HashSet<i64> = HashSet::new();
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO rag_vectors (chunk_id, vec)
                 SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM rag_chunks WHERE id = ?1)",
            )?;
            let mut doc_of = tx.prepare_cached("SELECT document_id FROM rag_chunks WHERE id = ?1")?;
            for (chunk_id, blob) in items {
                insert.execute(params![chunk_id, blob])?;
                if let Some(doc) = doc_of.query_row([chunk_id], |r| r.get::<_, i64>(0)).optional()? {
                    touched.insert(doc);
                }
            }
        }
        for doc in touched {
            tx.execute(
                "UPDATE rag_documents SET
                    embedded_count = (SELECT count(*) FROM rag_vectors v JOIN rag_chunks c ON c.id = v.chunk_id
                                      WHERE c.document_id = ?1),
                    embed_model = ?2,
                    status = CASE WHEN (SELECT count(*) FROM rag_vectors v JOIN rag_chunks c ON c.id = v.chunk_id
                                        WHERE c.document_id = ?1) >= chunk_count THEN 'embedded' ELSE 'embedding' END,
                    updated_at = CURRENT_TIMESTAMP
                 WHERE document_id = ?1",
                params![doc, model_key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every vector (the embedding model changed).
    pub fn clear_all_vectors(&self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM rag_vectors", [])?;
        let _ = conn.execute("DELETE FROM rag_message_vectors", []);
        let _ = conn.execute("DELETE FROM rag_message_part_vectors", []);
        conn.execute(
            "UPDATE rag_documents SET embedded_count = 0, embed_model = NULL,
                status = CASE WHEN status IN ('embedded','embedding') THEN 'chunked' ELSE status END",
            [],
        )?;
        info!("Cleared all stored embeddings");
        Ok(())
    }

    /// Stream all vectors for the in-memory index.
    pub fn for_each_vector(&self, mut f: impl FnMut(i64, i64, &[u8])) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT v.chunk_id, c.document_id, v.vec FROM rag_vectors v JOIN rag_chunks c ON c.id = v.chunk_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let doc: i64 = row.get(1)?;
            let blob = row.get_ref(2)?.as_blob()?;
            f(id, doc, blob);
        }
        Ok(())
    }

    pub fn vectors_for(&self, chunk_ids: &[i64]) -> anyhow::Result<HashMap<i64, Vec<u8>>> {
        let conn = self.pool.get()?;
        let ids: HashSet<i64> = chunk_ids.iter().copied().collect();
        let mut stmt = conn.prepare(
            "SELECT chunk_id, vec FROM rag_vectors WHERE chunk_id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt
            .query_map([ids_json(&ids)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(rows)
    }

    // ---- reads ------------------------------------------------------------

    pub fn chunks_by_ids(&self, ids: &[i64]) -> anyhow::Result<Vec<ChunkRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.pool.get()?;
        let set: HashSet<i64> = ids.iter().copied().collect();
        let sql = format!(
            "SELECT {} FROM rag_chunks c WHERE c.id IN (SELECT value FROM json_each(?1))",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([ids_json(&set)], row_to_chunk)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn document_chunks(&self, document_id: i64) -> anyhow::Result<Vec<ChunkRow>> {
        let conn = self.pool.get()?;
        let sql = format!("SELECT {} FROM rag_chunks c WHERE c.document_id = ?1 ORDER BY c.ord", CHUNK_COLUMNS);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([document_id], row_to_chunk)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn neighbour_chunks(&self, document_id: i64, ord_from: i64, ord_to: i64) -> anyhow::Result<Vec<ChunkRow>> {
        let conn = self.pool.get()?;
        let sql = format!(
            "SELECT {} FROM rag_chunks c WHERE c.document_id = ?1 AND c.ord BETWEEN ?2 AND ?3 AND c.is_toc = 0 ORDER BY c.ord",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![document_id, ord_from, ord_to], row_to_chunk)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Keyword search over chunks. `scope = None` searches everything.
    pub fn keyword_search(
        &self,
        fts_query: &str,
        scope: Option<&HashSet<i64>>,
        limit: i64,
    ) -> anyhow::Result<Vec<(i64, i64, f64)>> {
        let conn = self.pool.get()?;
        let scope_json = scope.map(ids_json);
        let mut stmt = conn.prepare_cached(
            "SELECT c.id, c.document_id, bm25(rag_chunks_fts, 3.0, 1.0) AS rank
             FROM rag_chunks_fts JOIN rag_chunks c ON c.id = rag_chunks_fts.rowid
             WHERE rag_chunks_fts MATCH ?1
               AND (?2 IS NULL OR c.document_id IN (SELECT value FROM json_each(?2)))
             ORDER BY rank ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![fts_query, scope_json, limit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, -r.get::<_, f64>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>();
        match rows {
            Ok(r) => Ok(r),
            Err(e) => {
                debug!("Chunk keyword search failed for query {:?}: {}", fts_query, e);
                Ok(Vec::new())
            }
        }
    }

    /// Chunks whose section number matches (`12.3` matches `12.3`, `12.3(b)`).
    pub fn section_search(&self, number_key: &str, scope: Option<&HashSet<i64>>, limit: i64) -> anyhow::Result<Vec<ChunkRow>> {
        let conn = self.pool.get()?;
        let scope_json = scope.map(ids_json);
        let sql = format!(
            "SELECT {} FROM rag_chunks c
             WHERE c.section_label IS NOT NULL AND c.is_toc = 0
               AND (?1 IS NULL OR c.document_id IN (SELECT value FROM json_each(?1)))
             ORDER BY c.document_id, c.ord",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut out = Vec::new();
        let mut rows = stmt.query(params![scope_json])?;
        while let Some(row) = rows.next()? {
            let chunk = row_to_chunk(row)?;
            let label = chunk.section_label.as_deref().unwrap_or("");
            let key = chunker::section_number_key(label);
            if key == number_key || key.starts_with(&format!("{}.", number_key)) {
                out.push(chunk);
                if out.len() as i64 >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Chunks holding the definition of a term.
    pub fn term_search(&self, term: &str, scope: Option<&HashSet<i64>>, limit: i64) -> anyhow::Result<Vec<ChunkRow>> {
        let conn = self.pool.get()?;
        let scope_json = scope.map(ids_json);
        let sql = format!(
            "SELECT {} FROM rag_terms t JOIN rag_chunks c ON c.id = t.chunk_id
             WHERE t.term_norm = ?1
               AND (?2 IS NULL OR t.document_id IN (SELECT value FROM json_each(?2)))
             ORDER BY t.document_id LIMIT ?3",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![term.trim().to_lowercase(), scope_json, limit], row_to_chunk)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_terms(&self, scope: Option<&HashSet<i64>>, limit: i64) -> anyhow::Result<Vec<String>> {
        let conn = self.pool.get()?;
        let scope_json = scope.map(ids_json);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT term_norm FROM rag_terms
             WHERE (?1 IS NULL OR document_id IN (SELECT value FROM json_each(?1)))
             ORDER BY length(term_norm) DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![scope_json, limit], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of pages a document has (highest page marker).
    pub fn page_count(&self, document_id: i64) -> anyhow::Result<Option<i32>> {
        let conn = self.pool.get()?;
        let n: Option<i32> = conn.query_row(
            "SELECT max(coalesce(page_end, page_start)) FROM rag_chunks WHERE document_id = ?1",
            [document_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    // ---- matters ------------------------------------------------------------

    pub fn link_document_to_matter(&self, matter_id: i64, document_id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR IGNORE INTO matter_documents (matter_id, document_id, linked_at)
             SELECT ?1, ?2, CURRENT_TIMESTAMP
             WHERE EXISTS (SELECT 1 FROM matters WHERE id = ?1)
               AND EXISTS (SELECT 1 FROM documents WHERE id = ?2)",
            params![matter_id, document_id],
        )?;
        Ok(())
    }

    pub fn unlink_document_from_matter(&self, matter_id: i64, document_id: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "DELETE FROM matter_documents WHERE matter_id = ?1 AND document_id = ?2",
            params![matter_id, document_id],
        )?;
        Ok(())
    }

    pub fn matter_document_ids(&self, matter_id: i64) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT document_id FROM matter_documents WHERE matter_id = ?1")?;
        let rows = stmt.query_map([matter_id], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn document_matter_ids(&self, document_id: i64) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT matter_id FROM matter_documents WHERE document_id = ?1")?;
        let rows = stmt.query_map([document_id], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Documents not linked to any matter ("General").
    pub fn unassigned_document_ids(&self) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT d.id FROM documents d
             WHERE NOT EXISTS (SELECT 1 FROM matter_documents m WHERE m.document_id = d.id)",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Reclaim free pages (left by the removed whole-document index, legacy
    /// chunk copies and re-indexing) when they are a meaningful share of the
    /// file. Returns bytes reclaimed.
    pub fn compact_if_worthwhile(&self, min_free_bytes: u64, min_free_ratio: f64) -> anyhow::Result<u64> {
        let conn = self.pool.get()?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let free_bytes = (free_pages * page_size).max(0) as u64;
        let ratio = if page_count > 0 { free_pages as f64 / page_count as f64 } else { 0.0 };
        if free_bytes < min_free_bytes || ratio < min_free_ratio {
            return Ok(0);
        }
        let before = (page_count * page_size) as u64;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        let after_pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let reclaimed = before.saturating_sub((after_pages * page_size) as u64);
        info!("Database compacted: {} bytes reclaimed ({:.0}% of the file was free)", reclaimed, ratio * 100.0);
        Ok(reclaimed)
    }

    /// Delete documents (and, by cascade, their index, chat links and matter
    /// links). Returns how many were removed.
    pub fn delete_documents(&self, ids: &[i64]) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.pool.get()?;
        let set: HashSet<i64> = ids.iter().copied().collect();
        let json = ids_json(&set);
        let tx = conn.unchecked_transaction()?;
        let _ = tx.execute(
            "DELETE FROM document_chunks WHERE document_id IN (SELECT value FROM json_each(?1))",
            [&json],
        );
        let n = tx.execute("DELETE FROM documents WHERE id IN (SELECT value FROM json_each(?1))", [&json])?;
        tx.commit()?;
        if n > 0 {
            info!("Deleted {} document(s) and their search index", n);
        }
        Ok(n)
    }

    /// Documents extracted from these Vault entries or anything inside them
    /// (folders resolved recursively). Call before the entries are deleted.
    pub fn documents_for_local_files(&self, local_file_ids: &[i64]) -> anyhow::Result<Vec<i64>> {
        if local_file_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.pool.get()?;
        let has_local_files = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_files'", [], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !has_local_files {
            return Ok(Vec::new());
        }
        let set: HashSet<i64> = local_file_ids.iter().copied().collect();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                SELECT value FROM json_each(?1)
                UNION
                SELECT lf.id FROM local_files lf JOIN tree t ON lf.parent_id = t.id
             )
             SELECT d.id FROM documents d WHERE d.local_file_id IN (SELECT id FROM tree)",
        )?;
        let rows = stmt.query_map([ids_json(&set)], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record that a draft produced a document. A newly saved copy replaces the
    /// previous saved copy, which is deleted unless something else still uses
    /// it (a chat, the Vault, the library, a published copy or another draft).
    pub fn link_draft_document(&self, draft_id: i64, document_id: i64, kind: &str) -> anyhow::Result<()> {
        let superseded: Vec<i64> = {
            let conn = self.pool.get()?;
            conn.execute(
                "INSERT INTO draft_documents (draft_id, document_id, kind) VALUES (?1, ?2, ?3)
                 ON CONFLICT(draft_id, document_id) DO UPDATE SET
                    kind = CASE WHEN draft_documents.kind = 'published' THEN 'published' ELSE excluded.kind END,
                    linked_at = CURRENT_TIMESTAMP",
                params![draft_id, document_id, kind],
            )?;
            if kind != "saved" {
                return Ok(());
            }
            let has_library = conn
                .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='library_items'", [], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            let library_clause = if has_library {
                "AND NOT EXISTS (SELECT 1 FROM library_items li WHERE li.document_id = dd.document_id)"
            } else {
                ""
            };
            let sql = format!(
                "SELECT dd.document_id FROM draft_documents dd JOIN documents d ON d.id = dd.document_id
                 WHERE dd.draft_id = ?1 AND dd.kind = 'saved' AND dd.document_id != ?2
                   AND d.local_file_id IS NULL
                   AND NOT EXISTS (SELECT 1 FROM session_documents sd WHERE sd.document_id = dd.document_id)
                   AND NOT EXISTS (SELECT 1 FROM draft_documents o WHERE o.document_id = dd.document_id
                                   AND (o.kind = 'published' OR o.draft_id != ?1))
                   {}",
                library_clause
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![draft_id, document_id], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            rows
        };
        if !superseded.is_empty() {
            self.delete_documents(&superseded)?;
        }
        Ok(())
    }

    /// Documents to delete with a draft: its saved copies (published copies
    /// belong to the Vault and stay).
    pub fn saved_documents_of_draft(&self, draft_id: i64) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT document_id FROM draft_documents dd WHERE draft_id = ?1 AND kind = 'saved'
               AND NOT EXISTS (SELECT 1 FROM draft_documents p WHERE p.document_id = dd.document_id AND p.kind = 'published')",
        )?;
        let rows = stmt.query_map([draft_id], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Sheet names in scope, e.g. ("Sheet: Fees", document id).
    pub fn sheet_labels(&self, scope: Option<&HashSet<i64>>) -> anyhow::Result<Vec<(String, i64)>> {
        let conn = self.pool.get()?;
        let scope_json = scope.map(ids_json);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT location_label, document_id FROM rag_chunks
             WHERE location_label LIKE 'Sheet:%'
               AND (?1 IS NULL OR document_id IN (SELECT value FROM json_each(?1)))",
        )?;
        let rows = stmt
            .query_map(params![scope_json], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Chunks of one sheet covering a given row.
    pub fn sheet_row_chunks(&self, document_id: i64, label: &str, row: i32) -> anyhow::Result<Vec<ChunkRow>> {
        let conn = self.pool.get()?;
        let sql = format!(
            "SELECT {} FROM rag_chunks c WHERE c.document_id = ?1 AND c.location_label = ?2
               AND c.para_start <= ?3 AND c.para_end >= ?3 ORDER BY c.ord",
            CHUNK_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![document_id, label, row], row_to_chunk)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- message vectors ----------------------------------------------------

    /// Chat messages without an embedding (user and assistant turns with
    /// enough text to mean something), newest first.
    pub fn messages_needing_vectors(&self, limit: i64) -> anyhow::Result<Vec<(i64, i64, String)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, s.rowid, m.content FROM messages m
             JOIN sessions s ON s.id = m.session_id
             LEFT JOIN rag_message_vectors v ON v.message_id = m.id
             WHERE v.message_id IS NULL AND m.role IN ('user', 'assistant') AND length(trim(m.content)) >= 20
             ORDER BY m.id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn store_message_vectors(&self, items: &[(i64, Vec<u8>)]) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.pool.get()?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO rag_message_vectors (message_id, vec)
                 SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?1)",
            )?;
            for (id, blob) in items {
                insert.execute(params![id, blob])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record (or replace) a saved KV state, indexed under every message
    /// prefix it holds: `prefixes[i]` = (key of messages[..=i], prompt chars
    /// up to and including message i).
    pub fn kv_put(&self, e: &KvStoreEntry, prefixes: &[(String, u64)]) -> anyhow::Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute(
            &format!("INSERT OR REPLACE INTO kv_store_entries ({}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", KV_COLUMNS),
            params![e.key, e.session_id, e.n_messages, e.model_id, e.engine_id, e.ctx_size as i64, e.n_tokens as i64, e.bytes as i64, e.file, e.last_used],
        )?;
        tx.execute("DELETE FROM kv_store_prefixes WHERE entry_key = ?1", [&e.key])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO kv_store_prefixes (prefix_key, entry_key, depth, chars) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, (k, chars)) in prefixes.iter().enumerate() {
                ins.execute(params![k, e.key, (i + 1) as i64, *chars as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The saved state sharing the longest message prefix with `keys`, from
    /// any conversation, for this model, engine build and context size.
    /// Returns the entry with `n_messages` set to the shared depth, and the
    /// estimated number of tokens that shared prefix holds.
    pub fn kv_best_prefix(
        &self,
        keys: &[String],
        model_id: &str,
        engine_id: &str,
        ctx_size: u32,
    ) -> anyhow::Result<Option<(KvStoreEntry, u64)>> {
        if keys.is_empty() {
            return Ok(None);
        }
        let conn = self.pool.get()?;
        let json = serde_json::to_string(keys)?;
        let cols = KV_COLUMNS
            .split(',')
            .map(|c| format!("e.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {cols}, p.depth, p.chars,
                    (SELECT q.chars FROM kv_store_prefixes q WHERE q.entry_key = e.key AND q.depth = e.n_messages)
             FROM kv_store_prefixes p JOIN kv_store_entries e ON e.key = p.entry_key
             WHERE p.prefix_key IN (SELECT value FROM json_each(?1))
               AND e.model_id = ?2 AND e.engine_id = ?3 AND e.ctx_size = ?4
             ORDER BY p.depth DESC, e.last_used DESC LIMIT 1"
        ))?;
        let hit = stmt
            .query_row(params![json, model_id, engine_id, ctx_size as i64], |r| {
                let mut e = kv_row(r)?;
                let depth: i64 = r.get(10)?;
                let chars: i64 = r.get(11)?;
                let total: Option<i64> = r.get(12)?;
                let tokens = match total {
                    Some(t) if t > 0 => (e.n_tokens as f64 * chars as f64 / t as f64) as u64,
                    _ => e.n_tokens,
                };
                e.n_messages = depth;
                Ok((e, tokens))
            })
            .optional()?;
        if hit.is_some() {
            return Ok(hit);
        }
        // States saved before the prefix index existed.
        Ok(self.kv_best(keys, model_id, engine_id, ctx_size)?.map(|e| {
            let t = e.n_tokens;
            (e, t)
        }))
    }

    /// The saved state for the longest of `keys` that was saved with this
    /// model, engine build and context size.
    pub fn kv_best(&self, keys: &[String], model_id: &str, engine_id: &str, ctx_size: u32) -> anyhow::Result<Option<KvStoreEntry>> {
        if keys.is_empty() {
            return Ok(None);
        }
        let conn = self.pool.get()?;
        let json = serde_json::to_string(keys)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM kv_store_entries
             WHERE key IN (SELECT value FROM json_each(?1)) AND model_id = ?2 AND engine_id = ?3 AND ctx_size = ?4
             ORDER BY n_messages DESC, last_used DESC LIMIT 1",
            KV_COLUMNS
        ))?;
        Ok(stmt.query_row(params![json, model_id, engine_id, ctx_size as i64], kv_row).optional()?)
    }

    pub fn kv_touch(&self, key: &str, now: i64) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("UPDATE kv_store_entries SET last_used = ?2 WHERE key = ?1", params![key, now])?;
        Ok(())
    }

    /// Every saved state, most recently used first.
    pub fn kv_all(&self) -> anyhow::Result<Vec<KvStoreEntry>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT {} FROM kv_store_entries ORDER BY last_used DESC, key", KV_COLUMNS))?;
        let rows = stmt.query_map([], kv_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn kv_delete(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM kv_store_entries WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Sessions that still exist, of those given (entries of deleted chats are evicted first).
    pub fn kv_live_sessions(&self) -> anyhow::Result<HashSet<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT DISTINCT e.session_id FROM kv_store_entries e JOIN sessions s ON s.id = e.session_id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// For an assistant reply, the user message it answers (the message just
    /// before it in the same conversation); None for anything else.
    pub fn question_answered_by(&self, message_id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT u.content FROM messages a
                 JOIN messages u ON u.session_id = a.session_id AND u.message_index = a.message_index - 1
                 WHERE a.id = ?1 AND a.role = 'assistant' AND u.role = 'user'",
                [message_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Vectors for later parts of long messages: (message id, part >= 1, vector).
    pub fn store_message_part_vectors(&self, items: &[(i64, i64, Vec<u8>)]) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.pool.get()?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO rag_message_part_vectors (message_id, part, vec)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?1)",
            )?;
            for (id, part, blob) in items {
                insert.execute(params![id, part, blob])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// (index entry, session rowid, vector) for every embedded message part.
    /// Part 0 uses the message id as its entry; later parts use
    /// [`message_part_entry`].
    pub fn for_each_message_part_vector(&self, mut f: impl FnMut(i64, i64, &[u8])) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT v.message_id, v.part, s.rowid, v.vec FROM rag_message_part_vectors v
             JOIN messages m ON m.id = v.message_id JOIN sessions s ON s.id = m.session_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let part: i64 = row.get(1)?;
            let session: i64 = row.get(2)?;
            let blob = row.get_ref(3)?.as_blob()?;
            f(message_part_entry(id, part), session, blob);
        }
        Ok(())
    }

    /// Vectors for index entries that name later message parts.
    pub fn message_part_vectors_for(&self, entries: &[i64]) -> anyhow::Result<HashMap<i64, Vec<u8>>> {
        let wanted: HashSet<(i64, i64)> = entries.iter().filter_map(|e| entry_part(*e)).collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: HashSet<i64> = wanted.iter().map(|(m, _)| *m).collect();
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT message_id, part, vec FROM rag_message_part_vectors WHERE message_id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt.query_map([ids_json(&ids)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (m, part, blob) = row?;
            if wanted.contains(&(m, part)) {
                out.insert(message_part_entry(m, part), blob);
            }
        }
        Ok(out)
    }

    /// (message id, session rowid, vector) for every embedded message.
    pub fn for_each_message_vector(&self, mut f: impl FnMut(i64, i64, &[u8])) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT v.message_id, s.rowid, v.vec FROM rag_message_vectors v
             JOIN messages m ON m.id = v.message_id JOIN sessions s ON s.id = m.session_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let session: i64 = row.get(1)?;
            let blob = row.get_ref(2)?.as_blob()?;
            f(id, session, blob);
        }
        Ok(())
    }

    pub fn message_vectors_for(&self, message_ids: &[i64]) -> anyhow::Result<HashMap<i64, Vec<u8>>> {
        let conn = self.pool.get()?;
        let ids: HashSet<i64> = message_ids.iter().copied().collect();
        let mut stmt = conn.prepare(
            "SELECT message_id, vec FROM rag_message_vectors WHERE message_id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt
            .query_map([ids_json(&ids)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(rows)
    }

    /// Row ids of the given sessions (the vector index groups messages by them).
    pub fn session_rowids(&self, session_ids: &HashSet<String>) -> anyhow::Result<HashSet<i64>> {
        let conn = self.pool.get()?;
        let mut v: Vec<&String> = session_ids.iter().collect();
        v.sort();
        let json = serde_json::to_string(&v)?;
        let mut stmt = conn.prepare("SELECT rowid FROM sessions WHERE id IN (SELECT value FROM json_each(?1))")?;
        let rows = stmt.query_map([json], |r| r.get(0))?.collect::<Result<HashSet<i64>, _>>()?;
        Ok(rows)
    }

    pub fn messages_by_ids(&self, ids: &[i64]) -> anyhow::Result<Vec<crate::memory_db::StoredMessage>> {
        let conn = self.pool.get()?;
        let set: HashSet<i64> = ids.iter().copied().collect();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, message_index, role, content, tokens, timestamp, importance_score
             FROM messages WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt
            .query_map([ids_json(&set)], |r| {
                let ts: String = r.get(6)?;
                Ok(crate::memory_db::StoredMessage {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    message_index: r.get(2)?,
                    role: r.get(3)?,
                    content: r.get(4)?,
                    tokens: r.get(5)?,
                    timestamp: chrono::DateTime::parse_from_rfc3339(&ts)
                        .map(|d| d.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now()),
                    importance_score: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_document_ids(&self) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT id FROM documents")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn knowledge_document_ids(&self) -> anyhow::Result<Vec<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT id FROM documents WHERE source_kind = 'knowledge'")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
