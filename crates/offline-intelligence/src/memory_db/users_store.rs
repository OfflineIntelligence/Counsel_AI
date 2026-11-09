//! Users Store - User authentication and management
//!
//! Stores user accounts with hashed passwords and email verification status.

use anyhow::Result;
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub password_hash: String,
    pub email_verified: bool,
    pub verification_token: Option<String>,
    pub created_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
}

pub struct UsersStore {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

impl Clone for UsersStore {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
        }
    }
}

impl UsersStore {
    pub fn new(pool: Arc<Pool<SqliteConnectionManager>>) -> Self {
        Self { pool }
    }

    /// Schema DDL for this store. Only ever called from the migration
    /// chain (`migration::get_migrations`), the single schema route.
    pub(crate) fn create_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                email TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                password_hash TEXT NOT NULL,
                email_verified INTEGER DEFAULT 0,
                verification_token TEXT,
                created_at TEXT NOT NULL,
                verified_at TEXT
            )",
            [],
        )?;
        Self::create_account_schema(conn)?;
        info!("Users table initialized");
        Ok(())
    }

    /// Bring the database this store uses up to the current schema. Goes
    /// through the migration chain like every other caller.
    pub fn initialize_schema(&self) -> anyhow::Result<()> {
        super::migration::ensure_schema(&self.pool)
    }

    pub fn create_user(
        &self,
        email: &str,
        name: &str,
        password_hash: &str,
        verification_token: &str,
    ) -> Result<i64> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO users (email, name, password_hash, email_verified, verification_token, created_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![email, name, password_hash, verification_token, now],
        )?;

        let id = conn.last_insert_rowid();
        info!("User created with id: {}", id);
        Ok(id)
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;

        let result = conn
            .query_row(
                "SELECT id, email, name, password_hash, email_verified, verification_token, created_at, verified_at
                 FROM users WHERE email = ?1",
                params![email],
                |row| {
                    let created_str: String = row.get(6)?;
                    let verified_str: Option<String> = row.get(7)?;

                    Ok(User {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        name: row.get(2)?,
                        password_hash: row.get(3)?,
                        email_verified: row.get::<_, i32>(4)? != 0,
                        verification_token: row.get(5)?,
                        created_at: DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        verified_at: verified_str.and_then(|s| {
                            DateTime::parse_from_rfc3339(&s)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                    })
                },
            )
            .optional()?;

        Ok(result)
    }

    pub fn get_user_by_id(&self, id: i64) -> Result<Option<User>> {
        let conn = self.pool.get()?;

        let result = conn
            .query_row(
                "SELECT id, email, name, password_hash, email_verified, verification_token, created_at, verified_at
                 FROM users WHERE id = ?1",
                params![id],
                |row| {
                    let created_str: String = row.get(6)?;
                    let verified_str: Option<String> = row.get(7)?;

                    Ok(User {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        name: row.get(2)?,
                        password_hash: row.get(3)?,
                        email_verified: row.get::<_, i32>(4)? != 0,
                        verification_token: row.get(5)?,
                        created_at: DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        verified_at: verified_str.and_then(|s| {
                            DateTime::parse_from_rfc3339(&s)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                    })
                },
            )
            .optional()?;

        Ok(result)
    }

    pub fn verify_email(&self, token: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();

        let rows_affected = conn.execute(
            "UPDATE users SET email_verified = 1, verified_at = ?1, verification_token = NULL
             WHERE verification_token = ?2 AND email_verified = 0",
            params![now, token],
        )?;

        if rows_affected > 0 {
            let user = conn
                .query_row(
                    "SELECT id, email, name, password_hash, email_verified, verification_token, created_at, verified_at
                     FROM users WHERE verification_token IS NULL AND email_verified = 1 ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        let created_str: String = row.get(6)?;
                        let verified_str: Option<String> = row.get(7)?;

                        Ok(User {
                            id: row.get(0)?,
                            email: row.get(1)?,
                            name: row.get(2)?,
                            password_hash: row.get(3)?,
                            email_verified: true,
                            verification_token: None,
                            created_at: DateTime::parse_from_rfc3339(&created_str)
                                .map(|dt| dt.with_timezone(&Utc))
                                .unwrap_or_else(|_| Utc::now()),
                            verified_at: verified_str.and_then(|s| {
                                DateTime::parse_from_rfc3339(&s)
                                    .ok()
                                    .map(|dt| dt.with_timezone(&Utc))
                            }),
                        })
                    },
                )
                .optional()?;

            Ok(user)
        } else {
            Ok(None)
        }
    }

    pub fn email_exists(&self, email: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE email = ?1",
            params![email],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // ------------------------------------------------------------ accounts
    // Profile fields and one-time codes for the email-verified sign-in flow
    // (api/account_api.rs). Added beside the original columns so existing
    // databases keep working.

    fn create_account_schema(conn: &rusqlite::Connection) -> Result<()> {
        for column in [
            "first_name TEXT",
            "last_name TEXT",
            "username TEXT",
            "age INTEGER",
            "dob TEXT",
            // When the user confirmed their date of birth (age check).
            "age_verified_at TEXT",
        ] {
            // Fails harmlessly when the column already exists.
            let _ = conn.execute(&format!("ALTER TABLE users ADD COLUMN {}", column), []);
        }
        conn.execute(
            "CREATE TABLE IF NOT EXISTS login_otps (
                email TEXT PRIMARY KEY,
                code_hash TEXT NOT NULL,
                payload TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn count_users(&self) -> Result<i64> {
        let conn = self.pool.get()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM users WHERE email_verified = 1", [], |r| r.get(0))?)
    }

    /// A verified user by e-mail address or user name, case-insensitively.
    pub fn get_user_by_login(&self, identifier: &str) -> Result<Option<User>> {
        let conn = self.pool.get()?;
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM users WHERE lower(email) = lower(?1) OR lower(username) = lower(?1) LIMIT 1",
                params![identifier],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.get_user_by_id(id),
            None => Ok(None),
        }
    }

    /// Whether another account already holds this user name.
    pub fn username_taken(&self, username: &str, except_email: &str) -> Result<bool> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE lower(username) = lower(?1) AND lower(email) <> lower(?2)",
            params![username, except_email],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Create the account, or replace the profile and password of an existing
    /// one (a recovery). Always leaves the e-mail verified. Returns the id and
    /// whether the account is new.
    pub fn upsert_verified_user(
        &self,
        email: &str,
        display_name: &str,
        username: &str,
        password_hash: &str,
        first: &str,
        last: &str,
        age: i64,
        dob: &str,
    ) -> Result<(i64, bool)> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM users WHERE lower(email) = lower(?1)", params![email], |r| r.get(0))
            .optional()?;
        if let Some(id) = existing {
            conn.execute(
                "UPDATE users SET name = ?1, username = ?2, password_hash = ?3, first_name = ?4, last_name = ?5,
                 age = ?6, dob = ?7, email_verified = 1, verification_token = NULL, verified_at = ?8 WHERE id = ?9",
                params![display_name, username, password_hash, first, last, age, dob, now, id],
            )?;
            Ok((id, false))
        } else {
            conn.execute(
                "INSERT INTO users (email, name, password_hash, email_verified, verification_token, created_at, verified_at,
                                    first_name, last_name, username, age, dob)
                 VALUES (?1, ?2, ?3, 1, NULL, ?4, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![email, display_name, password_hash, now, first, last, username, age, dob],
            )?;
            Ok((conn.last_insert_rowid(), true))
        }
    }

    /// The account's date of birth (YYYY-MM-DD), if it has one.
    pub fn get_dob(&self, id: i64) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let dob: Option<String> = conn
            .query_row("SELECT dob FROM users WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(dob.filter(|d| !d.trim().is_empty()))
    }

    /// Record a confirmed date of birth and the age it gives today.
    pub fn set_dob(&self, id: i64, dob: &str, age: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE users SET dob = ?1, age = ?2, age_verified_at = ?3 WHERE id = ?4",
            params![dob, age, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    pub fn save_otp(&self, email: &str, code_hash: &str, payload: &str, now: i64, expires_at: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO login_otps (email, code_hash, payload, attempts, created_at, expires_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)
             ON CONFLICT(email) DO UPDATE SET code_hash = ?2, payload = ?3, attempts = 0, created_at = ?4, expires_at = ?5",
            params![email.to_lowercase(), code_hash, payload, now, expires_at],
        )?;
        Ok(())
    }

    /// (code_hash, payload, attempts, created_at, expires_at)
    pub fn get_otp(&self, email: &str) -> Result<Option<(String, String, i64, i64, i64)>> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT code_hash, payload, attempts, created_at, expires_at FROM login_otps WHERE email = ?1",
                params![email.to_lowercase()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
    }

    pub fn bump_otp_attempts(&self, email: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("UPDATE login_otps SET attempts = attempts + 1 WHERE email = ?1", params![email.to_lowercase()])?;
        Ok(())
    }

    pub fn clear_otp(&self, email: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM login_otps WHERE email = ?1", params![email.to_lowercase()])?;
        Ok(())
    }
}
