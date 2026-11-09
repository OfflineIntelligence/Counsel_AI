//! Turns retrieval results into prompt blocks.
//!
//! * The **system block** is stable across turns (KV-cache friendly): short
//!   attached files in full, or, for large files, the list of documents and
//!   how to cite them.
//! * The **evidence block** is per question and is folded into the latest user
//!   message: cited passages from the attached documents, or possibly relevant
//!   passages from the chat's matter when nothing is attached.

use std::collections::HashSet;

use tracing::info;

use super::scope::{self, Scope};
use super::search::{self, Passage, RetrieveOptions};
use super::RagRuntime;
use crate::memory_db::MemoryDatabase;

const CHARS_PER_TOKEN: usize = 4;
/// Attached documents whose combined text fits this share of the prompt are
/// included whole (so "quote the full text" works for short files).
const FULL_INCLUSION_PCT: usize = 20;
/// Share of the prompt for passages retrieved for this question.
const EVIDENCE_PCT: usize = 35;
const EVIDENCE_MIN_BYTES: usize = 4_800;
const EVIDENCE_MAX_BYTES: usize = 48_000;
/// Cosine similarity (query vs best chunk) at or above which a message is
/// treated as being about the attached documents. Measured with the bundled
/// model: on-topic questions score roughly 0.55-0.85, unrelated ones 0.25-0.40.
const DOCUMENT_RELEVANCE_SIMILARITY: f32 = 0.45;

/// Told to the model when a message is CLEARLY not about the attached
/// documents: a greeting, "thanks", "ok".
///
/// This note is strong. It forbids the model from touching the documents at
/// all, which is right for "hello" and badly wrong for a real question the
/// gate merely failed to recognise. It is therefore only ever emitted for
/// [`DocumentIntent::Unrelated`], never on a maybe.
const UNRELATED_NOTE: &str = "[Note: no attached document is relevant to this message. Answer it \
normally from your own knowledge; do not mention or cite the attached documents.]";

/// Sent when the message WAS treated as being about the documents, but
/// retrieval found nothing above threshold.
///
/// Deliberately distinct from UNRELATED_NOTE. "This is not about the
/// documents" sends the model off to answer from general knowledge; "this is
/// about the documents and the answer is not in them" is the truth, and the
/// only honest reply to it is to say so. In legal work those two are not
/// interchangeable, and the first one quietly invites a confident answer
/// about a document the model has not read.
const NO_MATCH_NOTE: &str = "[Note: this message is about the attached documents, but no passage \
matching it was found. Tell the user plainly that you could not find it in the attached \
documents, and suggest they name the page, clause or term. Do not answer from general knowledge \
as though it came from the documents.]";

lazy_static::lazy_static! {
    /// Wording that points at the attached material itself.
    ///
    /// The third alternation is new, and it is the one that matters most in
    /// practice: the PARTS of a document people name instead of naming the
    /// document. "list down all the keywords", "show me the contents", "what
    /// does paragraph 3 say" are all plainly about an attached file, and not
    /// one of them contains the word "document".
    static ref DOCUMENT_REFERENCE: regex::Regex = regex::Regex::new(
        r"(?i)\b(?:documents?|docs?|pdfs?|attachments?|attached|uploaded)\b|\b(?:this|the|that|my|our|your|these|those)\s+(?:file|files|contract|contracts|agreement|agreements|lease|guide|policy|report|spreadsheet|sheet|text|form|deed|memo|letter|brief)\b|\b(?:keywords?|contents?|wording|passages?|excerpts?|paragraphs?|pages?|clauses?|sections?|headings?|appendix|annexures?|schedules?)\b|\baccording to (?:it|this|the)\b|\bdoes it (?:say|mention|state|cover)\b|\b(?:it|this) says\b"
    ).unwrap();
    /// Short follow-ups that lean on the previous answer ("and what about it?").
    static ref FOLLOW_UP: regex::Regex = regex::Regex::new(
        r"(?i)^\s*(?:and|also|so|but|what about|how about|then|why|explain (?:that|this|it)|elaborate|more on)\b|\b(?:it|that|this|they|those|these|there)\b"
    ).unwrap();
}

/// What a message looks like it wants from the attached documents.
///
/// This used to be a `bool`, and the missing third case is what made the gate
/// fail the way it did: "I could not recognise this" and "this is definitely
/// not about the documents" were the same answer, and both of them emitted
/// UNRELATED_NOTE - which does not merely withhold the excerpts, it instructs
/// the model to ignore the documents entirely. A question the gate simply did
/// not have a pattern for was therefore answered as though no file had ever
/// been attached.
///
/// The costs here are lopsided and the enum is shaped around that. Retrieving
/// for a message that turns out not to need it wastes a little context and
/// the model says so. NOT retrieving for a message that needed it produces
/// "I am ready to assist you" while the answer sits unread in the database.
/// So anything short of a confident "no" now retrieves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentIntent {
    /// Recognised as being about the documents. Retrieve, and if nothing is
    /// found say so (NO_MATCH_NOTE).
    Retrieve,
    /// Confidently not about the documents: a greeting, "thanks", "ok".
    /// The only case that earns UNRELATED_NOTE.
    Unrelated,
    /// No signal either way. Retrieve anyway and let the retriever's own
    /// similarity threshold be the judge, since it is looking at the actual
    /// passages and this function is only looking at the question. If it
    /// finds nothing, say nothing: silence lets the model answer the question
    /// normally, which is the right outcome when we never knew.
    Unclear,
}

