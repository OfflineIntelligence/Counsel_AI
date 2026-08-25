// "D:\_ProjectWorks\AUDIO_Interface\Server\src\memory_db\mod.rs"
//! Memory database module - SQLite-based storage for conversations, documents, and local files

pub mod schema;
pub mod migration;
pub mod conversation_store;
pub mod settings_store;
pub mod local_files_store;
pub mod all_files_store;
pub mod api_keys_store;
pub mod users_store;
pub mod documents_store;
pub mod drafts_store;
pub mod fts;

// Re-export commonly used types
pub use schema::*;
pub use migration::MigrationManager;
pub use conversation_store::ConversationStore;
pub use settings_store::SettingsStore;
pub use local_files_store::{LocalFilesStore, LocalFile, LocalFileTree};
pub use all_files_store::{AllFilesStore, AllFile, AllFileTree};
pub use api_keys_store::{ApiKeysStore, ApiKeyType, ApiKeyRecord, SimpleEncryption};
pub use users_store::{UsersStore, User};
pub use documents_store::{DocumentsStore, DocumentRecord, DocumentChunkRecord, NewDocument};
pub use drafts_store::{DraftsStore, DraftRecord, DraftVersionRecord, NewDraft};

use std::path::Path;
use std::sync::Arc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use tracing::info;

/// Main database manager that coordinates all stores
pub struct MemoryDatabase {
    pub conversations: ConversationStore,
    pub settings: SettingsStore,
    pub local_files: LocalFilesStore,
    pub all_files: AllFilesStore,
    pub api_keys: ApiKeysStore,
    pub users: UsersStore,
    pub documents: DocumentsStore,
    pub drafts: DraftsStore,
    pool: Arc<Pool<SqliteConnectionManager>>,
}

/// Transaction manager for atomic operations across stores
pub struct Transaction<'a> {
    conn: r2d2::PooledConnection<SqliteConnectionManager>,
    _marker: std::marker::PhantomData<&'a MemoryDatabase>,
}

impl<'a> Transaction<'a> {
    /// Commit the transaction
    pub fn commit(self) -> anyhow::Result<()> {
        // Changes are automatically committed when the connection is dropped
        Ok(())
    }

    /// Rollback the transaction
    pub fn rollback(self) -> anyhow::Result<()> {
        // SQLite auto-rolls back on DROP if not committed
        Ok(())
    }

    /// Get raw connection for store operations
    pub fn connection(&mut self) -> &mut rusqlite::Connection {
        &mut self.conn
    }
}

