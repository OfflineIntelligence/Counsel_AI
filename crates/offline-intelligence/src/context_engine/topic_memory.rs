//! Topic memory: what each conversation is about, as searchable terms.
//!
//! Recall from other conversations used to need a phrase that refers back
//! ("earlier", "we discussed") or a keyword overlap of most of the question.
//! A conversation's topics (the content words it keeps coming back to, the
//! clauses, sections and files it cites, the names of parties) are stored
//! here as weighted terms and indexed with FTS5, so a new question that
//! touches the same matter finds that conversation even when it is worded
//! differently and names none of its exact sentences.
//!
//! No model is involved: terms come from the text, are merged per turn in a
//! single small row per conversation, and are looked up with one indexed
//! FTS5 query (well under a millisecond).

use std::collections::{HashMap, HashSet};

use lazy_static::lazy_static;
use regex::Regex;
use rusqlite::params;

use crate::memory_db::MemoryDatabase;

/// Terms kept per conversation (the most frequent).
const MAX_TERMS: usize = 64;

lazy_static! {
    static ref CLAUSE: Regex =
        Regex::new(r"(?i)\b(section|clause|article|schedule|annex|exhibit|appendix|art\.|§)\s*([0-9]+(?:\.[0-9]+)*(?:\([a-z0-9]+\))?)").unwrap();
    static ref FILE: Regex = Regex::new(r"(?i)\b[\w\-]+\.(pdf|docx?|xlsx?|pptx?|txt|png|jpe?g)\b").unwrap();
    static ref NAME: Regex = Regex::new(r"\b([A-Z][a-z]+(?:\s+(?:[A-Z][a-z]+|&|of|de|da|der|van))*\s+[A-Z][a-z]+)\b").unwrap();
    static ref CITE: Regex = Regex::new(r"\[([^\]\n]{1,80}\.(?:pdf|docx?|xlsx?|pptx?|txt))[,\]]").unwrap();
}

/// Weighted terms of one piece of text. Clause references, file names and
/// multi-word names weigh more than single content words.
pub fn extract_terms(text: &str) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for t in crate::memory_db::fts::content_tokens(text) {
        if t.len() >= 4 && !t.chars().all(|c| c.is_ascii_digit()) {
            *out.entry(t).or_insert(0) += 1;
        }
    }
    for c in CLAUSE.captures_iter(text) {
        let kind = c[1].trim_end_matches('.').to_lowercase();
        *out.entry(format!("{} {}", kind, &c[2].to_lowercase())).or_insert(0) += 3;
    }
    for f in FILE.find_iter(text).chain(CITE.captures_iter(text).filter_map(|c| c.get(1))) {
        *out.entry(f.as_str().to_lowercase()).or_insert(0) += 3;
    }
    for n in NAME.find_iter(text) {
        let n = n.as_str();
        if n.split_whitespace().count() <= 5 {
            *out.entry(n.to_lowercase()).or_insert(0) += 2;
        }
    }
    out
}

fn top_terms(counts: &HashMap<String, u32>) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(MAX_TERMS);
    v
}

/// Merge one turn into the conversation's topics.
pub fn record_turn(db: &MemoryDatabase, session_id: &str, question: &str, answer: &str) -> anyhow::Result<()> {
    let conn = db.conversations.get_conn_public()?;
    let stored: Option<String> = conn
        .query_row("SELECT counts FROM conversation_topics WHERE session_id = ?1", [session_id], |r| r.get(0))
        .ok();
    let mut counts: HashMap<String, u32> = stored
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // The question says what the user is after; it counts double.
    for (t, n) in extract_terms(question) {
        *counts.entry(t).or_insert(0) += n * 2;
    }
    for (t, n) in extract_terms(answer) {
        *counts.entry(t).or_insert(0) += n;
    }
    let top = top_terms(&counts);
    let kept: HashMap<String, u32> = top.iter().cloned().collect();
    let terms = top.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" ; ");
    conn.execute(
        "INSERT INTO conversation_topics (session_id, terms, counts, updated_at)
         VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
         ON CONFLICT(session_id) DO UPDATE SET terms = ?2, counts = ?3, updated_at = CURRENT_TIMESTAMP",
        params![session_id, terms, serde_json::to_string(&kept)?],
    )?;
    Ok(())
}