/// What a message wants from the attached documents.
///
/// `attached_this_turn` short-circuits everything below it, and it is the
/// single most reliable signal available: a person who attaches a file and
/// types in the same breath is asking about the file. No pattern matching can
/// beat knowing that, and the case it fixes ("list down all the keywords",
/// sent with I94.pdf) is the one users hit first.
async fn about_documents(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    document_ids: &[i64],
    filenames: &[String],
    previous_answer_used_documents: bool,
    attached_this_turn: bool,
) -> DocumentIntent {
    let q = query.trim();
    if q.is_empty() || document_ids.is_empty() {
        return DocumentIntent::Unrelated;
    }
    if attached_this_turn {
        return DocumentIntent::Retrieve;
    }
    let lower = q.to_lowercase();
    // Exact lookups count only when they resolve in these documents: "page 7"
    // that exists, a section number that exists, a term they define.
    let scope_set: HashSet<i64> = document_ids.iter().copied().collect();
    let terms = db.rag.all_terms(Some(&scope_set), 400).unwrap_or_default();
    match super::router::classify(q, &terms) {
        super::router::Route::Page { from, .. } => {
            let max_page = document_ids
                .iter()
                .filter_map(|id| db.rag.page_count(*id).ok().flatten())
                .max()
                .unwrap_or(0);
            if from <= max_page {
                return DocumentIntent::Retrieve;
            }
        }
        super::router::Route::Section { number_key, .. } => {
            if !db.rag.section_search(&number_key, Some(&scope_set), 1).unwrap_or_default().is_empty() {
                return DocumentIntent::Retrieve;
            }
        }
        super::router::Route::Definition { term } => {
            if terms.iter().any(|t| t == &term.to_lowercase()) {
                return DocumentIntent::Retrieve;
            }
        }
        // "Summarise" / "compare" alone is too common to decide by itself.
        super::router::Route::Coverage | super::router::Route::Semantic => {}
    }
    // Does the question name one of the files?
    //
    // Two changes here, and they depend on each other.
    //
    // The word-length floor was 5, which is what let "I94.pdf" slip past: its
    // stem splits to ["i", "94"], neither reaches 5, so a question naming the
    // file by name did not count as naming it. Real documents have short
    // names (I94, W2, NDA, SPA), so the floor is 3.
    //
    // But lowering the floor is only safe with a WORD match. The test was
    // `lower.contains(w)` - a substring - and at 3 characters that is far too
    // eager: "OPT extension guide.pdf" contributes the stem word "opt", which
    // is inside "options", "optional" and "adopt". Every one of those would
    // have counted as naming the file. Matching whole words keeps "the opt
    // guide" and drops "what are my options?".
    let names_a_word = |needle: &str| {
        needle.len() >= 3
            && lower
                .split(|c: char| !c.is_alphanumeric())
                .any(|w| w == needle)
    };
    let names_a_file = filenames.iter().any(|f| {
        let stem = f.rsplit_once('.').map(|(s, _)| s).unwrap_or(f).to_lowercase();
        // The full name or full stem as typed ("opt extension guide"), then
        // any single distinctive word from it.
        lower.contains(&f.to_lowercase())
            || lower.contains(&stem)
            || stem
                .split(|c: char| !c.is_alphanumeric())
                .any(names_a_word)
    });
    if names_a_file || DOCUMENT_REFERENCE.is_match(q) {
        return DocumentIntent::Retrieve;
    }
    let content_words = crate::memory_db::fts::content_tokens(q).len();
    // Short follow-ups right after an answer from the documents.
    if previous_answer_used_documents && content_words <= 4 && FOLLOW_UP.is_match(q) {
        return DocumentIntent::Retrieve;
    }
    if content_words == 0 {
        return DocumentIntent::Unrelated; // greetings, thanks, "ok"
    }
    // Nothing in the question itself gave it away. Fall back to measuring it
    // against the documents - and crucially, a miss here is Unclear, not
    // Unrelated. Similarity is a weak signal for questions ABOUT a document
    // rather than questions drawn FROM it: "list all the keywords" and "what
    // are the main points" are asked in words that appear nowhere in the text
    // they are asking about, so they score low against every passage while
    // being entirely on topic.
    let scope: HashSet<i64> = document_ids.iter().copied().collect();
    let hit = match search::best_similarity(db, rt, q, Some(&scope)).await {
        Some(sim) => sim >= DOCUMENT_RELEVANCE_SIMILARITY,
        // No embedding model: require several of the question's words in one passage.
        None => {
            let overlap = search::keyword_overlap(db, q, Some(&scope));
            overlap >= 2.min(content_words).max(1) && (content_words <= 2 || overlap * 2 >= content_words)
        }
    };
    if hit { DocumentIntent::Retrieve } else { DocumentIntent::Unclear }
}

/// Whether the latest assistant turn answered from the attached documents.
pub fn previous_answer_used_documents(history: &[crate::memory::Message], filenames: &[String]) -> bool {
    let Some(last) = history.iter().rev().find(|m| m.role == "assistant") else { return false };
    let text = last.content.to_lowercase();
    filenames.iter().any(|f| text.contains(&f.to_lowercase())) || text.contains("[p.") || text.contains(", p.")
}

/// Share of the prompt for unrequested matter material.
const MATTER_PCT: usize = 15;
const MATTER_MIN_SIMILARITY: f32 = 0.55;

pub struct RagContext {
    pub system_block: Option<String>,
    pub evidence_block: Option<String>,
    pub consumed_chars: usize,
    pub scope: Scope,
    pub summary: String,
    /// Excerpts given to the model (for citation checking).
    pub passages: Vec<Passage>,
    /// Documents given to the model in full.
    pub full_document_ids: Vec<i64>,
}