impl MemoryDatabase {
    /// Create a new memory database at the specified path
    pub fn new(db_path: &Path) -> anyhow::Result<Self> {
        info!("Opening memory database at: {}", db_path.display());

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let manager = SqliteConnectionManager::file(db_path)
            .with_flags(
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_FULL_MUTEX,
            )
            // Pragmas MUST be applied per connection, which is what with_init
            // does - it runs for every connection the pool opens, not just the
            // first.
            //
            // These used to be executed once on a single borrowed connection
            // while the pool was sized at 10. `busy_timeout`, `foreign_keys` and
            // `synchronous` are all PER-CONNECTION settings in SQLite, so
            // exactly one connection had them and the other nine ran with the
            // defaults: busy_timeout = 0 (fail instantly on any write
            // contention) and foreign_keys = OFF (no referential integrity at
            // all). Which connection a request got was pure luck.
            //
            // That was survivable while extraction was effectively serial. It is
            // not now: several format lanes upsert documents simultaneously, so
            // write contention is routine and a zero busy_timeout turns it
            // straight into "database is locked" instead of a short wait.
            //
            // journal_mode is the exception - WAL is recorded in the database
            // file itself and persists - but it is set here anyway so a fresh
            // file gets it on the very first connection.
            .with_init(|conn| {
                conn.execute_batch(
                    "PRAGMA foreign_keys = ON;
                     PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = NORMAL;
                     PRAGMA busy_timeout = 5000;",
                )
            });

        let pool = Pool::builder()
            .max_size(10)
            .build(manager)
            .map_err(|e| anyhow::anyhow!("Failed to create connection pool: {}", e))?;

        {
            let mut conn = pool.get()?;
            let mut migrator = migration::MigrationManager::new(&mut conn);
            migrator.initialize_database()?;
        }

        let pool = Arc::new(pool);

        // Get app data directories
        let app_data_dir = crate::config::get_app_data_dir();
        let files_dir = app_data_dir.join("files");

        // Initialize API keys store
        let api_keys = ApiKeysStore::new(Arc::clone(&pool));
        if let Err(e) = api_keys.initialize_schema() {
            tracing::warn!("Failed to initialize API keys schema: {}", e);
        }

        // Initialize users store
        let users = UsersStore::new(Arc::clone(&pool));
        if let Err(e) = users.initialize_schema() {
            tracing::warn!("Failed to initialize users schema: {}", e);
        }

        // Initialize documents store (unified document intelligence)
        let drafts = DraftsStore::new(Arc::clone(&pool));
        if let Err(e) = drafts.initialize_schema() {
            // Same reasoning as documents below: in-memory pools never run the
            // migration chain, so each store creates its own tables idempotently.
            tracing::warn!("Failed to initialize drafts schema: {}", e);
        }
        let documents = DocumentsStore::new(Arc::clone(&pool));
        if let Err(e) = documents.initialize_schema() {
            tracing::warn!("Failed to initialize documents schema: {}", e);
        }


        // Full-text indexes over documents + messages. Built here, on BOTH the
        // on-disk and in-memory paths, from one definition - migrations only run
        // on the on-disk path, so a numbered migration would need its DDL
        // duplicated for tests and the two copies would drift. Non-fatal: search
        // returning nothing is a degraded feature, not a reason to refuse to start.
        {
            match pool.get() {
                Ok(conn) => {
                    if let Err(e) = fts::ensure_fts_schema(&conn) {
                        tracing::warn!(
                            "Failed to initialize full-text search{}: {} - retrieval of                              past documents and messages will find nothing until this succeeds",
                            "", e
                        );
                    }
                }
                Err(e) => tracing::warn!("Could not initialize full-text search{}: {}", "", e),
            }
        }

        info!("Memory database initialized successfully");

        Ok(Self {
            conversations: ConversationStore::new(Arc::clone(&pool)),
            settings: SettingsStore::new(Arc::clone(&pool)),
            local_files: LocalFilesStore::new(Arc::clone(&pool), files_dir.clone()),
            all_files: AllFilesStore::new(Arc::clone(&pool), files_dir),
            api_keys,
            users,
            documents,
            drafts,
            pool,
        })
    }

    /// Create an in-memory database (useful for testing)
    pub fn new_in_memory() -> anyhow::Result<Self> {
        // Same per-connection pragma requirement as the on-disk path. The
        // in-memory database previously set NONE, so every test connection ran
        // with foreign_keys OFF - meaning FK-violation behaviour under test did
        // not match production, and busy_timeout 0 made concurrent writes fail
        // instantly rather than wait.
        let manager = SqliteConnectionManager::memory().with_init(|conn| {
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;",
            )
        });
        let pool = Pool::builder()
            .max_size(5)
            .build(manager)?;

        {
            let conn = pool.get()?;
            conn.execute_batch(schema::SCHEMA_SQL)?;
        }

        let pool = Arc::new(pool);

        // Get app data directories
        let app_data_dir = crate::config::get_app_data_dir();
        let files_dir = app_data_dir.join("files");

        // Initialize API keys store
        let api_keys = ApiKeysStore::new(Arc::clone(&pool));
        if let Err(e) = api_keys.initialize_schema() {
            tracing::warn!("Failed to initialize API keys schema (in-memory): {}", e);
        }

        // Initialize users store
        let users = UsersStore::new(Arc::clone(&pool));
        if let Err(e) = users.initialize_schema() {
            tracing::warn!("Failed to initialize users schema (in-memory): {}", e);
        }