/// Conversations (other than `exclude`, inside `allowed` when given) whose
/// topics match the question best, best first.
pub fn matching_sessions(
    db: &MemoryDatabase,
    query: &str,
    exclude: &str,
    allowed: Option<&HashSet<String>>,
    limit: usize,
) -> Vec<String> {
    let Some(fts) = crate::memory_db::fts::fts_query_from_user_text(query) else { return Vec::new() };
    let Ok(conn) = db.conversations.get_conn_public() else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT t.session_id FROM conversation_topics_fts f
         JOIN conversation_topics t ON t.rowid = f.rowid
         WHERE conversation_topics_fts MATCH ?1 AND t.session_id <> ?2
         ORDER BY bm25(conversation_topics_fts) LIMIT ?3",
    ) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(params![fts, exclude, (limit * 4) as i64], |r| r.get::<_, String>(0))
        .map(|it| it.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    rows.into_iter()
        .filter(|s| allowed.map_or(true, |a| a.contains(s)))
        .take(limit)
        .collect()
}

/// Build topics for conversations that have none yet (installs that predate
/// topic memory). Bounded per call; runs in the background at startup.
pub fn backfill(db: &MemoryDatabase, max_sessions: usize) -> usize {
    let Ok(conn) = db.conversations.get_conn_public() else { return 0 };
    let ids: Vec<String> = conn
        .prepare(
            "SELECT s.id FROM sessions s LEFT JOIN conversation_topics t ON t.session_id = s.id
             WHERE t.session_id IS NULL ORDER BY s.last_accessed DESC LIMIT ?1",
        )
        .and_then(|mut st| {
            st.query_map([max_sessions as i64], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();
    drop(conn);
    let mut done = 0;
    for id in ids {
        let msgs = db.conversations.get_session_messages(&id, Some(400), Some(0)).unwrap_or_default();
        let mut question = String::new();
        let mut answer = String::new();
        for m in &msgs {
            let target = if m.role == "user" { &mut question } else { &mut answer };
            target.push_str(&m.content);
            target.push('\n');
        }
        if record_turn(db, &id, &question, &answer).is_ok() {
            done += 1;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clauses_files_and_names_outweigh_plain_words() {
        let t = extract_terms("Under Section 12.3(b) of lease.pdf, Acme Holdings must give notice. Notice notice.");
        assert!(t.get("section 12.3(b)").copied().unwrap_or(0) >= 3, "{:?}", t);
        assert!(t.get("lease.pdf").copied().unwrap_or(0) >= 3, "{:?}", t);
        assert!(t.contains_key("acme holdings"), "{:?}", t);
        assert!(t.contains_key("notice"));
    }

    #[test]
    fn a_differently_worded_question_finds_the_conversation_by_topic() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        for id in ["lisbon", "other", "here"] {
            db.conversations.create_session_with_id(id, None).unwrap();
        }
        record_turn(
            &db,
            "lisbon",
            "What notice period applies to the warehouse lease at Rua das Flores?",
            "Ninety days written notice under Section 14.2 of warehouse_lease.pdf.",
        )
        .unwrap();
        record_turn(&db, "other", "Draft a cover letter for the job", "Dear hiring manager").unwrap();
        let hits = matching_sessions(&db, "termination rules for the Flores warehouse", "here", None, 3);
        assert_eq!(hits.first().map(String::as_str), Some("lisbon"), "{:?}", hits);
        assert!(!hits.contains(&"other".to_string()));
        // Scope is respected.
        let only_other: HashSet<String> = ["other".to_string()].into_iter().collect();
        assert!(matching_sessions(&db, "Flores warehouse", "here", Some(&only_other), 3).is_empty());
    }
}
