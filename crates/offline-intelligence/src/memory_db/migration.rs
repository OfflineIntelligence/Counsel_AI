//! Database migration system

use rusqlite::{Connection, OptionalExtension, Result};
use std::path::Path;
use tracing::{error, info, warn};

// Import the schema module from the same memory_db module
use crate::memory_db::schema;

/// Manages database schema migrations
pub struct MigrationManager<'a> {
    conn: &'a mut Connection,
}

impl<'a> MigrationManager<'a> {
    /// Create a new migration manager
    pub fn new(conn: &'a mut Connection) -> Self {
        Self { conn }
    }

    /// Initialize database with current schema
    pub fn initialize_database(&mut self) -> Result<()> {
        info!("Initializing memory database schema...");

        // Create schema version table if it doesn't exist
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        // Get current version
        let current_version: i32 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        info!("Current database schema version: {}", current_version);

        // Apply migrations based on current version
        self.apply_migrations(current_version)?;

        Ok(())
    }

    /// Apply all pending migrations
    fn apply_migrations(&mut self, current_version: i32) -> Result<()> {
        let migrations = get_migrations();

        for (version, migration_sql) in migrations.iter() {
            if *version > current_version {
                info!("Applying migration {}...", version);

                // Begin transaction - requires mutable self
                let tx = self.conn.transaction()?;

                // Apply migration
                if let Err(e) = tx.execute_batch(migration_sql) {
                    error!("Failed to apply migration {}: {}", version, e);
                    return Err(e);
                }

                // Record migration
                tx.execute("INSERT INTO schema_version (version) VALUES (?)", [version])?;

                // Commit transaction
                tx.commit()?;

                info!("Migration {} applied successfully", version);
            }
        }

        Ok(())
    }

    /// Create database connection with migrations applied
    pub fn create_connection(db_path: &Path) -> Result<Connection> {
        // Open or create database
        let mut conn = Connection::open(db_path)?;

        // Enable foreign keys and WAL mode for better performance
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -2000; -- 2MB cache
        ",
        )?;

        // Apply migrations - need mutable access
        let mut migrator = MigrationManager::new(&mut conn);
        migrator.initialize_database()?;

        Ok(conn)
    }

    /// Clean up old data - needs mutable access
    pub fn cleanup_old_data(&mut self, older_than_days: i32) -> Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(older_than_days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        // Delete old sessions and their related data (cascading delete)
        let deleted = self.conn.execute(
            "DELETE FROM sessions WHERE last_accessed < ?1",
            [&cutoff_str],
        )?;

        info!("Cleaned up {} old sessions", deleted);

        // Vacuum to reclaim space
        if deleted > 0 {
            self.conn.execute_batch("VACUUM")?;
            info!("Database vacuum completed");
        }

        Ok(deleted)
    }

    /// Get current schema version
    pub fn get_current_version(&self) -> Result<i32> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .or_else(|_| Ok(0))
    }

    /// Check if a specific migration has been applied
    pub fn has_migration_applied(&self, version: i32) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM schema_version WHERE version = ?",
                [version],
                |_| Ok(1),
            )
            .optional()
            .map(|result| result.is_some())
    }
}

/// Get all migration SQL scripts
fn get_migrations() -> Vec<(i32, &'static str)> {
    vec![
        (1, include_str!("migrations/001_initial.sql")),
        (2, include_str!("migrations/002_add_embeddings.sql")),
        (3, include_str!("migrations/003_add_kv_snapshots.sql")),
        (4, include_str!("migrations/004_local_files.sql")),
        (5, include_str!("migrations/005_curated_files.sql")),
        (6, include_str!("migrations/006_all_files.sql")),
        (7, include_str!("migrations/007_documents.sql")),
        (8, include_str!("migrations/008_document_chunk_anchors.sql")),
        (9, include_str!("migrations/009_document_chunk_locations.sql")),
        (10, include_str!("migrations/010_drop_embeddings.sql")),
        (11, include_str!("migrations/011_drop_summaries.sql")),
        (12, include_str!("migrations/012_drop_kv_cache.sql")),
        (13, include_str!("migrations/013_app_settings.sql")),
        (14, include_str!("migrations/014_drafts.sql")),
    ]
}

/// Get database statistics from a connection
/// This is safe to call even with a locked connection since it only performs read queries
pub fn get_database_stats(conn: &Connection) -> Result<schema::DatabaseStats> {
    // Helper function to safely get count from a table
    fn get_table_count(conn: &Connection, table_name: &str) -> Result<i64> {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table_name), [], |row| {
            row.get(0)
        })
        .or_else(|e| {
            warn!("Failed to get count from table {}: {}", table_name, e);
            Ok(0) // Return 0 if table doesn't exist or query fails
        })
    }

    let total_sessions = get_table_count(conn, "sessions")?;
    let total_messages = get_table_count(conn, "messages")?;
    let total_details = get_table_count(conn, "details")?;

    // Get database size - this query is safe and doesn't modify anything
    let database_size_bytes: i64 = conn
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(schema::DatabaseStats {
        total_sessions,
        total_messages,
        total_details,
        database_size_bytes,
    })
}

