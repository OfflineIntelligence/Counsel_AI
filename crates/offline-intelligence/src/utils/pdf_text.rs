//! PDF text extraction via PDFium (the Chrome PDF engine).
//!
//! Engine resolution is explicit, first match wins, loud failure if absent:
//!   1. PDFIUM_PATH env var (file or directory)
//!   2. next to the executable (production: bundled by the installer)
//!   3. exe_dir/resources/
//!   4. vendor/pdfium/<platform>/ found via exe ancestors or CWD (development)
//!
//! Scanned-PDF handling: when the text layer is effectively empty, pages are
//! rasterized and OCR'd through the built-in Windows OCR engine (offline,
//! ships with Windows 10/11). On other platforms the condition is reported
//! explicitly instead of returning silent emptiness.

use anyhow::{anyhow, Result};
use pdfium_render::prelude::*;
use std::path::PathBuf;
use tracing::{error, info, warn};

#[cfg(target_os = "windows")]
const PDFIUM_LIB_NAME: &str = "pdfium.dll";
#[cfg(target_os = "macos")]
const PDFIUM_LIB_NAME: &str = "libpdfium.dylib";
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
const PDFIUM_LIB_NAME: &str = "libpdfium.so";

#[cfg(target_os = "windows")]
const PDFIUM_VENDOR_DIR: &str = "vendor/pdfium/win-x64";
#[cfg(target_os = "macos")]
const PDFIUM_VENDOR_DIR: &str = "vendor/pdfium/mac";
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
const PDFIUM_VENDOR_DIR: &str = "vendor/pdfium/linux-x64";

/// A page whose text layer averages fewer than this many non-whitespace
/// characters is considered image-only (scanned).
const SCANNED_PAGE_CHAR_THRESHOLD: usize = 24;
/// OCR page cap - a 500-page scan should not stall a chat request forever.
const MAX_OCR_PAGES: usize = 50;
/// Rasterization width for OCR (below OcrEngine::MaxImageDimension of 2600).
const OCR_RENDER_WIDTH: i32 = 1600;

/// One pdfium pass over a PDF, with no OCR in it.
///
/// Split out from OCR deliberately - see `extract_pdf_text_blocking`. Rendering
/// happens here (it needs pdfium); recognising the rendered pixels does not.
enum PdfPass {
    /// The native text layer was sufficient, already formatted with
    /// `[[page:N]]` provenance markers.
    Text(String),
    /// Image-only PDF. Pages were rendered to BMP by pdfium; each entry is
    /// either the bitmap or the reason that page could not be rendered. OCR is
    /// the CALLER's job, once pdfium is no longer alive.
    Scanned {
        page_count: usize,
        pages: Vec<Result<Vec<u8>, String>>,
    },
    /// A message to surface to the model verbatim (engine missing, file
    /// corrupt, engine crashed).
    Message(String),
}

/// Extract text from PDF bytes.
///
/// Runs on a blocking thread - PDFium is a synchronous C library and OCR can
/// take seconds on large scans.
pub async fn extract_pdf_text(bytes: Vec<u8>, filename: &str) -> Result<String> {
    let filename = filename.to_string();
    tokio::task::spawn_blocking(move || extract_pdf_text_blocking(&bytes, &filename))
        .await
        .map_err(|e| anyhow!("PDF extraction task failed: {}", e))?
}

