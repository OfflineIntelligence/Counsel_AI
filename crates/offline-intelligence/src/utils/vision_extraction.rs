//! Vision-model routing for standalone image extraction.
//!
//! Product behaviour (decided 2026-08-08):
//!   - A vision-capable model (loaded with --mmproj) is ACTIVE and ready →
//!     the image is transcribed by the VISION MODEL, only. It reads
//!     handwriting; Windows OCR does not.
//!   - No vision model → Windows OCR runs so the user is never blocked, and
//!     the result is labelled as basic OCR so the UI can tell the user that a
//!     vision model would read handwritten content better.
//!
//! This is a deterministic branch on one observable condition
//! (`SharedSystemState::vision_active`), never a silent failover: whichever
//! engine ran is named in the extracted text's provenance header, stored in
//! the database with the text, and surfaced to the UI via
//! `extraction_engine_of`. A vision-model FAILURE while vision is active is a
//! loud explicit failure marker — it does not quietly fall back to OCR.
//!
//! Scope (also a product decision): STANDALONE image files only. Scanned PDF
//! pages and DOCX-embedded images stay on Windows OCR (utils::pdf_text /
//! utils::file_processor).
//!
//! The shared state is registered once at server startup rather than threaded
//! through `extract_content_from_bytes`'s many call sites — extraction is
//! invoked from six API paths plus tests, and the routing condition is global
//! by nature (there is exactly one runtime). Tests never register state, so
//! they exercise the OCR path unchanged.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use tracing::{info, warn};

use crate::shared_state::SharedSystemState;

/// Provenance-header marker for vision-model extractions. The DB and UI
/// classification below keys on this exact phrase — change them together.
const VISION_MARKER: &str = "transcribed by the vision model";
/// Marker emitted by utils::image_ocr / utils::pdf_text for Windows OCR.
const OCR_MARKER: &str = "recovered via OCR";

static SHARED_STATE: OnceLock<Arc<SharedSystemState>> = OnceLock::new();

/// Serialise vision transcriptions among themselves: llama-server runs with
/// --parallel 1, and flooding it with N concurrent image requests would just
/// queue them there while starving interactive chat. One at a time, here.
static VISION_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Register the process-wide shared state. Called once from thread_server
/// during boot; later calls are no-ops (OnceLock).
pub fn register_shared_state(state: Arc<SharedSystemState>) {
    if SHARED_STATE.set(state).is_ok() {
        info!("Vision extraction routing registered with shared state");
    }
}

