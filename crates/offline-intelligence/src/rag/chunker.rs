//! Legal-aware chunking over `documents.extracted_text`.
//!
//! Chunks are stored as byte ranges into the extracted text rather than as
//! copies, so indexing a document costs a few integers per chunk on disk.
//! There is no cap on the number of chunks: a 1,000-page contract produces
//! page- and clause-sized chunks exactly like a 2-page letter does.

use lazy_static::lazy_static;
use regex::Regex;

/// Bumped whenever chunk boundaries or labels change, so the indexer
/// re-chunks documents produced by an older version.
pub const CHUNKER_VERSION: i64 = 5;

/// Preferred chunk size in bytes. Bytes, not chars, so CJK and Devanagari
/// text (3 bytes per char in UTF-8) stays well inside the embedding window.
pub const TARGET_BYTES: usize = 1400;
/// A chunk never exceeds this many bytes.
pub const HARD_MAX_BYTES: usize = 2800;
/// Overlap carried into the next piece when one paragraph is split.
const SPLIT_OVERLAP_BYTES: usize = 200;
/// Spreadsheet chunks stay small so a question about a few rows retrieves
/// those rows, not a screenful around them.
pub const SHEET_TARGET_BYTES: usize = 900;

lazy_static! {
    static ref LOCATION_MARKER: Regex =
        Regex::new(r"^\[\[(page|slide|sheet|image|clear):(.*?)\]\](?:\s*\(OCR\))?\s*$").unwrap();

    static ref ANY_MARKER_LINE: Regex =
        Regex::new(r"(?m)^\[\[(?:page|slide|sheet|image|clear):.*?\]\](?:\s*\(OCR\))?[ \t]*\r?\n?").unwrap();

    static ref HEADING_LABELED: Regex = Regex::new(
        r"(?i)^\s*(article|artículo|articulo|artigo|artikel|articolo|art\.|section|sección|seccion|secção|seção|abschnitt|paragraph|párrafo|paragraphe|clause|cláusula|clausula|klausel|clausola|schedule|annex|anexo|anlage|annexe|allegato|exhibit|appendix|apéndice|apendice|chapter|capítulo|capitulo|kapitel|chapitre|part|parte|teil|titre|título|titulo|статья|раздел|пункт|глава|खंड|धारा|अनुच्छेद|§)\s*([ivxlcdm\d]+(?:\.[ivxlcdm\d]+)*(?:\s*\([a-z0-9]+\))*)\s*[:\.\)\-–—]?\s*(.{0,80})"
    ).unwrap();

    static ref HEADING_CJK: Regex =
        Regex::new(r"^\s*(第[一二三四五六七八九十百千零〇0-9]+[条章节款編编部])\s*(.{0,40})").unwrap();

    static ref HEADING_NUMBERED: Regex =
        Regex::new(r"^\s*((?:\d{1,3}\.)+\d{0,3}|\d{1,3}\)|\([a-z]{1,3}\)|\([ivxlcdm]{1,6}\))\s+(.{1,80})").unwrap();

    static ref HEADING_ALLCAPS: Regex = Regex::new(r"^[A-Z][A-Z0-9 ,&'/\-]{3,79}$").unwrap();

    static ref HEADING_RECITALS: Regex =
        Regex::new(r"^\s*(WHEREAS|NOW,? THEREFORE|RECITALS|CONSIDERANDO|ATTENDU QUE|PRÄAMBEL)\b").unwrap();

    static ref SECTION_NUMBER: Regex =
        Regex::new(r"(\d{1,4}(?:\.\d{1,4})*)((?:\s*\([a-z0-9]{1,4}\))*)").unwrap();
    static ref SECTION_ROMAN: Regex =
        Regex::new(r"\b([ivxlcdm]{1,7})\b((?:\s*\([a-z0-9]{1,4}\))*)").unwrap();
    static ref HEADING_CJK_NUMBER: Regex =
        Regex::new(r"第([一二三四五六七八九十百千零〇0-9]+)[条章节款編编部]").unwrap();
    static ref PAREN_PART: Regex = Regex::new(r"\([a-z0-9]{1,4}\)").unwrap();

    /// A table-of-contents entry: text, a run of leader dots, a page number.
    static ref DOT_LEADER: Regex = Regex::new(r"(?:\.\s?){4,}\s*\d{1,4}\s*$").unwrap();

    /// Start of a list item: bullets, dashes, "o", "(a)", "(iv)", "1)".
    static ref BULLET: Regex =
        Regex::new(r"^(?:[•●▪■◦‣∙·\-–—*]|o\s|\([a-z0-9]{1,4}\)|[a-z0-9]{1,3}\))\s*").unwrap();

    static ref DEFINED_TERM_MEANS: Regex = Regex::new(
        r#"(?i)(?:^|[\s(\[:;,])["“‘']([^"”’'\n]{2,80})["”’']\s*(?:\(.*?\)\s*)?(?:shall\s+)?(?:means?|has\s+the\s+meaning|have\s+the\s+meaning|refers?\s+to|significa|signifie|désigne|bezeichnet|bedeutet|indica|se\s+refiere\s+a|tendrá\s+el\s+significado)\b"#
    ).unwrap();

    static ref DEFINED_TERM_PAREN: Regex = Regex::new(
        r#"\((?:the\s+|la\s+|el\s+|le\s+|die\s+|der\s+|das\s+)?["“]([^"”\n]{2,60})["”]\)"#
    ).unwrap();
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub byte_start: usize,
    pub byte_end: usize,
    pub page_start: Option<i32>,
    pub page_end: Option<i32>,
    pub location_label: Option<String>,
    pub section_label: Option<String>,
    pub section_path: Option<String>,
    pub para_start: i32,
    pub para_end: i32,
    /// Byte range of text shown before this chunk wherever it is used (the
    /// header row of a spreadsheet, so cell values keep their column names).
    pub prefix_start: Option<usize>,
    pub prefix_end: Option<usize>,
    /// Table-of-contents text: kept for page counts, never searched or shown
    /// as evidence (it repeats every heading and so matches every question).
    pub is_toc: bool,
}

