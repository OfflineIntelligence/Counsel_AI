//! Concurrency policy for document extraction: which files may be processed at
//! the same time, and which must wait for each other.
//!
//! Attaching six files should not take six times as long as attaching one. But
//! neither can everything run at once — the engines behind these formats have
//! very different concurrency characteristics, and two of them are hard limits
//! rather than preferences.
//!
//! # The model
//!
//! One LANE per format family. Within a lane, work is strictly serial; across
//! lanes it runs in parallel. Attaching three PDFs, a Word file, a spreadsheet
//! and an image therefore runs four things at once, with the PDFs queued behind
//! each other — which is exactly what the underlying engines allow.
//!
//! Cutting across all of that is the OCR GATE: a single process-wide permit,
//! because Windows OCR work must never overlap. That constraint is not
//! per-format — images, scanned PDFs and DOCX files with embedded pictures all
//! reach the same engine — so it cannot be expressed as a lane. A lane says
//! "these files wait for each other"; the gate says "this RESOURCE is used by
//! one caller at a time, whoever they are".
//!
//! # Why serial-within-lane is not an arbitrary choice for PDFs
//!
//! pdfium-render's `thread_safe` feature holds a global mutex for the entire
//! lifetime of a `Pdfium` object (measured, see `memory_db/fts.rs` for the
//! project's habit of pinning such findings). Concurrent PDF extraction was
//! therefore ALREADY serial — just badly: N blocking threads parked on one
//! mutex, and a panic in any of them poisons it for the whole process. The PDF
//! lane makes that serialisation intentional and cheap: one worker at a time,
//! the rest waiting on an async permit instead of occupying a blocking thread.
//!
//! # Deadlock
//!
//! A PDF holds pdfium's lock and may then wait for the OCR gate (scanned pages).
//! That is safe only because nothing that holds the OCR gate ever waits for
//! pdfium: the image path calls Windows OCR directly and never binds pdfium in
//! production (its only `bind_pdfium` calls are inside `#[cfg(test)]`, used to
//! generate fixtures). There is no cycle. Adding a pdfium call to the image or
//! DOCX path would create one — do not.
//!
//! # Fairness
//!
//! Lane permits and the OCR gate are both FIFO, so a file submitted first is
//! processed first. Without that, a steady trickle of small files could starve a
//! large one indefinitely.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Format families that own a lane.
///
/// Grouped by the ENGINE that processes them, not by extension: `doc`/`docx`
/// both go through the Word path, `xls`/`xlsx` both through calamine. Two files
/// share a lane exactly when they would contend on the same extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// pdfium, plus Windows OCR for scanned pages.
    Pdf,
    /// quick-xml, plus Windows OCR for embedded images.
    Word,
    /// calamine. Pure Rust, no shared state.
    Spreadsheet,
    /// quick-xml. Pure Rust, no shared state.
    Presentation,
    /// Windows OCR directly.
    Image,
    /// Encoding-detected decode. Cheap, but given its own lane so a trivial
    /// .txt never queues behind a 50-page scan.
    Text,
}

impl Lane {
    /// Lane for a filename, by extension.
    ///
    /// Unknown extensions land in `Text`, matching
    /// `file_processor::extract_content_from_bytes`'s own fallback. In practice
    /// the format gate rejects those before they reach here; this only decides
    /// scheduling, never whether a file is accepted.
    pub fn for_filename(filename: &str) -> Lane {
        let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
        match ext.as_str() {
            "pdf" => Lane::Pdf,
            "doc" | "docx" => Lane::Word,
            "xls" | "xlsx" => Lane::Spreadsheet,
            "ppt" | "pptx" => Lane::Presentation,
            "png" | "jpg" | "jpeg" => Lane::Image,
            _ => Lane::Text,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Lane::Pdf => "pdf",
            Lane::Word => "word",
            Lane::Spreadsheet => "spreadsheet",
            Lane::Presentation => "presentation",
            Lane::Image => "image",
            Lane::Text => "text",
        }
    }

