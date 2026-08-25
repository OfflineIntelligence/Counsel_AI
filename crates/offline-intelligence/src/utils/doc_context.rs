//! Shared document-chunking utilities.
//!
//! Used by memory_db::documents_store to chunk a document's extracted text at
//! document-creation time. Chunks are a PROVENANCE structure, not a retrieval
//! index: this system has no embedding model, so nothing ranks chunks against
//! a query. Their value is the physical location and clause labels computed
//! below, which let an excerpt be cited rather than shown anonymously.
//!
//! Chunking is structure-aware, not blind character-counting, across every
//! supported format:
//!   - PDF pages are tagged `[[page:N]]` by utils::pdf_text (native text
//!     layer and OCR alike), so a chunk can report the exact page(s) it was
//!     drawn from.
//!   - PPTX slides are tagged `[[slide:N]]` and spreadsheet sheets
//!     `[[sheet:Name]]` by utils::file_processor.
//!   - Formats with no physical page/slide/sheet concept (DOCX, TXT, RTF,
//!     ODT, HTML, code files) fall back to a paragraph range (e.g. "P12-18"),
//!     computed structurally - no markers needed, always available.
//!   - Legal-document headings (numbered clauses like "4.2(b)", "Section 7",
//!     "ARTICLE III", all-caps headings) are detected in ANY format's text
//!     and force a fresh chunk boundary rather than being buried mid-chunk.
//!
//! This is what lets the model cite "p.12, Section 4.2(b)" or "Slide 3" or
//! "P42-44" back to the user instead of an anonymous excerpt. It depends on
//! every extractor in utils::file_processor emitting a real blank line
//! (`\n\n`) at TRUE paragraph/slide/row boundaries, distinct from the single
//! `\n` used for in-unit line breaks (w:br, a:br, RTF \line) - otherwise the
//! paragraph splitter below sees one undifferentiated blob.

use lazy_static::lazy_static;
use regex::Regex;

/// Target chunk size in characters (~400 tokens).
const CHUNK_TARGET_CHARS: usize = 1600;
/// Overlap between adjacent chunks so sentences spanning a boundary survive.
const CHUNK_OVERLAP_CHARS: usize = 200;
/// Upper bound on chunks per document (chunk size grows for huge documents
/// instead of exceeding this, so a document's chunk count stays bounded).
const MAX_CHUNKS: usize = 96;

lazy_static! {
    /// A line embedded by an extractor marking a physical location: PDF page
    /// (utils::pdf_text), PPTX slide, spreadsheet sheet, or a DOCX embedded
    /// image OCR'd in place (all utils::file_processor). Value is numeric
    /// for page/slide/image, a free-form name for sheet, ignored for clear.
    /// "clear" ends a one-shot location (currently just embedded images):
    /// unlike a page/slide/sheet, which is a persistent region covering
    /// every following paragraph until the NEXT marker, an embedded image's
    /// OCR'd text is a single paragraph - text immediately after it must NOT
    /// inherit "Image N" as its citation.
    static ref LOCATION_MARKER: Regex = Regex::new(r"^\[\[(page|slide|sheet|image|clear):(.+?)\]\](?:\s*\(OCR\))?\s*$").unwrap();

    /// Legal/contract heading patterns, checked against a paragraph's first
    /// line. Ordered by specificity; the first match wins.
    ///   - "ARTICLE III", "Article 3", "Section 4.2(b)", "Clause 9"
    ///   - "4.2(b)", "1.", "(a)", "IV." numbered-clause openers
    ///   - "INDEMNIFICATION" all-caps short heading lines
    ///   - "WHEREAS" recital openers
    static ref HEADING_LABELED: Regex = Regex::new(
        r"(?i)^\s*(article|section|clause|schedule|exhibit|appendix|paragraph)\s+([ivxlcdm\d]+(?:\.[ivxlcdm\d]+)*(?:\([a-zA-Z0-9]+\))?)\s*[:\.\)]?\s*(.{0,60})"
    ).unwrap();
    static ref HEADING_NUMBERED: Regex = Regex::new(
        r"^\s*(\(?[ivxlcdm\d]+\)?(?:\.[ivxlcdm\d]+)*[\.\)])\s+(.{0,60})"
    ).unwrap();
    static ref HEADING_ALLCAPS: Regex = Regex::new(
        r"^[A-Z][A-Z0-9 ,&'/\-]{3,59}$"
    ).unwrap();
    static ref HEADING_WHEREAS: Regex = Regex::new(r"^\s*(WHEREAS|NOW,? THEREFORE|RECITALS)\b").unwrap();
}