/// Text of a chunk as indexed and shown: optional prefix (e.g. a sheet's
/// header row) followed by the chunk, markers removed.
pub fn chunk_text_with_prefix(text: &str, start: usize, end: usize, prefix: Option<(usize, usize)>) -> String {
    let end = end.min(text.len());
    if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return String::new();
    }
    let body = clean_text(&text[start..end]);
    match prefix {
        Some((ps, pe)) if ps < pe && pe <= text.len() && text.is_char_boundary(ps) && text.is_char_boundary(pe) && pe <= start => {
            format!("{}\n{}", clean_text(&text[ps..pe]), body)
        }
        _ => body,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DefinedTerm {
    pub term: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Clone)]
struct Heading {
    level: usize,
    label: String,
}

/// Remove `[[page:N]]`-style marker lines so text shown to the model or fed
/// to an index contains only document content.
pub fn clean_text(text: &str) -> String {
    ANY_MARKER_LINE.replace_all(text, "").trim().to_string()
}

/// Citation such as `[p.47, Section 12.3: Cap on liability]`.
pub fn citation(
    page_start: Option<i32>,
    page_end: Option<i32>,
    location_label: Option<&str>,
    section_label: Option<&str>,
    para_start: Option<i32>,
    para_end: Option<i32>,
) -> String {
    if let (Some(l), Some(s), Some(e)) = (location_label, para_start, para_end) {
        if l.starts_with("Sheet:") {
            let rows = if s == e { format!("row {}", s) } else { format!("rows {}-{}", s, e) };
            return format!("[{}, {}]", l, rows);
        }
    }
    let location = match (page_start, page_end) {
        (Some(s), Some(e)) if s == e => Some(format!("p.{}", s)),
        (Some(s), Some(e)) => Some(format!("pp.{}-{}", s, e)),
        (Some(s), None) => Some(format!("p.{}", s)),
        _ => match location_label {
            Some(l) => Some(l.to_string()),
            None => match (para_start, para_end) {
                (Some(s), Some(e)) if s == e => Some(format!("P{}", s + 1)),
                (Some(s), Some(e)) => Some(format!("P{}-{}", s + 1, e + 1)),
                _ => None,
            },
        },
    };
    match (location, section_label) {
        (Some(l), Some(s)) => format!("[{}, {}]", l, s),
        (Some(l), None) => format!("[{}]", l),
        (None, Some(s)) => format!("[{}]", s),
        (None, None) => String::new(),
    }
}