fn type_label(filename: &str) -> &'static str {
    match filename.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "pdf" => "PDF",
        "doc" | "docx" | "odt" | "rtf" => "Word document",
        "xls" | "xlsx" | "ods" | "csv" => "Spreadsheet",
        "ppt" | "pptx" => "Presentation",
        "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "gif" => "Image (OCR text)",
        "txt" | "md" | "log" => "Plain text",
        _ => "file",
    }
}

const RETRIEVAL_INSTRUCTIONS: &str = "\
[ATTACHED DOCUMENTS - SEARCHED FOR EACH QUESTION]\n\
The file(s) below are attached to this conversation and stay available for all of it. \
They are too large to show whole, so when a message is about them the most relevant \
passages are retrieved and placed after the user's message under [DOCUMENT EXCERPTS]. \
Pages, clauses and defined terms that the user names are looked up exactly.\n\
- A message that arrives WITHOUT [DOCUMENT EXCERPTS] is not about the documents (a \
greeting, or a question on another subject): answer it normally from your own knowledge \
and do not mention or cite the documents.\n\
- With excerpts, answer only from those excerpts, not from general knowledge or from what \
such a file usually contains.\n\
- Cite every point with the tag that heads its excerpt, e.g. [SPA.pdf, p.47, Section 12.3].\n\
- If the excerpts do not contain the answer, say so plainly and suggest naming the page, \
clause or term; never invent document content.\n\
- Excerpts are verbatim; when asked for exact wording, quote them exactly.\n\
- NEVER reprint the excerpt block. Do not repeat the [DOCUMENT EXCERPTS] heading, the \
--- tag lines, or the excerpt bodies back to the user: the sources are already shown \
beside your answer, so restating them answers nothing and spends the whole reply. Write \
the answer in your own prose and cite inline. Quote only the specific words you are \
making a point about, never a whole excerpt.\n";

fn document_list(db: &MemoryDatabase, docs: &[crate::memory_db::DocumentRecord]) -> String {
    let mut s = String::from("Files attached to this conversation:\n");
    let ids: Vec<i64> = docs.iter().map(|d| d.id).collect();
    let versions = super::versions::families(db, &ids);
    for d in docs {
        if let Some(v) = versions.get(&d.id) {
            s.push_str(&format!("  - {} ({})\n", d.original_filename, v.label()));
            continue;
        }
        if d.extraction_status != "ok" || d.extracted_text.trim().is_empty() {
            s.push_str(&format!(
                "  - {} ({}) - could not be read: {}\n",
                d.original_filename,
                type_label(&d.original_filename),
                d.extraction_error.as_deref().unwrap_or("no text extracted")
            ));
            continue;
        }
        let pages = db.rag.page_count(d.id).ok().flatten();
        match pages {
            Some(p) => s.push_str(&format!("  - {} ({}, {} pages)\n", d.original_filename, type_label(&d.original_filename), p)),
            None => s.push_str(&format!("  - {} ({})\n", d.original_filename, type_label(&d.original_filename))),
        }
    }
    s
}

pub fn format_passages(title: &str, guidance: &str, passages: &[Passage], note: Option<&str>) -> String {
    let mut s = format!("[{}]\n{}\n", title, guidance);
    if let Some(n) = note {
        s.push_str(n);
        s.push('\n');
    }
    if passages.iter().any(|p| p.version_label.is_some()) {
        s.push_str(
            "Some excerpts come from different versions of the same document; each is marked with its \
             version and upload date. Treat the latest upload as the current version unless the user \
             says otherwise, say which version each point comes from, and point out where the \
             versions differ.\n",
        );
    }
    for p in passages {
        let cite = if p.citation.is_empty() {
            format!("[{}]", p.filename)
        } else {
            format!("[{}, {}]", p.filename, p.citation.trim_start_matches('[').trim_end_matches(']'))
        };
        match &p.version_label {
            Some(v) => s.push_str(&format!("\n--- {} ({}) ---\n{}\n", cite, v, p.text)),
            None => s.push_str(&format!("\n--- {} ---\n{}\n", cite, p.text)),
        }
        if p.truncated {
            s.push_str("[excerpt shortened to fit]\n");
        }
    }
    s
}

pub async fn build(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    session_id: &str,
    query: Option<&str>,
    budget_tokens: usize,
    allow_matter_material: bool,
) -> RagContext {
    build_with_history(db, rt, session_id, query, budget_tokens, allow_matter_material, &[], false).await
}