/// Name of the active model, for provenance headers. Best-effort.
async fn active_model_name(state: &SharedSystemState) -> String {
    let rt = state.runtime_manager.read().ok().and_then(|g| g.clone());
    if let Some(rt) = rt {
        if let Some(cfg) = rt.get_current_config().await {
            if let Some(stem) = cfg.model_path.file_stem() {
                return stem.to_string_lossy().to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Which engine produced a stored extraction, classified from the provenance
/// header this codebase itself writes (first line only — a DOCX containing an
/// OCR'd embedded image mentions OCR mid-text, which must not reclassify the
/// whole document):
///   "vision_model" — transcribed by the active vision model
///   "windows_ocr"  — basic OCR (image or scanned PDF); for images this is
///                    what the UI flags as "use a vision model for handwriting"
///   "native"       — real text extraction (PDF text layer, DOCX, XLSX, ...)
pub fn extraction_engine_of(extracted_text: &str) -> &'static str {
    let first_line = extracted_text.lines().next().unwrap_or("");
    if first_line.starts_with('[') && first_line.contains(VISION_MARKER) {
        "vision_model"
    } else if first_line.starts_with('[') && first_line.contains(OCR_MARKER) {
        "windows_ocr"
    } else {
        "native"
    }
}

/// Transcribe with ONE retry after an inference-server restart.
///
/// A connection-level failure ("error sending request") means llama-server
/// died or was mid-restart when the request went out — the app auto-restarts
/// it within ~2 minutes. Without this, an attach that merely RACED a restart
/// was permanently recorded as failed. The retry is gated on the runtime
/// actually reporting vision-ready again; if it never comes back, the loud
/// failure stands. HTTP error responses from a HEALTHY server are not
/// retried — those are real answers.
async fn transcribe_with_restart_retry(
    state: &SharedSystemState,
    bytes: &[u8],
    filename: &str,
) -> anyhow::Result<String> {
    match state.llm_worker.transcribe_image(bytes, filename).await {
        Ok(text) => Ok(text),
        Err(first_err) => {
            let connection_level = first_err.to_string().contains("request failed");
            if !connection_level {
                return Err(first_err);
            }
            warn!(
                "Vision request for '{}' failed at the connection level ({}) — waiting for \
                 the inference server to come back, then retrying once",
                filename, first_err
            );
            const WAIT_SECS: u64 = 120;
            const POLL_SECS: u64 = 3;
            let mut waited = 0u64;
            while waited < WAIT_SECS {
                tokio::time::sleep(std::time::Duration::from_secs(POLL_SECS)).await;
                waited += POLL_SECS;
                if state.vision_active().await {
                    info!(
                        "Inference server is back after {}s — retrying vision transcription of '{}'",
                        waited, filename
                    );
                    return state.llm_worker.transcribe_image(bytes, filename).await;
                }
            }
            Err(anyhow::anyhow!(
                "{} (the inference server did not come back within {}s)",
                first_err, WAIT_SECS
            ))
        }
    }
}

/// Extract text from a standalone image, routing to the vision model when one
/// is active and to Windows OCR otherwise. Output keeps the established
/// "[header]\n<text>" provenance contract that `extraction_outcome`, chunking
/// and citations already understand.
pub async fn extract_image_text(bytes: Vec<u8>, filename: &str) -> Result<String> {
    let state = SHARED_STATE.get().cloned();

    let vision_ready = match &state {
        Some(s) => s.vision_active().await,
        None => false,
    };

    if !vision_ready {
        // No vision model: Windows OCR, labelled as such. The UI reads
        // extraction_engine_of() == "windows_ocr" and tells the user a vision
        // model would read handwriting better.
        return crate::utils::image_ocr::extract_image_text(bytes, filename).await;
    }

    let state = state.expect("vision_ready implies state");
    let model_name = active_model_name(&state).await;
    let _gate = VISION_GATE.lock().await;

    // Re-check after waiting at the gate: a model switch may have happened
    // while an earlier transcription ran.
    if !state.vision_active().await {
        warn!(
            "Vision model deactivated while '{}' waited for transcription — using Windows OCR",
            filename
        );
        return crate::utils::image_ocr::extract_image_text(bytes, filename).await;
    }

    // Bound the pixels BEFORE the request. Oversized images drove the vision
    // encoder to multi-GB GPU allocations that ABORTED llama-server on real
    // hardware (see win_ocr::downscale_to_png_blocking for the measurements).
    // 1.15MP is deterministic headroom: verified safe well past it on a 4GB
    // card, and VLMs are trained at exactly these budgets.
    const MAX_VISION_PIXELS: u64 = 1_150_000;
    #[cfg(target_os = "windows")]
    let bytes = {
        let original = bytes;
        let label = filename.to_string();
        let downscaled = tokio::task::spawn_blocking({
            let original = original.clone();
            move || crate::utils::win_ocr::downscale_to_png_blocking(&original, MAX_VISION_PIXELS, &label)
        })
        .await
        .map_err(|e| anyhow::anyhow!("vision downscale task failed: {}", e))?;
        match downscaled {
            Ok(Some(smaller)) => smaller,
            Ok(None) => original,
            Err(e) => {
                // Undecodable bytes will fail the vision call too — surface
                // the decode problem now, loudly, with the filename.
                return Ok(format!(
                    "[Cannot extract image '{}': {}]",
                    filename, e
                ));
            }
        }
    };

    match transcribe_with_restart_retry(&state, &bytes, filename).await {
        Ok(raw) => {
            let text = raw.trim();
            if text.is_empty() || text.starts_with("NO_TEXT_FOUND") {
                let description = text
                    .strip_prefix("NO_TEXT_FOUND")
                    .map(str::trim)
                    .filter(|d| !d.is_empty());
                Ok(match description {
                    Some(d) => format!(
                        "[Image '{}': the vision model ({}) found no readable text. \
                         Image description: {}]",
                        filename, model_name, d
                    ),
                    None => format!(
                        "[Image '{}': the vision model ({}) found no readable text in this image.]",
                        filename, model_name
                    ),
                })
            } else {
                Ok(format!(
                    "[Image '{}': text below was {} ({}) — handwriting included; may \
                     contain recognition errors.]\n{}",
                    filename, VISION_MARKER, model_name, text
                ))
            }
        }
        Err(e) => {
            // Vision is the selected engine; its failure is THE failure.
            // Falling back to OCR here would be exactly the silent failover
            // this product forbids.
            warn!("Vision transcription failed for '{}': {}", filename, e);
            Ok(format!(
                "[Cannot extract image '{}': the vision model ({}) failed to process it: {}]",
                filename, model_name, e
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No shared state registered (the test-process condition) → the OCR
    /// lane must be used, meaning behaviour is identical to before this
    /// module existed.
    #[tokio::test]
    async fn without_registered_state_images_route_to_ocr() {
        let text = extract_image_text(b"not an image".to_vec(), "img.png")
            .await
            .unwrap();
        // utils::image_ocr's contract: explicit bracketed marker naming the file.
        assert!(text.contains("img.png"), "{}", text);
        assert_eq!(extraction_engine_of(&text), "native"); // failure marker, not OCR text
    }

    #[test]
    fn engine_classification_reads_only_the_first_line() {
        let vision = format!(
            "[Image 'a.png': text below was {} (Qwen2.5-VL) — handwriting included; may contain recognition errors.]\nDear Sir...",
            VISION_MARKER
        );
        assert_eq!(extraction_engine_of(&vision), "vision_model");

        let ocr = "[Image 'a.png': text below was recovered via OCR (Windows OCR — may contain recognition errors).]\nDear Sir...";
        assert_eq!(extraction_engine_of(ocr), "windows_ocr");

        let scanned_pdf = "[Scanned PDF 'x.pdf': text below was recovered via OCR (2 of 2 pages).]\nCONTRACT...";
        assert_eq!(extraction_engine_of(scanned_pdf), "windows_ocr");

        // OCR marker mid-document (DOCX embedded image) must NOT reclassify.
        let docx = "First paragraph.\n[[image:1]]\n[Image 'embedded image 1': text below was recovered via OCR (Windows OCR — may contain recognition errors).]\nstamp text\n";
        assert_eq!(extraction_engine_of(docx), "native");

        assert_eq!(extraction_engine_of("Plain extracted text."), "native");
        assert_eq!(extraction_engine_of(""), "native");
    }
}
