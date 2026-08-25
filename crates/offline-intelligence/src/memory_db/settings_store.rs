//! Persistence for user-adjustable settings.
//!
//! See migration 013 for why this is a generic key/value table and why the
//! user's choice lives here rather than in `.env`.

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OptionalExtension;
use std::sync::Arc;
use tracing::debug;

/// Key for the total-app-disk-usage budget, in megabytes.
///
/// Named as a constant rather than spelled out at each call site so the
/// reader, the writer and the eviction pass cannot drift apart - a typo in a
/// string literal would silently read as "user has not set this" and fall
/// back to the env default, which is the kind of bug that looks like the
/// setting simply not working.
pub const DISK_LIMIT_MB_KEY: &str = "storage.disk_limit_mb";

/// Prefix for per-model last-activation timestamps (RFC-3339).
///
/// Recorded here rather than on `ModelInfo` because the model registry is
/// serialised to disk as a whole; adding a mutable per-model timestamp to it
/// would mean rewriting the registry file on every model switch. This table
/// is already the place for small mutable facts.
pub const MODEL_LAST_USED_PREFIX: &str = "model.last_used.";

/// Build the settings key recording when `model_id` was last activated.
pub fn model_last_used_key(model_id: &str) -> String {
    format!("{}{}", MODEL_LAST_USED_PREFIX, model_id)
}

pub struct SettingsStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl SettingsStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Create the table on the in-memory path.
    ///
    /// `MemoryDatabase::new_in_memory` builds its schema from
    /// `schema::SCHEMA_SQL` and never runs migrations, so without this the
    /// table would exist in production and be missing in every test - the
    /// exact split that forced migration 010 to special-case itself. Kept
    /// byte-identical in shape to migration 013.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_settings (
                 key        TEXT PRIMARY KEY,
                 value      TEXT NOT NULL,
                 updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
             );",
        )?;
        Ok(())
    }

    /// Read a raw setting. `None` means the user has never set it - which the
    /// caller must treat as "fall back to the env default", never as zero.
    pub fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.pool.get()?;
        let value = conn
            .query_row("SELECT value FROM app_settings WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Write a setting, replacing any previous value.
    pub fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at)
             VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = CURRENT_TIMESTAMP",
            rusqlite::params![key, value],
        )?;
        debug!("Setting '{}' set to '{}'", key, value);
        Ok(())
    }

    /// Remove a setting, restoring the env/default behaviour for it.
    pub fn clear(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM app_settings WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Read a setting as `u64`.
    ///
    /// A value that is present but unparseable returns `None` (with a
    /// warning) rather than an error: a corrupt row must not make the app
    /// unable to compute a disk budget, and falling back to the env default
    /// is both safe and visible in the logs.
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        match self.get(key) {
            Ok(Some(raw)) => match raw.trim().parse::<u64>() {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        "Setting '{}' holds {:?}, which is not a non-negative integer ({}). \
                         Falling back to the configured default.",
                        key,
                        raw,
                        e
                    );
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("Could not read setting '{}': {}", key, e);
                None
            }
        }
    }

    /// Record that `model_id` was activated, for disk-eviction recency.
    pub fn record_model_used(&self, model_id: &str) -> anyhow::Result<()> {
        self.set(
            &model_last_used_key(model_id),
            &chrono::Utc::now().to_rfc3339(),
        )
    }

    /// When `model_id` was last activated, if ever recorded.
    pub fn model_last_used(&self, model_id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        let raw = self.get(&model_last_used_key(model_id)).ok().flatten()?;
        chrono::DateTime::parse_from_rfc3339(&raw)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    }

    /// Forget a model's activation record, so a removed model does not leave
    /// a row behind that would make a later reinstall look stale.
    pub fn forget_model(&self, model_id: &str) -> anyhow::Result<()> {
        self.clear(&model_last_used_key(model_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::MemoryDatabase;

    fn db() -> MemoryDatabase {
        MemoryDatabase::new_in_memory().expect("in-memory database")
    }

    #[test]
    fn an_unset_setting_reads_as_none_not_zero() {
        let db = db();
        assert_eq!(db.settings.get(DISK_LIMIT_MB_KEY).unwrap(), None);
        // The distinction that matters: absent must not collapse to 0, which
        // as a disk budget would mean "evict everything".
        assert_eq!(db.settings.get_u64(DISK_LIMIT_MB_KEY), None);
    }

    #[test]
    fn setting_a_value_twice_updates_rather_than_erroring_on_the_primary_key() {
        let db = db();
        db.settings.set(DISK_LIMIT_MB_KEY, "10240").unwrap();
        db.settings.set(DISK_LIMIT_MB_KEY, "20480").unwrap();
        assert_eq!(db.settings.get_u64(DISK_LIMIT_MB_KEY), Some(20480));
    }

    #[test]
    fn a_corrupt_value_falls_back_instead_of_failing() {
        let db = db();
        db.settings.set(DISK_LIMIT_MB_KEY, "not a number").unwrap();
        // None means "use the env default" - the app still computes a budget.
        assert_eq!(db.settings.get_u64(DISK_LIMIT_MB_KEY), None);
    }

    #[test]
    fn clearing_restores_the_unset_state() {
        let db = db();
        db.settings.set(DISK_LIMIT_MB_KEY, "4096").unwrap();
        db.settings.clear(DISK_LIMIT_MB_KEY).unwrap();
        assert_eq!(db.settings.get_u64(DISK_LIMIT_MB_KEY), None);
    }

    #[test]
    fn model_recency_round_trips_and_is_scoped_per_model() {
        let db = db();
        db.settings.record_model_used("gemma-3-1b").unwrap();
        assert!(db.settings.model_last_used("gemma-3-1b").is_some());
        assert!(
            db.settings.model_last_used("some-other-model").is_none(),
            "recency must not leak between models - eviction would pick the wrong one"
        );

        db.settings.forget_model("gemma-3-1b").unwrap();
        assert!(db.settings.model_last_used("gemma-3-1b").is_none());
    }
}