/// # Why this is two phases
///
/// `pdfium-render`'s `thread_safe` feature takes a GLOBAL mutex when a `Pdfium`
/// object is created and holds it until that object drops - for the object's
/// whole lifetime, not per call (measured; a second `bind_pdfium` on another
/// thread blocks indefinitely while one is held). A Rust mutex POISONS if a
/// thread panics while holding it, and a poisoned mutex fails every later
/// caller. So a single panic anywhere inside a live `Pdfium` scope disables PDF
/// extraction for the rest of the process.
///
/// **OCR moved out of the pdfium scope.** It used to run inside the page loop,
/// so a crash in WinRT interop or in `bgra_to_bmp` would poison the PDF ENGINE -
/// two unrelated subsystems failing together for no structural reason. Phase 1
/// now only RENDERS; the `Pdfium` is dropped before a single pixel is
/// recognised. This is a genuine reduction in blast radius and is what the
/// two-phase shape below buys.
///
/// # The two mechanisms that keep one bad file from killing the engine
///
/// **The lock is ours, and it does not poison.** `pdfium-render`'s `thread_safe`
/// feature is disabled (see Cargo.toml); serialisation comes from
/// `extraction_scheduler::pdfium_permit`, which recovers a poisoned guard
/// instead of failing every later caller. This is what actually fixes the
/// permanent-outage failure, and it had to be done this way: `catch_unwind`
/// CANNOT prevent poisoning, because a `MutexGuard` poisons as it is dropped
/// during the unwind, deeper in the stack than any catch boundary. That was
/// measured, not assumed - with the crate's own mutex in place, a single
/// panicking test took five unrelated ones down with it.
///
/// **`catch_unwind` below still earns its place**, just not for that reason: it
/// turns a panic into an honest per-file error instead of tearing down the
/// blocking task and surfacing as a JoinError with no filename in it.
///
/// `a_panic_while_pdfium_is_live_does_not_disable_later_extraction` pins the
/// result - it panics with a `Pdfium` alive and then proves a normal extraction
/// still works. It runs as part of the ordinary suite; it could not before.
///
/// Deliberately NOT done: keeping one `Pdfium` alive for the process to save
/// re-initialising it. Measured at ~1.2 ms per bind - negligible beside
/// extraction - and holding it would keep that global mutex locked forever,
/// deadlocking every other binder (also measured: a second bind blocked past
/// 3 s while one was held).
fn extract_pdf_text_blocking(bytes: &[u8], filename: &str) -> Result<String> {
    // PHASE 1 - all pdfium work, isolated. Nothing that can panic independently
    // of pdfium belongs in here.
    let pass = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdfium_pass(bytes, filename)
    })) {
        Ok(pass) => pass,
        Err(_) => {
            // The panic was contained before it reached the mutex, so PDF
            // extraction still works for every other file - which is the whole
            // point of catching it here.
            error!(
                "PDF engine panicked while reading '{}' - this file is skipped; \
                 PDF extraction remains available for other files",
                filename
            );
            PdfPass::Message(format!(
                "[Cannot extract PDF '{}': the PDF engine failed on this file. \
                 Other documents are unaffected.]",
                filename
            ))
        }
    };

    // PHASE 2 - OCR, with pdfium already dropped.
    match pass {
        PdfPass::Text(text) => Ok(text),
        PdfPass::Message(message) => Ok(message),
        PdfPass::Scanned { page_count, pages } => {
            Ok(ocr_rendered_pages(filename, page_count, pages))
        }
    }
}