    fn all() -> [Lane; 6] {
        [
            Lane::Pdf,
            Lane::Word,
            Lane::Spreadsheet,
            Lane::Presentation,
            Lane::Image,
            Lane::Text,
        ]
    }
}

/// Exclusive right to run extraction in one lane. Held for the duration of the
/// work; dropping it admits the next file in that lane.
pub type LanePermit = OwnedSemaphorePermit;

pub struct ExtractionScheduler {
    lanes: HashMap<Lane, std::sync::Arc<Semaphore>>,
}

impl Default for ExtractionScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtractionScheduler {
    pub fn new() -> Self {
        let mut lanes = HashMap::new();
        for lane in Lane::all() {
            // One permit: serial within the lane. Distinct semaphores: parallel
            // across lanes.
            lanes.insert(lane, std::sync::Arc::new(Semaphore::new(1)));
        }
        Self { lanes }
    }

    /// Wait for the right to extract `filename`, based on its format lane.
    ///
    /// Hold the returned permit for the whole extraction. Dropping it early
    /// would let a second file in the same lane start alongside this one, which
    /// for PDFs means two threads contending on pdfium's global mutex again.
    ///
    /// # Do not call this before `extract_content_from_bytes`
    ///
    /// That function acquires its own lane permit internally, which is the whole
    /// point of it being the single choke point. Holding a permit for the same
    /// lane and then calling it SELF-DEADLOCKS: one permit, held by the caller,
    /// awaited by the callee, forever. This method exists for code that
    /// schedules extraction work WITHOUT going through the extractor (tests, and
    /// any future batching layer), not for wrapping it.
    pub async fn acquire(&self, filename: &str) -> LanePermit {
        self.acquire_lane(Lane::for_filename(filename)).await
    }

    pub async fn acquire_lane(&self, lane: Lane) -> LanePermit {
        let semaphore = self
            .lanes
            .get(&lane)
            .expect("every Lane variant is registered in new()")
            .clone();
        semaphore
            .acquire_owned()
            .await
            .expect("lane semaphores are never closed")
    }

    /// Files currently waiting for the given lane. Diagnostics only.
    pub fn queued_for(&self, lane: Lane) -> usize {
        self.lanes
            .get(&lane)
            .map(|s| if s.available_permits() == 0 { 1 } else { 0 })
            .unwrap_or(0)
    }
}

/// The process-wide scheduler.
///
/// A singleton rather than a field on `SharedState`, for the same reason the OCR
/// gate is: the constraints being modelled are properties of the MACHINE, not of
/// an application instance. pdfium's mutex is a global in its own crate, and
/// there is one Windows OCR engine. Two schedulers would each believe they had
/// exclusive use of resources they were in fact sharing.
///
/// The practical consequence is that `extract_content_from_bytes` can enforce
/// lane discipline itself, at the single point every extraction already passes
/// through, rather than relying on each of its six call sites to remember a
/// permit — and on every future call site to do the same.
pub fn global() -> &'static ExtractionScheduler {
    static SCHEDULER: OnceLock<ExtractionScheduler> = OnceLock::new();
    SCHEDULER.get_or_init(ExtractionScheduler::new)
}

/// Process-wide gate serialising ALL Windows OCR work.
///
/// A `std::sync::Mutex` rather than a tokio one on purpose: every caller is
/// already inside `spawn_blocking` (OCR is synchronous WinRT interop), so there
/// is no async context to await in, and blocking a blocking-pool thread is the
/// correct thing to do there.
fn ocr_lock() -> &'static Mutex<()> {
    static OCR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    OCR_LOCK.get_or_init(|| Mutex::new(()))
}

/// Process-wide gate serialising ALL pdfium use.
///
/// PDFium is genuinely not thread-safe, so this serialisation is required, not
/// a preference. It exists here rather than inside `pdfium-render` because that
/// crate's own `thread_safe` feature — which we disable for exactly this reason
/// — uses a poisoning mutex: one panic in a live pdfium scope permanently
/// disabled PDF extraction for the whole process.
fn pdfium_lock() -> &'static Mutex<()> {
    static PDFIUM_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PDFIUM_LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire exclusive use of pdfium. Held until the guard drops.