/// As `build`, using the conversation so far to recognise short follow-ups
/// to an answer that came from the documents.
///
/// `attached_this_turn` is whether a file arrived WITH the message being
/// answered, as opposed to earlier in the conversation. The caller computes
/// it, because the caller is the one that knows which turn this is: see the
/// `turn` it stamps onto `session_documents` in stream_api. Deriving it here
/// from `history.len()` instead would mean two files agreeing on an off-by-one
/// forever.
#[allow(clippy::too_many_arguments)]
pub async fn build_with_history(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    session_id: &str,
    query: Option<&str>,
    budget_tokens: usize,
    allow_matter_material: bool,
    history: &[crate::memory::Message],
    attached_this_turn: bool,
) -> RagContext {
    let scope = scope::resolve(db, session_id);
    let budget_chars = budget_tokens.saturating_mul(CHARS_PER_TOKEN);
    let session_docs = db.documents.get_session_documents(session_id).unwrap_or_default();
    let _ = db.rag.ensure_chunked(&scope.session_documents);

    let usable: Vec<i64> = session_docs
        .iter()
        .filter(|d| d.extraction_status == "ok" && !d.extracted_text.trim().is_empty())
        .map(|d| d.id)
        .collect();
    let total_text: usize = session_docs
        .iter()
        .filter(|d| d.extraction_status == "ok")
        .map(|d| d.extracted_text.len())
        .sum();
    let full_allowance = budget_chars * FULL_INCLUSION_PCT / 100;

    // Small attachments: include whole (stable system block, no retrieval).
    if !session_docs.is_empty() && total_text + 4_000 <= full_allowance {
        let blocks = crate::context_engine::build_document_context(db, session_id, full_allowance).await;
        let filenames: Vec<String> = session_docs.iter().map(|d| d.original_filename.clone()).collect();
        let intent = match query {
            Some(q) => {
                about_documents(
                    db,
                    rt,
                    q,
                    &usable,
                    &filenames,
                    previous_answer_used_documents(history, &filenames),
                    attached_this_turn,
                )
                .await
            }
            None => DocumentIntent::Retrieve,
        };
        // The whole document is already in the system block here, so there is
        // nothing to retrieve and the only question is whether to tell the
        // model to ignore it. Only a confident Unrelated does that; Unclear
        // leaves the document in front of the model and says nothing, which
        // is what we want when we do not know.
        let evidence_block = match intent {
            DocumentIntent::Unrelated => Some(UNRELATED_NOTE.to_string()),
            DocumentIntent::Retrieve | DocumentIntent::Unclear => None,
        };
        let consumed = blocks.consumed_chars + evidence_block.as_ref().map(|e| e.len()).unwrap_or(0);
        return RagContext {
            system_block: blocks.session_block,
            evidence_block,
            consumed_chars: consumed,
            scope,
            summary: format!("{} attached document(s) included in full", session_docs.len()),
            passages: Vec::new(),
            full_document_ids: usable.clone(),
        };
    }

    let evidence_budget = (budget_chars * EVIDENCE_PCT / 100).clamp(EVIDENCE_MIN_BYTES, EVIDENCE_MAX_BYTES).min(budget_chars.max(EVIDENCE_MIN_BYTES));

    if !session_docs.is_empty() {
        let mut system = String::from(RETRIEVAL_INSTRUCTIONS);
        system.push_str(&document_list(db, &session_docs));

        let mut evidence = None;
        let mut passages: Vec<Passage> = Vec::new();
        let mut summary = format!("{} large attached document(s): retrieval", session_docs.len());
        let filenames: Vec<String> = session_docs.iter().map(|d| d.original_filename.clone()).collect();
        let intent = match query {
            Some(q) if !usable.is_empty() => {
                about_documents(
                    db,
                    rt,
                    q,
                    &usable,
                    &filenames,
                    previous_answer_used_documents(history, &filenames),
                    attached_this_turn,
                )
                .await
            }
            _ => DocumentIntent::Unrelated,
        };
        // Retrieve for anything that is not a confident "no". The retriever
        // applies its own similarity threshold to actual passages, which is a
        // far better judge than a pattern match on the question, so Unclear
        // is handed to it rather than answered here.
        match (query, intent) {
            (_, DocumentIntent::Unrelated) => {
                evidence = Some(UNRELATED_NOTE.to_string());
                summary = "message not about the attached documents: no excerpts".to_string();
            }
            (Some(q), DocumentIntent::Retrieve | DocumentIntent::Unclear) => {
                let mut opts = RetrieveOptions::for_budget(evidence_budget);
                // On the Unclear path the retriever IS the gate, so it needs
                // an absolute floor. `for_budget` sets none: its only filter
                // is RELATIVE_SCORE_FLOOR, which drops weak matches relative
                // to the best one and so still returns something for a
                // question about nothing in the document.
                //
                // Worth running even though `best_similarity` already came
                // back under this same threshold: that is a pure vector
                // comparison, while `retrieve` also routes through FTS,
                // section and defined-term lookup. A question whose wording
                // matches the text exactly but embeds poorly is found here
                // and nowhere else.
                if intent == DocumentIntent::Unclear {
                    opts.min_similarity = Some(DOCUMENT_RELEVANCE_SIMILARITY);
                }
                let r = search::retrieve(db, rt, q, scope.allowed.as_ref(), &usable, &opts).await;
                summary = format!(
                    "intent={:?} route={} passages={} vectors={}",
                    intent,
                    r.route,
                    r.passages.len(),
                    r.used_vectors
                );
                if r.passages.is_empty() {
                    // Nothing matched, and what to say depends on how sure we
                    // were that the question was about the documents:
                    //
                    //   Retrieve - we recognised it as a document question, so
                    //              the honest answer is "it is not in there".
                    //              Telling the model to ignore the files here
                    //              invites a confident answer about a document
                    //              it never read.
                    //
                    //   Unclear  - we did not recognise it AND the retriever
                    //              found nothing for it. Between them that is
                    //              the strongest evidence available that the
                    //              message really is off topic, so it earns
                    //              the full disclaimer after all.
                    evidence = Some(
                        match intent {
                            DocumentIntent::Retrieve => NO_MATCH_NOTE,
                            _ => UNRELATED_NOTE,
                        }
                        .to_string(),
                    );
                } else {
                    evidence = Some(format_passages(
                        "DOCUMENT EXCERPTS",
                        "Retrieved from the attached documents for this question. Cite the tag heading each excerpt.",
                        &r.passages,
                        r.note.as_deref(),
                    ));
                }
                passages = r.passages;
            }
            (None, _) => {}
        }
        let consumed = system.len() + evidence.as_ref().map(|e| e.len()).unwrap_or(0);
        info!("RAG context for session {}: {}", session_id, summary);
        return RagContext {
            system_block: Some(system),
            evidence_block: evidence,
            consumed_chars: consumed,
            scope,
            summary,
            passages,
            full_document_ids: Vec::new(),
        };
    }

    // Nothing attached: offer clearly relevant material from the chat's matter.
    let mut evidence = None;
    let mut passages: Vec<Passage> = Vec::new();
    let mut summary = "no attached documents".to_string();
    if let (Some(q), true) = (query, allow_matter_material) {
        let pool: Option<HashSet<i64>> = scope.allowed.clone();
        let has_pool = pool.as_ref().map(|p| !p.is_empty()).unwrap_or(true);
        let indexed = db.rag.totals().map(|t| t.chunks > 0).unwrap_or(false);
        if has_pool && indexed && crate::memory_db::fts::content_tokens(q).len() >= 2 {
            let mut opts = RetrieveOptions::for_budget((budget_chars * MATTER_PCT / 100).clamp(2_000, 16_000));
            opts.min_similarity = Some(MATTER_MIN_SIMILARITY);
            let r = search::retrieve(db, rt, q, pool.as_ref(), &[], &opts).await;
            summary = format!("matter material route={} passages={} vectors={}", r.route, r.passages.len(), r.used_vectors);
            if !r.passages.is_empty() {
                evidence = Some(format_passages(
                    "POSSIBLY RELEVANT MATERIAL FROM THIS MATTER'S DOCUMENTS",
                    "Found in documents stored for this matter. Use it only if it bears on the question, \
                     cite the tag heading each excerpt, and do not treat it as attached to this chat.",
                    &r.passages,
                    None,
                ));
            }
            passages = r.passages;
        }
    }
    let consumed = evidence.as_ref().map(|e| e.len()).unwrap_or(0);
    RagContext { system_block: None, evidence_block: evidence, consumed_chars: consumed, scope, summary, passages, full_document_ids: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::NewDocument;
    use crate::rag::eval;

    fn add_doc(db: &MemoryDatabase, session: &str, name: &str, text: &str) -> i64 {
        let _ = db.conversations.create_session_with_id(session, None);
        let doc = db
            .documents
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
            .unwrap();
        db.documents.link_session(session, doc.id, "paperclip").unwrap();
        doc.id
    }

    #[tokio::test]
    async fn a_large_document_yields_cited_passages_from_deep_pages() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        let text = eval::build_document_text(pages, &facts);
        add_doc(&db, "big", name, &text);

        let ctx = build(&db, &rt, "big", Some("Is the seller's liability for warranty claims capped?"), 8_000, true).await;
        let system = ctx.system_block.unwrap();
        assert!(system.contains("SEARCHED FOR EACH QUESTION"), "{}", system);
        assert!(system.contains("120 pages"), "page count listed: {}", system);
        assert!(!system.contains("thirty per cent"), "large text must not be pasted into the system message");
        let evidence = ctx.evidence_block.unwrap();
        assert!(evidence.contains("thirty per cent"), "{}", evidence);
        assert!(evidence.contains("p.47"), "citation must carry the page: {}", evidence);

        let page = build(&db, &rt, "big", Some("what is on page 88?"), 8_000, true).await;
        let evidence = page.evidence_block.unwrap();
        assert!(evidence.contains("Non-Compete") && evidence.contains("p.88"), "{}", evidence);
    }

    /// The reported bug, as it actually happened.
    ///
    /// "list down all the keywords" was sent with I94.pdf attached, and the
    /// gate rejected it three ways at once: no doc-word in the pattern, a
    /// filename whose stem ("i", "94") was below the 5-character floor, and
    /// an embedding similarity that is always low because a question ABOUT a
    /// document shares no vocabulary WITH it. The model was then told the
    /// document was irrelevant and answered from thin air.
    #[tokio::test]
    async fn a_question_about_a_file_attached_in_the_same_message_always_retrieves() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        let text = eval::build_document_text(pages, &facts);
        add_doc(&db, "s", name, &text);

        // Exactly the query from the report: no filename, no "document", and
        // nothing in common with the text it is asking about.
        let q = Some("list down all the keywords");

        let ctx = build_with_history(&db, &rt, "s", q, 8_000, true, &[], true).await;

        // The gate must have treated it as a document question.
        assert!(
            ctx.summary.contains("intent=Retrieve"),
            "a file attached in this very message must be searched: {:?}",
            ctx.summary
        );

        // And the model must never be told to ignore a file the user attached
        // one keystroke ago. That single instruction is what produced "I am
        // ready to assist you" while the answer sat in the database.
        let ev = ctx.evidence_block.as_deref().unwrap_or("");
        assert!(
            !ev.contains("no attached document is relevant"),
            "must not disclaim a document the user just attached: {ev}"
        );
        // Without an embedding model the test runtime cannot match "keywords"
        // to anything lexically, so the outcome here is the honest "not found"
        // rather than excerpts. In production (a real embedder) the semantic
        // route returns passages; what this test pins is that the question is
        // SEARCHED and never disclaimed.
        assert!(
            ev.contains("DOCUMENT EXCERPTS") || ev.contains("no passage matching it was found"),
            "expected excerpts or an honest miss, got: {ev}"
        );
    }

    /// The same question without the same-turn shortcut: the widened pattern
    /// has to carry it on its own, so a file attached earlier in the chat
    /// still answers questions asked about it later.
    #[tokio::test]
    async fn questions_naming_parts_of_a_document_retrieve_without_saying_document() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        let text = eval::build_document_text(pages, &facts);
        add_doc(&db, "s", name, &text);

        for q in [
            "list down all the keywords",
            "show me the contents",
            "what does paragraph 3 say",
            "explain clause 4",
        ] {
            let ctx = build_with_history(&db, &rt, "s", Some(q), 8_000, true, &[], false).await;
            let ev = ctx.evidence_block.as_deref().unwrap_or("");
            assert!(
                !ev.contains("no attached document is relevant"),
                "{q:?} must not be disclaimed as unrelated (summary: {})",
                ctx.summary
            );
        }
    }

    /// A greeting is the one case that still earns the hard disclaimer, and it
    /// must keep earning it: without it a model will happily open with a
    /// summary of a document nobody asked about.
    #[tokio::test]
    async fn greetings_are_still_told_the_documents_are_not_relevant() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        let text = eval::build_document_text(pages, &facts);
        add_doc(&db, "s", name, &text);

        for q in ["hello", "thanks", "ok"] {
            let ctx = build_with_history(&db, &rt, "s", Some(q), 8_000, true, &[], false).await;
            assert!(
                ctx.evidence_block.as_deref().unwrap_or("").contains("no attached document is relevant"),
                "{q:?} should be answered without the documents"
            );
        }
    }

    /// The Unclear path, both ways round.
    ///
    /// An unrecognised question is now SEARCHED rather than refused outright.
    /// If the retriever finds nothing it is treated as off topic exactly as
    /// before (the disclaimer), so nothing regresses. The gain is the other
    /// branch: a question the patterns missed but the index can answer now
    /// gets its excerpts instead of being told the documents are irrelevant.
    #[tokio::test]
    async fn an_unrecognised_question_is_searched_before_being_dismissed() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        let text = eval::build_document_text(pages, &facts);
        add_doc(&db, "s", name, &text);

        // Genuinely off topic, and nothing in the index matches it: the
        // disclaimer stands.
        let off = build_with_history(
            &db, &rt, "s",
            Some("explain how mitochondria produce energy in a cell"),
            8_000, true, &[], false,
        ).await;
        assert!(
            off.evidence_block.as_deref().unwrap_or("").contains("no attached document is relevant"),
            "an off-topic question should still be answered without the documents: {:?}",
            off.evidence_block
        );
        assert!(off.passages.is_empty());

        // Phrased with none of the gate's trigger words, but the document
        // does answer it. Previously dismissed; now retrieved.
        let hit = build_with_history(
            &db, &rt, "s",
            Some("is the seller's liability for warranty claims capped?"),
            8_000, true, &[], false,
        ).await;
        assert!(
            hit.evidence_block.as_deref().unwrap_or("").contains("DOCUMENT EXCERPTS"),
            "a question the index can answer must not be dismissed: {:?}",
            hit.summary
        );
    }

    #[tokio::test]
    async fn short_documents_are_still_included_whole() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        add_doc(&db, "small", "note.txt", "The deposit is refundable within 14 days.");
        let ctx = build(&db, &rt, "small", Some("show the full text"), 8_000, true).await;
        assert!(ctx.evidence_block.is_none());
        let system = ctx.system_block.unwrap();
        assert!(system.contains("refundable within 14 days"), "{}", system);
        assert!(system.contains("IN FULL"), "{}", system);
    }

    #[tokio::test]
    async fn general_chats_do_not_see_matter_documents() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let matter = db.matters.create(crate::memory_db::MatterFields { name: Some("M".into()), ..Default::default() }).unwrap().id;
        let doc = add_doc(&db, "attached-chat", "m.txt", "Matter-only text about escrow release conditions.");
        db.rag.link_document_to_matter(matter, doc).unwrap();
        let _ = db.conversations.create_session_with_id("general", None);
        assert!(!scope::resolve(&db, "general").allows(doc));
    }

    #[tokio::test]
    async fn deleting_a_document_removes_its_index() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let id = add_doc(&db, "del", "gone.txt", &"Force majeure excuses performance. ".repeat(400));
        assert!(!db.rag.document_chunks(id).unwrap().is_empty());
        db.documents.delete_document(id).unwrap();
        assert!(db.rag.document_chunks(id).unwrap().is_empty());
        let q = crate::memory_db::fts::fts_query_from_user_text("force majeure").unwrap();
        assert!(db.rag.keyword_search(&q, None, 10).unwrap().is_empty(), "keyword index must forget deleted chunks");
    }

    #[test]
    fn compaction_reclaims_space_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoryDatabase::new(&dir.path().join("memory.db")).unwrap();
        let text = "Liability shall be capped at the fees paid. ".repeat(20_000);
        let id = add_doc(&db, "compact", "big.txt", &text);
        db.documents.delete_document(id).unwrap();
        let reclaimed = db.rag.compact_if_worthwhile(1024 * 1024, 0.2).unwrap();
        assert!(reclaimed > 1024 * 1024, "reclaimed only {} bytes", reclaimed);
        assert_eq!(db.rag.compact_if_worthwhile(1024 * 1024, 0.2).unwrap(), 0, "nothing left to reclaim");
    }


    #[tokio::test]
    async fn spreadsheet_questions_return_the_right_rows_with_headers() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let mut text = String::from("[[sheet:Invoices]]\nInvoice\tClient\tAmount\tStatus");
        for i in 2..=400 {
            let status = if i == 213 { "Disputed" } else { "Paid" };
            text.push_str(&format!("\n\nINV-{:04}\tClient {}\t{}\t{}", i, i % 37, 1000 + i * 3, status));
        }
        text.push_str("\n\n[[sheet:Summary]]\nMetric\tValue\n\nTotal invoices\t399");
        add_doc(&db, "sheet-chat", "ledger.xlsx", &text);

        let q = "Which invoice in the Invoices sheet is Disputed?";
        let r = search::retrieve(&db, &rt, q, None, &db.rag.all_document_ids().unwrap(), &RetrieveOptions { budget_bytes: 6_000, ..Default::default() }).await;
        assert_eq!(r.route, "sheet");
        let top = &r.passages[0];
        assert!(top.text.len() < 1_500, "a sheet hit must stay a small slice, got {} bytes: {}", top.text.len(), top.citation);
        assert!(top.text.contains("INV-0213") && top.text.contains("Disputed"), "{}", top.text);
        assert!(top.text.starts_with("Invoice\tClient\tAmount\tStatus"), "header row must lead: {}", top.text);
        assert!(top.citation.contains("Sheet: Invoices, rows"), "{}", top.citation);

        let r = search::retrieve(&db, &rt, "show row 300 of Invoices", None, &db.rag.all_document_ids().unwrap(), &RetrieveOptions::default()).await;
        assert_eq!(r.route, "sheet_row");
        assert!(r.passages[0].text.contains("INV-0300"), "{}", r.passages[0].text);
        assert!(r.passages[0].text.len() < 1_500, "a row lookup returns a small slice, got {} bytes", r.passages[0].text.len());
    }


    #[tokio::test]
    async fn older_and_newer_versions_both_appear_labelled() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let page = |cap: &str| {
            (1..=60)
                .map(|p| {
                    if p == 31 {
                        format!("[[page:{}]]\nSection 9.2 Cap on Liability\n\nThe Supplier's total liability is capped at {}.", p, cap)
                    } else {
                        format!("[[page:{}]]\nThe parties agree to cooperate in good faith on all operational matters.", p)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let old = add_doc(&db, "versions", "Supply_Agreement_v1.pdf", &page("EUR 1,000,000"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let new = add_doc(&db, "versions", "Supply Agreement v2 (signed).pdf", &page("EUR 2,500,000"));

        let ctx = build(&db, &rt, "versions", Some("What is the supplier's liability cap?"), 8_000, true).await;
        let evidence = ctx.evidence_block.unwrap();
        assert!(evidence.contains("EUR 1,000,000") && evidence.contains("EUR 2,500,000"), "both versions must appear: {}", evidence);
        assert!(evidence.contains("version 2 of 2") && evidence.contains("latest"), "{}", evidence);
        assert!(evidence.contains("version 1 of 2") && evidence.contains("superseded"), "{}", evidence);
        assert!(evidence.contains("point out where the"), "{}", evidence);
        let system = ctx.system_block.unwrap();
        assert!(system.contains("version 1 of 2") && system.contains("version 2 of 2"), "{}", system);

        // Only the older version attached: the newer one in scope still appears.
        let _ = db.conversations.create_session_with_id("only-old", None);
        db.documents.link_session("only-old", old, "paperclip").unwrap();
        let ctx = build(&db, &rt, "only-old", Some("page 31"), 8_000, true).await;
        let evidence = ctx.evidence_block.unwrap();
        assert!(evidence.contains("EUR 1,000,000") && evidence.contains("EUR 2,500,000"), "{}", evidence);
        let _ = new;
    }


    #[tokio::test]
    async fn earlier_chats_are_recalled_by_meaning() {
        let db = std::sync::Arc::new(MemoryDatabase::new_in_memory().unwrap());
        let rt = eval::test_runtime();
        for (sid, text) in [
            ("lease-chat", "We concluded the tenant break clause lets the tenant terminate after year five with notice."),
            ("other-chat", "Payroll for March was processed on the fourth business day."),
        ] {
            db.conversations.create_session_with_id(sid, None).unwrap();
            db.conversations.store_messages_batch(sid, &[("assistant".to_string(), text.to_string(), 1, 0, 0.5)]).unwrap();
        }
        db.conversations.create_session_with_id("now", None).unwrap();
        crate::rag::indexer::run_once_with(&db, &rt).await;

        let q = "remember what we concluded about the tenant break clause termination?";
        let allowed = scope::allowed_sessions(&db, "now");
        let hits = search::semantic_message_hits(&db, &rt, q, allowed.as_ref(), "now", None, 6, 0.0).await;
        assert!(!hits.is_empty() && hits[0].0.session_id == "lease-chat", "{:?}", hits.iter().map(|h| (&h.0.session_id, h.1)).collect::<Vec<_>>());

        let intent = crate::context_engine::detect_reference(q, &[], chrono::Utc::now());
        let past = crate::context_engine::past_retrieval::build_past_context_with(&db, "now", q, &intent, 6_000, &hits);
        assert!(past.block.unwrap_or_default().contains("break clause"));
    }


    #[test]
    fn deleting_vault_files_library_items_and_drafts_removes_their_documents() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        // Vault folder with a file inside.
        let conn = db.rag.conn().unwrap();
        let has_local = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_files'", [], |_| Ok(true))
            .unwrap_or(false);
        drop(conn);
        if has_local {
            let conn = db.rag.conn().unwrap();
            conn.execute("INSERT INTO local_files (id, name, path, is_directory, parent_id) VALUES (900, 'Folder', 'files/Folder', 1, NULL)", []).unwrap();
            conn.execute("INSERT INTO local_files (id, name, path, is_directory, parent_id) VALUES (901, 'nda.txt', 'files/Folder/nda.txt', 0, 900)", []).unwrap();
            drop(conn);
            let doc = db.documents.upsert_document(NewDocument {
                original_bytes: b"nda body text", original_filename: "nda.txt", source_path: None,
                source_kind: "local_storage", mime_type: None, size_bytes: 13,
                extracted_text: "Confidential information must not be disclosed to third parties.".into(),
                extraction_status: "ok", extraction_error: None, local_file_id: Some(901),
            }).unwrap();
            let doomed = db.rag.documents_for_local_files(&[900]).unwrap();
            assert_eq!(doomed, vec![doc.id], "files inside a deleted folder are found");
            db.rag.delete_documents(&doomed).unwrap();
            assert!(db.documents.get_document(doc.id).is_err());
            let q = crate::memory_db::fts::fts_query_from_user_text("confidential disclosed").unwrap();
            assert!(db.rag.keyword_search(&q, None, 5).unwrap().is_empty(), "deleted document must not be searchable");
        }

        // A draft's saved copy is replaced on each save and removed with the draft.
        let first = add_doc(&db, "unused", "Draft Memo.docx", "First version of the memo about escrow.");
        db.rag.link_draft_document(77, first, "saved").unwrap();
        db.documents.delete_document(first).ok();
        let a = add_doc(&db, "tmp-a", "Draft Memo v2.docx", "Second version of the memo about escrow terms.");
        let b = add_doc(&db, "tmp-b", "Draft Memo v3.docx", "Third version of the memo about escrow release.");
        let conn = db.rag.conn().unwrap();
        conn.execute("DELETE FROM session_documents", []).unwrap();
        drop(conn);
        db.rag.link_draft_document(77, a, "saved").unwrap();
        db.rag.link_draft_document(77, b, "saved").unwrap();
        assert!(db.documents.get_document(a).is_err(), "superseded saved copy is removed");
        assert_eq!(db.rag.saved_documents_of_draft(77).unwrap(), vec![b]);
        let published = add_doc(&db, "tmp-c", "Memo (published).docx", "Published memo about escrow.");
        db.rag.link_draft_document(77, published, "published").unwrap();
        db.rag.delete_documents(&db.rag.saved_documents_of_draft(77).unwrap()).unwrap();
        assert!(db.documents.get_document(b).is_err());
        assert!(db.documents.get_document(published).is_ok(), "published copies stay in the Vault");
    }


    #[tokio::test]
    async fn unrelated_questions_are_answered_normally_and_document_questions_still_retrieve() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let rt = eval::test_runtime();
        let (name, pages, facts) = eval::corpus().remove(0);
        add_doc(&db, "mixed-topics", name, &eval::build_document_text(pages, &facts));

        let doc_q = build(&db, &rt, "mixed-topics", Some("Is the seller's liability for warranty claims capped?"), 8_000, true).await;
        assert!(doc_q.evidence_block.as_deref().unwrap_or("").contains("DOCUMENT EXCERPTS"));

        for q in [
            "hello",
            "thanks!",
            "explain magnetic effects of electric current",
            "what does photosynthesis mean?",
            "how do I file my taxes?",
            "give me an overview of the French revolution",
        ] {
            let ctx = build(&db, &rt, "mixed-topics", Some(q), 8_000, true).await;
            let e = ctx.evidence_block.unwrap_or_default();
            assert!(!e.contains("DOCUMENT EXCERPTS") && e.contains("no attached document is relevant"), "{}: {}", q, e);
            assert!(ctx.passages.is_empty(), "{}", q);
        }

        let back = build(&db, &rt, "mixed-topics", Some("what does page 47 of the agreement say?"), 8_000, true).await;
        assert!(back.evidence_block.unwrap_or_default().contains("thirty per cent"));

        let history = vec![crate::memory::Message {
            role: "assistant".into(),
            content: "The cap is thirty per cent [Share_Purchase_Agreement.pdf, p.47].".into(),
        }];
        // attached_this_turn: false — this test is specifically about the
        // follow-up path recognising a short question in a LATER turn, so the
        // unconditional "a file arrived with this message" shortcut must not
        // be what makes it pass.
        let follow = build_with_history(&db, &rt, "mixed-topics", Some("and what about that?"), 8_000, true, &history, false).await;
        // What this pins is the GATE: a contentless follow-up, right after an
        // answer that cited a document, is recognised as still being about
        // that document.
        //
        // It cannot also assert excerpts, and could not before this enum
        // existed either. "and what about that?" has no content words to
        // match lexically, so retrieval needs vectors - and the documents in
        // this harness are chunked but never embedded (the summary reports
        // `vectors=false`). The assertion here used to be
        // `contains("DOCUMENT EXCERPTS")` and failed for that reason, masked
        // by an earlier assertion in the same test aborting first.
        assert!(
            follow.summary.contains("intent=Retrieve"),
            "short follow-up stays on the document (summary: {}; evidence: {:?})",
            follow.summary,
            follow.evidence_block
        );
        // And when the search comes back empty it must say "not found in the
        // documents", never "the documents are irrelevant": the user is three
        // words into a conversation about this file.
        assert!(
            !follow.evidence_block.clone().unwrap_or_default().contains("no attached document is relevant"),
            "a follow-up must not be told the documents are irrelevant: {:?}",
            follow.evidence_block
        );
    }

}
