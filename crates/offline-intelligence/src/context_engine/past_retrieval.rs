//! Retrieves earlier documents and conversation messages when — and only when —
//! the current question refers to them.
//!
//! This is where the two halves meet: [`reference_detector`] decides whether a
//! question points backwards, and the FTS5 indexes
//! ([`memory_db::documents_store::search_documents_fulltext`],
//! [`memory_db::conversation_store::search_messages_fulltext`]) find what it
//! points at. Without the detector this would search on every turn and pull old
//! material into unrelated new questions; without the indexes the detector would
//! have nothing to hand back.
//!
//! # The two gates, and what each is actually worth
//!
//! 1. **Detection.** No backward reference, no search. A brand-new question
//!    never touches the database.
//! 2. **Term presence.** FTS5 `MATCH` only returns rows containing at least one
//!    query term, so a false-positive detection whose words appear nowhere
//!    retrieves nothing on its own.
//!
//! What there is deliberately NOT is an absolute BM25 score threshold, and the
//! reason is measured rather than assumed. SQLite's `bm25()` includes an IDF
//! term that collapses toward zero as a word approaches appearing in every
//! indexed row — so in a small library a perfectly good match scores around
//! -1e-6, indistinguishable from a weak one. A fixed cutoff would therefore
//! reject correct matches in exactly the situation most users start in: a handful
//! of documents. Volume is bounded by rank and count instead, which is honest
//! about what the score can and cannot tell us.
//!
//! The residual risk is real and worth naming: a false-positive detection whose
//! words DO appear somewhere will retrieve the top-ranked few. That is why
//! [`ReferenceIntent::named_files`] takes precedence when present — a named or
//! described file is an exact instruction, not an inference.

use tracing::{debug, info};

use crate::context_engine::reference_detector::ReferenceIntent;
use crate::memory_db::MemoryDatabase;

/// Most past documents injected in one turn. Small on purpose: these are
/// SUPPLEMENTARY to the documents attached to this conversation, and a wall of
/// old material crowds out the thing actually being asked about.
const MAX_DOCUMENTS: i64 = 3;
/// Most past messages injected in one turn.
const MAX_MESSAGES: i64 = 6;
/// Share of this block's budget given to documents; the rest goes to messages.
/// Documents carry the substance, messages the context around it.
const DOCUMENT_SHARE_PCT: usize = 50;
/// Most earlier conversations summarised when a question asks about the
/// conversation itself. Three is enough to answer "what did we discuss" for a
/// normal working session without burying the current question.
const MAX_RECALL_SESSIONS: usize = 3;
/// Most of the user's own turns quoted from each recalled conversation.
const MAX_TURNS_PER_SESSION: usize = 4;

/// Fixed preamble of the topical block.
///
/// Held as a constant, rather than built inline, so its length can be RESERVED
/// from the budget before any content is fitted. It was previously prepended
/// after the fact, which meant the header was never counted and every block
/// overran its budget by its own size.
const BLOCK_INTRO: &str = "[MATERIAL FROM EARLIER, retrieved because this question refers back to it]\n\
     This is SUPPLEMENTARY context from previous conversations and documents, not \
     the subject of this turn. Prefer any document attached to this conversation \
     over the excerpts below. When an excerpt answers the question, answer from it \
     and cite the source name. Only if none of them bears on the question, say that \
     it was not found.\n";

/// Completeness note used when a document had to be cut.
const NOTE_EXCERPTED: &str = "One or more documents below are shown only as an EXCERPT - the rest of the file \
     is not available to you here. If the user asks for the full or exact wording of \
     such a document, say plainly that you have only an excerpt of it from an earlier \
     conversation, and that attaching the file to THIS chat will give you all of it. \
     Never present an excerpt as the complete text.\n";

/// Completeness note used when everything fitted.
const NOTE_COMPLETE: &str = "The document text below is complete, not excerpted.\n";

/// Space held back for the "[... excerpt ends here; N of M ...]" marker.
///
/// The marker is ~51 characters plus two numbers, so 80 covers seven-digit
/// character counts with room to spare. Held back rather than added on top:
/// a truncation notice that itself overruns the budget is the same bug it
/// exists to report.
const EXCERPT_MARKER_RESERVE: usize = 80;

/// Below this there is not enough room for a labelled excerpt plus its heading,
/// and a block that is all heading and no content is worse than none.
const MIN_USEFUL_BUDGET: usize = 400;

pub struct PastContext {
    /// Text to fold into the LATEST message. Query-dependent, so it can never
    /// be part of a stable prompt prefix — see
    /// `document_memory::fold_into_latest_message`.
    pub block: Option<String>,
    /// Character cost, for the caller to subtract from the conversation-history
    /// budget it hands the orchestrator.
    pub consumed_chars: usize,
    /// Why retrieval ran (or did not), for logging.
    pub reason: String,
}

impl PastContext {
    fn none(reason: impl Into<String>) -> Self {
        Self { block: None, consumed_chars: 0, reason: reason.into() }
    }
}