///
/// **Poisoning is deliberately ignored**, and that is the entire point of owning
/// this lock instead of letting `pdfium-render` own it.
///
/// A `std::sync::Mutex` poisons when a holder panics, and every later caller
/// then gets `Err`. Applied to a process-wide PDF engine that means one
/// malformed file kills PDF extraction until the app restarts. `catch_unwind`
/// does not help: the guard poisons as it is dropped DURING the unwind, below
/// any catch boundary (measured — see `utils::pdf_text`).
///
/// Recovering the guard is safe here because the mutex protects a RESOURCE, not
/// an invariant spanning calls. Each extraction binds its own `Pdfium`, uses it,
/// and drops it; a panicked extraction leaves no shared state for the next one
/// to observe. What the lock guarantees is only "one caller at a time", and that
/// is still true of the recovered guard.
pub fn pdfium_permit() -> MutexGuard<'static, ()> {
    match pdfium_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Acquire exclusive use of the OCR engine. Held until the guard drops.
///
/// **Poisoning is deliberately ignored.** A `std::sync::Mutex` poisons when a
/// holder panics, and a poisoned lock returns `Err` to every future caller —
/// which is exactly how pdfium's global mutex turns one bad file into a
/// process-wide, permanent failure of every PDF thereafter. That behaviour is
/// wrong here: the guarded resource is a stateless per-call WinRT engine, so a
/// panic in one OCR attempt leaves nothing for the next one to trip over. The
/// mutex exists to serialise access, not to protect invariants across calls, so
/// recovering the guard is safe and strictly better than inheriting that failure
/// mode.
pub fn ocr_permit() -> MutexGuard<'static, ()> {
    match ocr_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn every_supported_format_maps_to_the_expected_lane() {
        assert_eq!(Lane::for_filename("contract.pdf"), Lane::Pdf);
        assert_eq!(Lane::for_filename("brief.DOC"), Lane::Word);
        assert_eq!(Lane::for_filename("brief.docx"), Lane::Word);
        assert_eq!(Lane::for_filename("model.xls"), Lane::Spreadsheet);
        assert_eq!(Lane::for_filename("model.xlsx"), Lane::Spreadsheet);
        assert_eq!(Lane::for_filename("deck.ppt"), Lane::Presentation);
        assert_eq!(Lane::for_filename("deck.pptx"), Lane::Presentation);
        assert_eq!(Lane::for_filename("scan.png"), Lane::Image);
        assert_eq!(Lane::for_filename("scan.JPG"), Lane::Image);
        assert_eq!(Lane::for_filename("scan.jpeg"), Lane::Image);
        assert_eq!(Lane::for_filename("notes.txt"), Lane::Text);
        // Unknown falls back to Text, mirroring the extractor's own fallback.
        assert_eq!(Lane::for_filename("mystery"), Lane::Text);
    }

    /// Instrumented run: records the highest number of tasks inside the guarded
    /// section at once, which is the only direct evidence of whether work
    /// actually overlapped.
    async fn peak_concurrency<F, Fut>(count: usize, body: F) -> usize
    where
        F: Fn(Arc<AtomicUsize>, Arc<AtomicUsize>) -> Fut + Send + Sync + 'static + Clone,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..count {
            let body = body.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move { body(inside, peak).await }));
        }
        for h in handles {
            h.await.unwrap();
        }
        peak.load(Ordering::SeqCst)
    }

    /// The core guarantee for PDFs: three PDFs must extract one after another,
    /// because pdfium cannot do otherwise and doing it explicitly is cheaper.
    #[tokio::test]
    async fn files_in_the_same_lane_never_overlap() {
        let scheduler = Arc::new(ExtractionScheduler::new());
        let s = scheduler.clone();
        let peak = peak_concurrency(4, move |inside, peak| {
            let s = s.clone();
            async move {
                let _permit = s.acquire("contract.pdf").await;
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(25)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .await;
        assert_eq!(peak, 1, "same-lane extractions overlapped");
    }

    /// The whole point of the feature: six files of different formats must not
    /// take six times as long as one.
    #[tokio::test]
    async fn files_in_different_lanes_run_in_parallel() {
        let scheduler = Arc::new(ExtractionScheduler::new());
        let names = [
            "a.pdf", "b.docx", "c.xlsx", "d.pptx", "e.png", "f.txt",
        ];
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for name in names {
            let s = scheduler.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let _permit = s.acquire(name).await;
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            6,
            "all six format lanes must run concurrently"
        );
    }

    /// The exact scenario from the requirement: 3 PDFs + Word + Excel + image.
    /// Four lanes active at once, PDFs serialised among themselves.
    #[tokio::test]
    async fn the_mixed_batch_scenario_behaves_as_specified() {
        let scheduler = Arc::new(ExtractionScheduler::new());
        let batch = [
            "one.pdf", "two.pdf", "three.pdf", "report.docx", "budget.xlsx", "scan.png",
        ];
        let overall_inside = Arc::new(AtomicUsize::new(0));
        let overall_peak = Arc::new(AtomicUsize::new(0));
        let pdf_inside = Arc::new(AtomicUsize::new(0));
        let pdf_peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for name in batch {
            let s = scheduler.clone();
            let overall_inside = overall_inside.clone();
            let overall_peak = overall_peak.clone();
            let pdf_inside = pdf_inside.clone();
            let pdf_peak = pdf_peak.clone();
            let is_pdf = name.ends_with(".pdf");
            handles.push(tokio::spawn(async move {
                let _permit = s.acquire(name).await;
                let now = overall_inside.fetch_add(1, Ordering::SeqCst) + 1;
                overall_peak.fetch_max(now, Ordering::SeqCst);
                if is_pdf {
                    let p = pdf_inside.fetch_add(1, Ordering::SeqCst) + 1;
                    pdf_peak.fetch_max(p, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                if is_pdf {
                    pdf_inside.fetch_sub(1, Ordering::SeqCst);
                }
                overall_inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            pdf_peak.load(Ordering::SeqCst),
            1,
            "the three PDFs must be processed one after another"
        );
        assert_eq!(
            overall_peak.load(Ordering::SeqCst),
            4,
            "pdf + word + spreadsheet + image lanes must all be active together"
        );
    }

    /// The OCR gate is what makes images, scanned PDFs and DOCX-with-images take
    /// turns even though they are in three different lanes.
    #[test]
    fn ocr_work_never_overlaps_even_across_lanes() {
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(std::thread::spawn(move || {
                let _permit = ocr_permit();
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(15));
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "OCR calls overlapped - the engine must be used by one caller at a time"
        );
    }

    /// A panic while holding the OCR gate must NOT wedge OCR for the rest of the
    /// process. This is the pdfium failure mode, deliberately not inherited.
    #[test]
    fn a_panic_under_the_ocr_gate_does_not_disable_ocr_forever() {
        let panicked = std::thread::spawn(|| {
            let _permit = ocr_permit();
            panic!("OCR blew up on a malformed image");
        })
        .join();
        assert!(panicked.is_err(), "the thread was expected to panic");

        // Must still be usable - and must not block.
        let recovered = std::thread::spawn(|| {
            let _permit = ocr_permit();
            true
        })
        .join()
        .expect("acquiring the gate after a panic must succeed");
        assert!(recovered);
    }

    /// Lane permits must be released on panic too, or one bad PDF would stall
    /// every later PDF.
    #[tokio::test]
    async fn a_panicking_lane_holder_does_not_stall_the_lane() {
        let scheduler = Arc::new(ExtractionScheduler::new());
        let s = scheduler.clone();
        let result = tokio::spawn(async move {
            let _permit = s.acquire("bad.pdf").await;
            panic!("extraction blew up");
        })
        .await;
        assert!(result.is_err(), "the task was expected to panic");

        let again = tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.acquire("next.pdf"),
        )
        .await;
        assert!(again.is_ok(), "the PDF lane stayed locked after a panic");
    }
}
