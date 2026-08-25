//! Document text extraction - accuracy-first.
//!
//! Contract (matches the product's no-fallbacks philosophy): every input
//! either yields faithful text or an EXPLICIT bracketed failure marker that
//! names the file and the reason. Nothing is ever silently garbled:
//!   - Office XML formats are parsed with a real XML parser (quick-xml) with
//!     entity decoding, tab/break/table handling - never regex over XML.
//!   - Legacy binary Office formats (.doc/.ppt, OLE2/CFB containers) are
//!     rejected loudly with a "save as .docx/.pptx" instruction instead of a
//!     doomed parse attempt.
//!   - Text files go through encoding detection (UTF-8 first, then chardetng
//!     + encoding_rs) instead of from_utf8_lossy mangling.
//!   - PDF extraction is delegated to PDFium (utils::pdf_text), with OCR for
//!     scanned pages on Windows.

use std::io::{Cursor, Read};
use std::path::Path;
use tracing::debug;
use anyhow::Result;

/// OLE2 / Compound File Binary signature (legacy .doc/.xls/.ppt containers).
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

fn is_cfb(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == CFB_MAGIC
}

fn is_zip(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == b"PK\x03\x04"
}

/// A bracket-prefixed result from this module is NOT uniformly a failure.
/// Two shapes exist, and conflating them is a real bug that has bitten this
/// codebase before (it discarded successfully OCR'd content from scanned
/// PDFs and from image attachments, since those success messages ALSO start
/// with '['):
///   - A bare one-line marker with no trailing content, e.g.
///     "[Cannot extract 'X': reason]" or "[PDF 'X' contains no extractable
///     text.]" - genuinely nothing usable. -> "failed".
///   - A marker line followed by real recovered content, e.g.
///     "[Scanned PDF 'X': ... recovered via OCR (2 of 2 pages).]\n<text>" -
///     the header is honest provenance, not an error; the model should get
///     the text. -> "ok", full string (header included) kept as-is.
/// Distinguished by whether anything substantial follows the first "]\n" - a
/// general rule, not a hardcoded match on "OCR" specifically, so any future
/// header+body message from this module is handled correctly too.
///
/// This lives here, next to the extractors whose output contract it encodes,
/// because EVERY caller of extract_content_from_bytes must classify the
/// result identically. Three divergent private copies of this logic existed
/// previously (api::stream_api had the correct one; api::files_api and
/// api::documents_api had a naive `starts_with('[') == failure` version),
/// which meant a scanned PDF or an image uploaded to Local Storage was
/// permanently recorded as "failed" with empty text while the very same file
/// attached via paperclip extracted fine.
pub fn extraction_outcome(
    result: Result<String>,
) -> (String, &'static str, Option<String>) {
    const MIN_TRAILING_CONTENT_CHARS: usize = 20;

    match result {
        Ok(text) if !text.starts_with('[') => (text, "ok", None),
        Ok(text) => match text.find("]\n") {
            Some(idx) if text[idx + 2..].trim().chars().count() > MIN_TRAILING_CONTENT_CHARS => {
                (text, "ok", None)
            }
            _ => (String::new(), "failed", Some(text)),
        },
        Err(e) => (String::new(), "failed", Some(e.to_string())),
    }
}

/// The file types this product accepts, as a PRODUCT policy - deliberately
/// narrower than what `extract_content_from_bytes` below is technically
/// capable of parsing.
///
/// That gap is intentional and worth stating plainly, because the obvious
/// "improvement" is to widen this list to match the extractor. The extractor
/// handles RTF, ODT, HTML, CSV, Markdown and ~25 source-code extensions, and
/// falls back to a text decode for anything unrecognised - so without this
/// gate a user could store a `.zip` and get mojibake presented as document
/// content. Every type listed here is one the product commits to reading
/// properly via a real engine (pdfium, Windows OCR, calamine, quick-xml).
///
/// This is the ONLY definition of that policy on the Rust side. Its
/// TypeScript counterpart is `apps/desktop/src/supportedFormats.ts`; the two
/// must be changed together, and `supported_formats_match_the_frontend_list`
/// in this module's tests pins them so a one-sided edit fails the build.
///
/// Note on `doc` and `ppt`: accepted here because a user picking "Word" or
/// "PowerPoint" reasonably expects them, but the LEGACY BINARY (pre-2007 CFB)
/// variants are refused with an explicit message by extract_word /
/// extract_presentation rather than silently yielding garbage. Accepting the
/// extension and then failing loudly is the honest behaviour: the alternative
/// is a file picker that appears to not see the user's file at all.
pub const SUPPORTED_ATTACHMENT_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "png", "jpg", "jpeg",
];

/// Whether `filename`'s extension is one the product accepts.
///
/// Case-insensitive. A filename with no extension is rejected: there is no
/// reliable way to route it to an engine, and the text-decode fallback would
/// turn an arbitrary binary into mojibake.
pub fn is_supported_attachment(filename: &str) -> bool {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() || filename.starts_with('.') => {
            let ext = ext.to_lowercase();
            SUPPORTED_ATTACHMENT_EXTENSIONS.contains(&ext.as_str())
        }
        _ => false,
    }
}