/// One chunk of a document's extracted text, with provenance so retrieval
/// results can be cited back to a physical location and/or clause instead of
/// shown as an anonymous excerpt.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentChunk {
    pub content: String,
    /// First/last PDF page this chunk's text came from, if `[[page:N]]`
    /// markers were present in the source (native PDF/OCR extraction only).
    pub page_start: Option<i32>,
    pub page_end: Option<i32>,
    /// Free-form physical location for formats without numeric pages, e.g.
    /// "Slide 3" or "Sheet: Q1 Data". Mutually exclusive with page_start in
    /// practice (a document is one format), never both set.
    pub location_label: Option<String>,
    /// The nearest heading/clause label in effect when this chunk starts
    /// (e.g. "Section 4.2(b)", "ARTICLE III"), if the document exhibits any
    /// recognizable structure. None for unstructured text.
    pub section_label: Option<String>,
    /// 0-based paragraph index range this chunk spans within the document
    /// (counting paragraphs after location-marker lines are stripped). Always
    /// set - the last-resort citation anchor when a document has neither a
    /// physical-location marker nor a recognizable heading.
    pub paragraph_start: i32,
    pub paragraph_end: i32,
}

impl DocumentChunk {
    /// Human-readable citation prefix, e.g. "[p.12]", "[Slide 3]",
    /// "[P12-18]", "[p.12, Section 4.2(b)]". Always non-empty: paragraph
    /// range is the guaranteed fallback when no physical marker or heading
    /// was found, so a citation can always be offered, never omitted.
    pub fn citation_label(&self) -> String {
        let location = match (self.page_start, self.page_end) {
            (Some(s), Some(e)) if s == e => format!("p.{}", s),
            (Some(s), Some(e)) => format!("pp.{}-{}", s, e),
            (Some(s), None) => format!("p.{}", s),
            _ => match &self.location_label {
                Some(l) => l.clone(),
                None if self.paragraph_start == self.paragraph_end => {
                    format!("P{}", self.paragraph_start + 1)
                }
                None => format!("P{}-{}", self.paragraph_start + 1, self.paragraph_end + 1),
            },
        };
        match &self.section_label {
            Some(s) => format!("[{}, {}]", location, s),
            None => format!("[{}]", location),
        }
    }
}

/// Detect a heading on a paragraph's first line and return its label, or
/// None if the paragraph doesn't look like a heading. Cheap, pure regex -
/// no NLP - deliberately conservative so body text isn't misclassified.
fn detect_heading(paragraph: &str) -> Option<String> {
    let first_line = paragraph.lines().next().unwrap_or("").trim();
    if first_line.is_empty() || first_line.len() > 120 {
        return None;
    }
    if let Some(caps) = HEADING_LABELED.captures(first_line) {
        let kind = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let num = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let title = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
        let kind_cap = {
            let mut c = kind.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => kind.to_string(),
            }
        };
        return Some(if title.is_empty() {
            format!("{} {}", kind_cap, num)
        } else {
            format!("{} {}: {}", kind_cap, num, title)
        });
    }
    if HEADING_WHEREAS.is_match(first_line) {
        return Some(first_line.to_string());
    }
    // All-caps short line: a heading only if it's the ENTIRE first line and
    // there's no trailing lowercase body text glued onto the same line
    // (avoids misfiring on shouted body sentences).
    if first_line == first_line.to_uppercase()
        && HEADING_ALLCAPS.is_match(first_line)
        && first_line.chars().any(|c| c.is_alphabetic())
    {
        return Some(first_line.to_string());
    }
    if let Some(caps) = HEADING_NUMBERED.captures(first_line) {
        // Numbered-clause openers are common but noisy (e.g. plain list
        // items) - only treat as a heading when the marker is followed by a
        // short, capitalized title, not a long run-on sentence.
        let marker = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let rest = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let starts_capitalized = rest.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        if starts_capitalized && rest.len() <= 60 && !rest.trim_end().ends_with('.') {
            return Some(format!("{} {}", marker, rest.trim()));
        }
    }
    None
}

