//! API Keys Store - Secure storage for external service tokens
//!
//! Stores and manages API keys for:
//! - HuggingFace (for gated model downloads)
//! - Cloud model providers (see crate::cloud_providers), keyed by provider id
//!
//! Keys are encrypted at rest and persist across sessions.

use anyhow::Result;
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Type of API key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyType {
    HuggingFace,
    /// A cloud model provider, by its id in `crate::cloud_providers`.
    Cloud(&'static str),
}

impl ApiKeyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiKeyType::HuggingFace => "huggingface",
            ApiKeyType::Cloud(id) => id,
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "huggingface" | "hf" => Some(ApiKeyType::HuggingFace),
            other => crate::cloud_providers::find(other).map(|p| ApiKeyType::Cloud(p.id)),
        }
    }
}

/// API key record with usage tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: i64,
    pub key_type: String,
    pub encrypted_value: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_mode: Option<String>, // "offline" or "online"
    pub usage_count: i64,
}

/// Manages API keys in SQLite database
pub struct ApiKeysStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl ApiKeysStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Initialize the api_keys table if it doesn't exist
    /// Schema DDL for this store. Only ever called from the migration
    /// chain (`migration::get_migrations`), the single schema route.
    pub(crate) fn create_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key_type TEXT NOT NULL UNIQUE,
                encrypted_value TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_used_at TEXT,
                last_mode TEXT,
                usage_count INTEGER DEFAULT 0
            )",
            [],
        )?;
        info!("API keys table initialized");
        Ok(())
    }

    /// Bring the database this store uses up to the current schema. Goes
    /// through the migration chain like every other caller.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        super::migration::ensure_schema(&self.pool)
    }

    /// Save or update an API key
    pub fn save_key(&self, key_type: ApiKeyType, encrypted_value: String) -> Result<()> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO api_keys (key_type, encrypted_value, created_at, usage_count)
             VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(key_type) DO UPDATE SET
                encrypted_value = excluded.encrypted_value,
                created_at = excluded.created_at",
            params![key_type.as_str(), encrypted_value, now],
        )?;

        info!("API key saved for: {}", key_type.as_str());
        Ok(())
    }

    /// Get an API key by type
    pub fn get_key(&self, key_type: ApiKeyType) -> Result<Option<ApiKeyRecord>> {
        let conn = self.pool.get()?;

        let result = conn
            .query_row(
                "SELECT id, key_type, encrypted_value, created_at, last_used_at, last_mode, usage_count
                 FROM api_keys WHERE key_type = ?1",
                params![key_type.as_str()],
                |row| {
                    let created_str: String = row.get(3)?;
                    let last_used_str: Option<String> = row.get(4)?;

                    Ok(ApiKeyRecord {
                        id: row.get(0)?,
                        key_type: row.get(1)?,
                        encrypted_value: row.get(2)?,
                        created_at: DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        last_used_at: last_used_str.and_then(|s| {
                            DateTime::parse_from_rfc3339(&s)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                        last_mode: row.get(5)?,
                        usage_count: row.get(6)?,
                    })
                },
            )
            .optional()?;

        Ok(result)
    }

    /// Update last used timestamp and mode
    pub fn mark_used(&self, key_type: ApiKeyType, mode: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "UPDATE api_keys SET last_used_at = ?1, last_mode = ?2, usage_count = usage_count + 1
             WHERE key_type = ?3",
            params![now, mode, key_type.as_str()],
        )?;

        Ok(())
    }

    /// Delete an API key
    pub fn delete_key(&self, key_type: ApiKeyType) -> Result<bool> {
        let conn = self.pool.get()?;

        let rows_affected = conn.execute(
            "DELETE FROM api_keys WHERE key_type = ?1",
            params![key_type.as_str()],
        )?;

        if rows_affected > 0 {
            info!("API key deleted for: {}", key_type.as_str());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get all API keys
    pub fn get_all_keys(&self) -> Result<Vec<ApiKeyRecord>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, key_type, encrypted_value, created_at, last_used_at, last_mode, usage_count
             FROM api_keys",
        )?;

        let keys = stmt
            .query_map([], |row| {
                let created_str: String = row.get(3)?;
                let last_used_str: Option<String> = row.get(4)?;

                Ok(ApiKeyRecord {
                    id: row.get(0)?,
                    key_type: row.get(1)?,
                    encrypted_value: row.get(2)?,
                    created_at: DateTime::parse_from_rfc3339(&created_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    last_used_at: last_used_str.and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                    }),
                    last_mode: row.get(5)?,
                    usage_count: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(keys)
    }
}

/// Encryption of stored API keys: AES-256-GCM under a DPAPI-wrapped
/// per-install master key (see `utils::secret_box`).
pub struct KeyEncryption;

impl KeyEncryption {
    pub fn encrypt(plaintext: &str) -> Result<String> {
        crate::utils::secret_box::encrypt(plaintext)
    }

    pub fn decrypt(stored: &str) -> Result<String> {
        crate::utils::secret_box::decrypt(stored)
    }
}

impl ApiKeysStore {
    /// Re-encrypt rows still in the old XOR format. Runs at startup; a row
    /// that cannot be read is left alone (the user re-enters that key).
    pub fn upgrade_legacy_encryption(&self) -> Result<usize> {
        let conn = self.pool.get()?;
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT id, encrypted_value FROM api_keys")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut upgraded = 0;
        for (id, value) in rows {
            if crate::utils::secret_box::is_encrypted(&value) {
                continue;
            }
            match crate::utils::secret_box::decrypt_legacy(&value) {
                Ok(plain) => {
                    let enc = crate::utils::secret_box::encrypt(&plain)?;
                    conn.execute("UPDATE api_keys SET encrypted_value = ?1 WHERE id = ?2", params![enc, id])?;
                    upgraded += 1;
                }
                Err(e) => tracing::warn!("API key {} could not be upgraded: {}", id, e),
            }
        }
        if upgraded > 0 {
            info!("Re-encrypted {} API key(s) with AES-256-GCM", upgraded);
        }
        Ok(upgraded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encryption_roundtrip() {
        let original = "hf_1234567890abcdef";
        let encrypted = KeyEncryption::encrypt(original).unwrap();
        assert!(!encrypted.contains(original));
        let decrypted = KeyEncryption::decrypt(&encrypted).unwrap();
        assert_eq!(original, decrypted);
    }
}
