-- Migration 012: Remove the KV-cache persistence subsystem entirely.
--
-- These tables backed `cache_management/`, which is deleted in the same
-- change. That subsystem never ran: the only producer of the entries stored
-- here was `LlamaKVCacheInterface`, which fabricated its output (fixed
-- `vec![0u8; 128]` tensors for 8 layers x 8 heads) rather than reading
-- anything from llama.cpp. Its single live caller flushed an EMPTY slice at
-- shutdown, so in practice these tables were never written on any install.
--
-- The design could not have worked as written. `kv_cache_entries` models
-- per-layer, per-head attention tensors that can be individually scored,
-- ranked and re-injected. llama.cpp exposes no such API: the only real KV
-- persistence llama-server offers is `POST /slots/{id}?action=save|restore`
-- with `--slot-save-path`, which writes ONE opaque blob for an entire slot,
-- tied to an exact token prefix. There is no addressable unit smaller than
-- the whole slot, so nothing here could ever have been filled with real data.
--
-- Real KV persistence, when it is built, will therefore store a FILE PATH and
-- a prefix hash, not tensor rows - it deliberately does not reuse this
-- schema, and this migration is what stops the old shape from being mistaken
-- for a foundation to build on.
--
-- Nothing a user authored is affected. Every table below holds derived
-- machine state; `sessions`, `messages` and `documents` are untouched.
--
-- Determinism note: migrations 001..011 are left as the historical record.
-- All four tables are created by 003_add_kv_snapshots.sql, so every on-disk
-- database has them. IF EXISTS is used regardless - a DROP can afford to be
-- defensive, unlike `ALTER TABLE ... DROP COLUMN` in migration 010.
--
-- The in-memory database used by tests is built from
-- memory_db::schema::SCHEMA_SQL, which never declared these tables at all, so
-- there is no Rust-side counterpart to clean here (unlike migrations 010 and
-- 011, where SCHEMA_SQL did have to be edited to match).

-- Child-before-parent. Both kv_cache_entries and kv_metadata carry foreign
-- keys into kv_snapshots, so they have to go first.
DROP TABLE IF EXISTS kv_cache_entries;
DROP TABLE IF EXISTS kv_metadata;
DROP TABLE IF EXISTS kv_snapshots;

-- Session-level cache statistics. FKs `sessions`, not `kv_snapshots`, so its
-- position here is independent of the three above.
DROP TABLE IF EXISTS kv_cache_metadata;

-- All indexes on the dropped tables (idx_kv_snapshots_session/_message/_type/
-- _hash, idx_kv_cache_snapshot/_key_hash/_importance/_access) are removed
-- automatically with their tables.