/// A stripped physical-location marker: a numeric page, a named slide/sheet
/// (or embedded image), or an explicit end of a one-shot location.
enum Location {
    Page(i32),
    Named(String),
    Clear,
}

/// Strip a leading `[[page:N]]` / `[[slide:N]]` / `[[sheet:Name]]` marker
/// line from a paragraph, returning the parsed location (if present) and the
/// remaining content.
fn strip_location_marker(paragraph: &str) -> (Option<Location>, &str) {
    let (first, rest) = match paragraph.find('\n') {
        Some(nl) => {
            let (f, r) = paragraph.split_at(nl);
            (f, r.trim_start_matches('\n'))
        }
        None => (paragraph, ""),
    };
    let Some(caps) = LOCATION_MARKER.captures(first.trim()) else {
        return (None, paragraph);
    };
    let kind = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    let location = match kind {
        "page" => value.parse::<i32>().ok().map(Location::Page),
        "slide" => value.parse::<i32>().ok().map(|n| Location::Named(format!("Slide {}", n))),
        "sheet" => Some(Location::Named(format!("Sheet: {}", value))),
        "image" => value.parse::<i32>().ok().map(|n| Location::Named(format!("Image {}", n))),
        "clear" => Some(Location::Clear),
        _ => None,
    };
    (location, rest)
}

