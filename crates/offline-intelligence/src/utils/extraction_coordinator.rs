//! Serializes text extraction for a single Local Storage file.
//!
//! Local Storage uploads return immediately and extract in the BACKGROUND
//! (api::files_api::upload_file), which creates a race the product hits
//! routinely: a user uploads a file and attaches it to a chat seconds later,
//! while the background extraction is still running.
//!
//! Without coordination both paths extract the same bytes concurrently. That
//! is not a correctness bug — `documents.content_hash` is UNIQUE and
//! `upsert_document` returns the existing row for a known hash, so exactly
//! one row results either way. It is a real WASTE bug: extraction is the
//! expensive step in this system (pdfium plus Windows OCR on a scanned PDF is
//! seconds to tens of seconds, capped at 50 pages), and doing it twice for one
//! file burns that twice while the user waits on their own attach.
//!
//! So this is an optimisation guarding a path that is already correct, and it
//! is deliberately built so that a failure of the optimisation degrades to the
//! old behaviour (duplicate work) rather than to incorrectness. Callers do
//! double-checked locking:
//!
//! 1. `acquire(local_file_id).await` — exclusive extraction rights
//! 2. re-check the database; another holder may have finished while we waited
//! 3. extract and write only if the row is still missing or unusable
//!
//! Step 2 is not optional. Skipping it reintroduces exactly the duplicate
//! extraction this type exists to prevent, just moved behind a lock.
//!
//! Scope: this coordinates Local-Storage-backed files, keyed by
//! `local_files.id`. Paperclip attachments have no such id (no permanent byte
//! copy is kept) and cannot collide this way — a paperclip attachment is
//! extracted exactly once, in the request that carries its bytes.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Above this many tracked files, an `acquire` opportunistically drops locks
/// nobody is holding. Purely to stop unbounded growth in a long-lived
/// process; the value only needs to be comfortably larger than the number of
/// files one user realistically uploads in a single burst.
const PRUNE_THRESHOLD: usize = 512;

/// Exclusive extraction rights for one Local Storage file. Extraction is
/// serialized for as long as this is held; dropping it releases the next
/// waiter.
pub type ExtractionPermit = OwnedMutexGuard<()>;

#[derive(Default)]
pub struct ExtractionCoordinator {
    locks: DashMap<i64, Arc<Mutex<()>>>,
}

impl ExtractionCoordinator {
    pub fn new() -> Self {
        Self { locks: DashMap::new() }
    }

    /// Wait for exclusive extraction rights to `local_file_id`.
    ///
    /// Re-check the database after this returns — see the module docs for why.
    pub async fn acquire(&self, local_file_id: i64) -> ExtractionPermit {
        // Clone the Arc out and release the DashMap shard guard BEFORE
        // awaiting. Holding a DashMap guard across an await would block every
        // other file's `acquire` on the same shard for the duration of an OCR
        // pass, turning a per-file lock into a near-global one.
        let lock = self
            .locks
            .entry(local_file_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();

        if self.locks.len() > PRUNE_THRESHOLD {
            self.prune(local_file_id);
        }

        lock.lock_owned().await
    }

    /// Drop locks that only this map still references.
    ///
    /// `strong_count == 1` means no task holds or is waiting on the lock, so
    /// removing it is safe. A task that has cloned the Arc but not yet locked
    /// keeps the count above 1 and is therefore preserved.
    ///
    /// There is a narrow window where a task has read the map but not yet
    /// cloned, and a prune could then let it build a NEW lock for the same
    /// id — permitting two concurrent extractions. That is precisely the
    /// pre-existing, already-correct behaviour described in the module docs
    /// (hash dedup makes it wasteful, never wrong), which is why this is safe
    /// to do opportunistically instead of with a heavier global lock.
    ///
    /// `keep` is the id the current caller is about to lock; never evict it.
    fn prune(&self, keep: i64) {
        self.locks
            .retain(|id, lock| *id == keep || Arc::strong_count(lock) > 1);
    }

    /// Number of tracked files. Test/diagnostic use only.
    pub fn tracked_len(&self) -> usize {
        self.locks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// The core guarantee: two tasks racing on the SAME file are serialized,
    /// so the second sees the first's work and can skip its own.
    ///
    /// `peak` records the highest number of tasks inside the critical section
    /// at once. If the lock is per-file and honoured it can never exceed 1 —
    /// this is what proves upload-then-attach cannot double-extract.
    #[tokio::test]
    async fn same_file_extractions_never_overlap() {
        let coordinator = Arc::new(ExtractionCoordinator::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let coordinator = coordinator.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let _permit = coordinator.acquire(42).await;
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Long enough that genuine overlap would be observed.
                tokio::time::sleep(Duration::from_millis(20)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "extractions of the same file overlapped - upload and attach could double-extract"
        );
    }

    /// DIFFERENT files must NOT block each other. A per-file lock that
    /// serialized everything would make a batch of uploads extract one at a
    /// time, which is slower than the behaviour this type replaced.
    #[tokio::test]
    async fn different_files_extract_concurrently() {
        let coordinator = Arc::new(ExtractionCoordinator::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for id in 0..6i64 {
            let coordinator = coordinator.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let _permit = coordinator.acquire(id).await;
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "distinct files were serialized against each other - the lock is not per-file"
        );
    }

    /// A permit must be released when dropped even if the holder panicked,
    /// or one failed extraction would wedge that file forever.
    #[tokio::test]
    async fn a_panicking_holder_does_not_wedge_the_file() {
        let coordinator = Arc::new(ExtractionCoordinator::new());
        let c = coordinator.clone();
        let panicked = tokio::spawn(async move {
            let _permit = c.acquire(7).await;
            panic!("extraction blew up");
        })
        .await;
        assert!(panicked.is_err(), "the task was expected to panic");

        // Must not hang.
        let again = tokio::time::timeout(Duration::from_secs(5), coordinator.acquire(7)).await;
        assert!(again.is_ok(), "the file stayed locked after its holder panicked");
    }

    #[tokio::test]
    async fn pruning_keeps_the_map_bounded_without_breaking_locking() {
        let coordinator = ExtractionCoordinator::new();
        for id in 0..(PRUNE_THRESHOLD as i64 + 50) {
            drop(coordinator.acquire(id).await);
        }
        assert!(
            coordinator.tracked_len() <= PRUNE_THRESHOLD + 1,
            "lock map grew unbounded: {}",
            coordinator.tracked_len()
        );
        // Still functional after pruning.
        let _permit = coordinator.acquire(1).await;
    }
}