/// Everything that requires a live `Pdfium`, and nothing that does not.
fn pdfium_pass(bytes: &[u8], filename: &str) -> PdfPass {
    let pdfium = match bind_pdfium() {
        Ok(p) => p,
        Err(e) => {
            // Explicit failure marker in the prompt, full detail in the log.
            warn!("PDFium unavailable: {}", e);
            return PdfPass::Message(format!(
                "[Cannot extract PDF '{}': the PDF engine (pdfium) is not \
                 available on this installation: {}]",
                filename, e
            ));
        }
    };

    let document = match pdfium.load_pdf_from_byte_slice(bytes, None) {
        Ok(d) => d,
        Err(e) => {
            return PdfPass::Message(format!(
                "[Cannot extract PDF '{}': PDF could not be opened: {:?} (the file may be corrupt or password-protected)]",
                filename, e
            ))
        }
    };

    let page_count = document.pages().len() as usize;
    let mut page_texts: Vec<String> = Vec::with_capacity(page_count);
    for page in document.pages().iter() {
        match page.text() {
            Ok(text_page) => page_texts.push(text_page.all()),
            Err(_) => page_texts.push(String::new()),
        }
    }

    let total_chars: usize = page_texts
        .iter()
        .map(|t| t.chars().filter(|c| !c.is_whitespace()).count())
        .sum();

    // Scanned-document detection: image-only PDFs have (near-)empty text layers.
    if page_count > 0 && total_chars / page_count < SCANNED_PAGE_CHAR_THRESHOLD {
        info!(
            "PDF '{}' looks scanned ({} chars across {} pages) - rendering for OCR",
            filename, total_chars, page_count
        );
        return PdfPass::Scanned {
            page_count,
            pages: render_pages_for_ocr(&document, page_count),
        };
    }

    // Each page is tagged with a machine-parseable anchor (consumed by
    // doc_context::chunk_text to attribute chunks to a page range for
    // provenance) that also reads naturally to the model, so it can cite
    // "page 12" back to the user instead of an opaque excerpt.
    let joined = page_texts
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(idx, t)| format!("[[page:{}]]\n{}", idx + 1, t.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string();
    if joined.is_empty() {
        PdfPass::Message(format!("[PDF '{}' contains no extractable text.]", filename))
    } else {
        PdfPass::Text(joined)
    }
}

/// Render up to MAX_OCR_PAGES pages to BMP. Requires pdfium; produces no text.
#[cfg(target_os = "windows")]
fn render_pages_for_ocr(document: &PdfDocument, page_count: usize) -> Vec<Result<Vec<u8>, String>> {
    let render_config = PdfRenderConfig::new().set_target_width(OCR_RENDER_WIDTH);
    let pages_to_ocr = page_count.min(MAX_OCR_PAGES);
    let mut out = Vec::with_capacity(pages_to_ocr);

    for (idx, page) in document.pages().iter().enumerate() {
        if idx >= pages_to_ocr {
            break;
        }
        let rendered = page
            .render_with_config(&render_config)
            .map_err(|e| format!("render failed: {:?}", e))
            .and_then(|bitmap| {
                let width = bitmap.width() as i32;
                let height = bitmap.height() as i32;
                let raw = bitmap.as_raw_bytes();
                // PDFium hands back raw BGRA pixels, not an encoded image, so a
                // container is genuinely needed here - unlike the attached-image
                // path, this is not a decode/re-encode round-trip of a user
                // file, and BMP is lossless and unscaled. WIC parses BMP
                // natively, so recognize_image_bytes takes it unchanged.
                bgra_to_bmp(&raw, width, height).map_err(|e| e.to_string())
            });
        out.push(rendered);
    }
    out
}

#[cfg(not(target_os = "windows"))]
fn render_pages_for_ocr(_document: &PdfDocument, _page_count: usize) -> Vec<Result<Vec<u8>, String>> {
    Vec::new()
}

/// OCR pages that pdfium already rendered. Runs with NO `Pdfium` alive, so a
/// failure in the OCR stack cannot affect the PDF engine.
///
/// Output format is byte-identical to the previous inline implementation -
/// `extraction_outcome` classifies scanned-PDF results by the exact shape of the
/// header plus body, and `doc_context::chunk_text` parses the `[[page:N]]`
/// markers, so both depend on this text not drifting.
#[cfg(target_os = "windows")]
fn ocr_rendered_pages(
    filename: &str,
    page_count: usize,
    pages: Vec<Result<Vec<u8>, String>>,
) -> String {
    let pages_to_ocr = pages.len();
    let mut out = String::new();
    let mut ocr_errors = 0usize;

    for (idx, page) in pages.into_iter().enumerate() {
        let recognized = page.and_then(|bmp| {
            crate::utils::win_ocr::recognize_image_bytes(&bmp, &format!("{} p.{}", filename, idx + 1))
                .map_err(|e| e.to_string())
        });
        match recognized {
            Ok(text) => {
                out.push_str(&format!("\n[[page:{}]] (OCR)\n{}", idx + 1, text));
            }
            Err(e) => {
                ocr_errors += 1;
                warn!("OCR failed on page {} of '{}': {}", idx + 1, filename, e);
                out.push_str(&format!("\n[[page:{}]]\n[OCR failed: {}]\n", idx + 1, e));
            }
        }
    }

    let mut header = format!(
        "[Scanned PDF '{}': no text layer found; text below was recovered via \
         OCR ({} of {} pages processed{}).]\n",
        filename,
        pages_to_ocr,
        page_count,
        if ocr_errors > 0 {
            format!(", {} pages failed", ocr_errors)
        } else {
            String::new()
        }
    );
    if out.trim().is_empty() {
        header = format!(
            "[Scanned PDF '{}': no text layer found and OCR recovered no text. \
             The scan quality may be too low.]",
            filename
        );
        return header;
    }
    header.push_str(&out);
    header
}

#[cfg(not(target_os = "windows"))]
fn ocr_rendered_pages(
    filename: &str,
    page_count: usize,
    _pages: Vec<Result<Vec<u8>, String>>,
) -> String {
    format!(
        "[Scanned PDF '{}' ({} pages): this file has no text layer (it is a \
         scan/image-only PDF) and OCR is not available on this platform yet. \
         Please provide a text-based copy.]",
        filename, page_count
    )
}

/// Wrap raw BGRA8 pixels in a minimal 32bpp BMP container (top-down via
/// negative height) so Windows BitmapDecoder can consume it without an
/// image-encoding dependency.
#[cfg(target_os = "windows")]
fn bgra_to_bmp(pixels: &[u8], width: i32, height: i32) -> Result<Vec<u8>> {
    let expected = (width as usize) * (height as usize) * 4;
    if pixels.len() < expected {
        return Err(anyhow!(
            "pixel buffer too small: {} < {}",
            pixels.len(),
            expected
        ));
    }
    let data_size = expected as u32;
    let file_size = 54u32 + data_size;

    let mut bmp = Vec::with_capacity(file_size as usize);
    // BITMAPFILEHEADER
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    // BITMAPINFOHEADER
    bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
    bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&(-height).to_le_bytes()); // negative = top-down
    bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
    bmp.extend_from_slice(&32u16.to_le_bytes()); // bpp
    bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    bmp.extend_from_slice(&data_size.to_le_bytes());
    bmp.extend_from_slice(&2835u32.to_le_bytes()); // 72 DPI
    bmp.extend_from_slice(&2835u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // palette
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&pixels[..expected]);
    Ok(bmp)
}