/// Human-readable list for error messages shown to the user, e.g.
/// "PDF, DOC, DOCX, ...". Derived from the constant so a message can never
/// drift from the policy it describes.
pub fn supported_attachment_list() -> String {
    SUPPORTED_ATTACHMENT_EXTENSIONS
        .iter()
        .map(|e| e.to_uppercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Extract text content from a file on disk (routes by extension).
pub async fn extract_file_content(file_path: &Path) -> Result<String> {
    let bytes = std::fs::read(file_path)?;
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    extract_content_from_bytes(&bytes, filename).await
}

/// Extract text from in-memory bytes, routed by the filename's extension.
pub async fn extract_content_from_bytes(bytes: &[u8], filename: &str) -> Result<String> {
    // Lane discipline, enforced at the one point every extraction passes
    // through. Files of DIFFERENT formats proceed in parallel; files of the SAME
    // format wait for each other, because their engines cannot usefully overlap
    // (pdfium serialises on a global mutex regardless, so three concurrent PDFs
    // were only ever three blocking threads queued on one lock).
    //
    // Placed here rather than at each caller deliberately: there are six call
    // sites across documents_api, files_api and stream_api, and a seventh added
    // later would silently bypass a per-caller permit. Safe against
    // self-deadlock because no extractor re-enters this function - DOCX embedded
    // images go straight to image_ocr, and scanned PDF pages straight to
    // win_ocr, neither of which routes back through here.
    let _lane = crate::utils::extraction_scheduler::global()
        .acquire(filename)
        .await;

    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();

    match ext.as_str() {
        // Text / code files - encoding-detected decode
        "txt" | "md" | "json" | "yaml" | "yml" | "xml" | "csv" | "log" |
        "js" | "ts" | "jsx" | "tsx" | "py" | "java" | "cpp" | "c" | "cs" |
        "scss" | "go" | "rs" | "php" | "rb" | "swift" |
        "kt" | "scala" | "sql" | "sh" | "bat" | "ps1" | "dockerfile" | "env" => {
            Ok(decode_text_bytes(bytes))
        }
        // HTML - strip markup, keep readable text
        "html" | "htm" => Ok(extract_html(bytes)),
        // RTF - control-word aware extraction
        "rtf" => Ok(extract_rtf(bytes)),
        // PDF - PDFium engine (+ OCR for scanned pages on Windows)
        "pdf" => crate::utils::pdf_text::extract_pdf_text(bytes.to_vec(), filename).await,
        // Image files - documents/handwritten notes that exist only as a
        // photo or scan. Routed by utils::vision_extraction: the ACTIVE
        // VISION model (reads handwriting) when one is loaded, else Windows
        // OCR (printed text). Deterministic branch; whichever engine ran is
        // named in the stored provenance header.
        "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "gif" =>
            crate::utils::vision_extraction::extract_image_text(bytes.to_vec(), filename).await,
        // Word documents. Runs on a blocking thread - embedded images (if
        // any) are OCR'd synchronously in place, and OCR can take real
        // seconds, same reasoning as the PDF/image paths above.
        "docx" | "doc" => {
            let bytes = bytes.to_vec();
            let filename = filename.to_string();
            tokio::task::spawn_blocking(move || extract_word(&bytes, &filename))
                .await
                .map_err(|e| anyhow::anyhow!("DOCX extraction task failed: {}", e))
        }
        // Spreadsheets (auto-detects xlsx/xls/ods from content)
        "xls" | "xlsx" | "ods" => Ok(extract_spreadsheet(bytes, filename)),
        // Presentations
        "pptx" | "ppt" => Ok(extract_presentation(bytes, filename)),
        // OpenDocument text
        "odt" => Ok(extract_odt(bytes, filename)),
        // Unknown - attempt encoding-detected text decode
        _ => {
            debug!("Unknown file type {}, attempting text extraction", ext);
            Ok(decode_text_bytes(bytes))
        }
    }
}

// ---------------------------------------------------------------------------
// Text decoding with encoding detection
// ---------------------------------------------------------------------------

/// Decode text bytes: valid UTF-8 is used as-is; anything else goes through
/// chardetng detection + encoding_rs decode (fixes Windows-1252 legal docs
/// that from_utf8_lossy used to mangle into replacement characters).
pub fn decode_text_bytes(bytes: &[u8]) -> String {
    // Strip UTF-8 BOM if present
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);

    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }

    // UTF-16 BOMs
    if bytes.len() >= 2 && (bytes[..2] == [0xFF, 0xFE] || bytes[..2] == [0xFE, 0xFF]) {
        let enc = if bytes[0] == 0xFF { encoding_rs::UTF_16LE } else { encoding_rs::UTF_16BE };
        let (text, _, _) = enc.decode(bytes);
        return text.into_owned();
    }

    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    let (text, actual_encoding, had_errors) = encoding.decode(bytes);
    debug!(
        "Non-UTF-8 text decoded as {} (decode errors: {})",
        actual_encoding.name(),
        had_errors
    );
    text.into_owned()
}

// ---------------------------------------------------------------------------
// DOCX (quick-xml, entity-decoded, tabs/breaks/tables/headers/footnotes)
// ---------------------------------------------------------------------------

/// Upper bound on embedded images OCR'd per document - an OCR call is the
/// only genuinely expensive step here (can take real seconds each), so a
/// document with a pathological number of embedded pictures can't stall a
/// single chat request indefinitely. Mirrors utils::pdf_text's MAX_OCR_PAGES
/// bound in spirit: images beyond the cap get an explicit, named marker
/// (never silently dropped), not OCR'd.
const MAX_DOCX_IMAGES_OCR: usize = 30;

fn extract_word(bytes: &[u8], filename: &str) -> String {
    if is_cfb(bytes) {
        return format!(
            "[Cannot extract '{}': this is a legacy binary Word file (.doc). \
             Please open it in Word and save it as .docx, then attach again.]",
            filename
        );
    }
    if !is_zip(bytes) {
        return format!(
            "[Cannot extract '{}': not a valid Word document (unrecognized container).]",
            filename
        );
    }

    let cursor = Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return format!("[Cannot extract '{}': corrupt archive: {}]", filename, e),
    };

    // Resolve r:embed relationship ids -> word/media/* paths, then pull only
    // the image bytes actually referenced, once, up front - avoids touching
    // the zip archive mid-XML-walk (which would need a second mutable
    // borrow) and avoids reading unreferenced media.
    let rels = read_zip_entry(&mut archive, "word/_rels/document.xml.rels")
        .map(|xml| parse_relationships(&xml))
        .unwrap_or_default();
    let mut images: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for (rid, target) in &rels {
        if target.contains("media/") {
            let path = format!("word/{}", target.trim_start_matches('/'));
            if let Some(bytes) = read_zip_entry_bytes(&mut archive, &path) {
                images.insert(rid.clone(), bytes);
            }
        }
    }

    let mut out = String::new();
    let mut image_count = 0usize;

    // Main body first
    match read_zip_entry(&mut archive, "word/document.xml") {
        Some(xml) => out.push_str(&wordml_to_text(&xml, &images, &mut image_count)),
        None => {
            return format!(
                "[Cannot extract '{}': word/document.xml missing from the archive.]",
                filename
            )
        }
    }

    // Headers, footers, footnotes, endnotes - labeled so provenance is clear
    let extra_parts: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            (n.starts_with("word/header") || n.starts_with("word/footer")
                || n == "word/footnotes.xml" || n == "word/endnotes.xml")
                && n.ends_with(".xml")
        })
        .collect();
    for part in extra_parts {
        if let Some(xml) = read_zip_entry(&mut archive, &part) {
            let text = wordml_to_text(&xml, &images, &mut image_count);
            if !text.trim().is_empty() {
                let label = part
                    .trim_start_matches("word/")
                    .trim_end_matches(".xml");
                out.push_str(&format!("\n[{}]\n{}", label, text));
            }
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        format!("[Word document '{}' contains no extractable text.]", filename)
    } else {
        trimmed.to_string()
    }
}

