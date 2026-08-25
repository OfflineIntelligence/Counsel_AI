//! Warm start: keep one conversation's computed KV state across app restarts.
//!
//! # What this does, and what it deliberately does not
//!
//! llama-server can write a slot's KV cache to disk (`--slot-save-path` plus
//! `POST /slots/{id}?action=save`) and read it back (`action=restore`). The
//! unit is one opaque blob for an entire slot, tied to the token sequence that
//! produced it. There is no addressable structure inside it: nothing can be
//! scored, ranked, merged or partially injected. Any design that assumes
//! otherwise is describing an API llama.cpp does not have.
//!
//! So this module does exactly one thing: when the app shuts down cleanly it
//! saves the slot for the conversation the user was last talking in, and when
//! the app starts again it restores that slot. The first message after a
//! restart then skips re-prefilling the whole conversation.
//!
//! It is **not** a cache across conversations. llama-server already keeps
//! recent prefixes in host RAM (`--cache-ram`, 8192 MiB by default) and
//! hot-swaps them when switching between slots, so switching chats within a
//! single run is already handled by the engine. What RAM cannot do is survive
//! the process exiting - and that gap is the whole of this module's job.
//!
//! # Why identity is checked before restoring
//!
//! A KV blob is only meaningful to the exact model that computed it, at the
//! same context size, under the same engine build. Restoring one into a
//! different model is not a cache miss, it is undefined behaviour. Rather than
//! trust that llama-server rejects a mismatch, this module records what
//! produced the blob and refuses to restore unless all of it still matches,
//! deleting the blob and saying why. That is the [no-fallbacks] posture: a
//! recoverable miss is fine, a silent wrong answer is not.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use crate::memory_db::MemoryDatabase;

/// Settings key holding the single warm-start record (see `WarmStartRecord`).
///
/// One record, not a table: the agreed scope is one blob - the conversation
/// that was active at shutdown - so a row in the existing key/value settings
/// table carries it without another migration. If this ever becomes
/// per-session, that is the point to promote it to its own table.
const WARM_START_KEY: &str = "kv_cache.warm_start";

/// The slot index used for every operation.
///
/// Always 0 because `slot_save_path` forces `--parallel 1` (see
/// `RuntimeConfig::slot_save_path`), so there is exactly one slot to address.
const SLOT_ID: u32 = 0;

/// Saving or restoring several hundred megabytes is disk-bound and can take a
/// while on a cold cache; the operation is still bounded so a hung engine
/// cannot stall shutdown or startup indefinitely.
const SLOT_IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Directory llama-server writes slot blobs into.
///
/// Shares `storage_governor::KV_CACHE_DIR` rather than restating the name, so
/// disk accounting and eviction see exactly the files written here - a second
/// literal would let the two drift and leave blobs uncounted.
pub fn slot_cache_dir() -> PathBuf {
    crate::config::get_app_data_dir().join(crate::utils::storage_governor::KV_CACHE_DIR)
}

/// What produced the saved blob. Every field is part of the restore guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmStartRecord {
    /// Conversation the blob belongs to.
    pub session_id: String,
    /// File name inside `slot_cache_dir()`. llama-server resolves it relative
    /// to `--slot-save-path`, so only the bare name is sent over HTTP.
    pub filename: String,
    /// Model that computed this KV state. A blob from another model is not a
    /// cache miss, it is meaningless.
    pub model_id: String,
    /// Engine build. A different llama.cpp build may write a different state
    /// format; the version is part of the blob's identity.
    pub engine_id: String,
    /// Context size the slot was configured with.
    pub ctx_size: u32,
    /// Tokens saved, as reported by llama-server.
    pub n_tokens: u64,
    /// Bytes written, as reported by llama-server.
    pub bytes: u64,
    pub saved_at: chrono::DateTime<chrono::Utc>,
}

impl WarmStartRecord {
    fn path(&self) -> PathBuf {
        slot_cache_dir().join(&self.filename)
    }
}

#[derive(Debug, Deserialize)]
struct SlotSaveResponse {
    #[serde(default)]
    n_saved: u64,
    #[serde(default)]
    n_written: u64,
}