/// Get database statistics with connection creation
/// Useful when you don't have an existing connection
pub fn get_database_stats_from_path(db_path: &Path) -> Result<schema::DatabaseStats> {
    let conn = Connection::open(db_path)?;
    get_database_stats(&conn)
}

/// Run database maintenance tasks
pub fn run_maintenance(conn: &mut Connection) -> Result<()> {
    info!("Running database maintenance...");

    // Analyze for better query optimization
    conn.execute_batch("ANALYZE")?;

    // Incremental vacuum if needed
    conn.execute_batch("PRAGMA incremental_vacuum(100)")?;

    // Check integrity
    conn.execute_batch("PRAGMA integrity_check")?;

    info!("Database maintenance completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True if `table` has a column named `column`.
    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{}')", table))
            .expect("pragma_table_info");
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query columns")
            .filter_map(|r| r.ok())
            .collect();
        names.iter().any(|n| n == column)
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .expect("sqlite_master query")
        .is_some()
    }

    /// Fresh-install path: every migration runs in order, 001 through the
    /// latest. Migration 010's `ALTER TABLE ... DROP COLUMN` statements have no
    /// IF EXISTS form, so if 001/007 were ever retro-edited to stop creating
    /// those columns, 010 would abort the whole startup transaction and the app
    /// would not launch. Nothing else in the suite would catch that: the
    /// in-memory database used by every other test is built from
    /// schema::SCHEMA_SQL and never runs migrations at all.
    #[test]
    fn full_migration_chain_applies_cleanly_on_a_fresh_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        MigrationManager::new(&mut conn)
            .initialize_database()
            .expect("the whole migration chain must apply to a fresh database");

        let version: i32 = conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        // Derived from the migration list rather than restated, so adding a
        // migration does not require editing this assertion — a restated
        // literal is just a second number free to drift from the first.
        let expected = get_migrations().iter().map(|(v, _)| *v).max().unwrap_or(0);
        assert_eq!(
            version, expected,
            "schema_version must reach the latest migration in the list"
        );

        // The Tier-2 summary subsystem must be gone (migration 011).
        assert!(!table_exists(&conn, "summaries"), "summaries table must be dropped");

        // The KV-cache subsystem must be gone (migration 012). All four tables,
        // not just the parent - a surviving child table with a dangling foreign
        // key is exactly the state that makes a later migration fail.
        for kv_table in ["kv_cache_entries", "kv_metadata", "kv_snapshots", "kv_cache_metadata"] {
            assert!(
                !table_exists(&conn, kv_table),
                "{} must be dropped by migration 012",
                kv_table
            );
        }

        // `details` is a separate dormant table that 011 deliberately leaves
        // alone. Asserted so a later "tidy up the dead tables" pass has to make
        // that decision explicitly rather than by accident.
        assert!(table_exists(&conn, "details"), "details must NOT be dropped by 011");

        // The embedding subsystem must be gone.
        assert!(!table_exists(&conn, "embeddings"), "embeddings table must be dropped");
        assert!(!table_exists(&conn, "embedding_metadata"), "embedding_metadata must be dropped");
        assert!(!table_exists(&conn, "embedding_similarities"), "embedding_similarities must be dropped");
        assert!(!has_column(&conn, "document_chunks", "embedding"));
        assert!(!has_column(&conn, "document_chunks", "embedding_model"));
        assert!(!has_column(&conn, "messages", "embedding_generated"));

        // ...and nothing else may have gone with it. These are the columns the
        // product actually depends on, including the chunk provenance that
        // makes a "[p.12, Section 4.2(b)]" citation possible.
        assert!(has_column(&conn, "messages", "content"));
        assert!(has_column(&conn, "messages", "importance_score"));
        assert!(has_column(&conn, "document_chunks", "content"));
        for anchor in ["page_start", "page_end", "section_label", "location_label", "paragraph_start", "paragraph_end"] {
            assert!(
                has_column(&conn, "document_chunks", anchor),
                "chunk provenance column '{}' must survive migration 010",
                anchor
            );
        }
        assert!(table_exists(&conn, "documents"));
        assert!(table_exists(&conn, "session_documents"));
    }

    /// Upgrade path — the case that actually ships to existing users: a
    /// database already at version 9 that CONTAINS embedding rows and has
    /// embedding_generated set. Migrations 010 and 011 must drop all of that
    /// derived material without touching the authored content it came from.
    #[test]
    fn upgrading_a_populated_v9_database_drops_vectors_and_keeps_authored_content() {
        let mut conn = Connection::open_in_memory().unwrap();

        // Build a v9 database exactly as an existing install would have it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .unwrap();
        for (version, sql) in get_migrations().into_iter().filter(|(v, _)| *v <= 9) {
            conn.execute_batch(sql)
                .unwrap_or_else(|e| panic!("migration {} failed: {}", version, e));
            conn.execute("INSERT INTO schema_version (version) VALUES (?)", [version])
                .unwrap();
        }

        // Real data: a session, a message, a document + chunk, and the derived
        // vectors that migration 010 is meant to remove.
        conn.execute_batch(
            "INSERT INTO sessions (id, created_at, last_accessed, metadata)
                 VALUES ('s1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, '{}');
             INSERT INTO messages (session_id, message_index, role, content, tokens,
                                   timestamp, importance_score, embedding_generated)
                 VALUES ('s1', 0, 'user', 'the indemnity cap is five million', 7,
                         CURRENT_TIMESTAMP, 0.5, 1);
             INSERT INTO embeddings (message_id, embedding, embedding_model, generated_at)
                 VALUES (1, X'0102030405060708', 'bge-small-en-v1.5', CURRENT_TIMESTAMP);
             INSERT INTO documents (content_hash, original_filename, source_kind, size_bytes,
                                    extracted_text, extraction_status, char_count)
                 VALUES ('hash1', 'contract.pdf', 'paperclip', 10, 'Section 4.2 text', 'ok', 16);
             INSERT INTO document_chunks (document_id, chunk_index, content, embedding, embedding_model)
                 VALUES (1, 0, 'Section 4.2 text', X'0102030405060708', 'bge-small-en-v1.5');
             INSERT INTO summaries (session_id, message_range_start, message_range_end,
                                    summary_text, compression_ratio, key_topics, generated_at)
                 VALUES ('s1', 0, 0, 'discussed the indemnity cap', 0.25, '[\"indemnity\"]',
                         CURRENT_TIMESTAMP);
             INSERT INTO kv_snapshots (session_id, message_id, snapshot_type, kv_state,
                                       kv_state_hash, size_bytes)
                 VALUES ('s1', 1, 'full', X'0102030405060708', 'kvhash1', 8);
             INSERT INTO kv_metadata (snapshot_id, key_count, avg_key_size, avg_value_size)
                 VALUES (1, 2, 16, 128);
             INSERT INTO kv_cache_entries (snapshot_id, key_hash, key_data, value_data,
                                           key_type, layer_index, head_index)
                 VALUES (1, 'layer0_head0_k', X'0102', X'0304', 'attention_key', 0, 0);
             INSERT INTO kv_cache_metadata (session_id, total_entries, total_size_bytes)
                 VALUES ('s1', 1, 8);",
        )
        .unwrap();

        // Migrations 010, 011 and 012 remain to be applied.
        //
        // The summaries and kv_* rows above are deliberately POPULATED. No code
        // path in this codebase ever produced either, so in practice both ship
        // empty - but a DROP against tables that hold rows AND carry live
        // foreign keys (kv_cache_entries and kv_metadata into kv_snapshots,
        // kv_snapshots into both sessions and messages) is the only version
        // that could fail, and it is the version an unusual install would hit.
        // Testing the empty case would prove nothing.
        MigrationManager::new(&mut conn)
            .initialize_database()
            .expect("migrations 010, 011 and 012 must apply to a populated v9 database");

        assert!(!table_exists(&conn, "summaries"), "summaries must be dropped by 011");
        for kv_table in ["kv_cache_entries", "kv_metadata", "kv_snapshots", "kv_cache_metadata"] {
            assert!(
                !table_exists(&conn, kv_table),
                "{} must be dropped by migration 012 even when populated",
                kv_table
            );
        }
        assert!(!table_exists(&conn, "embeddings"));
        assert!(!has_column(&conn, "messages", "embedding_generated"));
        assert!(!has_column(&conn, "document_chunks", "embedding"));

        // The authored content the vectors were computed FROM must be intact -
        // dropping derived data must never take user content with it.
        let msg: String = conn
            .query_row("SELECT content FROM messages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg, "the indemnity cap is five million");
        let chunk: String = conn
            .query_row("SELECT content FROM document_chunks WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk, "Section 4.2 text");
        let doc: String = conn
            .query_row("SELECT extracted_text FROM documents WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(doc, "Section 4.2 text");
    }
}