fn capitalize(word: &str) -> String {
    let mut c = word.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn number_depth(number: &str) -> usize {
    let dotted = number.split('.').filter(|p| !p.trim().is_empty()).count().max(1);
    let parens = number.matches('(').count();
    dotted + parens
}

/// "When must I register my company?" — FAQ-style guides title each
/// section with a question. Short, capitalised, ends with "?".
fn is_question_heading(line: &str) -> bool {
    let t = line.trim();
    if !t.ends_with('?') || t.len() > 160 || BULLET.is_match(t) {
        return false;
    }
    let words = t.split_whitespace().count();
    (2..=20).contains(&words)
        && t.chars().find(|c| c.is_alphabetic()).map(|c| c.is_uppercase()).unwrap_or(false)
}

fn is_list_item(line: &str) -> bool {
    BULLET.is_match(line.trim_start())
}

/// Heading of a paragraph, including an all-caps title broken over two
/// lines ("3. CERTIFY THAT YOUR COMPANY" / "MEETS THE REQUIREMENTS").
fn detect_heading(paragraph: &str) -> Option<Heading> {
    let mut heading = detect_heading_line(paragraph)?;
    let mut lines = paragraph.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    if let Some(second) = lines.next() {
        let first_is_caps = first == first.to_uppercase() && first.chars().any(|c| c.is_alphabetic());
        let second_is_caps_title = second == second.to_uppercase()
            && HEADING_ALLCAPS.is_match(second)
            && !second.starts_with(|c: char| c.is_ascii_digit());
        if first_is_caps && second_is_caps_title && heading.label.chars().count() + second.len() <= 160 {
            heading.label = format!("{} {}", heading.label, second);
        }
    }
    Some(heading)
}

fn detect_heading_line(paragraph: &str) -> Option<Heading> {
    let first_line = paragraph.lines().next().unwrap_or("").trim();
    if first_line.is_empty() || first_line.len() > 160 {
        return None;
    }
    // Table-of-contents entries look like headings but are not.
    if DOT_LEADER.is_match(first_line) {
        return None;
    }
    if is_question_heading(first_line) {
        // Deeper than any numbered level, so a run of questions sits under
        // the section they belong to and each replaces the previous one.
        return Some(Heading { level: 9, label: first_line.to_string() });
    }

    if let Some(caps) = HEADING_LABELED.captures(first_line) {
        let kind = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let number = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        let title = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
        // "Section 5 of the Act applies" is prose, not a heading.
        let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
        let after_kind = whole.trim_start()[kind.len()..].chars().next();
        let separated = kind == "§" || kind.ends_with('.') || after_kind.map(|c| c.is_whitespace()).unwrap_or(false);
        let starts_lowercase = title.chars().next().map(|c| c.is_lowercase()).unwrap_or(false);
        let prose = title.len() > 60
            || starts_lowercase
            || (title.ends_with('.') && title.split_whitespace().count() > 6);
        if separated && !prose {
            let kind_lower = kind.to_lowercase();
            let top_level = matches!(
                kind_lower.as_str(),
                "article" | "artículo" | "articulo" | "artigo" | "artikel" | "articolo" | "art."
                    | "chapter" | "capítulo" | "capitulo" | "kapitel" | "chapitre" | "part"
                    | "parte" | "teil" | "titre" | "título" | "titulo" | "schedule" | "annex"
                    | "anexo" | "anlage" | "annexe" | "allegato" | "exhibit" | "appendix"
                    | "apéndice" | "apendice" | "статья" | "глава" | "раздел"
            );
            let level = if top_level { 1 } else { number_depth(number) };
            let kind_label = if kind == "§" { "§".to_string() } else { capitalize(kind) };
            let label = if title.is_empty() {
                format!("{} {}", kind_label, number)
            } else {
                format!("{} {}: {}", kind_label, number, title)
            };
            return Some(Heading { level, label });
        }
    }

    if let Some(caps) = HEADING_CJK.captures(first_line) {
        let marker = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        let level = if marker.ends_with('章') || marker.ends_with('编') || marker.ends_with('編') || marker.ends_with('部') { 1 } else { 2 };
        let label = if title.is_empty() { marker.to_string() } else { format!("{} {}", marker, title) };
        return Some(Heading { level, label });
    }

    if HEADING_RECITALS.is_match(first_line) {
        return Some(Heading { level: 1, label: first_line.chars().take(60).collect() });
    }

    if first_line == first_line.to_uppercase()
        && HEADING_ALLCAPS.is_match(first_line)
        && first_line.chars().filter(|c| c.is_alphabetic()).count() >= 3
    {
        return Some(Heading { level: 1, label: first_line.to_string() });
    }

    if first_line.chars().count() > 90 {
        return None;
    }
    if let Some(caps) = HEADING_NUMBERED.captures(first_line) {
        let marker = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let rest = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        let starts_capitalized = rest.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        let short_title = rest.chars().count() <= 70 && !rest.ends_with('.') && !rest.ends_with(';');
        if starts_capitalized && short_title {
            let level = if marker.starts_with('(') {
                3
            } else {
                marker.trim_end_matches(|c| c == '.' || c == ')').split('.').count()
            };
            return Some(Heading { level, label: format!("{} {}", marker, rest) });
        }
    }
    None
}

enum Location {
    Page(i32),
    Named(String),
    Clear,
}

/// Returns the location marker on the paragraph's first line (if any) and
/// the byte offset where the paragraph's content begins.
fn split_location_marker(paragraph: &str) -> (Option<Location>, usize) {
    let first_end = paragraph.find('\n').unwrap_or(paragraph.len());
    let first = &paragraph[..first_end];
    let Some(caps) = LOCATION_MARKER.captures(first.trim()) else {
        return (None, 0);
    };
    let kind = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    let location = match kind {
        "page" => value.trim().parse::<i32>().ok().map(Location::Page),
        "slide" => value.trim().parse::<i32>().ok().map(|n| Location::Named(format!("Slide {}", n))),
        "sheet" => Some(Location::Named(format!("Sheet: {}", value))),
        "image" => value.trim().parse::<i32>().ok().map(|n| Location::Named(format!("Image {}", n))),
        "clear" => Some(Location::Clear),
        _ => None,
    };
    let content_start = if first_end < paragraph.len() { first_end + 1 } else { paragraph.len() };
    (location, content_start)
}

fn trim_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let slice = &text[start..end];
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    let s = start + leading;
    let e = end - trailing;
    if s >= e { (start, start) } else { (s, e) }
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Best split point in `text[start..limit]`: a sentence end, then a line
/// break, then whitespace, then a hard char boundary.
fn split_point(text: &str, start: usize, limit: usize) -> usize {
    let limit = floor_boundary(text, limit);
    let window_start = floor_boundary(text, start + (limit - start) / 2);
    let window = &text[window_start..limit];
    let sentence_ends = [". ", "; ", "! ", "? ", "。", "；", "！", "？", ".\n", ";\n", "\n"];
    let mut best: Option<usize> = None;
    for pat in sentence_ends {
        if let Some(pos) = window.rfind(pat) {
            let at = window_start + pos + pat.len();
            best = Some(best.map_or(at, |b: usize| b.max(at)));
        }
    }
    if let Some(b) = best {
        return b;
    }
    if let Some(pos) = window.rfind(char::is_whitespace) {
        return window_start + pos + 1;
    }
    limit
}

/// Content byte ranges of each line (line terminators excluded).
fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            let mut end = i;
            if end > start && text.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
            out.push((start, end));
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

fn boilerplate_key(line: &str) -> String {
    line.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Line indices of running headers/footers: lines at the top or bottom of a
/// page that recur on at least half the pages (e.g. "September 2024",
/// "Confidential"), and bare page numbers. They carry no content and would
/// otherwise open every page's first chunk.
fn boilerplate_lines(text: &str, lines: &[(usize, usize)]) -> std::collections::HashSet<usize> {
    use std::collections::{HashMap, HashSet};
    // Group content lines by page.
    let mut pages: Vec<Vec<usize>> = Vec::new();
    for (i, (s, e)) in lines.iter().enumerate() {
        let t = text[*s..*e].trim();
        if t.starts_with("[[page:") && LOCATION_MARKER.is_match(t) {
            pages.push(Vec::new());
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if let Some(p) = pages.last_mut() {
            p.push(i);
        }
    }
    let mut out = HashSet::new();
    if pages.len() < 3 {
        return out;
    }
    let edge = |p: &Vec<usize>| -> Vec<usize> {
        let mut v: Vec<usize> = p.iter().take(3).copied().collect();
        v.extend(p.iter().rev().take(3).copied());
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut seen_on: HashMap<String, HashSet<usize>> = HashMap::new();
    for (page_no, p) in pages.iter().enumerate() {
        for i in edge(p) {
            let (s, e) = lines[i];
            seen_on.entry(boilerplate_key(&text[s..e])).or_default().insert(page_no);
        }
    }
    let threshold = ((pages.len() as f64) * 0.5).ceil().max(3.0) as usize;
    for p in &pages {
        for i in edge(p) {
            let (s, e) = lines[i];
            let key = boilerplate_key(&text[s..e]);
            let page_number_only = !key.is_empty()
                && key.len() <= 12
                && key.chars().all(|c| c == '#' || c == ' ' || c == '-' || c == '/' || c == '|')
                && key.contains('#');
            let recurring = seen_on.get(&key).map(|pgs| pgs.len() >= threshold).unwrap_or(false);
            if page_number_only || (recurring && key.chars().count() <= 80) {
                out.insert(i);
            }
        }
    }
    out
}

/// Split text into the units the chunker groups: blank-line paragraphs, and
/// within them headings, list items and runs of body lines. Many PDFs put a
/// whole page on single line breaks with no blank lines, so headings like
/// "When must I register my company?" only become visible line by line.
fn segments(text: &str) -> Vec<(usize, usize)> {
    let lines = line_ranges(text);
    let skip = boilerplate_lines(text, &lines);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut cur: Option<(usize, usize)> = None;
    let mut cur_is_heading = false;
    let mut in_sheet = false;

    fn close(cur: &mut Option<(usize, usize)>, out: &mut Vec<(usize, usize)>, cur_is_heading: &mut bool) {
        if let Some(c) = cur.take() {
            out.push(c);
        }
        *cur_is_heading = false;
    }

    for (idx, (s, e)) in lines.iter().copied().enumerate() {
        let t = text[s..e].trim();
        if t.is_empty() {
            close(&mut cur, &mut out, &mut cur_is_heading);
            continue;
        }
        if t.starts_with("[[") && LOCATION_MARKER.is_match(t) {
            close(&mut cur, &mut out, &mut cur_is_heading);
            in_sheet = t.starts_with("[[sheet:");
            out.push((s, e));
            continue;
        }
        if in_sheet {
            // Spreadsheet rows are already one per paragraph.
            match cur {
                Some(ref mut c) => c.1 = e,
                None => cur = Some((s, e)),
            }
            continue;
        }
        if skip.contains(&idx) {
            close(&mut cur, &mut out, &mut cur_is_heading);
            continue;
        }
        if detect_heading(t).is_some() {
            // A heading broken over two lines ("3. CERTIFY THAT YOUR COMPANY"
            // / "MEETS THE REQUIREMENTS") stays one heading.
            let caps_continuation = cur_is_heading
                && t == t.to_uppercase()
                && HEADING_ALLCAPS.is_match(t)
                && !t.starts_with(|c: char| c.is_ascii_digit());
            if caps_continuation {
                if let Some(ref mut c) = cur {
                    c.1 = e;
                }
                continue;
            }
            close(&mut cur, &mut out, &mut cur_is_heading);
            cur = Some((s, e));
            cur_is_heading = true;
            continue;
        }
        if cur_is_heading || is_list_item(t) {
            close(&mut cur, &mut out, &mut cur_is_heading);
        }
        match cur {
            Some(ref mut c) => c.1 = e,
            None => cur = Some((s, e)),
        }
    }
    close(&mut cur, &mut out, &mut cur_is_heading);
    out
}

/// A chunk is table-of-contents text when several of its lines end in
/// leader dots and a page number.
pub fn is_toc_text(text: &str) -> bool {
    let entries = text.lines().filter(|l| DOT_LEADER.is_match(l.trim_end())).count();
    entries >= 2
}

/// Chunk page-tagged extracted text into citation-anchored byte ranges.
pub fn chunk_document(text: &str) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    if text.trim().is_empty() {
        return chunks;
    }

    let mut heading_stack: Vec<Heading> = Vec::new();
    let mut current_page: Option<i32> = None;
    let mut current_location: Option<String> = None;
    let mut paragraph_index: i32 = 0;
    // Spreadsheet state: header row of the current sheet and the row number.
    let mut sheet_header: Option<(usize, usize)> = None;
    let mut sheet_row: i32 = 0;

    // The chunk being accumulated.
    let mut open: Option<Chunk> = None;

    let section_path = |stack: &Vec<Heading>| -> Option<String> {
        if stack.is_empty() {
            None
        } else {
            Some(stack.iter().map(|h| h.label.as_str()).collect::<Vec<_>>().join(" › "))
        }
    };

    let flush = |open: &mut Option<Chunk>, chunks: &mut Vec<Chunk>| {
        if let Some(c) = open.take() {
            if c.byte_end > c.byte_start {
                chunks.push(c);
            }
        }
    };

    // Walk the text unit by unit (see `segments`), tracking absolute offsets.
    for (para_start_abs, para_end_abs) in segments(text) {
        let raw = &text[para_start_abs..para_end_abs];
        if raw.trim().is_empty() {
            continue;
        }

        let (location, content_off) = split_location_marker(raw);
        let mut boundary = false;
        match location {
            Some(Location::Page(p)) => {
                if current_page != Some(p) {
                    boundary = true;
                }
                current_page = Some(p);
                current_location = None;
            }
            Some(Location::Named(l)) => {
                if current_location.as_deref() != Some(l.as_str()) {
                    boundary = true;
                }
                current_location = Some(l);
                current_page = None;
            }
            Some(Location::Clear) => {
                if current_location.is_some() || current_page.is_some() {
                    boundary = true;
                }
                current_location = None;
                current_page = None;
            }
            None => {}
        }

        let (cs, ce) = trim_range(text, para_start_abs + content_off, para_end_abs);
        if boundary {
            flush(&mut open, &mut chunks);
            sheet_header = None;
            sheet_row = 0;
        }

        // Spreadsheets: each row is a paragraph. Group a few rows per chunk,
        // number them, and carry the header row with every chunk.
        if current_location.as_deref().map(|l| l.starts_with("Sheet:")).unwrap_or(false) {
            // Count the row only once it is known to HAVE content.
            //
            // `segments()` emits the `[[sheet:Name]]` marker as a segment of
            // its own (see the `in_sheet` branch there), and stripping the
            // marker leaves it empty. Incrementing before this guard meant
            // that empty segment silently consumed row number 1, so every
            // sheet began at row 2 and every row citation in a workbook was
            // one too high: "[Sheet: Fees, rows 2-9]" for what are rows 1-8.
            //
            // The off-by-one applied to the first sheet as much as the rest,
            // which is why the test that caught it ("row numbers restart per
            // sheet") described it as a per-sheet reset problem. The reset
            // was always working; the start was wrong everywhere.
            if cs >= ce {
                continue;
            }
            sheet_row += 1;
            if sheet_header.is_none() {
                sheet_header = Some((cs, ce));
            }
            if let Some(ref c) = open {
                if ce - c.byte_start > SHEET_TARGET_BYTES {
                    flush(&mut open, &mut chunks);
                }
            }
            match open {
                Some(ref mut c) => {
                    c.byte_end = ce;
                    c.para_end = sheet_row;
                }
                None => {
                    let prefix = sheet_header.filter(|(hs, _)| *hs != cs);
                    open = Some(Chunk {
                        byte_start: cs,
                        byte_end: ce,
                        page_start: None,
                        page_end: None,
                        location_label: current_location.clone(),
                        section_label: None,
                        section_path: None,
                        para_start: sheet_row,
                        para_end: sheet_row,
                        prefix_start: prefix.map(|p| p.0),
                        prefix_end: prefix.map(|p| p.1),
                        is_toc: false,
                    });
                }
            }
            paragraph_index += 1;
            continue;
        }

        if cs >= ce {
            continue;
        }
        let content = &text[cs..ce];

        if let Some(h) = detect_heading(content) {
            flush(&mut open, &mut chunks);
            while heading_stack.last().map(|top| top.level >= h.level).unwrap_or(false) {
                heading_stack.pop();
            }
            heading_stack.push(h);
        }
        let label = heading_stack.last().map(|h| h.label.clone());
        let path = section_path(&heading_stack);

        if ce - cs > HARD_MAX_BYTES {
            flush(&mut open, &mut chunks);
            let mut start = cs;
            loop {
                let end = if ce - start <= TARGET_BYTES {
                    ce
                } else {
                    let sp = split_point(text, start, start + TARGET_BYTES);
                    if sp <= start { floor_boundary(text, start + TARGET_BYTES) } else { sp }
                };
                let (ts, te) = trim_range(text, start, end);
                if te > ts {
                    chunks.push(Chunk {
                        byte_start: ts,
                        byte_end: te,
                        page_start: current_page,
                        page_end: current_page,
                        location_label: current_location.clone(),
                        section_label: label.clone(),
                        section_path: path.clone(),
                        para_start: paragraph_index,
                        para_end: paragraph_index,
                        prefix_start: None,
                        prefix_end: None,
                        is_toc: false,
                    });
                }
                if end >= ce {
                    break;
                }
                let mut next = floor_boundary(text, end.saturating_sub(SPLIT_OVERLAP_BYTES));
                if let Some(ws) = text[next..end].find(char::is_whitespace) {
                    next += ws + 1;
                }
                start = if next > start && next < end { next } else { end };
            }
            paragraph_index += 1;
            continue;
        }

        if let Some(ref c) = open {
            if ce - c.byte_start > TARGET_BYTES {
                flush(&mut open, &mut chunks);
            }
        }
        match open {
            Some(ref mut c) => {
                c.byte_end = ce;
                c.para_end = paragraph_index;
                c.page_end = current_page.or(c.page_end);
            }
            None => {
                open = Some(Chunk {
                    byte_start: cs,
                    byte_end: ce,
                    page_start: current_page,
                    page_end: current_page,
                    location_label: current_location.clone(),
                    section_label: label,
                    section_path: path,
                    para_start: paragraph_index,
                    para_end: paragraph_index,
                    prefix_start: None,
                    prefix_end: None,
                    is_toc: false,
                });
            }
        }
        paragraph_index += 1;
    }
    flush(&mut open, &mut chunks);
    for c in &mut chunks {
        c.is_toc = is_toc_text(&text[c.byte_start..c.byte_end]);
    }
    chunks
}

/// Defined terms (`"Material Adverse Effect" means ...`, `(the "Buyer")`).
pub fn extract_defined_terms(text: &str) -> Vec<DefinedTerm> {
    let mut out: Vec<DefinedTerm> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for re in [&*DEFINED_TERM_MEANS, &*DEFINED_TERM_PAREN] {
        for caps in re.captures_iter(text) {
            let (Some(whole), Some(term)) = (caps.get(0), caps.get(1)) else { continue };
            let t = term.as_str().trim();
            if t.chars().count() < 2 || t.split_whitespace().count() > 8 {
                continue;
            }
            let key = t.to_lowercase();
            if seen.insert(key) {
                out.push(DefinedTerm {
                    term: t.to_string(),
                    byte_start: whole.start(),
                    byte_end: whole.end(),
                });
            }
        }
    }
    out
}

/// Numbers that identify a section (`12.3(b)` -> ["12", "3", "(b)"]), used to
/// match "clause 12.3" against stored section paths.
/// Parse Arabic digits or Chinese numerals ("十二" -> 12).
pub fn cjk_number(s: &str) -> Option<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let digit = |c: char| -> Option<i64> {
        Some(match c {
            '零' | '〇' => 0, '一' => 1, '二' => 2, '三' => 3, '四' => 4,
            '五' => 5, '六' => 6, '七' => 7, '八' => 8, '九' => 9,
            _ => return None,
        })
    };
    let mut total = 0i64;
    let mut current = 0i64;
    for c in s.chars() {
        match c {
            '十' => { total += current.max(1) * 10; current = 0; }
            '百' => { total += current.max(1) * 100; current = 0; }
            '千' => { total += current.max(1) * 1000; current = 0; }
            _ => current = digit(c)?,
        }
    }
    Some(total + current)
}

pub fn section_number_key(label: &str) -> String {
    if let Some(caps) = HEADING_CJK_NUMBER.captures(label) {
        if let Some(n) = caps.get(1).and_then(|m| cjk_number(m.as_str())) {
            return n.to_string();
        }
    }
    let lower = label.to_lowercase();
    let caps = SECTION_NUMBER.captures(&lower).or_else(|| {
        // Roman numerals only count right after a heading word ("Article IV").
        let after_kind = lower.split_whitespace().nth(1).unwrap_or("");
        SECTION_ROMAN.captures(after_kind)
    });
    let Some(caps) = caps else { return String::new() };
    let mut parts: Vec<String> = caps.get(1).map(|m| m.as_str().split('.').map(|p| p.to_string()).collect()).unwrap_or_default();
    if let Some(paren) = caps.get(2) {
        parts.extend(PAREN_PART.find_iter(paren.as_str()).map(|m| m.as_str().to_string()));
    }
    parts.retain(|p| !p.is_empty());
    parts.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice<'a>(text: &'a str, c: &Chunk) -> &'a str {
        &text[c.byte_start..c.byte_end]
    }

    /// Pages whose bodies differ by more than a digit — see
    /// `eval::distinct_page_text` for why that matters here.
    #[test]
    fn every_paragraph_is_covered_and_ranges_are_valid() {
        use crate::rag::eval::distinct_page_text;
        let text = (1..=40)
            .map(|p| format!("[[page:{}]]\nPage {} text. {}", p, p, distinct_page_text(p)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_document(&text);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(c.byte_end <= text.len() && c.byte_start < c.byte_end);
            assert!(text.is_char_boundary(c.byte_start) && text.is_char_boundary(c.byte_end));
            assert!(!slice(&text, c).contains("[[page:"), "markers are outside content ranges");
        }
        for p in 1..=40 {
            assert!(
                chunks.iter().any(|c| c.page_start == Some(p) && slice(&text, c).contains(&format!("Page {} text", p))),
                "page {} must be retrievable by its own chunk",
                p
            );
        }
    }

    #[test]
    fn a_thousand_page_document_is_not_capped() {
        let page = "The Borrower shall repay the Loan in full on the Maturity Date together with accrued interest. ".repeat(30);
        let text = (1..=1000).map(|p| format!("[[page:{}]]\n{}", p, page)).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_document(&text);
        assert!(chunks.len() >= 1000, "one or more chunks per page, got {}", chunks.len());
        assert!(chunks.iter().all(|c| c.byte_end - c.byte_start <= HARD_MAX_BYTES));
        assert!(chunks.iter().any(|c| c.page_start == Some(1000)));
    }

    #[test]
    fn section_paths_are_hierarchical() {
        let text = "ARTICLE 12 INDEMNIFICATION\n\n12.1 General\n\nThe Seller shall indemnify the Buyer.\n\n12.3 Cap on Liability\n\nLiability shall not exceed USD 10,000,000.\n\nARTICLE 13 TERMINATION\n\nEither party may terminate.";
        let chunks = chunk_document(text);
        let cap = chunks.iter().find(|c| slice(text, c).contains("10,000,000")).unwrap();
        let path = cap.section_path.clone().unwrap();
        assert!(path.contains("ARTICLE 12") || path.contains("Article 12"), "{}", path);
        assert!(path.contains("12.3"), "{}", path);
        let term = chunks.iter().find(|c| slice(text, c).contains("Either party")).unwrap();
        assert!(!term.section_path.clone().unwrap().contains("12.3"));
    }

    #[test]
    fn multilingual_headings_are_detected() {
        for (heading, expect) in [
            ("Artículo 5. Ley aplicable", "Artículo 5"),
            ("Artikel 7 Kündigung", "Artikel 7"),
            ("Clause 9 - Confidentiality", "Clause 9"),
            ("第十二条 违约责任", "第十二条"),
            ("§ 4 Haftung", "§ 4"),
            ("खंड 3 किराया", "खंड 3"),
        ] {
            let text = format!("{}\n\nBody text of the provision goes here.", heading);
            let chunks = chunk_document(&text);
            let label = chunks[0].section_label.clone().unwrap_or_default();
            assert!(label.starts_with(expect), "{} -> {}", heading, label);
        }
    }

    #[test]
    fn giant_paragraph_splits_under_hard_max_with_valid_boundaries() {
        let text = "Ce contrat est régi par le droit français et toute contestation sera soumise aux tribunaux compétents. ".repeat(200);
        let chunks = chunk_document(&text);
        assert!(chunks.len() > 5);
        for c in &chunks {
            assert!(c.byte_end - c.byte_start <= HARD_MAX_BYTES);
            assert!(text.is_char_boundary(c.byte_start) && text.is_char_boundary(c.byte_end));
        }
        assert_eq!(chunks.last().unwrap().byte_end, text.trim_end().len());
    }

    #[test]
    fn cjk_text_splits_on_char_boundaries() {
        let text = "本协议受中华人民共和国法律管辖。".repeat(400);
        let chunks = chunk_document(&text);
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert!(text.is_char_boundary(c.byte_start) && text.is_char_boundary(c.byte_end));
        }
    }

    #[test]
    fn defined_terms_are_found() {
        let text = "\"Material Adverse Effect\" means any event that is materially adverse. ABC Ltd (the \"Buyer\") agrees. “Closing Date” shall have the meaning given in Clause 2. \"Fecha de Cierre\" significa el día.";
        let terms: Vec<String> = extract_defined_terms(text).into_iter().map(|t| t.term).collect();
        for t in ["Material Adverse Effect", "Buyer", "Closing Date", "Fecha de Cierre"] {
            assert!(terms.iter().any(|x| x == t), "missing {} in {:?}", t, terms);
        }
    }

    #[test]
    fn spreadsheet_rows_are_small_numbered_chunks_carrying_the_header() {
        let mut text = String::from("[[sheet:Fees]]\nParty\tService\tAmount\tDue");
        for i in 2..=120 {
            text.push_str(&format!("\n\nClient {}\tConsulting\t{}\t2026-0{}-15", i, i * 1000, 1 + i % 9));
        }
        text.push_str("\n\n[[sheet:Summary]]\nTotal\tValue\n\nGrand total\t7259000");
        let chunks = chunk_document(&text);
        let fees: Vec<&Chunk> = chunks.iter().filter(|c| c.location_label.as_deref() == Some("Sheet: Fees")).collect();
        assert!(fees.len() > 5, "rows must be split into several small chunks, got {}", fees.len());
        assert!(fees.iter().all(|c| c.byte_end - c.byte_start <= SHEET_TARGET_BYTES + 200));
        let row_77 = fees.iter().find(|c| c.para_start <= 77 && c.para_end >= 77).unwrap();
        let shown = chunk_text_with_prefix(&text, row_77.byte_start, row_77.byte_end, row_77.prefix_start.zip(row_77.prefix_end));
        assert!(shown.starts_with("Party\tService\tAmount\tDue"), "header must lead: {}", shown);
        assert!(shown.contains("Client 77\tConsulting\t77000"), "{}", shown);
        let cite = citation(None, None, row_77.location_label.as_deref(), None, Some(row_77.para_start), Some(row_77.para_end));
        assert!(cite.starts_with("[Sheet: Fees, rows"), "{}", cite);

        // Row numbering starts at 1 on the FIRST sheet, not just on later
        // ones. This is the assertion that was missing: the `[[sheet:Fees]]`
        // marker is a segment of its own and used to consume row 1, so every
        // sheet in every workbook began at 2 and every row citation was one
        // too high. The old test only checked the second sheet, where the
        // same off-by-one looked like a failure to reset per sheet.
        assert_eq!(fees[0].para_start, 1, "the first sheet's first row is row 1");

        // And the numbers must line up with the rows a person can count: the
        // header is row 1, so "Client N" is row N. A citation that is merely
        // self-consistent is no use for checking an answer against the file.
        let row_for = |n: i32| {
            fees.iter()
                .find(|c| c.para_start <= n && c.para_end >= n)
                .map(|c| {
                    chunk_text_with_prefix(&text, c.byte_start, c.byte_end, c.prefix_start.zip(c.prefix_end))
                })
                .unwrap_or_default()
        };
        assert!(row_for(1).starts_with("Party\tService\tAmount\tDue"), "row 1 is the header: {}", row_for(1));
        for n in [2, 50, 120] {
            assert!(
                row_for(n).contains(&format!("Client {}\tConsulting", n)),
                "row {n} should hold Client {n}: {}",
                row_for(n)
            );
        }

        let summary = chunks.iter().find(|c| c.location_label.as_deref() == Some("Sheet: Summary")).unwrap();
        assert_eq!(summary.para_start, 1, "row numbers restart per sheet");
        assert!(summary.prefix_start.is_none(), "the chunk holding the header needs no prefix");
    }

    /// Shape of pdfium output for many real PDFs: each page is a run of
    /// single line breaks, pages start with a running footer, and page 2 is a
    /// table of contents that repeats every heading.
    fn faq_pdf_text() -> String {
        let mut pages: Vec<String> = Vec::new();
        pages.push("[[page:1]]\nSeptember 2024\r\n1\r\nGuide to Program Eligibility\r\nUpdated: September 2024".to_string());
        pages.push("[[page:2]]\nSeptember 2024\r\n2\r\nContents\r\n1. OVERVIEW........................ 3\r\n2. REGISTER YOUR COMPANY........................ 3\r\nWho must register?........................ 3\r\nWhen must I register my company?........................ 4".to_string());
        pages.push("[[page:3]]\nSeptember 2024\r\n3\r\n1. OVERVIEW\r\nThe programs support small businesses. All awardees must certify at the time of award.\r\n2. REGISTER YOUR COMPANY\r\nWho must register?\r\n• All applicants must register on the program website.".to_string());
        pages.push("[[page:4]]\nSeptember 2024\r\n4\r\n• After registering, print the proof of registration form.\r\nWhen must I register my company?\r\n• You must register prior to submitting an application.\r\n3. CERTIFY THAT YOUR COMPANY\r\nMEETS THE REQUIREMENTS\r\nWhen must I certify regarding my firm's eligibility?\r\nAt the time of award. All awardees must sign a certification form at the time of award.".to_string());
        pages.join("\n\n")
    }

    #[test]
    fn single_line_break_pages_split_at_question_headings() {
        let text = faq_pdf_text();
        let chunks = chunk_document(&text);
        let answer = chunks
            .iter()
            .find(|c| slice(&text, c).contains("You must register prior to submitting an application"))
            .expect("answer chunk");
        assert_eq!(answer.page_start, Some(4));
        assert_eq!(answer.section_label.as_deref(), Some("When must I register my company?"), "{:?}", answer);
        let body = slice(&text, answer);
        assert!(!body.contains("certification form"), "the next question must start a new chunk: {:?}", body);
        assert!(answer.section_path.as_deref().unwrap_or("").contains("REGISTER YOUR COMPANY"), "{:?}", answer.section_path);

        let certify = chunks.iter().find(|c| slice(&text, c).contains("All awardees must sign")).unwrap();
        assert_eq!(certify.section_label.as_deref(), Some("When must I certify regarding my firm's eligibility?"));
        assert!(
            certify.section_path.as_deref().unwrap_or("").contains("MEETS THE REQUIREMENTS"),
            "two-line heading is one heading: {:?}",
            certify.section_path
        );
    }

    #[test]
    fn table_of_contents_is_flagged_and_footers_are_left_out() {
        let text = faq_pdf_text();
        let chunks = chunk_document(&text);
        let toc: Vec<&Chunk> = chunks.iter().filter(|c| c.is_toc).collect();
        assert!(!toc.is_empty() && toc.iter().all(|c| c.page_start == Some(2)), "{:?}", toc);
        assert!(chunks.iter().filter(|c| !c.is_toc).all(|c| !slice(&text, c).contains("........")));
        for c in &chunks {
            assert!(!slice(&text, c).starts_with("September 2024"), "running footer leaked into {:?}", slice(&text, c));
        }
    }

    /// The behaviour every attached PDF depends on, and which nothing pinned.
    ///
    /// Modelled on a real five-page OCR'd guide: a three-line institutional
    /// footer on every page, and real content between. Retrieval is only as
    /// good as this: strip too little and every excerpt opens with a postal
    /// address, strip too much and the answer is not in the index at all.
    ///
    /// `table_of_contents_is_flagged_and_footers_are_left_out` above checks
    /// that a footer does not LEAD a chunk. It does not check that the body
    /// survived, which is the half that loses answers.
    #[test]
    fn running_footers_are_stripped_and_every_line_of_body_text_survives() {
        const FOOTER: &str = "Email: intl.stu@example.edu. P: 903.886.5097. www.example.edu\n\
                              2200 Campbell st, Commerce, TX 75428\n\
                              A Member of the State University System";
        // One distinct, substantive sentence per page, as a real guide has.
        let bodies: [&str; 5] = [
            "Guidance for online completion of Form I-765 for the STEM extension.",
            "Select the STEM Extension eligibility category before continuing.",
            "List the degree level and major field of study for your qualifying degree.",
            "Upload a recent passport style photograph taken within the last six months.",
            "Do you want the SSA to issue you a Social Security card? Answer yes.",
        ];
        let text = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| format!("[[page:{}]]\n{}\n\n{}\n\n{}", i + 1, FOOTER, body, FOOTER))
            .collect::<Vec<_>>()
            .join("\n\n");

        let chunks = chunk_document(&text);
        let indexed: String = chunks.iter().map(|c| slice(&text, c)).collect::<Vec<_>>().join("\n");

        // The footer is gone, in every form: no line of it survives anywhere.
        for line in FOOTER.lines() {
            assert!(
                !indexed.contains(line),
                "running footer line leaked into the index: {line:?}"
            );
        }
        // And every sentence of actual content is still retrievable, on the
        // page it came from. Losing one of these is losing an answer.
        for (i, body) in bodies.iter().enumerate() {
            let page = (i + 1) as i32;
            assert!(
                chunks
                    .iter()
                    .any(|c| c.page_start == Some(page) && slice(&text, c).contains(body)),
                "page {page} body missing from the index: {body:?}"
            );
        }
    }

    /// Where the running-header rule stops, written down.
    ///
    /// A line is treated as boilerplate when it sits within three lines of a
    /// page edge AND recurs on at least half the pages (minimum three). Both
    /// halves matter, and this pins both: body text in the middle of a page
    /// is never touched however often it repeats, and a line that recurs on
    /// only a minority of pages is kept wherever it sits.
    ///
    /// The threshold is deliberately recorded rather than assumed. On a very
    /// short document (three pages) it is at its most aggressive: a genuine
    /// line appearing at the edge of all three pages will be removed. That is
    /// the right trade for headers and the wrong one for a three-page form,
    /// and anyone retuning `boilerplate_lines` should start here.
    #[test]
    fn boilerplate_removal_spares_mid_page_text_and_minority_repeats() {
        let repeated = "Commercial in confidence";
        // The same sentence on every page, far enough from both edges that
        // the first-three / last-three window cannot reach it. The page needs
        // enough content lines for a middle to exist at all: with only four,
        // every line is within three of one edge or the other.
        let buried = "The parties agree that time is of the essence in all respects.";
        let page = |n: usize| {
            let mut lines = vec![repeated.to_string()];
            for i in 1..=3 {
                lines.push(crate::rag::eval::distinct_page_text(n * 10 + i));
            }
            lines.push(buried.to_string());
            for i in 4..=7 {
                lines.push(crate::rag::eval::distinct_page_text(n * 10 + i));
            }
            format!("[[page:{n}]]\n{}", lines.join("\n\n"))
        };
        let text = (1..=6).map(page).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_document(&text);
        let indexed: String = chunks.iter().map(|c| slice(&text, c)).collect::<Vec<_>>().join("\n");

        assert!(!indexed.contains(repeated), "an edge line on every page is a header");
        assert_eq!(
            indexed.matches(buried).count(),
            6,
            "mid-page text is never boilerplate, however often it repeats"
        );

        // A line on only two of six pages is a minority: below the threshold,
        // so it stays even though it sits on the edge.
        let minority = "Draft for discussion only";
        let text2 = (1..=6)
            .map(|n| {
                if n <= 2 {
                    format!("[[page:{n}]]\n{minority}\nBody text for page {n} about an unrelated topic.")
                } else {
                    format!("[[page:{n}]]\nBody text for page {n} about an unrelated topic.")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let indexed2: String = chunk_document(&text2)
            .iter()
            .map(|c| slice(&text2, c))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            indexed2.contains(minority),
            "a line on 2 of 6 pages is below the threshold and must be kept"
        );
    }

    #[test]
    fn toc_lines_are_not_headings_and_questions_in_prose_need_capitals() {
        assert!(detect_heading("2. REGISTER YOUR COMPANY........................ 3").is_none());
        assert!(detect_heading("When must I register my company?").is_some());
        assert!(detect_heading("• Is this a list item?").is_none());
        assert!(!is_question_heading("what about lowercase questions?"));
    }

    #[test]
    fn clean_text_removes_marker_lines_only() {
        let t = "[[page:3]]\nHello world\n\n[[slide:2]]\nNext";
        assert_eq!(clean_text(t), "Hello world\n\nNext");
    }

    #[test]
    fn citation_formats() {
        assert_eq!(citation(Some(4), Some(4), None, Some("Section 2"), None, None), "[p.4, Section 2]");
        assert_eq!(citation(Some(4), Some(6), None, None, None, None), "[pp.4-6]");
        assert_eq!(citation(None, None, Some("Slide 3"), None, None, None), "[Slide 3]");
    }

    #[test]
    fn section_number_key_normalises() {
        assert_eq!(section_number_key("Section 12.3(b): Cap"), "12.3.(b)");
        assert_eq!(section_number_key("第十二条 争议解决"), "12");
        assert_eq!(section_number_key("第5条"), "5");
    }
}