/// Build the past-material block for this turn.
///
/// `session_id` is the CURRENT conversation. Its own documents are excluded
/// (document_memory already injects the full set) and so are its own messages
/// (the context engine supplies recent history via tier 1). Retrieval here is
/// strictly about material the current turn would not otherwise see.
pub fn build_past_context(
    database: &MemoryDatabase,
    session_id: &str,
    user_query: &str,
    intent: &ReferenceIntent,
    budget_chars: usize,
) -> PastContext {
    build_past_context_with(database, session_id, user_query, intent, budget_chars, &[])
}

/// As `build_past_context`, merging `semantic_messages` (earlier messages
/// found by meaning, already scoped to this chat's matter) with the keyword
/// matches.
pub fn build_past_context_with(
    database: &MemoryDatabase,
    session_id: &str,
    user_query: &str,
    intent: &ReferenceIntent,
    budget_chars: usize,
    semantic_messages: &[(crate::memory_db::StoredMessage, f64)],
) -> PastContext {
    if !intent.refers_to_past() {
        return PastContext::none(format!("no retrieval: {}", intent.explain()));
    }
    if budget_chars < MIN_USEFUL_BUDGET {
        return PastContext::none(format!(
            "no retrieval: only {} chars of budget available, below the {} needed for a \
             usable excerpt",
            budget_chars, MIN_USEFUL_BUDGET
        ));
    }

    // WHICH KIND of backward reference is this?
    //
    // "What did we discuss about the indemnity clause?" names a subject, so
    // searching for that subject is right. "What else did we discuss?" names
    // nothing at all - every word in it describes the ACT of conversing. A
    // keyword search for those words was measured returning noise, because
    // they appear in nearly every stored message.
    //
    // So a question with no content-bearing term is not a search at all. It is
    // a request to be reminded, and the right answer is ordered by RECENCY, not
    // by relevance.
    if crate::memory_db::fts::content_tokens(user_query).is_empty() {
        return build_recall_context(database, session_id, intent, budget_chars);
    }

    // Documents already attached HERE are excluded: document_memory injects them
    // in full, so retrieving them again would duplicate the same text in one
    // prompt and waste budget the rest of the block needs.
    let in_session: std::collections::HashSet<i64> = database
        .documents
        .get_session_documents(session_id)
        .map(|docs| docs.iter().map(|d| d.id).collect())
        .unwrap_or_default();

    // Reserve the header BEFORE splitting the budget, using the longer of the
    // two completeness notes so the reservation holds whichever one is chosen.
    // The caller subtracts `consumed_chars` from what it gives the orchestrator,
    // so a block that overruns its budget silently shrinks the conversation
    // history the model gets.
    let header_reserve = BLOCK_INTRO.len() + NOTE_EXCERPTED.len().max(NOTE_COMPLETE.len());
    let usable = budget_chars.saturating_sub(header_reserve);
    if usable < MIN_USEFUL_BUDGET {
        return PastContext::none(format!(
            "no retrieval: {} chars of budget leaves {} after the explanatory header, \
             below the {} needed for a usable excerpt",
            budget_chars, usable, MIN_USEFUL_BUDGET
        ));
    }

    let doc_budget = usable * DOCUMENT_SHARE_PCT / 100;

    let scope = crate::rag::scope::resolve(database, session_id);
    let documents = find_documents(database, user_query, intent, &in_session, &scope);
    let allowed_sessions = crate::rag::scope::allowed_sessions(database, session_id);
    let keyword_messages = match database.conversations.search_messages_fulltext_scoped(
        user_query,
        MAX_MESSAGES,
        Some(session_id),
        intent.time_range,
        allowed_sessions.as_ref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            debug!("Past-message search failed: {}", e);
            Vec::new()
        }
    };
    // Reciprocal-rank fusion of keyword and meaning matches.
    let messages: Vec<(crate::memory_db::StoredMessage, f64)> = {
        let mut scores: std::collections::HashMap<i64, (crate::memory_db::StoredMessage, f64)> = std::collections::HashMap::new();
        for list in [&keyword_messages[..], semantic_messages] {
            for (rank, (m, _)) in list.iter().enumerate() {
                if m.session_id == session_id {
                    continue;
                }
                if let Some(ref allowed) = allowed_sessions {
                    if !allowed.contains(&m.session_id) {
                        continue;
                    }
                }
                let entry = scores.entry(m.id).or_insert_with(|| (m.clone(), 0.0));
                entry.1 += 1.0 / (60.0 + rank as f64 + 1.0);
            }
        }
        let mut v: Vec<(crate::memory_db::StoredMessage, f64)> = scores.into_values().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
        v.truncate(MAX_MESSAGES as usize);
        v
    };

    if documents.is_empty() && messages.is_empty() {
        // Detection fired but nothing matched. Common and healthy: it is what
        // makes a generous detector safe.
        return PastContext::none(format!(
            "retrieval ran ({}) but nothing matched",
            intent.explain()
        ));
    }

    // Body first, header second: the header must state whether what follows is
    // complete, and that is not known until the documents have been fitted to
    // the budget.
    let mut block = String::new();
    let mut any_truncated = false;

    let mut doc_used = 0usize;
    let mut docs_included = 0usize;
    for (doc, score) in &documents {
        let heading = format!(
            "\n--- From document '{}' (attached {}) ---\n",
            doc.original_filename,
            doc.last_referenced_at.format("%Y-%m-%d")
        );
        let remaining = doc_budget.saturating_sub(doc_used);
        if remaining <= heading.len() + 120 {
            break;
        }
        let (excerpt, truncated) = relevant_excerpt(database, doc, user_query, remaining - heading.len());
        any_truncated |= truncated;
        doc_used += heading.len() + excerpt.len();
        block.push_str(&heading);
        block.push_str(&excerpt);
        docs_included += 1;
        debug!(
            "Past retrieval: document '{}' (score {:.6}) included, {} chars",
            doc.original_filename, score, excerpt.len()
        );
    }

    // Messages get whatever the documents left, not a fixed remainder.
    let msg_budget = usable.saturating_sub(doc_used);
    let mut msg_used = 0usize;
    let mut msgs_included = 0usize;
    if !messages.is_empty() {
        let heading = "\n--- From earlier conversations ---\n";
        if msg_budget > heading.len() + 120 {
            block.push_str(heading);
            msg_used += heading.len();
            // Whole exchanges: each match comes back with its question or
            // answer, so the model knows what an answer was answering.
            let hits: Vec<crate::memory_db::StoredMessage> = messages.iter().map(|(m, _)| m.clone()).collect();
            let terms = crate::memory_db::fts::content_tokens(user_query);
            let current = super::duplicates::Current::load(database, session_id, user_query);
            let pairs = super::duplicates::filter_pairs(database, &current, super::conversation_memory::pairs_for(database, &hits));
            for pair in pairs {
                let when = pair
                    .user
                    .as_ref()
                    .or(pair.assistant.as_ref())
                    .map(|m| m.timestamp.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let line = pair.render(&format!("Earlier conversation, {}", when), &terms);
                if msg_used + line.len() > msg_budget {
                    break;
                }
                msg_used += line.len();
                block.push_str(&line);
                msgs_included += 1;
            }
        }
    }

    if docs_included == 0 && msgs_included == 0 {
        return PastContext::none(
            "no retrieval: matches were found but none fitted the available budget",
        );
    }

    // Completeness, stated explicitly - the same contract `document_memory`
    // keeps for attached files. Without it, a user asking for the exact or full
    // wording of a retrieved document gets an answer assembled from a fragment
    // with no indication that it was one. In legal work that is the most
    // damaging thing this system can do, and it is invisible to the user.
    //
    // The remedy is named too, not just the limitation: attaching the file to
    // the current conversation puts document_memory in charge of it, which does
    // supply the whole text.
    let completeness = if any_truncated { NOTE_EXCERPTED } else { NOTE_COMPLETE };
    let block = format!("{}{}{}", BLOCK_INTRO, completeness, block);

    let reason = format!(
        "retrieved {} document(s) and {} message(s) because {}",
        docs_included, msgs_included, intent.explain()
    );
    info!("Past retrieval for session {}: {}", session_id, reason);

    let consumed_chars = block.len();
    PastContext { block: Some(block), consumed_chars, reason }
}