        let drafts = DraftsStore::new(Arc::clone(&pool));
        if let Err(e) = drafts.initialize_schema() {
            // Same reasoning as documents below: in-memory pools never run the
            // migration chain, so each store creates its own tables idempotently.
            tracing::warn!("Failed to initialize drafts schema: {}", e);
        }
        let documents = DocumentsStore::new(Arc::clone(&pool));
        if let Err(e) = documents.initialize_schema() {
            tracing::warn!("Failed to initialize documents schema (in-memory): {}", e);
        }

        let local_files = LocalFilesStore::new(Arc::clone(&pool), files_dir.clone());
        if let Err(e) = local_files.initialize_schema() {
            tracing::warn!("Failed to initialize local_files schema (in-memory): {}", e);
        }

        let settings = SettingsStore::new(Arc::clone(&pool));
        if let Err(e) = settings.initialize_schema() {
            tracing::warn!("Failed to initialize app_settings schema (in-memory): {}", e);
        }

        // Full-text indexes over documents + messages. Built here, on BOTH the
        // on-disk and in-memory paths, from one definition - migrations only run
        // on the on-disk path, so a numbered migration would need its DDL
        // duplicated for tests and the two copies would drift. Non-fatal: search
        // returning nothing is a degraded feature, not a reason to refuse to start.
        {
            match pool.get() {
                Ok(conn) => {
                    if let Err(e) = fts::ensure_fts_schema(&conn) {
                        tracing::warn!(
                            "Failed to initialize full-text search{}: {} - retrieval of                              past documents and messages will find nothing until this succeeds",
                            " (in-memory)", e
                        );
                    }
                }
                Err(e) => tracing::warn!("Could not initialize full-text search{}: {}", " (in-memory)", e),
            }
        }

        Ok(Self {
            conversations: ConversationStore::new(Arc::clone(&pool)),
            settings,
            local_files,
            all_files: AllFilesStore::new(Arc::clone(&pool), files_dir),
            api_keys,
            users,
            documents,
            drafts,
            pool,
        })
    }

    /// Begin a transaction for atomic operations
    pub fn begin_transaction(&self) -> anyhow::Result<Transaction<'_>> {
        let conn = self.pool.get()?;
        conn.execute_batch("BEGIN IMMEDIATE TRANSACTION;")?;
        Ok(Transaction {
            conn,
            _marker: std::marker::PhantomData,
        })
    }

    /// Execute operations in a transaction
    pub fn with_transaction<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Transaction<'_>) -> anyhow::Result<T>,
    {
        let mut tx = self.begin_transaction()?;
        match f(&mut tx) {
            Ok(result) => {
                tx.commit()?;
                Ok(result)
            }
            Err(e) => {
                tx.rollback()?;
                Err(e)
            }
        }
    }

    /// Get database statistics
    pub fn get_stats(&self) -> anyhow::Result<DatabaseStats> {
        let conn = self.pool.get()?;
        Ok(migration::get_database_stats(&conn)?)
    }

    /// Cleanup old data (older than specified days)
    pub fn cleanup_old_data(&self, older_than_days: i32) -> anyhow::Result<usize> {
        let mut conn = self.pool.get()?;
        let mut migrator = migration::MigrationManager::new(&mut conn);
        Ok(migrator.cleanup_old_data(older_than_days)?)
    }

}

impl Drop for MemoryDatabase {
    fn drop(&mut self) {
        // Perform a final checkpoint on shutdown
        if let Ok(conn) = self.pool.get() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }
}

#[cfg(test)]
mod fts_integration_tests {
    use super::*;

    fn seeded() -> MemoryDatabase {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("chat-a", None).ok();
        db.conversations.create_session_with_id("chat-b", None).ok();
        db
    }

