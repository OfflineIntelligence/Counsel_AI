//! Citation checking for answers.
//!
//! After an answer is generated, every bracketed citation that points into a
//! document (`[Lease.pdf, p.47, Section 12.3]`, `[p.12]`,
//! `[Fees.xlsx, Sheet: Invoices, rows 4-9]`) is checked against what the model
//! was actually given:
//!
//! * the cited document exists and was available to this answer;
//! * the cited page exists in it;
//! * the cited page was among the excerpts shown (or the document was shown
//!   in full);
//! * text quoted just before the citation really appears on that page.
//!
//! Problems are reported to the user under the answer; nothing is rewritten.

use std::collections::HashSet;

use lazy_static::lazy_static;
use regex::Regex;

use super::search::{page_text, Passage};
use crate::memory_db::MemoryDatabase;

lazy_static! {
    static ref BRACKET: Regex = Regex::new(r"\[([^\[\]\n]{1,240})\]").unwrap();
    static ref PAGE: Regex = Regex::new(r"(?i)\bpp?\.\s*(\d{1,5})(?:\s*[-–—]\s*(\d{1,5}))?").unwrap();
    static ref PAGE_WORD: Regex = Regex::new(r"(?i)\bpages?\s+(\d{1,5})(?:\s*[-–—]\s*(\d{1,5}))?").unwrap();
    static ref SHEET: Regex = Regex::new(r"(?i)Sheet:\s*([^,\]]+?)\s*(?:,|$)").unwrap();
    static ref FILE: Regex = Regex::new(r"(?i)([\w][\w\-. ()]{0,160}?\.(?:pdf|docx?|xlsx?|pptx?|txt|rtf|odt|md|csv|png|jpe?g))").unwrap();
    static ref QUOTE: Regex = Regex::new(r#"["“]([^"”\n]{20,400})["”]"#).unwrap();
}

/// What the answer was based on.
#[derive(Debug, Clone, Default)]
pub struct CitationInputs {
    pub passages: Vec<Passage>,
    /// Documents shown to the model in full.
    pub full_document_ids: Vec<i64>,
    /// Documents attached to the chat.
    pub session_document_ids: Vec<i64>,
    /// Documents named in the retrieved "earlier material" block, whose exact
    /// excerpt pages are not tracked.
    pub earlier_material: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CitationReport {
    pub checked: usize,
    pub issues: Vec<String>,
}

impl CitationReport {
    /// Text appended under the answer when something did not check out.
    pub fn note(&self) -> Option<String> {
        if self.issues.is_empty() {
            return None;
        }
        let mut s = format!(
            "\n\n---\n**Citation check:** {} of {} citation{} could not be confirmed against the documents.\n",
            self.issues.len(),
            self.checked,
            if self.checked == 1 { "" } else { "s" }
        );
        for issue in &self.issues {
            s.push_str(&format!("- {}\n", issue));
        }
        Some(s)
    }
}

struct Candidate {
    id: i64,
    filename: String,
}

fn normalise(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn page_range(inner: &str) -> Option<(i32, i32)> {
    let caps = PAGE.captures(inner).or_else(|| PAGE_WORD.captures(inner))?;
    let from: i32 = caps.get(1)?.as_str().parse().ok()?;
    let to: i32 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(from);
    Some((from.min(to), from.max(to)))
}

pub fn verify(db: &MemoryDatabase, answer: &str, inputs: &CitationInputs) -> CitationReport {
    let mut report = CitationReport::default();

    let mut ids: Vec<i64> = inputs.session_document_ids.clone();
    ids.extend(inputs.full_document_ids.iter().copied());
    ids.extend(inputs.passages.iter().map(|p| p.document_id));
    ids.sort_unstable();
    ids.dedup();
    let known: Vec<Candidate> = ids
        .iter()
        .filter_map(|id| db.documents.get_document_meta(*id).ok().map(|d| Candidate { id: d.id, filename: d.original_filename }))
        .collect();
    let earlier_lower = inputs.earlier_material.as_deref().unwrap_or("").to_lowercase();
    if known.is_empty() && earlier_lower.is_empty() {
        // Nothing was provided from documents, so there is nothing to check against.
        return report;
    }

    let mut last_end = 0usize;
    for caps in BRACKET.captures_iter(answer) {
        let (Some(whole), Some(inner_m)) = (caps.get(0), caps.get(1)) else { continue };
        if answer[whole.end()..].starts_with('(') {
            continue; // a markdown link, not a citation
        }
        let inner = inner_m.as_str();
        let pages = page_range(inner);
        let sheet = SHEET.captures(inner).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string());
        let file = FILE.captures(inner).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string());
        if pages.is_none() && sheet.is_none() && file.is_none() {
            continue; // not a document citation
        }
        report.checked += 1;
        let preceding = if last_end <= whole.start() { &answer[last_end..whole.start()] } else { "" };
        last_end = whole.end();
        let shown_as = format!("[{}]", inner);

        // Resolve the document.
        let doc: Option<&Candidate> = match &file {
            Some(f) => {
                let fl = f.to_lowercase();
                known.iter().find(|c| c.filename.to_lowercase() == fl).or_else(|| {
                    let stem = fl.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(fl.clone());
                    known.iter().find(|c| c.filename.to_lowercase().contains(&stem))
                })
            }
            None => {
                let with_page: Vec<&Candidate> = match pages {
                    Some((from, to)) => known
                        .iter()
                        .filter(|c| {
                            inputs.full_document_ids.contains(&c.id)
                                || inputs.passages.iter().any(|p| {
                                    p.document_id == c.id
                                        && p.page_start.map(|s| s <= from).unwrap_or(false)
                                        && p.page_end.or(p.page_start).map(|e| e >= to).unwrap_or(false)
                                })
                        })
                        .collect(),
                    None => Vec::new(),
                };
                if known.len() == 1 {
                    known.first()
                } else if with_page.len() == 1 {
                    with_page.first().copied()
                } else {
                    None
                }
            }
        };

        let Some(doc) = doc else {
            match &file {
                Some(f) if earlier_lower.contains(&f.to_lowercase()) => {}
                Some(f) => report.issues.push(format!("{} cites \"{}\", which was not provided for this answer.", shown_as, f)),
                None => {
                    if known.len() > 1 {
                        report.issues.push(format!(
                            "{} does not name a document, and more than one document could match.",
                            shown_as
                        ));
                    } else {
                        report.issues.push(format!("{} does not match any document provided for this answer.", shown_as));
                    }
                }
            }
            continue;
        };

        let text = match db.documents.get_document(doc.id) {
            Ok(d) => d.extracted_text,
            Err(_) => continue,
        };

        if let Some((from, to)) = pages {
            let count = db.rag.page_count(doc.id).ok().flatten();
            if let Some(n) = count {
                if to > n {
                    report.issues.push(format!(
                        "{} cites page {} of {}, which has {} page{}.",
                        shown_as,
                        to,
                        doc.filename,
                        n,
                        if n == 1 { "" } else { "s" }
                    ));
                    continue;
                }
            }
            let shown_in_full = inputs.full_document_ids.contains(&doc.id);
            let shown_in_excerpts = inputs.passages.iter().any(|p| {
                p.document_id == doc.id
                    && p.page_start.map(|s| s <= to).unwrap_or(false)
                    && p.page_end.or(p.page_start).map(|e| e >= from).unwrap_or(false)
            });
            let in_earlier_material = earlier_lower.contains(&doc.filename.to_lowercase());
            if !shown_in_full && !shown_in_excerpts && !in_earlier_material {
                report.issues.push(format!(
                    "{} cites {} of {}, but that page was not among the excerpts the answer was based on.",
                    shown_as,
                    if from == to { format!("page {}", from) } else { format!("pages {}-{}", from, to) },
                    doc.filename
                ));
                continue;
            }
        }

        if let Some(ref name) = sheet {
            let label = format!("Sheet: {}", name);
            let exists = db
                .rag
                .sheet_labels(Some(&[doc.id].into_iter().collect::<HashSet<i64>>()))
                .map(|v| v.iter().any(|(l, _)| l.eq_ignore_ascii_case(&label)))
                .unwrap_or(true);
            if !exists {
                report.issues.push(format!("{} cites sheet \"{}\", which does not exist in {}.", shown_as, name, doc.filename));
                continue;
            }
        }

        // Quoted text just before the citation must appear where it is cited.
        let scope_text = match pages {
            Some((from, to)) => page_text(&text, from, to).unwrap_or_else(|| text.clone()),
            None => text.clone(),
        };
        let haystack = normalise(&scope_text);
        let window_start = preceding.len().saturating_sub(600);
        let mut ws = window_start;
        while ws > 0 && !preceding.is_char_boundary(ws) {
            ws -= 1;
        }
        for q in QUOTE.captures_iter(&preceding[ws..]) {
            let Some(quoted) = q.get(1) else { continue };
            let needle = normalise(quoted.as_str());
            if needle.split_whitespace().count() < 4 {
                continue;
            }
            if !haystack.contains(&needle) {
                let short: String = quoted.as_str().chars().take(80).collect();
                report.issues.push(format!(
                    "{}: the quoted text \"{}{}\" was not found {} of {}.",
                    shown_as,
                    short,
                    if quoted.as_str().chars().count() > 80 { "…" } else { "" },
                    match pages {
                        Some((f, t)) if f == t => format!("on page {}", f),
                        Some((f, t)) => format!("on pages {}-{}", f, t),
                        None => "anywhere".to_string(),
                    },
                    doc.filename
                ));
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::NewDocument;

    fn doc(db: &MemoryDatabase, name: &str, text: &str) -> i64 {
        db.documents
            .upsert_document(NewDocument {
                original_bytes: format!("{}{}", name, text).as_bytes(),
                original_filename: name,
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: text.len() as i64,
                extracted_text: text.to_string(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap()
            .id
    }

    fn passage(id: i64, name: &str, from: i32, to: i32) -> Passage {
        Passage {
            document_id: id,
            filename: name.to_string(),
            page_start: Some(from),
            page_end: Some(to),
            section_label: None,
            citation: String::new(),
            text: String::new(),
            score: 1.0,
            truncated: false,
            version_label: None,
        }
    }

    /// A ten-page lease whose page 4 carries the break clause.
    ///
    /// The other nine pages used to read "General provisions apply to this
    /// page." verbatim, which the chunker's running-header detector removed
    /// on sight: nine identical lines across ten pages is a running header by
    /// any reasonable definition. The document therefore reached the citation
    /// checker as page 4 and nothing else, and the issues it reported came
    /// back in a different order than this file asserts.
    ///
    /// `eval::distinct_page_text` gives each page its own wording, which is
    /// what the checker needs anyway: verifying a quote "was not found on
    /// page N" is only meaningful when page N has something on it.
    fn lease(db: &MemoryDatabase) -> i64 {
        let text = (1..=10)
            .map(|p| {
                if p == 4 {
                    "[[page:4]]\nThe Tenant may terminate this Lease on the fifth anniversary by giving six months notice.".to_string()
                } else {
                    format!("[[page:{}]]\n{}", p, crate::rag::eval::distinct_page_text(p))
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        doc(db, "Lease.pdf", &text)
    }

    #[test]
    fn correct_citations_and_quotes_pass() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let id = lease(&db);
        let inputs = CitationInputs { passages: vec![passage(id, "Lease.pdf", 4, 4)], session_document_ids: vec![id], ..Default::default() };
        let answer = "The lease says \"The Tenant may terminate this Lease on the fifth anniversary\" [Lease.pdf, p.4].";
        let r = verify(&db, answer, &inputs);
        assert_eq!(r.checked, 1);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert!(r.note().is_none());
    }

    #[test]
    fn wrong_page_missing_page_unknown_file_and_misquote_are_flagged() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let id = lease(&db);
        let inputs = CitationInputs { passages: vec![passage(id, "Lease.pdf", 4, 4)], session_document_ids: vec![id], ..Default::default() };
        let answer = "Break option [Lease.pdf, p.7]. Also [Lease.pdf, p.42]. See [Guarantee.pdf, p.2]. \
                      It states \"the Landlord may terminate at any time without notice\" [Lease.pdf, p.4].";
        let r = verify(&db, answer, &inputs);
        assert_eq!(r.checked, 4);
        assert_eq!(r.issues.len(), 4, "{:#?}", r.issues);
        assert!(r.issues[0].contains("not among the excerpts"));
        assert!(r.issues[1].contains("has 10 pages"));
        assert!(r.issues[2].contains("Guarantee.pdf"));
        assert!(r.issues[3].contains("was not found on page 4"));
        assert!(r.note().unwrap().contains("Citation check"));
    }

    #[test]
    fn page_only_citations_resolve_and_markdown_links_are_ignored() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let id = lease(&db);
        let inputs = CitationInputs { full_document_ids: vec![id], session_document_ids: vec![id], ..Default::default() };
        let answer = "Notice is six months [p.4, Section 3]. More in [the guide](https://example.com).";
        let r = verify(&db, answer, &inputs);
        assert_eq!(r.checked, 1);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
    }
}