/// Split text into overlapping chunks, preferring paragraph boundaries and,
/// where the document exhibits legal structure, clause/section boundaries.
/// Each chunk carries physical-location and section provenance for citation.
pub fn chunk_text(text: &str) -> Vec<DocumentChunk> {
    let total = text.len();
    if total == 0 {
        return Vec::new();
    }
    // Grow the chunk size when the document would exceed MAX_CHUNKS.
    let target = CHUNK_TARGET_CHARS.max(total / MAX_CHUNKS + CHUNK_OVERLAP_CHARS);

    let raw_paragraphs: Vec<&str> = text.split("\n\n").collect();

    let mut chunks: Vec<DocumentChunk> = Vec::new();
    let mut current = String::new();
    let mut current_section: Option<String> = None;
    let mut chunk_section: Option<String> = None;
    let mut chunk_page_start: Option<i32> = None;
    let mut chunk_page_end: Option<i32> = None;
    let mut chunk_location_label: Option<String> = None;
    let mut chunk_first_paragraph: i32 = 0;
    let mut chunk_last_paragraph: i32 = 0;
    let mut current_page: Option<i32> = None;
    let mut current_location_label: Option<String> = None;
    let mut paragraph_index: i32 = 0;

    macro_rules! flush {
        () => {
            if !current.trim().is_empty() {
                chunks.push(DocumentChunk {
                    content: current.trim().to_string(),
                    page_start: chunk_page_start,
                    page_end: chunk_page_end.or(chunk_page_start),
                    location_label: chunk_location_label.clone(),
                    section_label: chunk_section.clone(),
                    paragraph_start: chunk_first_paragraph,
                    paragraph_end: chunk_last_paragraph,
                });
            }
            current.clear();
            chunk_page_start = None;
            chunk_page_end = None;
            chunk_location_label = None;
            chunk_section = None;
        };
    }

    for raw_para in raw_paragraphs {
        if raw_para.trim().is_empty() {
            continue;
        }
        let (location, para) = strip_location_marker(raw_para);
        // Unlike a PDF page (which may legitimately span a chunk, expressed
        // as a page RANGE), a named location (slide/sheet/image) is always a
        // fresh chunk boundary: "Slide 1-2" or "Sheet: Q1-Sheet: Q2" isn't a
        // meaningful citation, and each slide/sheet/image is a genuinely
        // distinct unit worth keeping separate regardless of size. Clear
        // (ends a one-shot location, e.g. after an embedded image) is
        // likewise a boundary: text following it must not inherit the
        // location that just ended.
        let mut is_location_boundary = false;
        match location {
            Some(Location::Page(p)) => {
                current_page = Some(p);
                current_location_label = None;
            }
            Some(Location::Named(l)) => {
                if current_location_label.as_deref() != Some(l.as_str()) {
                    is_location_boundary = true;
                }
                current_location_label = Some(l);
                current_page = None;
            }
            Some(Location::Clear) => {
                if current_location_label.is_some() || current_page.is_some() {
                    is_location_boundary = true;
                }
                current_location_label = None;
                current_page = None;
            }
            None => {}
        }

        // A heading or a location transition starts a fresh chunk (never
        // buried mid-chunk) unless the buffer is already empty. Checked
        // BEFORE the empty-body skip below, so a marker-only paragraph (e.g.
        // a bare [[clear:x]]) still forces the boundary even though it has
        // no body of its own to chunk.
        let heading = if para.trim().is_empty() { None } else { detect_heading(para) };
        let is_heading_start = heading.is_some();
        if let Some(h) = heading {
            current_section = Some(h);
        }
        if (is_heading_start || is_location_boundary) && !current.trim().is_empty() {
            flush!();
        }

        if para.trim().is_empty() {
            // Location marker with no body text on its own (rare) - just
            // records the location transition, nothing to chunk.
            continue;
        }

        // A single paragraph larger than the target is split hard.
        if para.len() > target {
            flush!();
            let bytes = para.as_bytes();
            let mut start = 0usize;
            while start < bytes.len() {
                let mut end = (start + target).min(bytes.len());
                while end < bytes.len() && !para.is_char_boundary(end) {
                    end += 1;
                }
                while !para.is_char_boundary(start) {
                    start -= 1;
                }
                chunks.push(DocumentChunk {
                    content: para[start..end].trim().to_string(),
                    page_start: current_page,
                    page_end: current_page,
                    location_label: current_location_label.clone(),
                    section_label: current_section.clone(),
                    paragraph_start: paragraph_index,
                    paragraph_end: paragraph_index,
                });
                if end == bytes.len() {
                    break;
                }
                start = end.saturating_sub(CHUNK_OVERLAP_CHARS);
                while !para.is_char_boundary(start) {
                    start -= 1;
                }
            }
            paragraph_index += 1;
            continue;
        }

        if current.len() + para.len() + 2 > target && !current.trim().is_empty() {
            // Paragraph-boundary splitting (never mid-sentence) already
            // gives adjacent chunks natural continuity, so no explicit
            // char-level overlap seeding is needed here.
            flush!();
        }
        if current.is_empty() {
            chunk_first_paragraph = paragraph_index;
            chunk_page_start = current_page;
            chunk_location_label = current_location_label.clone();
            chunk_section = current_section.clone();
        }
        chunk_page_end = current_page;
        chunk_last_paragraph = paragraph_index;
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
        paragraph_index += 1;
    }
    flush!();
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_covers_all_content_with_overlap() {
        let paras: Vec<String> = (0..40).map(|i| format!("Paragraph number {} with some legal wording to fill space in the chunk buffer.", i)).collect();
        let text = paras.join("\n\n");
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
        for p in &paras {
            assert!(chunks.iter().any(|c| c.content.contains(p.as_str())), "lost: {}", p);
        }
        for c in &chunks {
            assert!(c.content.len() <= CHUNK_TARGET_CHARS + CHUNK_OVERLAP_CHARS + 200, "oversized chunk: {}", c.content.len());
        }
    }

    #[test]
    fn chunker_handles_single_giant_paragraph() {
        let text = "x".repeat(10_000);
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
        let combined: usize = chunks.iter().map(|c| c.content.len()).sum();
        assert!(combined >= 10_000, "content lost: {} < 10000", combined);
    }

    #[test]
    fn page_markers_attribute_chunk_to_its_page_span() {
        let text = "[[page:1]]\nThis is page one text about indemnification obligations of the parties.\n\n[[page:2]]\nThis is page two text about termination rights and notice periods required.";
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].page_start, Some(1));
        assert_eq!(chunks[0].page_end, Some(2));
        assert_eq!(chunks[0].citation_label(), "[pp.1-2]");
    }

    #[test]
    fn large_pages_produce_separately_anchored_chunks() {
        let page1 = format!("[[page:1]]\n{}", "Indemnification clause text. ".repeat(80));
        let page2 = format!("[[page:2]]\n{}", "Termination clause text. ".repeat(80));
        let text = format!("{}\n\n{}", page1, page2);
        let chunks = chunk_text(&text);
        assert!(chunks.iter().any(|c| c.page_start == Some(1)));
        assert!(chunks.iter().any(|c| c.page_start == Some(2)));
    }

    #[test]
    fn slide_markers_produce_slide_citation_as_separate_chunks() {
        // Each slide is its own citable unit - never merged with the next,
        // even when both are small enough to otherwise fit one chunk.
        let text = "[[slide:1]]\nWelcome to the quarterly review deck.\n\n[[slide:2]]\nRevenue grew twelve percent year over year across all regions.";
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 2, "{:?}", chunks);
        assert_eq!(chunks[0].citation_label(), "[Slide 1]");
        assert_eq!(chunks[1].citation_label(), "[Slide 2]");
    }

    #[test]
    fn sheet_markers_produce_sheet_citation_as_separate_chunks() {
        let text = "[[sheet:Q1 Data]]\nRevenue\t100000\nExpenses\t45000\n\n[[sheet:Q2 Data]]\nRevenue\t120000\nExpenses\t50000";
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 2, "{:?}", chunks);
        assert_eq!(chunks[0].citation_label(), "[Sheet: Q1 Data]");
        assert_eq!(chunks[1].citation_label(), "[Sheet: Q2 Data]");
    }

    #[test]
    fn unstructured_text_falls_back_to_paragraph_range_citation() {
        let text = "First paragraph of plain notes with no headings at all in this document.\n\nSecond paragraph continuing the plain notes without any structure present.";
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].paragraph_start, 0);
        assert_eq!(chunks[0].paragraph_end, 1);
        assert_eq!(chunks[0].citation_label(), "[P1-2]");
    }

    #[test]
    fn numbered_section_headings_are_detected_and_start_new_chunks() {
        let text = "Section 4.2(b): Indemnification\nThe Vendor shall indemnify the Client against all claims arising from breach of this Agreement, including but not limited to direct and consequential damages incurred.\n\nSection 5: Termination\nEither party may terminate this Agreement upon thirty (30) days written notice to the other party.";
        let chunks = chunk_text(&text);
        assert!(
            chunks.iter().any(|c| c.section_label.as_deref() == Some("Section 4.2(b): Indemnification")),
            "expected a chunk labeled with Section 4.2(b): {:?}", chunks
        );
        assert!(
            chunks.iter().any(|c| c.section_label.as_deref() == Some("Section 5: Termination")),
            "expected a chunk labeled with Section 5: {:?}", chunks
        );
    }

    #[test]
    fn allcaps_heading_is_detected() {
        let text = "INDEMNIFICATION AND LIABILITY\nEach party agrees to defend, indemnify, and hold harmless the other party from any third-party claims.";
        let chunks = chunk_text(&text);
        assert_eq!(chunks[0].section_label.as_deref(), Some("INDEMNIFICATION AND LIABILITY"));
    }

    #[test]
    fn plain_unstructured_text_has_no_section_label() {
        let text = "Just some ordinary notes without any legal structure or headings at all, written casually.";
        let chunks = chunk_text(&text);
        assert!(chunks[0].section_label.is_none());
    }
}