/// Resolve and bind the PDFium library. Candidates are checked in a fixed,
/// A bound pdfium instance together with the process-wide permit that makes
/// using it safe.
///
/// PDFium is not thread-safe, and `pdfium-render`'s own serialisation is
/// disabled (see the dependency comment in Cargo.toml) because its mutex
/// poisons. This type is the replacement, and the shape matters: the permit and
/// the instance are acquired together and dropped together, so there is no way
/// to obtain a `Pdfium` without also holding the lock. A free function returning
/// a bare `Pdfium` alongside a separate `pdfium_permit()` call would work
/// exactly as well right up until someone forgot the second call.
///
/// Derefs to `Pdfium`, so callers use it unchanged.
pub(crate) struct PdfiumSession {
    // Field order is load-bearing: Rust drops fields in declaration order, so
    // the instance is destroyed BEFORE the permit is released. Releasing first
    // would let the next waiter start while FPDF_DestroyLibrary was still
    // running.
    pdfium: Pdfium,
    _permit: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for PdfiumSession {
    type Target = Pdfium;
    fn deref(&self) -> &Pdfium {
        &self.pdfium
    }
}

/// documented order; a miss on all of them is a named error, never a silent
/// degradation.
pub(crate) fn bind_pdfium() -> Result<PdfiumSession> {
    // Taken BEFORE the library search so that two callers cannot both be
    // initialising pdfium at once. Non-poisoning: a previous holder panicking
    // must not disable PDF extraction for the rest of the process, which is the
    // whole reason this lock is ours and not pdfium-render's.
    let permit = crate::utils::extraction_scheduler::pdfium_permit();

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(env_path) = std::env::var("PDFIUM_PATH") {
        let p = PathBuf::from(&env_path);
        if p.is_dir() {
            candidates.push(p.join(PDFIUM_LIB_NAME));
        } else {
            candidates.push(p);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join(PDFIUM_LIB_NAME));
            candidates.push(exe_dir.join("resources").join(PDFIUM_LIB_NAME));
        }
        // Development: walk up from target/{debug,release}[/deps] to the
        // workspace root and look in vendor/.
        for ancestor in exe.ancestors().take(6) {
            candidates.push(ancestor.join(PDFIUM_VENDOR_DIR).join(PDFIUM_LIB_NAME));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(PDFIUM_VENDOR_DIR).join(PDFIUM_LIB_NAME));
    }

    for candidate in &candidates {
        if candidate.exists() {
            match Pdfium::bind_to_library(candidate) {
                Ok(bindings) => {
                    return Ok(PdfiumSession {
                        pdfium: Pdfium::new(bindings),
                        _permit: permit,
                    });
                }
                Err(e) => {
                    warn!("Found pdfium at {:?} but binding failed: {:?}", candidate, e);
                }
            }
        }
    }

