//! API Keys Store - Secure storage for external service tokens
//!
//! Stores and manages API keys for:
//! - HuggingFace (for gated model downloads)
//!
//! Keys are encrypted at rest and persist across sessions.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Type of API key
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApiKeyType {
    HuggingFace,
}

impl ApiKeyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiKeyType::HuggingFace => "huggingface",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "huggingface" | "hf" => Some(ApiKeyType::HuggingFace),
            _ => None,
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
    pub fn initialize_schema(&self) -> Result<()> {
        let conn = self.pool.get()?;
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

/// Simple XOR-based encryption for API keys
/// NOTE: For production, consider using a proper encryption library like `ring` or `sodiumoxide`
pub struct SimpleEncryption;

impl SimpleEncryption {
    // Derived from system-specific data for basic obfuscation
    fn get_key() -> Vec<u8> {
        // Simple key derivation - in production use proper key management
        let machine_id = whoami::devicename();
        let mut key = Vec::new();
        for byte in machine_id.bytes() {
            key.push(byte);
        }
        // Pad to at least 32 bytes
        while key.len() < 32 {
            key.push((key.len() as u8).wrapping_mul(17));
        }
        key.truncate(32);
        key
    }

    pub fn encrypt(plaintext: &str) -> String {
        let key = Self::get_key();
        let bytes = plaintext.as_bytes();
        let encrypted: Vec<u8> = bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ key[i % key.len()])
            .collect();
        BASE64.encode(&encrypted)
    }

    pub fn decrypt(encrypted: &str) -> Result<String> {
        let key = Self::get_key();
        let encrypted_bytes = BASE64.decode(encrypted)?;
        let decrypted: Vec<u8> = encrypted_bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ key[i % key.len()])
            .collect();
        Ok(String::from_utf8(decrypted)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encryption_roundtrip() {
        let original = "hf_1234567890abcdef";
        let encrypted = SimpleEncryption::encrypt(original);
        let decrypted = SimpleEncryption::decrypt(&encrypted).unwrap();
        assert_eq!(original, decrypted);
    }
}