#[derive(Debug, Deserialize)]
struct SlotRestoreResponse {
    #[serde(default)]
    n_restored: u64,
    #[serde(default)]
    n_read: u64,
}

/// Blob file name for a session. Sanitised because session ids come from the
/// frontend (`Date.now().toString()` today, but that is not a guarantee) and
/// must never be able to escape the cache directory.
fn filename_for(session_id: &str) -> String {
    let safe: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("session_{}.bin", safe)
}

/// Read the stored warm-start record, if there is one.
pub fn load_record(database: &MemoryDatabase) -> Option<WarmStartRecord> {
    let raw = database.settings.get(WARM_START_KEY).ok().flatten()?;
    match serde_json::from_str::<WarmStartRecord>(&raw) {
        Ok(record) => Some(record),
        Err(e) => {
            warn!("Discarding unreadable warm-start record: {}", e);
            let _ = database.settings.clear(WARM_START_KEY);
            None
        }
    }
}

/// Delete the record and the blob it points at.
///
/// Both together, always: a record without its file produces a failed restore
/// on every startup, and a file without its record is a few hundred megabytes
/// nothing will ever read.
pub fn discard(database: &MemoryDatabase, reason: &str) {
    if let Some(record) = load_record(database) {
        let path = record.path();
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("Could not delete stale warm-start blob {}: {}", path.display(), e);
            }
        }
    }
    if let Err(e) = database.settings.clear(WARM_START_KEY) {
        warn!("Could not clear the warm-start record: {}", e);
    }
    info!("Warm-start cache discarded: {}", reason);
}

/// Save the active conversation's slot so the next launch can skip its prefill.
///
/// Returns the record written, or `None` when nothing was saved - which is a
/// normal outcome (no conversation yet, no runtime, engine refused) and never
/// an error the caller must handle. Shutdown must not fail because a cache
/// could not be written.
pub async fn save_active_slot(
    database: &MemoryDatabase,
    base_url: &str,
    session_id: &str,
    model_id: &str,
    engine_id: &str,
    ctx_size: u32,
) -> Option<WarmStartRecord> {
    let dir = slot_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Could not create the KV cache directory {}: {}", dir.display(), e);
        return None;
    }

    // Only one blob is kept. Clearing the previous one first means a failed
    // save leaves nothing stale behind rather than a blob whose record no
    // longer describes it.
    discard(database, "superseded by a newer conversation");

    let filename = filename_for(session_id);
    let client = match reqwest::Client::builder().timeout(SLOT_IO_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn!("Could not build the slot-cache HTTP client: {}", e);
            return None;
        }
    };

    let url = format!("{}/slots/{}?action=save", base_url.trim_end_matches('/'), SLOT_ID);
    let response = match client
        .post(&url)
        .json(&serde_json::json!({ "filename": filename }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Warm-start save request failed: {}", e);
            return None;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!(
            "Warm-start save refused by the engine ({}): {}. The next launch will \
             re-process this conversation's prompt instead.",
            status, body
        );
        return None;
    }

    let parsed: SlotSaveResponse = match response.json().await {
        Ok(p) => p,
        Err(e) => {
            warn!("Could not read the warm-start save response: {}", e);
            return None;
        }
    };

    let record = WarmStartRecord {
        session_id: session_id.to_string(),
        filename,
        model_id: model_id.to_string(),
        engine_id: engine_id.to_string(),
        ctx_size,
        n_tokens: parsed.n_saved,
        bytes: parsed.n_written,
        saved_at: chrono::Utc::now(),
    };

    match serde_json::to_string(&record) {
        Ok(json) => {
            if let Err(e) = database.settings.set(WARM_START_KEY, &json) {
                warn!("Saved a warm-start blob but could not record it ({}); removing the blob", e);
                let _ = std::fs::remove_file(record.path());
                return None;
            }
        }
        Err(e) => {
            warn!("Could not serialise the warm-start record: {}", e);
            return None;
        }
    }

    info!(
        "Warm-start cache saved for session {}: {} tokens, {}",
        session_id,
        record.n_tokens,
        crate::utils::storage_governor::format_bytes(record.bytes)
    );
    Some(record)
}