    Err(anyhow!(
        "pdfium library ({}) not found; searched: {}",
        PDFIUM_LIB_NAME,
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only when the vendored pdfium.dll is present (dev machines).
    #[tokio::test]
    async fn pdfium_extracts_text_from_generated_pdf() {
        if bind_pdfium().is_err() {
            eprintln!("SKIP: pdfium library not available on this machine");
            return;
        }
        // Minimal valid single-page PDF with the text "Hello Legal World"
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 24 Tf 72 700 Td (Hello Legal World) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let text = extract_pdf_text(pdf.to_vec(), "test.pdf").await.unwrap();
        assert!(
            text.contains("Hello Legal World"),
            "pdfium should extract the text layer, got: {}",
            text
        );
    }

    /// Full OCR chain proof: pdfium renders a page with large text, the raw
    /// BGRA pixels are wrapped in our hand-built BMP, decoded by WinRT, and
    /// read by the Windows OCR engine. Exercises every link that the
    /// scanned-PDF path uses in production.
    ///
    /// Now driven through the two-phase split - render under pdfium, drop it,
    /// then OCR - which is the production shape.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_ocr_reads_rendered_page_end_to_end() {
        if bind_pdfium().is_err() {
            eprintln!("SKIP: pdfium library not available");
            return;
        }
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 48 Tf 72 600 Td (HELLO OCR WORLD) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let result = tokio::task::spawn_blocking(move || {
            // Phase 1: pdfium renders, then is dropped at the end of this block.
            let pages = {
                let pdfium = bind_pdfium().unwrap();
                let doc = pdfium.load_pdf_from_byte_slice(pdf, None).unwrap();
                render_pages_for_ocr(&doc, 1)
            };
            // Phase 2: OCR with no Pdfium alive.
            ocr_rendered_pages("ocr-test.pdf", 1, pages)
        })
        .await
        .unwrap();
        let upper = result.to_uppercase();
        assert!(
            upper.contains("HELLO") && upper.contains("WORLD"),
            "OCR should read the rendered text, got: {}",
            result
        );
    }

    /// The containment guarantee: a panic while pdfium holds its global mutex
    /// must NOT disable PDF extraction for the rest of the process.
    ///
    /// Without `catch_unwind`, the panic unwinds past `pdfium-render`'s global
    /// mutex and POISONS it - after which every later `Pdfium::new` returns Err
    /// and PDF extraction is dead until restart. That is not hypothetical: it is
    /// exactly what happened during an earlier audit, where one panicking test
    /// took out five unrelated ones.
    ///
    /// Simulated the same way it occurs in production - a panic raised while a
    /// `Pdfium` is alive - and then proving a normal extraction still works.
    /// One bad file must not disable PDF extraction for the rest of the session.
    ///
    /// This is the regression guard for the failure that motivated disabling
    /// `pdfium-render`'s `thread_safe` feature. With the crate's own global
    /// mutex in place this test COULD NOT PASS, and worse, running it poisoned
    /// pdfium process-wide and failed five unrelated tests - which is exactly
    /// what production did after any panic in a live pdfium scope.
    ///
    /// It is also the proof that `catch_unwind` alone was never sufficient: a
    /// `MutexGuard` poisons as it is dropped DURING the unwind, below any catch
    /// boundary. Only owning a non-poisoning lock fixes it.
    ///
    /// That it now runs in the ordinary suite, alongside the other pdfium tests
    /// in the same process, is the assertion - if the crate's mutex ever comes
    /// back, this test and its neighbours fail together again.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn a_panic_while_pdfium_is_live_does_not_disable_later_extraction() {
        if bind_pdfium().is_err() {
            eprintln!("SKIP: pdfium library not available");
            return;
        }

        let panicked = tokio::task::spawn_blocking(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _pdfium = bind_pdfium().unwrap();
                panic!("simulated crash while the PDF engine is live");
            }))
        })
        .await
        .unwrap();
        assert!(panicked.is_err(), "the closure was expected to panic");

        // The real assertion: PDF extraction still works afterwards.
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 48 Tf 72 700 Td (STILL WORKING AFTER PANIC) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let text = extract_pdf_text(pdf.to_vec(), "after-panic.pdf")
            .await
            .expect("extraction must still succeed after a contained panic");
        assert!(
            !text.contains("not available on this installation"),
            "the PDF engine was disabled by the earlier panic: {}",
            text
        );
        assert!(
            text.to_uppercase().contains("STILL WORKING"),
            "expected real extracted text, got: {}",
            text
        );
    }
}
