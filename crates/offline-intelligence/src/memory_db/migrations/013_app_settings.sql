-- Migration 013: User-adjustable application settings.
--
-- The app had no settings storage at all before this: `Config::from_env()`
-- parses `.env` once at process start and nothing could change a value
-- afterwards. That is fine for deployment defaults but cannot express "the
-- user chose 20 GB in the Settings panel", which has to survive restarts and
-- must not be written back into `.env` (a shipped resource sitting next to
-- the executable - rewriting it from the running app is fragile and racy).
--
-- So values resolve in three layers, most specific first:
--   1. a row in this table   (the user's explicit choice)
--   2. the matching env var  (the shipped default in .env)
--   3. a constant in Rust    (last resort, so a missing/corrupt .env still boots)
--
-- Deliberately a generic key/value table rather than one column per setting:
-- adding a setting then needs no migration, and settings are read
-- individually by name rather than as a wide row. `value` is TEXT because
-- every value here arrives from an HTTP body or an env var as text anyway;
-- parsing and range-checking belong in Rust where the error can be reported
-- to the caller, not in a CHECK constraint that can only abort a write.
--
-- `updated_at` exists to answer "did the user actually set this, or is it a
-- leftover?" when a setting's meaning changes in a later version.

CREATE TABLE IF NOT EXISTS app_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- No index beyond the primary key: this table is read by exact key and is
-- expected to hold tens of rows, not thousands.