    fn add_doc(db: &MemoryDatabase, name: &str, text: &str) -> i64 {
        db.documents
            .upsert_document(NewDocument {
                original_bytes: text.as_bytes(),
                original_filename: name,
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: text.len() as i64,
                extracted_text: text.to_string(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap()
            .id
    }

    /// The FTS index must be live on the real database object, not just in the
    /// fts module's own hand-built fixtures. This is what proves the wiring in
    /// MemoryDatabase::new_in_memory actually ran.
    #[test]
    fn documents_are_searchable_through_the_real_database() {
        let db = seeded();
        add_doc(&db, "merger.pdf", "The acquirer shall assume all outstanding liabilities.");
        add_doc(&db, "lease.pdf", "The tenant shall pay rent monthly in advance.");

        let hits = db.documents.search_documents_fulltext("outstanding liabilities", 10, None).unwrap();
        assert_eq!(hits.len(), 1, "expected one match, got {:?}",
            hits.iter().map(|(d, _)| &d.original_filename).collect::<Vec<_>>());
        assert_eq!(hits[0].0.original_filename, "merger.pdf");
        assert!(hits[0].1 > 0.0, "score must be sign-normalised to positive: {}", hits[0].1);
    }

    /// Stemming through the full stack - the property that makes lexical search
    /// usable at all for legal text, where the same concept appears as
    /// terminate / terminated / termination across a single document.
    #[test]
    fn stemmed_queries_find_documents_through_the_real_database() {
        let db = seeded();
        add_doc(&db, "terms.pdf", "Either party may terminate this agreement upon notice.");

        for q in ["termination", "terminated", "terminating"] {
            let hits = db.documents.search_documents_fulltext(q, 10, None).unwrap();
            assert_eq!(hits.len(), 1, "'{}' must find the document", q);
            assert_eq!(hits[0].0.original_filename, "terms.pdf");
        }
    }

    /// Ranking must be meaningful, not incidental ordering. The document whose
    /// filename names the query term outranks one that merely mentions it.
    #[test]
    fn results_are_ranked_with_filename_matches_first() {
        let db = seeded();
        add_doc(&db, "indemnity.pdf", "General provisions and definitions apply.");
        add_doc(&db, "misc.pdf", "See the indemnity section for indemnity terms.");

        let hits = db.documents.search_documents_fulltext("indemnity", 10, None).unwrap();
        assert_eq!(hits.len(), 2, "both documents mention the term");
        assert_eq!(
            hits[0].0.original_filename, "indemnity.pdf",
            "the file NAMED for the query must rank first, got {:?}",
            hits.iter().map(|(d, s)| (&d.original_filename, s)).collect::<Vec<_>>()
        );
        assert!(
            hits[0].1 >= hits[1].1,
            "scores must be descending: {:?}",
            hits.iter().map(|(_, s)| s).collect::<Vec<_>>()
        );
    }

    /// A failed extraction has nothing to retrieve. Surfacing it as a match
    /// would put a "[Could not read ...]" marker in front of the model as
    /// though it were an answer.
    #[test]
    fn documents_with_failed_extraction_are_never_returned_as_matches() {
        let db = seeded();
        db.documents
            .upsert_document(NewDocument {
                original_bytes: b"\x00\xFF broken bytes",
                original_filename: "corrupt-contract.pdf",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: 15,
                extracted_text: String::new(),
                extraction_status: "failed",
                extraction_error: Some("[Cannot extract 'corrupt-contract.pdf']".to_string()),
                local_file_id: None,
            })
            .unwrap();
        // A document that WOULD match on filename alone.
        let hits = db.documents.search_documents_fulltext("contract", 10, None).unwrap();
        assert!(
            hits.is_empty(),
            "an unreadable document must not be offered as a retrieval hit: {:?}",
            hits.iter().map(|(d, _)| &d.original_filename).collect::<Vec<_>>()
        );
    }

    /// Repairing an extraction must make the NEW text findable and the OLD text
    /// unfindable. This is the trigger path on a real UPDATE issued by
    /// production code (repair_extraction), not a hand-written UPDATE.
    #[test]
    fn repairing_an_extraction_updates_what_is_searchable() {
        let db = seeded();
        let id = add_doc(&db, "scan.pdf", "gibberish ocr artefacts");
        assert_eq!(db.documents.search_documents_fulltext("gibberish", 10, None).unwrap().len(), 1);

        db.documents
            .repair_extraction(id, "Arbitration shall be seated in Singapore.", "ok", None)
            .unwrap();

        assert!(
            db.documents.search_documents_fulltext("gibberish", 10, None).unwrap().is_empty(),
            "stale pre-repair text must stop being searchable"
        );
        let hits = db.documents.search_documents_fulltext("arbitration", 10, None).unwrap();
        assert_eq!(hits.len(), 1, "repaired text must be searchable");
        assert_eq!(hits[0].0.id, id);
    }

    /// Messages written by the real persistence path must be searchable, and
    /// the current conversation must be excludable (the context engine already
    /// supplies it via tier 1, so including it would duplicate content).
    #[test]
    fn messages_are_searchable_and_the_current_session_can_be_excluded() {
        let db = seeded();
        db.conversations
            .store_messages_batch(
                "chat-a",
                &[("user".to_string(), "the deposit is refundable within 14 days".to_string(), 0, 0, 0.5)],
            )
            .unwrap();
        db.conversations
            .store_messages_batch(
                "chat-b",
                &[("user".to_string(), "what about the parking allocation".to_string(), 0, 0, 0.5)],
            )
            .unwrap();

        let all = db.conversations.search_messages_fulltext("refundable deposit", 10, None, None).unwrap();
        assert_eq!(all.len(), 1, "the message must be findable");
        assert_eq!(all[0].0.session_id, "chat-a");
        assert!(all[0].1 > 0.0, "score must be positive after sign normalisation");

        let excluded = db
            .conversations
            .search_messages_fulltext("refundable deposit", 10, Some("chat-a"), None)
            .unwrap();
        assert!(excluded.is_empty(), "excluding chat-a must drop its own messages");

        // Excluding a DIFFERENT session must not hide the match.
        let other = db
            .conversations
            .search_messages_fulltext("refundable deposit", 10, Some("chat-b"), None)
            .unwrap();
        assert_eq!(other.len(), 1, "excluding an unrelated session must not filter this out");
    }

    /// Deleting a conversation must remove its messages from the index. A
    /// deleted chat resurfacing in retrieval would be both wrong and a privacy
    /// problem - the user asked for it to be gone.
    #[test]
    fn deleting_a_session_removes_its_messages_from_the_index() {
        let db = seeded();
        db.conversations
            .store_messages_batch(
                "chat-a",
                &[("user".to_string(), "the escrow amount is disputed".to_string(), 0, 0, 0.5)],
            )
            .unwrap();
        assert_eq!(
            db.conversations.search_messages_fulltext("escrow", 10, None, None).unwrap().len(),
            1
        );

        db.conversations.delete_session("chat-a").unwrap();

        assert!(
            db.conversations.search_messages_fulltext("escrow", 10, None, None).unwrap().is_empty(),
            "a deleted conversation must not remain searchable"
        );
    }

    /// A query with nothing searchable in it must retrieve NOTHING rather than
    /// everything. "everything" as a retrieval result would flood the prompt
    /// with unrelated documents and crowd out the real answer.
    #[test]
    fn unsearchable_queries_return_nothing_not_everything() {
        let db = seeded();
        add_doc(&db, "a.pdf", "some content here");
        add_doc(&db, "b.pdf", "other content here");

        for q in ["", "   ", "?!", "a of is"] {
            assert!(
                db.documents.search_documents_fulltext(q, 10, None).unwrap().is_empty(),
                "query {:?} must return nothing",
                q
            );
            assert!(
                db.conversations.search_messages_fulltext(q, 10, None, None).unwrap().is_empty(),
                "query {:?} must return nothing",
                q
            );
        }
    }

    /// User text containing FTS5 operator characters must not raise a SQL error.
    /// Real questions contain quotes, hyphens and question marks routinely.
    #[test]
    fn operator_laden_user_questions_do_not_error() {
        let db = seeded();
        add_doc(&db, "nda.pdf", "Confidential information must be protected.");

        for q in [
            "what about \"confidential\" info - specifically?",
            "NEAR AND OR NOT confidential",
            "confidential * 100%",
            "^confidential: yes",
        ] {
            let r = db.documents.search_documents_fulltext(q, 10, None);
            assert!(r.is_ok(), "query {:?} raised an error: {:?}", q, r.err());
        }
    }

    #[test]
    fn the_limit_parameter_is_honoured() {
        let db = seeded();
        for i in 0..10 {
            // Content must DIFFER per file. Documents are identified by the
            // hash of their bytes, so ten files with identical text are one
            // document by design - which would make this assert a limit that
            // was never exercised.
            add_doc(
                &db,
                &format!("doc{}.pdf", i),
                &format!("Clause {} sets out shared indemnity language.", i),
            );
        }
        let hits = db.documents.search_documents_fulltext("indemnity", 3, None).unwrap();
        assert_eq!(hits.len(), 3, "limit must bound the result set");
    }

    /// The ON-DISK database path — the one that actually ships.
    ///
    /// Every other test in this crate uses `new_in_memory()`, which takes a
    /// DIFFERENT route to the same schema: `schema::SCHEMA_SQL` plus each store's
    /// `initialize_schema`, with migrations never running at all. So the real
    /// path — migrations 001..010, then the stores, then `ensure_fts_schema` —
    /// was covered by compilation only.
    ///
    /// That is the gap where a schema/ordering mistake would hide: it would pass
    /// the whole suite and fail on a user's first launch. This drives the actual
    /// `MemoryDatabase::new` against a real file and asserts the end state.
    #[test]
    fn the_on_disk_database_initialises_with_a_working_full_text_index() {
        let dir = std::env::temp_dir().join(format!(
            "oca-ondisk-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("memory.db");

        {
            let db = MemoryDatabase::new(&db_path).expect("on-disk database must initialise");

            // Migration 010 must have run: the embedding subsystem is gone.
            {
                let conn = db.conversations.get_conn_public().unwrap();
                let embeddings_exists: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='embeddings'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                assert!(
                    embeddings_exists.is_none(),
                    "migration 010 must have dropped the embeddings table on disk"
                );
            }

            // Documents and messages must both be searchable through the index
            // built by ensure_fts_schema on this path.
            db.conversations.create_session_with_id("s1", None).ok();
            db.documents
                .upsert_document(NewDocument {
                    original_bytes: b"Arbitration shall be seated in Singapore.",
                    original_filename: "clause.txt",
                    source_path: None,
                    source_kind: "paperclip",
                    mime_type: None,
                    size_bytes: 41,
                    extracted_text: "Arbitration shall be seated in Singapore.".to_string(),
                    extraction_status: "ok",
                    extraction_error: None,
                    local_file_id: None,
                })
                .unwrap();
            db.conversations
                .store_messages_batch(
                    "s1",
                    &[("user".to_string(), "the deposit is refundable".to_string(), 0, 0, 0.5)],
                )
                .unwrap();

            let docs = db.documents.search_documents_fulltext("arbitration", 5, None).unwrap();
            assert_eq!(docs.len(), 1, "documents must be searchable on the on-disk path");
            let msgs = db
                .conversations
                .search_messages_fulltext("refundable", 5, None, None)
                .unwrap();
            assert_eq!(msgs.len(), 1, "messages must be searchable on the on-disk path");
        }

        // Reopening must not double-apply anything, and must not lose the index -
        // this is what every launch after the first one does.
        {
            let db = MemoryDatabase::new(&db_path).expect("reopening must succeed");
            let docs = db.documents.search_documents_fulltext("arbitration", 5, None).unwrap();
            assert_eq!(
                docs.len(),
                1,
                "the index must survive a reopen without being rebuilt or lost"
            );
            let msgs = db
                .conversations
                .search_messages_fulltext("refundable", 5, None, None)
                .unwrap();
            assert_eq!(msgs.len(), 1, "message index must survive a reopen");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