/// Convert WordprocessingML to plain text with a real XML parser.
/// Handles: w:t runs (entity-decoded), w:tab, w:br/w:cr, paragraph breaks,
/// table cell (tab-separated) and row (newline) boundaries, and embedded
/// images (a:blip r:embed="rIdN") - OCR'd in place via the same Windows OCR
/// engine used for scanned PDFs/direct image attachments, inserted as its
/// own citable [[image:N]] block at the exact position it appears in the
/// document. `images` maps relationship id -> raw media bytes (pre-resolved
/// by extract_word); `image_count` is a running counter shared across every
/// part of the document (body, headers, footers, ...) so image numbering is
/// document-wide, not reset per part.
fn wordml_to_text(
    xml: &str,
    images: &std::collections::HashMap<String, Vec<u8>>,
    image_count: &mut usize,
) -> String {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text_run = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = true,
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                b"blip" => append_embedded_image(&mut out, e, images, image_count),
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                b"blip" => append_embedded_image(&mut out, e, images, image_count),
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = false,
                // Real paragraph boundary: "\n\n" so utils::doc_context's
                // chunker (which splits on blank lines) sees each Word
                // paragraph as its own unit, distinct from a forced w:br/w:cr
                // line break within the same paragraph (single '\n' above).
                b"p" => out.push_str("\n\n"),
                // Table cell boundary -> tab, row boundary -> newline. The
                // paragraph inside the cell already emitted "\n\n"; convert
                // the trailing newlines to a cell separator instead.
                b"tc" => {
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push('\t');
                }
                b"tr" => {
                    while out.ends_with('\t') {
                        out.pop();
                    }
                    out.push('\n');
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_text_run {
                    if let Ok(text) = e.unescape() {
                        out.push_str(&text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // best-effort on malformed XML; what was read stands
            _ => {}
        }
    }

    out
}

/// Handle one `<a:blip r:embed="rIdN">` element: resolve to its media bytes,
/// OCR them (subject to MAX_DOCX_IMAGES_OCR), and insert the result as its
/// own [[image:N]]-anchored block, isolated from surrounding paragraph text
/// on both sides. Every embedded image is processed unconditionally - no
/// size/decorative heuristic - and a failure (missing relationship, OCR
/// error, platform without OCR, or the per-document cap) always produces an
/// explicit, named marker, never silent omission.
fn append_embedded_image(
    out: &mut String,
    blip: &quick_xml::events::BytesStart,
    images: &std::collections::HashMap<String, Vec<u8>>,
    image_count: &mut usize,
) {
    let rid = blip
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == b"embed")
        .map(|a| String::from_utf8_lossy(&a.value).into_owned());
    let Some(rid) = rid else { return };

    *image_count += 1;
    let n = *image_count;
    let label = format!("embedded image {}", n);

    let body = if n > MAX_DOCX_IMAGES_OCR {
        format!(
            "[Image {} not OCR'd: this document's embedded-image limit ({}) was reached.]",
            n, MAX_DOCX_IMAGES_OCR
        )
    } else {
        match images.get(&rid) {
            Some(bytes) => crate::utils::image_ocr::ocr_image_bytes_blocking(bytes, &label),
            None => format!(
                "[Image {} not OCR'd: no matching media file found for relationship '{}'.]",
                n, rid
            ),
        }
    };

    if !out.ends_with("\n\n") {
        while out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\n\n");
    }
    // [[clear:x]] ends the image's one-shot location: without it, text that
    // follows in the same paragraph flow (e.g. a caption run appended after
    // the drawing in the same w:p, or a later paragraph with no marker of
    // its own) would inherit "Image N" as its citation via
    // utils::doc_context's persistent-region logic, which is correct for a
    // slide/sheet but wrong for a single OCR'd image block.
    out.push_str(&format!("[[image:{}]]\n{}\n\n[[clear:x]]\n\n", n, body.trim_end()));
}

fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;
    Some(content)
}

/// Binary counterpart of read_zip_entry, for embedded media (images) rather
/// than XML/text parts.
fn read_zip_entry_bytes<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut file = archive.by_name(name).ok()?;
    let mut content = Vec::new();
    file.read_to_end(&mut content).ok()?;
    Some(content)
}