/// Answer "what did we discuss before?" from the most recent conversations.
///
/// Deliberately NOT a search. The question carries no subject to search for, so
/// this reports what is most recent: each conversation's title, when it
/// happened, which files were involved, and the questions the user actually
/// asked in it. Those questions are the best available description of what a
/// conversation was about, and they are short - quoting whole assistant replies
/// would fill the budget with two answers instead of summarising several
/// conversations.
fn build_recall_context(
    database: &MemoryDatabase,
    current_session_id: &str,
    intent: &ReferenceIntent,
    budget_chars: usize,
) -> PastContext {
    let allowed_sessions = crate::rag::scope::allowed_sessions(database, current_session_id);
    let sessions = match database.conversations.get_all_sessions() {
        Ok(s) => s,
        Err(e) => {
            debug!("Recall: could not list sessions: {}", e);
            return PastContext::none(format!("no retrieval: could not read past conversations ({})", e));
        }
    };

    let mut header = String::from(
        "[YOUR EARLIER CONVERSATIONS, retrieved because this question asks what was          discussed rather than asking about a topic]
         These are previous chat sessions, most recent first, listed with the files          involved and the questions asked in them. Answer from these. If the specific          thing being asked about is not here, say so plainly - do not guess at what          was said.
",
    );

    let mut body = String::new();
    let mut included = 0usize;
    // A conversation that repeats this one is a copy, not earlier material.
    let current = super::duplicates::Current::load(database, current_session_id, "");
    let duplicates = current.duplicate_sessions(database, sessions.iter().map(|s| &s.id));

    // get_all_sessions is ordered by last_accessed DESC, so this is
    // most-recent-first without a second sort.
    for session in sessions.iter() {
        if included >= MAX_RECALL_SESSIONS || header.len() + body.len() >= budget_chars {
            break;
        }
        if session.id == current_session_id || duplicates.contains(&session.id) {
            continue;
        }
        if let Some(ref allowed) = allowed_sessions {
            if !allowed.contains(&session.id) {
                continue;
            }
        }
        // A date in the question ("what did we discuss yesterday") narrows
        // which conversations are eligible at all.
        if let Some((start, end)) = intent.time_range {
            if session.last_accessed < start || session.last_accessed > end {
                continue;
            }
        }

        let messages = match database.conversations.get_session_messages(&session.id, Some(200), None) {
            Ok(m) => m,
            Err(e) => {
                debug!("Recall: could not read messages for {}: {}", session.id, e);
                continue;
            }
        };
        // An empty session is a chat the user opened and never used.
        if messages.is_empty() {
            continue;
        }

        let title = session
            .metadata
            .title
            .clone()
            .unwrap_or_else(|| "Untitled conversation".to_string());

        let mut entry = format!(
            "
--- Conversation: \"{}\" ({}) ---
",
            title,
            session.last_accessed.format("%Y-%m-%d")
        );

        // Naming the files is often the whole answer to "what did we discuss".
        if let Ok(docs) = database.documents.get_session_documents(&session.id) {
            if !docs.is_empty() {
                let names: Vec<&str> = docs.iter().map(|d| d.original_filename.as_str()).collect();
                entry.push_str(&format!("Files discussed: {}
", names.join(", ")));
            }
        }

        for msg in messages.iter().filter(|m| m.role == "user").take(MAX_TURNS_PER_SESSION) {
            entry.push_str(&format!("You asked: \"{}\"
", one_line(&msg.content, 220)));
        }

        if header.len() + body.len() + entry.len() > budget_chars {
            break;
        }
        body.push_str(&entry);
        included += 1;
    }

    if included == 0 {
        return PastContext::none(
            "no retrieval: the question asks about earlier conversations, but there are              no earlier conversations on record",
        );
    }

    header.push_str(&body);
    let consumed_chars = header.len();
    let reason = format!(
        "recalled {} earlier conversation(s) by recency because the question asks what          was discussed and names no subject to search for",
        included
    );
    info!("Past retrieval for session {}: {}", current_session_id, reason);

    PastContext { block: Some(header), consumed_chars, reason }
}

/// Find past documents, preferring files the user actually named.
///
/// A named or described file is an explicit instruction, so it is looked up
/// directly by name and takes the whole document budget before ranked search is
/// consulted. Ranked search only fills what is left — which is what stops a
/// generic phrase in the same sentence from displacing the file the user asked
/// for by name.
fn find_documents(
    database: &MemoryDatabase,
    user_query: &str,
    intent: &ReferenceIntent,
    in_session: &std::collections::HashSet<i64>,
    scope: &crate::rag::scope::Scope,
) -> Vec<(crate::memory_db::DocumentRecord, f64)> {
    let mut out: Vec<(crate::memory_db::DocumentRecord, f64)> = Vec::new();
    let in_range = |doc: &crate::memory_db::DocumentRecord| match intent.time_range {
        Some((start, end)) => doc.last_referenced_at >= start && doc.last_referenced_at <= end,
        None => true,
    };

    // Files named or described in the question, by file name.
    for filename in &intent.named_files {
        let stem = filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(filename);
        match database.documents.search_documents(stem, MAX_DOCUMENTS * 4) {
            Ok(hits) => {
                for doc in hits {
                    let named = doc.original_filename.eq_ignore_ascii_case(filename)
                        || doc.original_filename.to_lowercase().contains(&filename.to_lowercase());
                    if named
                        && doc.extraction_status == "ok"
                        && !doc.extracted_text.trim().is_empty()
                        && !in_session.contains(&doc.id)
                        && scope.allows(doc.id)
                        && !out.iter().any(|(d, _)| d.id == doc.id)
                    {
                        out.push((doc, f64::MAX));
                    }
                }
            }
            Err(e) => debug!("Named-file lookup failed for '{}': {}", filename, e),
        }
    }

    if out.len() as i64 >= MAX_DOCUMENTS {
        out.truncate(MAX_DOCUMENTS as usize);
        return out;
    }

    // Documents whose passages match the question, within this chat's scope.
    let Some(fts) = crate::memory_db::fts::fts_query_from_user_text(user_query) else {
        return out;
    };
    let terms = crate::memory_db::fts::content_tokens(user_query);
    let beyond = scope.beyond_session(database);
    let hits = database.rag.keyword_search(&fts, beyond.as_ref(), 60).unwrap_or_default();
    let mut best: Vec<(i64, f64)> = Vec::new();
    for (_, doc_id, score) in hits {
        if in_session.contains(&doc_id) {
            continue;
        }
        match best.iter_mut().find(|(d, _)| *d == doc_id) {
            Some(entry) => entry.1 = entry.1.max(score),
            None => best.push((doc_id, score)),
        }
    }
    best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (doc_id, score) in best {
        if out.len() as i64 >= MAX_DOCUMENTS {
            break;
        }
        if out.iter().any(|(d, _)| d.id == doc_id) {
            continue;
        }
        if let Ok(doc) = database.documents.get_document(doc_id) {
            if in_range(&doc) && covers_enough(&doc.extracted_text, &terms) {
                out.push((doc, score));
            }
        }
    }
    out
}

/// Whether `text` contains enough of the question's content words to be worth
/// showing: at least half of them (rounded up, and at least one), matched by
/// their first five letters. The keyword index ORs every term, so a document
/// that shares one common word with a question still ranks; this keeps such a
/// document (for example a placeholder file) out of the prompt.
pub(crate) fn covers_enough(text: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let lower = text.to_lowercase();
    let hit = terms
        .iter()
        .filter(|t| lower.contains(&t.chars().take(5).collect::<String>()))
        .count();
    hit >= terms.len().div_ceil(2).max(1)
}

/// The whole document when it fits; otherwise the passages that match the
/// question (in document order, with citations), marked as an excerpt.
fn relevant_excerpt(
    database: &MemoryDatabase,
    doc: &crate::memory_db::DocumentRecord,
    user_query: &str,
    budget: usize,
) -> (String, bool) {
    let full = crate::rag::chunker::clean_text(&doc.extracted_text);
    if full.len() <= budget {
        return excerpt(&full, budget);
    }
    let _ = database.rag.ensure_chunked(&[doc.id]);
    let scope: std::collections::HashSet<i64> = [doc.id].into_iter().collect();
    let hits = crate::memory_db::fts::fts_query_from_user_text(user_query)
        .map(|q| database.rag.keyword_search(&q, Some(&scope), 12).unwrap_or_default())
        .unwrap_or_default();
    if hits.is_empty() {
        return excerpt(&full, budget);
    }
    let ids: Vec<i64> = hits.iter().map(|(id, _, _)| *id).collect();
    let mut rows = database.rag.chunks_by_ids(&ids).unwrap_or_default();
    rows.sort_by_key(|c| ids.iter().position(|id| *id == c.id).unwrap_or(usize::MAX));
    let content_budget = budget.saturating_sub(EXCERPT_MARKER_RESERVE);
    let mut picked: Vec<crate::rag::store::ChunkRow> = Vec::new();
    let mut used = 0usize;
    for c in rows {
        let (s, e) = (c.byte_start as usize, (c.byte_end as usize).min(doc.extracted_text.len()));
        if s >= e || !doc.extracted_text.is_char_boundary(s) || !doc.extracted_text.is_char_boundary(e) {
            continue;
        }
        let len = c.text(&doc.extracted_text).len() + 24;
        if used + len > content_budget {
            continue;
        }
        used += len;
        picked.push(c);
    }
    if picked.is_empty() {
        return excerpt(&full, budget);
    }
    picked.sort_by_key(|c| c.ord);
    let mut body = String::new();
    for c in &picked {
        let text = c.text(&doc.extracted_text);
        body.push_str(&format!("{} {}
", c.citation(), text));
    }
    (format!("{}[... excerpt ends here; matching passages shown from a {}-character document ...]
", body, full.len()), true)
}

fn excerpt(text: &str, budget: usize) -> (String, bool) {
    let trimmed = text.trim();
    if trimmed.len() <= budget {
        return (format!("{}\n", trimmed), false);
    }
    // Room for the truncation marker must come OUT of the budget, not be added
    // on top of it. Appending it afterwards is what made every truncated block
    // overrun by the marker's own length.
    let content_budget = budget.saturating_sub(EXCERPT_MARKER_RESERVE);
    let mut end = content_budget.min(trimmed.len());
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!(
            "{}\n[... excerpt ends here; {} of {} characters shown ...]\n",
            &trimmed[..end],
            end,
            trimmed.len()
        ),
        true,
    )
}

/// Collapse a message to a single bounded line. Past messages are context, not
/// content: a 4000-character assistant reply quoted whole would consume the
/// entire block for one entry.
fn one_line(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(max).collect();
    format!("{}…", truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::reference_detector::detect_reference;
    use crate::memory_db::NewDocument;
    use chrono::{NaiveDate, TimeZone, Utc};

    fn now() -> chrono::DateTime<Utc> {
        Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 7, 29)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
        )
    }

    fn db() -> MemoryDatabase {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("current", None).ok();
        db.conversations.create_session_with_id("older", None).ok();
        db
    }

    fn add_doc(db: &MemoryDatabase, name: &str, text: &str) -> i64 {
        db.documents
            .upsert_document(NewDocument {
                original_bytes: text.as_bytes(),
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

    fn build(db: &MemoryDatabase, query: &str, budget: usize) -> PastContext {
        let known: Vec<String> = db
            .documents
            .all_documents(200)
            .unwrap()
            .into_iter()
            .map(|d| d.original_filename)
            .collect();
        let intent = detect_reference(query, &known, now());
        build_past_context(db, "current", query, &intent, budget)
    }

    /// Seed an earlier conversation the way a real one looks: a title, a
    /// document, and the questions the user asked.
    fn seed_earlier_conversation(db: &MemoryDatabase) {
        db.conversations
            .update_session_title("older", "Software agreement review")
            .unwrap();
        let doc = add_doc(
            db,
            "Software_Agreement_Akhil_Mansoor.docx",
            "The Developer retains no ownership in the Software once delivered.",
        );
        db.documents.link_session("older", doc, "paperclip").unwrap();
        db.conversations
            .store_messages_batch(
                "older",
                &[
                    ("user".to_string(), "review and summarize the content inside the word document".to_string(), 0, 10, 0.5),
                    ("assistant".to_string(), "The agreement transfers all ownership to the client.".to_string(), 1, 10, 0.5),
                    ("user".to_string(), "what are the payment terms".to_string(), 2, 6, 0.5),
                ],
            )
            .unwrap();
    }

    /// THE reported failure. "what else did we discuss in our previous
    /// conversation?" contains no subject to search for, so the old path built
    /// a query out of `what OR else OR did OR discuss OR our` and returned
    /// noise. It must now recall by recency instead.
    #[test]
    fn a_question_about_the_conversation_itself_recalls_by_recency_not_by_search() {
        let db = db();
        seed_earlier_conversation(&db);

        let ctx = build(&db, "what else did we discuss in our previous conversation?", 6000);
        let block = ctx.block.expect("a recall question must produce context");

        assert!(block.contains("Software agreement review"), "must name the conversation: {}", block);
        assert!(
            block.contains("Software_Agreement_Akhil_Mansoor.docx"),
            "must name the file that was discussed: {}",
            block
        );
        assert!(
            block.contains("review and summarize the content inside the word document"),
            "must quote what the user actually asked: {}",
            block
        );
        assert!(ctx.reason.contains("recalled"), "reason: {}", ctx.reason);
    }

    #[test]
    fn do_you_remember_our_previous_conversation_also_recalls() {
        let db = db();
        seed_earlier_conversation(&db);
        let ctx = build(&db, "Do you remember our previous conversation?", 6000);
        assert!(
            ctx.block.is_some(),
            "this exact question returned nothing in the reported session: {}",
            ctx.reason
        );
    }

    /// The current chat is already in the prompt; repeating it would waste the
    /// budget and make the model think it happened twice.
    #[test]
    fn recall_never_includes_the_current_conversation() {
        let db = db();
        seed_earlier_conversation(&db);
        db.conversations
            .store_messages_batch(
                "current",
                &[("user".to_string(), "a question asked in the CURRENT chat".to_string(), 0, 8, 0.5)],
            )
            .unwrap();

        let block = build(&db, "what did we discuss before?", 6000).block.unwrap();
        assert!(
            !block.contains("CURRENT chat"),
            "the current conversation must not be recalled into itself: {}",
            block
        );
    }

    /// Honesty over invention: with nothing on record, say so.
    #[test]
    fn recall_with_no_earlier_conversations_says_so() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("current", None).ok();

        let ctx = build(&db, "what did we discuss previously?", 6000);
        assert!(ctx.block.is_none());
        assert!(
            ctx.reason.contains("no earlier conversations"),
            "the reason must be specific, got: {}",
            ctx.reason
        );
    }

    /// A conversation the user opened and never used is not something that was
    /// "discussed" and must not be listed.
    #[test]
    fn an_empty_conversation_is_not_recalled() {
        let db = db();
        db.conversations.create_session_with_id("never_used", None).ok();
        db.conversations.update_session_title("never_used", "Empty chat").unwrap();
        seed_earlier_conversation(&db);

        let block = build(&db, "what did we talk about earlier?", 6000).block.unwrap();
        assert!(!block.contains("Empty chat"), "block: {}", block);
    }

    /// The other half of the split: a question that DOES name a subject must
    /// still go through search, not recall.
    #[test]
    fn a_question_naming_a_subject_still_searches() {
        let db = db();
        seed_earlier_conversation(&db);

        let ctx = build(&db, "what did we discuss about the ownership of the software?", 6000);
        let block = ctx.block.expect("a topical backward reference must retrieve");
        assert!(
            !ctx.reason.contains("recalled"),
            "a question with a subject must search, not recall: {}",
            ctx.reason
        );
        assert!(block.contains("MATERIAL FROM EARLIER"), "block: {}", block);
    }

    #[test]
    fn recall_stays_within_its_budget() {
        let db = db();
        seed_earlier_conversation(&db);
        for budget in [MIN_USEFUL_BUDGET, 800, 2000, 6000] {
            let ctx = build(&db, "what did we discuss before?", budget);
            if let Some(block) = ctx.block {
                assert!(
                    block.len() <= budget,
                    "recall block of {} chars exceeded its {} budget",
                    block.len(),
                    budget
                );
            }
        }
    }

    /// The central guarantee: a NEW question must not retrieve, even when the
    /// database is full of material whose words would match.
    #[test]
    fn a_new_topic_does_not_retrieve_anything() {
        let db = db();
        add_doc(&db, "merger.pdf", "The termination provisions require notice.");
        db.conversations
            .store_messages_batch(
                "older",
                &[("user".to_string(), "how does termination notice work".to_string(), 0, 0, 0.5)],
            )
            .unwrap();

        let ctx = build(&db, "How do I terminate an agreement?", 8000);
        assert!(
            ctx.block.is_none(),
            "a new question must not pull in past material: {:?}",
            ctx.block
        );
        assert_eq!(ctx.consumed_chars, 0);
        assert!(ctx.reason.contains("no retrieval"), "{}", ctx.reason);
    }

    #[test]
    fn a_backward_reference_retrieves_a_past_document() {
        let db = db();
        add_doc(&db, "merger.pdf", "The escrow amount is held for eighteen months.");
        let ctx = build(&db, "What did we discuss about the escrow amount?", 8000);
        let block = ctx.block.expect("must retrieve");
        assert!(block.contains("merger.pdf"), "the source must be named: {}", block);
        assert!(block.contains("eighteen months"), "content must be present: {}", block);
        assert!(ctx.consumed_chars > 0);
    }

    #[test]
    fn a_backward_reference_retrieves_past_messages_from_other_sessions() {
        let db = db();
        db.conversations
            .store_messages_batch(
                "older",
                &[(
                    "assistant".to_string(),
                    "The deposit is refundable within fourteen days.".to_string(),
                    0,
                    0,
                    0.5,
                )],
            )
            .unwrap();
        let ctx = build(&db, "What did you say about the refundable deposit earlier?", 8000);
        let block = ctx.block.expect("must retrieve");
        assert!(
            block.contains("fourteen days"),
            "the past message must be included: {}",
            block
        );
        assert!(block.contains("earlier conversations"), "{}", block);
    }

    /// The current conversation's own messages must not be retrieved - the
    /// context engine already supplies them, and duplicating them wastes budget.
    #[test]
    fn the_current_sessions_own_messages_are_not_retrieved() {
        let db = db();
        db.conversations
            .store_messages_batch(
                "current",
                &[("user".to_string(), "the escrow is disputed".to_string(), 0, 0, 0.5)],
            )
            .unwrap();
        let ctx = build(&db, "What did we say about the disputed escrow earlier?", 8000);
        assert!(
            ctx.block.is_none(),
            "the current session's own messages must not be re-injected: {:?}",
            ctx.block
        );
    }

    /// A document already attached HERE is injected in full by document_memory,
    /// so retrieving it again would put the same text in the prompt twice.
    #[test]
    fn documents_already_attached_to_this_session_are_not_retrieved_again() {
        let db = db();
        let id = add_doc(&db, "lease.pdf", "Rent is payable monthly in advance.");
        db.documents.link_session("current", id, "paperclip").unwrap();

        let ctx = build(&db, "What did we discuss about the rent payable?", 8000);
        assert!(
            ctx.block.is_none(),
            "an in-session document must not be duplicated by retrieval: {:?}",
            ctx.block
        );
    }

    /// Detection firing with nothing to match is the normal, healthy case that
    /// makes a generous detector safe.
    #[test]
    fn detection_without_a_match_injects_nothing() {
        let db = db();
        add_doc(&db, "lease.pdf", "Rent is payable monthly in advance.");
        let ctx = build(&db, "What did we discuss about cryptocurrency custody?", 8000);
        assert!(ctx.block.is_none(), "{:?}", ctx.block);
        assert!(
            ctx.reason.contains("nothing matched"),
            "the reason must distinguish 'searched and found nothing' from 'did not \
             search': {}",
            ctx.reason
        );
    }

    /// A named file is an explicit instruction and must win over a document that
    /// merely ranks well for the rest of the sentence.
    #[test]
    fn a_named_file_is_prioritised_over_generic_matches() {
        let db = db();
        add_doc(&db, "schedule-b.pdf", "Deliverable milestones and acceptance criteria.");
        for i in 0..5 {
            add_doc(
                &db,
                &format!("other{}.pdf", i),
                &format!("Document {} discusses deliverable acceptance at length.", i),
            );
        }
        let ctx = build(&db, "what did schedule-b.pdf say about deliverable acceptance?", 8000);
        let block = ctx.block.expect("must retrieve");
        assert!(
            block.contains("schedule-b.pdf"),
            "the named file must be included: {}",
            block
        );
    }

    /// Truncation must be announced, never silent - the same contract as
    /// document_memory's PARTIAL marker.
    #[test]
    fn oversized_retrieved_documents_are_announced_as_excerpts() {
        let db = db();
        let long = format!(
            "The escrow provisions are as follows. {}",
            "This clause continues at considerable length. ".repeat(200)
        );
        add_doc(&db, "long.pdf", &long);

        let ctx = build(&db, "What did we discuss about the escrow provisions?", 1500);
        let block = ctx.block.expect("must retrieve");
        assert!(
            block.contains("excerpt ends here"),
            "a truncated excerpt must say so: {}",
            block
        );
        assert!(
            block.len() < long.len(),
            "the block must actually be bounded by the budget"
        );
    }

    /// The block must respect the budget it is given, because the caller
    /// subtracts it from the conversation-history allowance. Overrunning would
    /// silently squeeze out chat history.
    /// A retrieved document that had to be cut must SAY it was cut, and say
    /// what to do about it. Without this the model answers "show me the exact
    /// wording" from a fragment and presents it as the whole thing - which in
    /// legal work is the most damaging failure available to it.
    #[test]
    fn a_truncated_document_announces_that_it_is_only_an_excerpt() {
        let db = db();
        add_doc(
            &db,
            "escrow-terms.pdf",
            &format!("Escrow provision. {}", "additional clause text. ".repeat(400)),
        );

        let block = build(&db, "what did we discuss about the escrow provision?", 3000)
            .block
            .expect("must retrieve");

        assert!(block.contains("EXCERPT"), "must flag the excerpt: {}", block);
        assert!(
            block.contains("attaching the file to THIS chat"),
            "must name the remedy, not just the limitation: {}",
            block
        );
        assert!(!block.contains("complete, not excerpted"));
    }

    /// The converse: when everything fitted, say so, or the model hedges about
    /// completeness it actually has.
    #[test]
    fn an_untruncated_document_is_declared_complete() {
        let db = db();
        add_doc(&db, "escrow-terms.pdf", "Escrow provision: funds are held for 30 days.");

        let block = build(&db, "what did we discuss about the escrow provision?", 6000)
            .block
            .expect("must retrieve");

        assert!(block.contains("complete, not excerpted"), "block: {}", block);
        assert!(!block.contains("EXCERPT"), "block: {}", block);
    }

    #[test]
    fn the_block_stays_within_its_budget() {
        let db = db();
        for i in 0..6 {
            add_doc(
                &db,
                &format!("doc{}.pdf", i),
                &format!("Escrow provision {} explained at length. {}", i, "filler ".repeat(500)),
            );
        }
        db.conversations
            .store_messages_batch(
                "older",
                &(0..10)
                    .map(|i| {
                        (
                            "user".to_string(),
                            format!("message {} about escrow provisions and more text", i),
                            i,
                            0,
                            0.5,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        for budget in [500usize, 1500, 4000, 12000] {
            let ctx = build(&db, "What did we discuss about escrow provisions?", budget);
            if let Some(ref block) = ctx.block {
                // No slack. The header is RESERVED from the budget before any
                // content is fitted, so the whole block - explanation included -
                // fits the number the caller was promised. This assertion used
                // to allow 700 chars of overrun, which was the header escaping
                // the budget entirely.
                assert!(
                    block.len() <= budget,
                    "block of {} chars overran a {}-char budget",
                    block.len(),
                    budget
                );
                assert_eq!(ctx.consumed_chars, block.len());
            }
        }
    }

    #[test]
    fn a_budget_too_small_to_be_useful_retrieves_nothing() {
        let db = db();
        add_doc(&db, "merger.pdf", "The escrow amount is held for eighteen months.");
        let ctx = build(&db, "What did we discuss about escrow?", 100);
        assert!(ctx.block.is_none());
        assert!(ctx.reason.contains("budget"), "{}", ctx.reason);
    }

    /// A date in the question must narrow retrieval to that window, not merely
    /// trigger it.
    #[test]
    fn a_time_reference_narrows_which_messages_are_retrieved() {
        let db = db();
        db.conversations
            .store_messages_batch(
                "older",
                &[("assistant".to_string(), "the escrow figure is two million".to_string(), 0, 0, 0.5)],
            )
            .unwrap();

        // Backdate the message well outside a "yesterday" window.
        {
            let conn = db.conversations.get_conn_public().unwrap();
            conn.execute(
                "UPDATE messages SET timestamp = ?1",
                [Utc
                    .from_utc_datetime(
                        &NaiveDate::from_ymd_opt(2026, 1, 5)
                            .unwrap()
                            .and_hms_opt(9, 0, 0)
                            .unwrap(),
                    )
                    .to_rfc3339()],
            )
            .unwrap();
        }

        let out_of_window = build(&db, "what did you say about the escrow figure yesterday?", 8000);
        assert!(
            out_of_window.block.is_none()
                || !out_of_window.block.as_ref().unwrap().contains("two million"),
            "a message outside the stated window must not be returned: {:?}",
            out_of_window.block
        );

        // Without the date, the same question finds it.
        let unrestricted = build(&db, "what did you say about the escrow figure earlier?", 8000);
        assert!(
            unrestricted.block.expect("must retrieve").contains("two million"),
            "the same message must be findable when no window is stated"
        );
    }

    /// The reason string is what makes an unexpected retrieval debuggable.
    #[test]
    fn the_outcome_is_always_explained() {
        let db = db();
        add_doc(&db, "merger.pdf", "The escrow amount is held for eighteen months.");

        let retrieved = build(&db, "What did we discuss about the escrow amount?", 8000);
        assert!(retrieved.reason.contains("retrieved"), "{}", retrieved.reason);
        // Names the triggering signal, whichever phrase pattern matched - the
        // point is that the reason identifies WHY, not which synonym won.
        assert!(
            retrieved.reason.contains("phrase"),
            "the reason must name the signal that triggered retrieval: {}",
            retrieved.reason
        );
        assert!(retrieved.reason.contains("1 document"), "{}", retrieved.reason);

        let skipped = build(&db, "What is an escrow account?", 8000);
        assert!(skipped.reason.contains("no retrieval"), "{}", skipped.reason);
    }
}

#[cfg(test)]
mod relevance_bar_tests {
    use super::*;

    fn t(q: &str) -> Vec<String> {
        crate::memory_db::fts::content_tokens(q)
    }

    #[test]
    fn a_placeholder_sharing_one_word_with_the_question_is_kept_out() {
        let placeholder = "About the Knowledge Library. This is a placeholder document included so the \
            Knowledge Library works end to end while official legal reference documents are prepared.";
        assert!(!covers_enough(placeholder, &t("What is the name of the playlist I created on Spotify?")));
        assert!(!covers_enough(placeholder, &t("Which documents did the landlord send about the deposit refund?")));
    }

    #[test]
    fn a_document_about_the_question_passes() {
        let lease = "The tenant shall pay a deposit equal to two months of rent. The deposit refund is due \
            within thirty days after the landlord inspects the premises.";
        assert!(covers_enough(lease, &t("When is the deposit refund due from the landlord?")));
        // Inflections still count: the first five letters are compared.
        assert!(covers_enough(lease, &t("refunding deposits")));
    }

    #[test]
    fn the_intro_asks_for_an_answer_from_the_excerpts() {
        assert!(!BLOCK_INTRO.contains("rather than inferring"));
        assert!(BLOCK_INTRO.contains("answer from it"));
    }
}
