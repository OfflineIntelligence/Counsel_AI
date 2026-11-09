//! Downloads that have started but not finished, remembered across restarts.
//!
//! A record is written when an install starts and removed when it completes,
//! fails or is cancelled. Whatever is still here at the next launch was
//! interrupted — the app was closed (downloads are paused first, see
//! `admin::stop_backend`) or the process was killed — and the UI offers to
//! resume it. The partial files stay in the model's directory, and a resumed
//! install continues from them with HTTP Range requests instead of starting
//! over.
//!
//! The Hugging Face token is never written here; the client supplies it again
//! when it resumes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::warn;

const FILE_NAME: &str = "pending_downloads.json";

/// Everything needed to start the same install again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDownload {
    pub model_id: String,
    pub model_name: String,
    /// The install request's `source`, as sent.
    pub source: serde_json::Value,
    pub description: Option<String>,
    pub size_bytes: u64,
    pub format: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Serialises read-modify-write of the file; installs run concurrently.
static LOCK: Mutex<()> = Mutex::new(());

fn path(registry_dir: &Path) -> PathBuf {
    registry_dir.join(FILE_NAME)
}

pub fn load(registry_dir: &Path) -> Vec<PendingDownload> {
    let _guard = LOCK.lock();
    read(registry_dir)
}

fn read(registry_dir: &Path) -> Vec<PendingDownload> {
    match std::fs::read(path(registry_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!("Ignoring unreadable {}: {}", FILE_NAME, e);
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

fn write(registry_dir: &Path, records: &[PendingDownload]) {
    let target = path(registry_dir);
    // Write-then-rename so a crash mid-write cannot leave a truncated file.
    let tmp = target.with_extension("json.tmp");
    let result = serde_json::to_vec_pretty(records)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| std::fs::write(&tmp, bytes).map_err(anyhow::Error::from))
        .and_then(|_| std::fs::rename(&tmp, &target).map_err(anyhow::Error::from));
    if let Err(e) = result {
        warn!("Could not save {}: {}", FILE_NAME, e);
    }
}

/// Record (or refresh) an install that is starting.
pub fn upsert(registry_dir: &Path, record: PendingDownload) {
    let _guard = LOCK.lock();
    let mut records = read(registry_dir);
    records.retain(|r| r.model_id != record.model_id);
    records.push(record);
    write(registry_dir, &records);
}

/// Forget an install: it finished, failed, was cancelled or was discarded.
pub fn remove(registry_dir: &Path, model_id: &str) {
    let _guard = LOCK.lock();
    let mut records = read(registry_dir);
    let before = records.len();
    records.retain(|r| r.model_id != model_id);
    if records.len() != before {
        write(registry_dir, &records);
    }
}

/// Bytes already on disk for an interrupted install (every file in the
/// model's directory), so the UI can say how far it got.
pub fn bytes_on_disk(model_dir: &Path) -> u64 {
    std::fs::read_dir(model_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