/// Parse an OPC relationships part (e.g. word/_rels/document.xml.rels) into
/// an Id -> Target map. Used to resolve a:blip r:embed="rIdN" references to
/// the actual media file inside word/media/.
fn parse_relationships(xml: &str) -> std::collections::HashMap<String, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut map = std::collections::HashMap::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) if e.local_name().as_ref() == b"Relationship" => {
                let mut id = None;
                let mut target = None;
                for attr in e.attributes().flatten() {
                    match attr.key.local_name().as_ref() {
                        b"Id" => id = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        b"Target" => target = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        _ => {}
                    }
                }
                if let (Some(id), Some(target)) = (id, target) {
                    map.insert(id, target);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Spreadsheets (calamine auto-detect: xlsx / legacy xls / ods, dates rendered)
// ---------------------------------------------------------------------------

fn extract_spreadsheet(bytes: &[u8], filename: &str) -> String {
    use calamine::{open_workbook_auto_from_rs, Data, Reader};

    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook = match open_workbook_auto_from_rs(cursor) {
        Ok(wb) => wb,
        Err(e) => {
            return format!(
                "[Cannot extract spreadsheet '{}': {}]",
                filename, e
            )
        }
    };

    let mut out = String::new();
    for sheet_name in workbook.sheet_names().to_vec() {
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            // [[sheet:Name]] is parsed by utils::doc_context::chunk_text to
            // anchor chunks to a sheet for citation (e.g. "Sheet: Q1 Data").
            // Rows are separated by a real blank line ("\n\n", not "\n") so
            // the chunker can group a manageable run of rows per chunk
            // instead of dumping an entire large sheet into one blob.
            out.push_str(&format!("\n\n[[sheet:{}]]\n", sheet_name));
            let mut first_row = true;
            for row in range.rows() {
                let cells: Vec<String> = row
                    .iter()
                    .map(|c| match c {
                        // Render Excel serial dates as readable timestamps
                        Data::DateTime(dt) => dt
                            .as_datetime()
                            .map(|d| {
                                if d.time() == chrono::NaiveTime::MIN {
                                    d.date().format("%Y-%m-%d").to_string()
                                } else {
                                    d.format("%Y-%m-%d %H:%M:%S").to_string()
                                }
                            })
                            .unwrap_or_else(|| c.to_string()),
                        _ => c.to_string(),
                    })
                    .collect();
                if !first_row {
                    out.push_str("\n\n");
                }
                first_row = false;
                out.push_str(&cells.join("\t"));
            }
        }
    }

    if out.trim().is_empty() {
        format!("[Spreadsheet '{}' appears to be empty.]", filename)
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// PPTX (quick-xml, slides in order + speaker notes; legacy .ppt rejected)
// ---------------------------------------------------------------------------

fn extract_presentation(bytes: &[u8], filename: &str) -> String {
    if is_cfb(bytes) {
        return format!(
            "[Cannot extract '{}': this is a legacy binary PowerPoint file (.ppt). \
             Please save it as .pptx and attach again.]",
            filename
        );
    }
    if !is_zip(bytes) {
        return format!(
            "[Cannot extract '{}': not a valid PowerPoint file (unrecognized container).]",
            filename
        );
    }

    let cursor = Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return format!("[Cannot extract '{}': corrupt archive: {}]", filename, e),
    };

    let mut out = String::new();
    let mut slide_num = 1;
    loop {
        let slide_path = format!("ppt/slides/slide{}.xml", slide_num);
        let Some(xml) = read_zip_entry(&mut archive, &slide_path) else {
            break;
        };
        // [[slide:N]] is parsed by utils::doc_context::chunk_text to anchor
        // chunks to a slide number for citation (e.g. "Slide 3").
        out.push_str(&format!("\n\n[[slide:{}]]\n", slide_num));
        out.push_str(&drawingml_to_text(&xml));

        // Speaker notes, when present
        let notes_path = format!("ppt/notesSlides/notesSlide{}.xml", slide_num);
        if let Some(notes_xml) = read_zip_entry(&mut archive, &notes_path) {
            let notes = drawingml_to_text(&notes_xml);
            if !notes.trim().is_empty() {
                out.push_str(&format!("\n\n[Notes]\n{}", notes));
            }
        }
        slide_num += 1;
    }

    if out.trim().is_empty() {
        format!("[Presentation '{}' contains no extractable text.]", filename)
    } else {
        out
    }
}

/// Convert DrawingML (a: namespace) to text: a:t runs entity-decoded,
/// paragraph breaks on a:p, explicit breaks on a:br.
fn drawingml_to_text(xml: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text_run = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = true,
                b"br" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(ref e)) => {
                if e.local_name().as_ref() == b"br" {
                    out.push('\n');
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = false,
                // Real paragraph/bullet boundary: "\n\n" (see wordml_to_text
                // for why this must differ from the forced-break '\n' above).
                b"p" => out.push_str("\n\n"),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_text_run {
                    if let Ok(text) = e.unescape() {
                        out.push_str(&text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    out
}

// ---------------------------------------------------------------------------
// ODT (quick-xml over content.xml)
// ---------------------------------------------------------------------------

fn extract_odt(bytes: &[u8], filename: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    if !is_zip(bytes) {
        return format!(
            "[Cannot extract '{}': not a valid OpenDocument file.]",
            filename
        );
    }
    let cursor = Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return format!("[Cannot extract '{}': corrupt archive: {}]", filename, e),
    };
    let Some(xml) = read_zip_entry(&mut archive, "content.xml") else {
        return format!("[Cannot extract '{}': content.xml missing.]", filename);
    };

    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_body = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name().as_ref().to_vec();
                if name == b"office:body" {
                    in_body = true;
                } else if in_body && name == b"text:tab" {
                    out.push('\t');
                } else if in_body && name == b"text:line-break" {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(ref e)) => {
                let qname = e.name();
                let name = qname.as_ref();
                if in_body && name == b"text:tab" {
                    out.push('\t');
                } else if in_body && name == b"text:line-break" {
                    out.push('\n');
                }
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let name = qname.as_ref();
                if name == b"office:body" {
                    in_body = false;
                } else if in_body && (name == b"text:p" || name == b"text:h") {
                    // Real paragraph/heading boundary: "\n\n" (distinct from
                    // the single '\n' pushed by an explicit text:line-break).
                    out.push_str("\n\n");
                }
            }
            Ok(Event::Text(e)) => {
                if in_body {
                    if let Ok(text) = e.unescape() {
                        out.push_str(&text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        format!("[ODT file '{}' contains no extractable text.]", filename)
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// HTML (html5ever-backed readable-text conversion)
// ---------------------------------------------------------------------------

fn extract_html(bytes: &[u8]) -> String {
    let text = html2text::from_read(bytes, 100);
    if text.trim().is_empty() {
        "[HTML file contains no extractable text.]".to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// RTF (control-word aware; \par, \tab, \'hex, \uN handled; destination
// groups like fonttbl/pict skipped)
// ---------------------------------------------------------------------------

fn extract_rtf(bytes: &[u8]) -> String {
    let src = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let mut skip_depth: Option<usize> = None; // group depth at which skipping started
    let mut depth = 0usize;
    // \ucN: number of fallback chars following \uN to swallow (default 1)
    let mut uc_skip = 1usize;
    let mut pending_uc_skip = 0usize;

    const SKIP_DESTINATIONS: &[&str] = &[
        "fonttbl", "colortbl", "stylesheet", "info", "pict", "object",
        "themedata", "colorschememapping", "datastore", "latentstyles",
        "listtable", "listoverridetable", "rsidtbl", "generator", "xmlnstbl",
    ];

    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                if let Some(d) = skip_depth {
                    if depth == d {
                        skip_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                let next = chars[i];
                if next == '\'' {
                    // \'hh - single byte in the document codepage (cp1252 assumed)
                    if i + 2 < chars.len() {
                        let hex: String = chars[i + 1..=i + 2].iter().collect();
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            if skip_depth.is_none() {
                                if pending_uc_skip > 0 {
                                    pending_uc_skip -= 1;
                                } else {
                                    let byte_arr = [byte];
                                    let (s, _, _) = encoding_rs::WINDOWS_1252.decode(&byte_arr);
                                    out.push_str(&s);
                                }
                            }
                        }
                        i += 3;
                    } else {
                        break;
                    }
                } else if next == '*' {
                    // \* introduces an ignorable destination group - skip it
                    if skip_depth.is_none() {
                        skip_depth = Some(depth);
                    }
                    i += 1;
                } else if next.is_ascii_alphabetic() {
                    // Control word: letters then optional numeric parameter
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let mut param = String::new();
                    if i < chars.len() && (chars[i] == '-' || chars[i].is_ascii_digit()) {
                        param.push(chars[i]);
                        i += 1;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            param.push(chars[i]);
                            i += 1;
                        }
                    }
                    // A single space after a control word is part of it
                    if i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }

                    if skip_depth.is_none() {
                        match word.as_str() {
                            // Real paragraph/section/page breaks: "\n\n" (so
                            // utils::doc_context's chunker treats each as a
                            // distinct unit), vs. "\line" - a soft break
                            // within the same paragraph - which stays '\n'.
                            "par" | "sect" | "page" => out.push_str("\n\n"),
                            "line" => out.push('\n'),
                            "tab" => out.push('\t'),
                            "cell" => out.push('\t'),
                            "row" => out.push('\n'),
                            "emdash" => out.push('-'),
                            "endash" => out.push('-'),
                            "lquote" | "rquote" => out.push('\''),
                            "ldblquote" | "rdblquote" => out.push('"'),
                            "uc" => {
                                uc_skip = param.parse().unwrap_or(1);
                            }
                            "u" => {
                                if let Ok(mut code) = param.parse::<i32>() {
                                    if code < 0 {
                                        code += 65536;
                                    }
                                    if let Some(ch) = char::from_u32(code as u32) {
                                        out.push(ch);
                                    }
                                    pending_uc_skip = uc_skip;
                                }
                            }
                            w if SKIP_DESTINATIONS.contains(&w) => {
                                skip_depth = Some(depth);
                            }
                            _ => {}
                        }
                    }
                } else {
                    // Escaped literal: \\ \{ \}
                    if skip_depth.is_none() && (next == '\\' || next == '{' || next == '}') {
                        out.push(next);
                    } else if skip_depth.is_none() && next == '~' {
                        out.push(' ');
                    }
                    i += 1;
                }
            }
            '\r' | '\n' => {
                i += 1;
            }
            _ => {
                if skip_depth.is_none() {
                    if pending_uc_skip > 0 {
                        pending_uc_skip -= 1;
                    } else {
                        out.push(c);
                    }
                }
                i += 1;
            }
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        "[RTF file contains no extractable text.]".to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests (golden-file style: constructed real containers -> expected text)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn supported_attachment_accepts_the_policy_list_and_rejects_everything_else() {
        for ext in SUPPORTED_ATTACHMENT_EXTENSIONS {
            assert!(is_supported_attachment(&format!("contract.{}", ext)), "{}", ext);
            // Case must not matter - Windows users routinely have .PDF/.JPG.
            assert!(
                is_supported_attachment(&format!("contract.{}", ext.to_uppercase())),
                "uppercase .{} must be accepted",
                ext
            );
        }
        // Types the EXTRACTOR can parse but the product does not accept. If
        // one of these ever starts passing, the policy gate has been widened
        // to match the extractor - which is exactly the mistake the doc
        // comment on SUPPORTED_ATTACHMENT_EXTENSIONS warns against.
        for name in [
            "notes.rtf", "page.html", "sheet.csv", "readme.md", "book.odt",
            "main.rs", "script.py", "data.json", "photo.gif", "scan.tiff",
        ] {
            assert!(!is_supported_attachment(name), "{} must be rejected", name);
        }
        // No extension at all, and a bare dotfile, are not routable.
        assert!(!is_supported_attachment("archive"));
        assert!(!is_supported_attachment("Makefile"));
        assert!(!is_supported_attachment("contract."));
    }

    /// Cross-language invariant: the Rust policy list and its TypeScript
    /// counterpart must be identical.
    ///
    /// This is enforced by PARSING the real .ts file rather than duplicating
    /// its contents here - a duplicated copy would itself be a third list to
    /// drift. The format gate is enforced in two languages (picker + upload
    /// API), and a one-sided edit would produce the worst kind of bug: a
    /// picker that offers a file the server then refuses, or a picker that
    /// hides a file the server would happily accept.
    #[test]
    fn supported_formats_match_the_frontend_list() {
        let ts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src/supportedFormats.ts");
        let source = std::fs::read_to_string(&ts_path).unwrap_or_else(|e| {
            panic!(
                "cannot read the frontend format list at {}: {} - if the file moved, \
                 update this test rather than deleting it; it is the only thing keeping \
                 the two lists in agreement",
                ts_path.display(), e
            )
        });

        let body = source
            .split_once("export const SUPPORTED_EXTENSIONS = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(inside, _)| inside)
            .expect("SUPPORTED_EXTENSIONS array literal not found in supportedFormats.ts");

        let ts_extensions: Vec<String> = body
            .split(',')
            .map(|entry| entry.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|entry| !entry.is_empty())
            .collect();

        let rust_extensions: Vec<String> =
            SUPPORTED_ATTACHMENT_EXTENSIONS.iter().map(|e| e.to_string()).collect();

        assert_eq!(
            ts_extensions, rust_extensions,
            "supportedFormats.ts and SUPPORTED_ATTACHMENT_EXTENSIONS have diverged - \
             both must list the same extensions in the same order"
        );
    }

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            for (name, content) in entries {
                writer
                    .start_file(name.to_string(), SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    #[tokio::test]
    async fn docx_entities_tabs_and_tables_are_faithful() {
        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>Smith &amp; Jones</w:t><w:tab/><w:t>&lt;Contract&gt;</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>CellA</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>CellB</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#;
        let bytes = make_zip(&[("word/document.xml", doc_xml)]);
        let text = extract_content_from_bytes(&bytes, "test.docx").await.unwrap();
        assert!(text.contains("Smith & Jones"), "entities must decode: {}", text);
        assert!(text.contains("Smith & Jones\t<Contract>"), "tab must survive: {}", text);
        assert!(text.contains("CellA\tCellB"), "table cells must be tab-separated: {}", text);
    }

    fn make_zip_with_binary(text_entries: &[(&str, &str)], binary_entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            for (name, content) in text_entries {
                writer.start_file(name.to_string(), SimpleFileOptions::default()).unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
            for (name, content) in binary_entries {
                writer.start_file(name.to_string(), SimpleFileOptions::default()).unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    /// Full chain proof: a real DOCX with a genuine embedded image (rendered
    /// via PDFium with known text, exactly like utils::image_ocr's own
    /// tests) gets that image OCR'd IN PLACE, in correct reading order
    /// relative to the surrounding paragraphs, tagged with a citable
    /// [[image:1]] marker. Exercises relationship resolution
    /// (word/_rels/document.xml.rels), media lookup, OCR, and insertion -
    /// every link the DOCX embedded-image path uses in production.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn docx_embedded_image_is_ocrd_in_reading_order() {
        let pdfium = match crate::utils::pdf_text::bind_pdfium() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("SKIP: pdfium library not available on this machine");
                return;
            }
        };
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 48 Tf 72 600 Td (EXHIBIT SCAN TEXT) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let doc = pdfium.load_pdf_from_byte_slice(pdf, None).unwrap();
        let page = doc.pages().get(0).unwrap();
        let bitmap = page
            .render_with_config(&pdfium_render::prelude::PdfRenderConfig::new().set_target_width(1200))
            .unwrap();
        let (width, height) = (bitmap.width() as u32, bitmap.height() as u32);
        let mut rgba = bitmap.as_raw_bytes().to_vec();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
        }
        let dynamic = image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(width, height, rgba).unwrap());
        let mut png_bytes = Vec::new();
        dynamic.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
<w:p><w:r><w:t>Before the exhibit.</w:t></w:r></w:p>
<w:p><w:r><w:drawing><wp:inline xmlns:wp="wp"><a:graphic xmlns:a="a"><a:graphicData><pic:pic xmlns:pic="pic"><pic:blipFill><a:blip r:embed="rId1"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>
<w:p><w:r><w:t>After the exhibit.</w:t></w:r></w:p>
</w:body></w:document>"#;
        let rels_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"#;

        let bytes = make_zip_with_binary(
            &[("word/document.xml", doc_xml), ("word/_rels/document.xml.rels", rels_xml)],
            &[("word/media/image1.png", &png_bytes)],
        );
        let text = extract_content_from_bytes(&bytes, "Agreement.docx").await.unwrap();

        assert!(text.contains("Before the exhibit."), "{}", text);
        assert!(text.contains("After the exhibit."), "{}", text);
        assert!(text.contains("[[image:1]]"), "{}", text);
        assert!(
            text.to_uppercase().contains("EXHIBIT SCAN TEXT")
                || text.to_uppercase().contains("EXHIBIT")
                    && text.to_uppercase().contains("SCAN"),
            "OCR'd text from the embedded image must appear: {}", text
        );
        let before_pos = text.find("Before the exhibit.").unwrap();
        let image_pos = text.find("[[image:1]]").unwrap();
        let after_pos = text.find("After the exhibit.").unwrap();
        assert!(
            before_pos < image_pos && image_pos < after_pos,
            "reading order must be preserved: {}", text
        );

        let chunks = crate::utils::doc_context::chunk_text(&text);
        // Exact label may also carry a section tag if the OCR'd text
        // happens to look like a heading (e.g. all-caps) - what matters
        // here is that the image is its own chunk, distinctly citable as
        // "Image 1", and that text after it is NOT tagged as image content.
        let image_chunk = chunks.iter().find(|c| c.location_label.as_deref() == Some("Image 1"));
        assert!(image_chunk.is_some(), "expected a chunk anchored to Image 1: {:?}", chunks);
        // The chunk is the OCR provenance header followed by the recognized
        // text (utils::image_ocr::ocr_image_bytes_blocking deliberately emits
        // "[Image '...': ... recovered via OCR ...]\n<text>" so the model
        // knows not to quote a possible misread as clean extracted text).
        // Asserted by parts rather than as one literal: the header's exact
        // wording is a prompt-tuning concern that may legitimately change,
        // whereas the three properties below are the actual contract.
        let image_content = &image_chunk.unwrap().content;
        assert!(
            image_content.contains("recovered via OCR"),
            "image chunk must carry its OCR provenance header: {:?}", image_content
        );
        assert!(
            image_content.contains("EXHIBIT SCAN TEXT"),
            "image chunk must contain the OCR'd text: {:?}", image_content
        );
        // The point the original exact-equality assertion was really making:
        // the image is its OWN chunk. Neither surrounding paragraph may bleed
        // into it, or a citation to "Image 1" would cover text that is not in
        // the image at all.
        assert!(
            !image_content.contains("Before the exhibit.")
                && !image_content.contains("After the exhibit."),
            "image chunk must contain ONLY the image's own text: {:?}", image_content
        );
        let after_chunk = chunks.iter().find(|c| c.content == "After the exhibit.").unwrap();
        assert!(
            after_chunk.location_label.is_none(),
            "text after the image must not inherit its location: {:?}", after_chunk
        );
    }

    /// A broken/missing relationship must produce an explicit marker, not
    /// silently omit the image or panic - never-silent contract check.
    #[tokio::test]
    async fn docx_blip_with_no_matching_relationship_is_explicit_not_silent() {
        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
<w:p><w:r><w:t>Text before.</w:t></w:r></w:p>
<w:p><w:r><a:blip xmlns:a="a" r:embed="rId99"/></w:r></w:p>
</w:body></w:document>"#;
        // No word/_rels/document.xml.rels entry at all.
        let bytes = make_zip(&[("word/document.xml", doc_xml)]);
        let text = extract_content_from_bytes(&bytes, "broken.docx").await.unwrap();
        assert!(text.contains("Text before."), "{}", text);
        assert!(text.contains("not OCR'd"), "must name the failure explicitly: {}", text);
        assert!(text.contains("rId99"), "{}", text);
    }

    /// A plain DOCX with no embedded images must extract exactly as before -
    /// the embedded-image machinery must be a no-op when there's nothing to
    /// find, not alter ordinary text extraction.
    #[tokio::test]
    async fn docx_without_images_is_unaffected() {
        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body><w:p><w:r><w:t>No pictures here, just words.</w:t></w:r></w:p></w:body>
</w:document>"#;
        let bytes = make_zip(&[("word/document.xml", doc_xml)]);
        let text = extract_content_from_bytes(&bytes, "plain.docx").await.unwrap();
        assert_eq!(text, "No pictures here, just words.");
    }

    #[tokio::test]
    async fn legacy_doc_is_rejected_loudly() {
        let mut bytes = CFB_MAGIC.to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        let text = extract_content_from_bytes(&bytes, "old.doc").await.unwrap();
        assert!(text.contains("legacy binary Word file"), "{}", text);
        assert!(text.contains("old.doc"));
    }

    #[tokio::test]
    async fn pptx_slides_and_notes_extracted_in_order() {
        let slide = r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>Q1 &amp; Q2 results</a:t></a:r></a:p></p:sld>"#;
        let notes = r#"<p:notes xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>Remember the caveat</a:t></a:r></a:p></p:notes>"#;
        let bytes = make_zip(&[
            ("ppt/slides/slide1.xml", slide),
            ("ppt/notesSlides/notesSlide1.xml", notes),
        ]);
        let text = extract_content_from_bytes(&bytes, "deck.pptx").await.unwrap();
        assert!(text.contains("Q1 & Q2 results"), "{}", text);
        assert!(text.contains("[Notes]"), "{}", text);
        assert!(text.contains("Remember the caveat"), "{}", text);
    }

    #[tokio::test]
    async fn odt_body_text_extracted() {
        let content = r#"<?xml version="1.0"?>
<office:document-content xmlns:office="o" xmlns:text="t">
<office:automatic-styles><text:p>STYLE NOISE</text:p></office:automatic-styles>
<office:body><office:text><text:p>Hello &amp; welcome</text:p><text:p>Line two</text:p></office:text></office:body>
</office:document-content>"#;
        let bytes = make_zip(&[("content.xml", content)]);
        let text = extract_content_from_bytes(&bytes, "doc.odt").await.unwrap();
        assert!(text.contains("Hello & welcome"), "{}", text);
        assert!(text.contains("Line two"));
        assert!(!text.contains("STYLE NOISE"), "styles must not leak: {}", text);
    }

    #[tokio::test]
    async fn rtf_control_words_and_escapes() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0 Arial;}}Hello \'e9t\'e9 World\par Second\tab Col}";
        let text = extract_content_from_bytes(rtf.as_bytes(), "note.rtf").await.unwrap();
        assert!(text.contains("Hello \u{e9}t\u{e9} World"), "{}", text);
        assert!(text.contains("Second\tCol"), "{}", text);
        assert!(!text.contains("Arial"), "font table must be skipped: {}", text);
        assert!(!text.contains("rtf1"), "{}", text);
    }

    #[tokio::test]
    async fn windows_1252_text_is_decoded_not_mangled() {
        // "café" in Windows-1252: 0xE9 is not valid UTF-8
        let bytes = [b'c', b'a', b'f', 0xE9];
        let text = extract_content_from_bytes(&bytes, "memo.txt").await.unwrap();
        assert_eq!(text, "caf\u{e9}");
    }

    #[tokio::test]
    async fn html_tags_are_stripped() {
        let html = b"<html><body><h1>Title</h1><p>Body &amp; text</p></body></html>";
        let text = extract_content_from_bytes(html, "page.html").await.unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Body & text"), "{}", text);
        assert!(!text.contains("<p>"));
    }

    #[tokio::test]
    async fn plain_utf8_passthrough() {
        let content = "Test file content\nwith multiple lines";
        let text = extract_content_from_bytes(content.as_bytes(), "a.txt").await.unwrap();
        assert_eq!(text, content);
    }

    /// Regression test for the exact bug this routing closes: before
    /// utils::image_ocr existed, "jpg"/"png" had no match arm here, so they
    /// fell to the `_` catch-all and were decoded as TEXT via chardetng -
    /// silently producing mojibake from raw binary image bytes, with no
    /// error at all. Proves a JPG/PNG never reaches decode_text_bytes: any
    /// corrupt/non-image bytes under these extensions must come back as an
    /// explicit "[Cannot extract..." marker instead.
    #[tokio::test]
    async fn images_are_routed_to_ocr_not_decoded_as_text() {
        let garbage = vec![0xFFu8, 0xD8, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        for ext in ["jpg", "jpeg", "png", "bmp", "gif", "tiff"] {
            let filename = format!("scan.{ext}");
            let text = extract_content_from_bytes(&garbage, &filename).await.unwrap();
            assert!(
                text.starts_with('['),
                "extension '{}' must produce an explicit marker for undecodable bytes, not silently-decoded text: {:?}",
                ext, text
            );
            assert!(text.contains(&filename), "{:?}", text);
        }
    }

    // -----------------------------------------------------------------
    // End-to-end: extraction -> utils::doc_context::chunk_text. Proves
    // structure-aware chunking actually engages on every office format, not
    // just PDF - i.e. that each extractor's paragraph/slide/sheet boundaries
    // are real blank-line ("\n\n") separators the chunker can see, not
    // collapsed into one undifferentiated blob.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn docx_paragraphs_chunk_separately() {
        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>Section 1: Definitions</w:t></w:r></w:p>
<w:p><w:r><w:t>"Agreement" means this contract between the parties as amended from time to time.</w:t></w:r></w:p>
<w:p><w:r><w:t>Section 2: Term</w:t></w:r></w:p>
<w:p><w:r><w:t>This Agreement commences on the Effective Date and continues for two years.</w:t></w:r></w:p>
</w:body></w:document>"#;
        let bytes = make_zip(&[("word/document.xml", doc_xml)]);
        let text = extract_content_from_bytes(&bytes, "contract.docx").await.unwrap();
        assert!(text.contains("\n\n"), "paragraphs must be blank-line separated: {:?}", text);

        let chunks = crate::utils::doc_context::chunk_text(&text);
        assert!(
            chunks.iter().any(|c| c.section_label.as_deref() == Some("Section 1: Definitions")),
            "{:?}", chunks
        );
        assert!(
            chunks.iter().any(|c| c.section_label.as_deref() == Some("Section 2: Term")),
            "{:?}", chunks
        );
    }

    #[tokio::test]
    async fn pptx_slides_chunk_as_separate_citable_units() {
        let slide1 = r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>Opening remarks for the quarterly review.</a:t></a:r></a:p></p:sld>"#;
        let slide2 = r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>Revenue grew twelve percent year over year across all regions.</a:t></a:r></a:p></p:sld>"#;
        let bytes = make_zip(&[
            ("ppt/slides/slide1.xml", slide1),
            ("ppt/slides/slide2.xml", slide2),
        ]);
        let text = extract_content_from_bytes(&bytes, "deck.pptx").await.unwrap();

        let chunks = crate::utils::doc_context::chunk_text(&text);
        assert_eq!(chunks.len(), 2, "{:?}", chunks);
        assert_eq!(chunks[0].citation_label(), "[Slide 1]");
        assert_eq!(chunks[1].citation_label(), "[Slide 2]");
    }

    #[tokio::test]
    async fn odt_paragraphs_chunk_separately() {
        let content = r#"<?xml version="1.0"?>
<office:document-content xmlns:office="o" xmlns:text="t">
<office:body><office:text>
<text:p>WHEREAS the parties wish to enter into this agreement</text:p>
<text:p>NOW THEREFORE the parties agree as follows in consideration of the mutual promises herein.</text:p>
</office:text></office:body>
</office:document-content>"#;
        let bytes = make_zip(&[("content.xml", content)]);
        let text = extract_content_from_bytes(&bytes, "doc.odt").await.unwrap();
        assert!(text.contains("\n\n"), "paragraphs must be blank-line separated: {:?}", text);

        let chunks = crate::utils::doc_context::chunk_text(&text);
        assert!(
            chunks.iter().any(|c| c.section_label.as_deref().unwrap_or("").starts_with("WHEREAS")),
            "{:?}", chunks
        );
    }

    #[tokio::test]
    async fn rtf_pars_chunk_separately() {
        let rtf = r"{\rtf1\ansi Section One heading text here.\par This is the body of the first section with enough words to read as a real paragraph.\par Section Two heading text here.\par This is the body of the second section with its own distinct content for testing.}";
        let text = extract_content_from_bytes(rtf.as_bytes(), "note.rtf").await.unwrap();
        assert!(text.contains("\n\n"), "\\par must produce a real paragraph break: {:?}", text);

        // Small enough to land in one chunk, but \par must have produced 4
        // distinct paragraphs (not one blob) for the chunker to have seen -
        // proven by the paragraph range the chunk reports spanning.
        let chunks = crate::utils::doc_context::chunk_text(&text);
        assert_eq!(chunks.len(), 1, "{:?}", chunks);
        assert_eq!(chunks[0].paragraph_start, 0);
        assert_eq!(chunks[0].paragraph_end, 3, "{:?}", chunks);
    }

    #[tokio::test]
    async fn xlsx_sheet_rows_chunk_with_sheet_citation() {
        let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
        let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
        let workbook_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Schedule A" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
        let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet1_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="str"><v>Item</v></c><c r="B1" t="str"><v>Amount</v></c></row>
<row r="2"><c r="A2" t="str"><v>Consulting Fees</v></c><c r="B2"><v>15000</v></c></row>
</sheetData>
</worksheet>"#;
        let bytes = make_zip(&[
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", root_rels),
            ("xl/workbook.xml", workbook_xml),
            ("xl/_rels/workbook.xml.rels", workbook_rels),
            ("xl/worksheets/sheet1.xml", sheet1_xml),
        ]);
        let text = extract_content_from_bytes(&bytes, "schedule.xlsx").await.unwrap();
        assert!(text.contains("[[sheet:Schedule A]]"), "{:?}", text);
        assert!(text.contains("Consulting Fees"), "{:?}", text);

        let chunks = crate::utils::doc_context::chunk_text(&text);
        assert!(
            chunks.iter().any(|c| c.citation_label() == "[Sheet: Schedule A]"),
            "{:?}", chunks
        );
        assert!(
            chunks.iter().any(|c| c.content.contains("Consulting Fees")),
            "{:?}", chunks
        );
    }

    /// Legacy binary PowerPoint must be refused with the same clarity as
    /// legacy .doc. The rejection existed in extract_presentation but had no
    /// test, so nothing would have caught it regressing into the
    /// unknown-type text decode - which for a CFB container means the model
    /// receives a screenful of binary garbage described as a presentation.
    #[tokio::test]
    async fn legacy_ppt_is_rejected_loudly() {
        let mut bytes = CFB_MAGIC.to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        let text = extract_content_from_bytes(&bytes, "deck.ppt").await.unwrap();
        assert!(
            text.contains("legacy binary PowerPoint file"),
            "a .ppt must be refused explicitly, got: {}",
            text
        );
        assert!(text.contains("deck.ppt"), "the failure must name the file: {}", text);
    }

    /// THE FORMAT MATRIX CONTRACT, for every type the product accepts.
    ///
    /// Each supported extension must route to a real engine, and when the
    /// bytes are not actually that format the result must be an ANNOUNCED
    /// failure - never plausible-looking text.
    ///
    /// This is the guard against the single most damaging failure available
    /// here: `extract_content_from_bytes` ends in an unknown-type branch that
    /// text-decodes ANY bytes. If a format's routing is ever removed or
    /// mistyped, extraction does not error - it silently produces mojibake,
    /// stores it as `extraction_status = "ok"`, and hands it to the model as
    /// authoritative document content. A per-format content test would not
    /// catch that for the format it forgot to cover; this iterates the policy
    /// list itself, so a newly added type is covered the moment it is added.
    ///
    /// `.txt` is deliberately exempt: decoding arbitrary bytes as text is the
    /// CORRECT behaviour there, which is exactly why every other type needs
    /// its own routing.
    #[tokio::test]
    async fn every_supported_binary_format_fails_loudly_on_bytes_that_are_not_that_format() {
        // Not valid in any container format, and not valid UTF-8 either, so a
        // text decode would visibly produce replacement characters.
        let junk: Vec<u8> = vec![
            0x00, 0xFF, 0xFE, 0x42, 0x00, 0x01, 0x80, 0x93, 0xC3, 0x28, 0xA0, 0xA1,
            0xF8, 0x88, 0x80, 0x80, 0x80, 0x00, 0xED, 0xA0, 0x80, 0x7F, 0x1B, 0x00,
        ];

        for ext in SUPPORTED_ATTACHMENT_EXTENSIONS {
            if *ext == "txt" {
                continue;
            }
            let filename = format!("mystery.{}", ext);
            let outcome = extract_content_from_bytes(&junk, &filename).await;

            // Either an Err, or an announced bracketed failure marker. Both are
            // honest; silent text is not.
            let text = match outcome {
                Err(_) => continue,
                Ok(t) => t,
            };
            assert!(
                text.trim_start().starts_with('['),
                ".{} produced un-announced output for bytes that are not a .{} - \
                 this is the silent-mojibake path: {:?}",
                ext,
                ext,
                text.chars().take(160).collect::<String>()
            );

            // And the announcement must name the file, so a user can tell WHICH
            // attachment failed when several are attached at once.
            assert!(
                text.contains(&filename),
                ".{} failure must name the file it refers to: {:?}",
                ext,
                text.chars().take(160).collect::<String>()
            );
        }
    }

    /// The classifier that decides whether an extraction counts as usable must
    /// agree with the matrix above: an announced failure is NOT stored as
    /// content. Without this, the bracketed markers asserted above would still
    /// reach the model - just labelled "ok".
    #[tokio::test]
    async fn announced_format_failures_are_classified_as_failed_not_stored_as_content() {
        let junk: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x42, 0xC3, 0x28, 0x80, 0x93];

        for ext in SUPPORTED_ATTACHMENT_EXTENSIONS {
            if *ext == "txt" {
                continue;
            }
            let filename = format!("mystery.{}", ext);
            let (text, status, error) =
                extraction_outcome(extract_content_from_bytes(&junk, &filename).await);
            assert_eq!(
                status, "failed",
                ".{} must be classified as failed, got status='{}' with text {:?}",
                ext, status, text.chars().take(120).collect::<String>()
            );
            assert!(
                text.is_empty(),
                ".{} must store NO content on failure, got {:?}",
                ext,
                text.chars().take(120).collect::<String>()
            );
            assert!(
                error.is_some(),
                ".{} must record a reason so the user can be told why",
                ext
            );
        }
    }

    /// Plain text is the one type where decoding arbitrary bytes is correct -
    /// pinned so the exemption above is a deliberate rule rather than an
    /// untested assumption.
    #[tokio::test]
    async fn plain_text_decodes_rather_than_failing() {
        let (text, status, _) = extraction_outcome(
            extract_content_from_bytes(b"Section 3 is binding.", "note.txt").await,
        );
        assert_eq!(status, "ok");
        assert!(text.contains("Section 3 is binding."));
    }
}