/// Why a restore did not happen. Every variant is reported, never swallowed.
#[derive(Debug)]
pub enum RestoreOutcome {
    Restored { session_id: String, n_tokens: u64 },
    NoRecord,
    Rejected(String),
    Failed(String),
}

/// Restore the saved slot, but only if it still belongs to this exact setup.
///
/// The identity check is the load-bearing part. `model_id`, `engine_id` and
/// `ctx_size` describe the runtime that just started; if any of them differs
/// from what produced the blob, the blob is deleted rather than restored.
pub async fn restore_if_valid(
    database: &MemoryDatabase,
    base_url: &str,
    model_id: &str,
    engine_id: &str,
    ctx_size: u32,
) -> RestoreOutcome {
    let record = match load_record(database) {
        Some(r) => r,
        None => return RestoreOutcome::NoRecord,
    };

    // Each mismatch is named individually: "the cache was not used" is not
    // actionable, "the cache was for a different model" is.
    if record.model_id != model_id {
        let reason = format!(
            "it was saved for model '{}' but '{}' is loaded",
            record.model_id, model_id
        );
        discard(database, &reason);
        return RestoreOutcome::Rejected(reason);
    }
    if record.engine_id != engine_id {
        let reason = format!(
            "it was saved by engine '{}' but '{}' is installed",
            record.engine_id, engine_id
        );
        discard(database, &reason);
        return RestoreOutcome::Rejected(reason);
    }
    if record.ctx_size != ctx_size {
        let reason = format!(
            "it was saved at a context size of {} but the runtime started at {}",
            record.ctx_size, ctx_size
        );
        discard(database, &reason);
        return RestoreOutcome::Rejected(reason);
    }

    // The blob can vanish without the record: disk eviction treats KV blobs as
    // the first thing to delete (see storage_governor), which is correct - this
    // is regenerable cache - but it leaves the record pointing at nothing.
    let path = record.path();
    if !path.exists() {
        let reason = format!("the blob at {} is gone (likely reclaimed for disk space)", path.display());
        discard(database, &reason);
        return RestoreOutcome::Rejected(reason);
    }

    let client = match reqwest::Client::builder().timeout(SLOT_IO_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return RestoreOutcome::Failed(format!("HTTP client: {}", e)),
    };

    let url = format!("{}/slots/{}?action=restore", base_url.trim_end_matches('/'), SLOT_ID);
    let response = match client
        .post(&url)
        .json(&serde_json::json!({ "filename": record.filename }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let reason = format!("restore request failed: {}", e);
            discard(database, &reason);
            return RestoreOutcome::Failed(reason);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let reason = format!("engine refused the restore ({}): {}", status, body);
        // A blob the engine will not accept is worse than no blob: it would be
        // retried on every launch.
        discard(database, &reason);
        return RestoreOutcome::Failed(reason);
    }

    let parsed: SlotRestoreResponse = match response.json().await {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("could not read the restore response: {}", e);
            discard(database, &reason);
            return RestoreOutcome::Failed(reason);
        }
    };

    info!(
        "Warm start: restored {} tokens ({}) for session {} - its next message skips re-processing the conversation",
        parsed.n_restored,
        crate::utils::storage_governor::format_bytes(parsed.n_read),
        record.session_id
    );

    RestoreOutcome::Restored {
        session_id: record.session_id,
        n_tokens: parsed.n_restored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::MemoryDatabase;

    fn record(model: &str, engine: &str, ctx: u32) -> WarmStartRecord {
        WarmStartRecord {
            session_id: "s1".to_string(),
            filename: "session_s1.bin".to_string(),
            model_id: model.to_string(),
            engine_id: engine.to_string(),
            ctx_size: ctx,
            n_tokens: 1745,
            bytes: 14_309_796,
            saved_at: chrono::Utc::now(),
        }
    }

    fn store(db: &MemoryDatabase, r: &WarmStartRecord) {
        db.settings
            .set(WARM_START_KEY, &serde_json::to_string(r).unwrap())
            .unwrap();
    }

    #[test]
    fn a_session_id_can_never_escape_the_cache_directory() {
        // Session ids originate in the frontend. A traversal attempt must not
        // produce a path outside slot_cache_dir().
        let name = filename_for("../../windows/system32/evil");
        assert!(!name.contains(".."));
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(name.starts_with("session_") && name.ends_with(".bin"));
    }

    #[test]
    fn a_record_round_trips_through_the_settings_store() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let r = record("gemma-3-1b", "llama-vulkan-b8037", 8192);
        store(&db, &r);

        let loaded = load_record(&db).expect("record must load");
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.model_id, "gemma-3-1b");
        assert_eq!(loaded.n_tokens, 1745);
    }

    #[test]
    fn a_corrupt_record_is_discarded_rather_than_returned() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.settings.set(WARM_START_KEY, "{ not json").unwrap();
        assert!(load_record(&db).is_none());
        // ...and it is cleared, so it cannot fail again on the next launch.
        assert!(db.settings.get(WARM_START_KEY).unwrap().is_none());
    }

    /// The identity guard is the whole safety story for this feature: a blob
    /// restored into the wrong model is undefined behaviour, not a cache miss.
    /// These run without any engine because every rejection is decided from
    /// the record alone, before any HTTP call is made.
    #[tokio::test]
    async fn a_blob_from_a_different_model_is_rejected_and_deleted() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        store(&db, &record("gemma-3-1b", "engine-b8037", 8192));

        let outcome = restore_if_valid(&db, "http://127.0.0.1:1", "qwen-2.5-3b", "engine-b8037", 8192).await;

        match outcome {
            RestoreOutcome::Rejected(reason) => {
                // The message must name BOTH models. "the cache was not used"
                // is not actionable; knowing which model it belonged to and
                // which is loaded explains it completely.
                assert!(
                    reason.contains("gemma-3-1b") && reason.contains("qwen-2.5-3b"),
                    "reason must name both the saved and the loaded model: {}",
                    reason
                );
            }
            other => panic!("expected rejection, got {:?}", other),
        }
        assert!(load_record(&db).is_none(), "a rejected record must not survive to be retried");
    }

    #[tokio::test]
    async fn a_blob_from_a_different_engine_build_is_rejected() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        store(&db, &record("gemma-3-1b", "engine-b8037", 8192));

        let outcome = restore_if_valid(&db, "http://127.0.0.1:1", "gemma-3-1b", "engine-b9000", 8192).await;
        assert!(matches!(outcome, RestoreOutcome::Rejected(_)));
        assert!(load_record(&db).is_none());
    }

    #[tokio::test]
    async fn a_blob_saved_at_another_context_size_is_rejected() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        store(&db, &record("gemma-3-1b", "engine-b8037", 8192));

        let outcome = restore_if_valid(&db, "http://127.0.0.1:1", "gemma-3-1b", "engine-b8037", 32768).await;
        assert!(matches!(outcome, RestoreOutcome::Rejected(_)));
        assert!(load_record(&db).is_none());
    }

    #[tokio::test]
    async fn no_record_is_a_quiet_normal_outcome() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let outcome = restore_if_valid(&db, "http://127.0.0.1:1", "m", "e", 8192).await;
        assert!(matches!(outcome, RestoreOutcome::NoRecord));
    }

    /// Eviction deletes KV blobs first (they are regenerable), which can leave
    /// the record pointing at a file that no longer exists. That must be
    /// detected BEFORE any HTTP call, or every launch would attempt a restore
    /// that cannot succeed.
    #[tokio::test]
    async fn a_record_whose_blob_was_evicted_is_rejected_without_contacting_the_engine() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let r = record("gemma-3-1b", "engine-b8037", 8192);
        store(&db, &r);
        // The blob was never written, standing in for one eviction removed.
        assert!(!r.path().exists());

        // The URL points at a closed port: reaching HTTP at all would surface
        // as Failed rather than Rejected, so this also proves the check runs
        // first.
        let outcome = restore_if_valid(&db, "http://127.0.0.1:1", "gemma-3-1b", "engine-b8037", 8192).await;
        match outcome {
            RestoreOutcome::Rejected(reason) => assert!(reason.contains("gone")),
            other => panic!("expected rejection before any request, got {:?}", other),
        }
        assert!(load_record(&db).is_none());
    }
}
