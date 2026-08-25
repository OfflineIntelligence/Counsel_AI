-- Migration 010: Remove the embedding subsystem entirely.
--
-- This product no longer ships an embedding model. Document retrieval is
-- structural (whole text when it fits the context budget, otherwise an
-- announced head excerpt - see context_engine::document_memory) and tier-3
-- conversation retrieval is keyword-based, so nothing reads a vector any
-- more. The tables and columns below are dropped rather than left dormant:
-- a populated `embeddings` table with no reader is a standing invitation to
-- assume semantic search still works.
--
-- The dropped data is DERIVED, never authored: every vector was computed
-- from `messages.content` / `document_chunks.content`, both of which are
-- retained. Nothing a user wrote is lost here.
--
-- Determinism note: migrations 001/002/007 are deliberately left untouched
-- as the historical record, which is also what makes the DROPs below safe to
-- write unconditionally. Every on-disk database - fresh or upgraded - has
-- passed through those migrations, so each object dropped here is guaranteed
-- to exist. `ALTER TABLE ... DROP COLUMN` (SQLite >= 3.35, bundled here)
-- has no IF EXISTS form, so that guarantee is what keeps this migration from
-- failing the whole startup transaction. Do NOT retro-edit 001/002/007 to
-- stop creating these objects without also making the statements below
-- conditional.
--
-- The in-memory database used by tests takes a different path
-- (memory_db::schema::SCHEMA_SQL + DocumentsStore::initialize_schema) and
-- never runs migrations; those definitions are cleaned up in Rust instead.

-- Child-before-parent: embedding_similarities carries foreign keys into
-- embeddings, so it has to go first.
DROP TABLE IF EXISTS embedding_similarities;
DROP TABLE IF EXISTS embedding_metadata;
DROP TABLE IF EXISTS embeddings;

-- Indexes on the dropped tables (idx_embeddings_message,
-- idx_embeddings_session_model, idx_embedding_similarities_*) are removed
-- automatically with their tables.

-- Cached per-chunk vectors. The chunk TEXT and its provenance columns
-- (page_start/page_end/section_label/location_label/paragraph_*) all stay:
-- those are what let an excerpt still be cited as "p.12, Section 4.2(b)".
ALTER TABLE document_chunks DROP COLUMN embedding;
ALTER TABLE document_chunks DROP COLUMN embedding_model;

-- Bookkeeping flag for "has this message been embedded yet". No index or
-- view references it, so dropping the column is a metadata-only operation.
ALTER TABLE messages DROP COLUMN embedding_generated;
