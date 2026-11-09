//! Conversation memory: the database as the model's long-term memory.
//!
//! The model only sees one context window at a time. Every message is kept in
//! SQLite (full-text and vector indexed), and each turn's prompt is rebuilt
//! from it:
//!
//! - **Rolling summary.** When a conversation's history passes 80% of the
//!   chat prompt budget, the oldest turns leave the prompt and are condensed,
//!   without the LLM, into a capped summary (question, key sentences,
//!   sources) stored per conversation and carried forward.
//! - **Recall of dropped turns.** Every turn, this conversation's dropped
//!   messages are searched by keyword and by meaning, and the best matches
//!   come back as whole question-and-answer pairs with their turn number.
//! - **Ordinal questions.** "My first question", "the last thing I asked",
//!   "three questions ago" and "question 12" are answered by position.
//! - **Other conversations.** Even without a phrase like "earlier", a
//!   question is checked against the user's other conversations, and close
//!   matches are offered as possibly related.
//!
//! No step here runs the language model.

use std::collections::{HashMap, HashSet};

use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::memory::Message;
use crate::memory_db::{MemoryDatabase, StoredMessage};
use crate::rag::RagRuntime;

pub const RECALL_HEADING: &str = "[EARLIER IN THIS CONVERSATION";
pub const SUMMARY_HEADING: &str = "[CONVERSATION SUMMARY";
pub const RELATED_HEADING: &str = "[POSSIBLY RELATED FROM YOUR OTHER CONVERSATIONS";

/// Characters kept from each side of a recalled question-and-answer pair.
const PAIR_SIDE_CHARS: usize = 700;
/// Most earlier exchanges recalled into one prompt (the character budget
/// usually binds first).
const MAX_RECALL_PAIRS: usize = 15;
/// Candidates taken from each of keyword and vector search before fusion;
/// wider than what is shown, so fusion can rank evidence that one list
/// places low.
const IN_SESSION_CANDIDATES: i64 = 30;
/// Entries kept in a stored summary before the oldest lose their key points.
const MAX_STORED_ENTRIES: usize = 400;
/// Minimum similarity for a dropped turn of THIS conversation to be recalled.
const IN_SESSION_MIN_SIMILARITY: f32 = 0.45;
/// Minimum similarity for another conversation to be offered unasked. High on
/// purpose: an unrelated chat leaking into an answer is worse than missing one.
const CROSS_MIN_SIMILARITY: f32 = 0.62;
const RRF_K: f64 = 60.0;

lazy_static! {
    static ref ORD_WORD: Regex = Regex::new(
        r"(?i)\b(very first|first|1st|second|2nd|third|3rd|fourth|4th|fifth|5th|sixth|6th|seventh|7th|eighth|8th|ninth|9th|tenth|10th|initial|opening|original|last|previous|prior|latest|most recent)\s+(?:question|message|query|prompt|request|thing i (?:asked|said|wrote))"
    ).unwrap();
    static ref ASKED_FIRST: Regex =
        Regex::new(r"(?i)\bwhat\s+(?:did|was it)\s+i\s+(first|initially|originally)\s+(?:ask|say|write)").unwrap();
    static ref AGO: Regex = Regex::new(
        r"(?i)\b(\d{1,3}|one|two|three|four|five|six|seven|eight|nine|ten)\s+(?:questions|messages|turns|queries|prompts)\s+ago"
    ).unwrap();
    static ref NUMBERED: Regex =
        Regex::new(r"(?i)\b(?:question|message|query|turn)\s+(?:number\s+|no\.?\s*|#)(\d{1,3})\b|\b(?:my|the)\s+(?:question|message|query|turn)\s+(\d{1,3})\b").unwrap();
    static ref CITE: Regex = Regex::new(r"\[[^\[\]\n]{2,120}\]").unwrap();
    static ref FILENAME: Regex =
        Regex::new(r"(?i)\b[\w][\w\-. ]{0,80}?\.(?:pdf|docx?|xlsx?|pptx?|txt|png|jpe?g)\b").unwrap();
    static ref DECISION: Regex = Regex::new(
        r"(?i)\b(must|shall|agreed|agree|decided|decision|recommend\w*|conclusion|therefore|deadline|due|liab\w+|terminat\w+|notice|period|fee|fees|price|penalt\w+|clause|section|article|obligation\w*|right to|entitled|not permitted|prohibit\w*|required|warrant\w*|indemn\w+)\b"
    ).unwrap();
}

// ── Ordinal questions ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ordinal {
    /// 1-based position among the user's questions.
    Nth(usize),
    /// The most recent earlier question.
    Last,
    /// N questions before this one (1 = the previous question).
    Ago(usize),
}

fn word_number(w: &str) -> Option<usize> {
    let w = w.to_ascii_lowercase();
    Some(match w.as_str() {
        "one" => 1, "two" => 2, "three" => 3, "four" => 4, "five" => 5,
        "six" => 6, "seven" => 7, "eight" => 8, "nine" => 9, "ten" => 10,
        _ => return w.parse().ok(),
    })
}

/// Recognise a question about the conversation's own sequence.
pub fn detect_ordinal(query: &str) -> Option<Ordinal> {
    if let Some(c) = ASKED_FIRST.captures(query) {
        let _ = c;
        return Some(Ordinal::Nth(1));
    }
    if let Some(c) = ORD_WORD.captures(query) {
        let w = c[1].to_ascii_lowercase();
        return Some(match w.as_str() {
            "very first" | "first" | "1st" | "initial" | "opening" | "original" => Ordinal::Nth(1),
            "second" | "2nd" => Ordinal::Nth(2),
            "third" | "3rd" => Ordinal::Nth(3),
            "fourth" | "4th" => Ordinal::Nth(4),
            "fifth" | "5th" => Ordinal::Nth(5),
            "sixth" | "6th" => Ordinal::Nth(6),
            "seventh" | "7th" => Ordinal::Nth(7),
            "eighth" | "8th" => Ordinal::Nth(8),
            "ninth" | "9th" => Ordinal::Nth(9),
            "tenth" | "10th" => Ordinal::Nth(10),
            _ => Ordinal::Last,
        });
    }
    if let Some(c) = AGO.captures(query) {
        if let Some(n) = word_number(&c[1]).filter(|n| *n >= 1) {
            return Some(Ordinal::Ago(n));
        }
    }
    if let Some(c) = NUMBERED.captures(query) {
        let n = c.get(1).or_else(|| c.get(2)).and_then(|m| m.as_str().parse::<usize>().ok());
        if let Some(n) = n.filter(|n| *n >= 1) {
            return Some(Ordinal::Nth(n));
        }
    }
    None
}

/// The question an ordinal points at, among `users` (oldest first, the
/// current question already excluded). Returns its index into `users`.
pub fn resolve_ordinal(users_len: usize, ordinal: Ordinal) -> Option<usize> {
    match ordinal {
        Ordinal::Nth(n) => (n >= 1 && n <= users_len).then(|| n - 1),
        Ordinal::Last => users_len.checked_sub(1),
        Ordinal::Ago(n) => users_len.checked_sub(n),
    }
}

// ── Question-and-answer pairs ────────────────────────────────────────────────

fn flat(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// `text` cut to `max` characters around what the query asks about: the
/// sentences that contain query terms, in their original order, with "…"
/// where text was left out. Falls back to the start of the text when no
/// sentence matches, so a recalled message is never empty.
fn excerpt(text: &str, terms: &[String], max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max || terms.is_empty() {
        return flat(&collapsed, max);
    }
    let parts = sentences(text);
    let stems: Vec<String> = terms.iter().map(|t| t.chars().take(5).collect()).collect();
    let mut scored: Vec<(usize, usize)> = parts
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let lower = s.to_lowercase();
            (i, stems.iter().filter(|t| lower.contains(t.as_str())).count())
        })
        .filter(|(_, n)| *n > 0)
        .collect();
    if scored.is_empty() {
        return flat(&collapsed, max);
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut chosen: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for (i, _) in scored {
        let len = parts[i].chars().count() + 2;
        if used + len > max {
            if chosen.is_empty() {
                return flat(&parts[i], max);
            }
            continue;
        }
        used += len;
        chosen.push(i);
    }
    chosen.sort_unstable();
    let mut out = String::new();
    let mut prev: Option<usize> = None;
    for i in chosen {
        match prev {
            None if i > 0 => out.push_str("… "),
            Some(p) if i > p + 1 => out.push_str(" … "),
            Some(_) => out.push(' '),
            None => {}
        }
        out.push_str(&parts[i]);
        prev = Some(i);
    }
    if prev.is_some_and(|p| p + 1 < parts.len()) {
        out.push_str(" …");
    }
    out
}

/// A recalled exchange: the user's question and the answer it received.
#[derive(Debug, Clone)]
pub struct Pair {
    pub session_id: String,
    pub user: Option<StoredMessage>,
    pub assistant: Option<StoredMessage>,
}

impl Pair {
    fn key(&self) -> (String, i32) {
        let idx = self
            .user
            .as_ref()
            .map(|m| m.message_index)
            .or_else(|| self.assistant.as_ref().map(|m| m.message_index - 1))
            .unwrap_or(-1);
        (self.session_id.clone(), idx)
    }

    pub fn render(&self, label: &str, terms: &[String]) -> String {
        // "--- [label] ---", the tag-line shape the echo guard recognises.
        let mut s = format!("--- [{}] ---\n", label);
        if let Some(u) = &self.user {
            s.push_str(&format!("You asked: {}\n", excerpt(&u.content, terms, PAIR_SIDE_CHARS)));
        }
        if let Some(a) = &self.assistant {
            s.push_str(&format!("Answer: {}\n", excerpt(&a.content, terms, PAIR_SIDE_CHARS)));
        }
        s
    }
}

/// Complete each hit into its exchange: a question gets the answer after it,
/// an answer gets the question before it. Order is preserved; duplicates
/// (both halves of one exchange hit) are merged.
pub fn pairs_for(db: &MemoryDatabase, hits: &[StoredMessage]) -> Vec<Pair> {
    let mut by_session: HashMap<String, Vec<i32>> = HashMap::new();
    for h in hits {
        let partner = if h.role == "user" { h.message_index + 1 } else { h.message_index - 1 };
        let e = by_session.entry(h.session_id.clone()).or_default();
        e.push(h.message_index);
        e.push(partner);
    }
    let mut lookup: HashMap<(String, i32), StoredMessage> = HashMap::new();
    for (session, indices) in by_session {
        for m in db.conversations.get_messages_at(&session, &indices).unwrap_or_default() {
            lookup.insert((session.clone(), m.message_index), m);
        }
    }
    let mut seen: HashSet<(String, i32)> = HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        let (u_idx, a_idx) = if h.role == "user" {
            (h.message_index, h.message_index + 1)
        } else {
            (h.message_index - 1, h.message_index)
        };
        let user = lookup.get(&(h.session_id.clone(), u_idx)).filter(|m| m.role == "user").cloned();
        let assistant = lookup.get(&(h.session_id.clone(), a_idx)).filter(|m| m.role == "assistant").cloned();
        let pair = Pair { session_id: h.session_id.clone(), user, assistant };
        if pair.user.is_none() && pair.assistant.is_none() {
            continue;
        }
        if seen.insert(pair.key()) {
            out.push(pair);
        }
    }
    out
}

/// Reciprocal-rank fusion of ranked message lists, best first.
fn fuse(lists: &[Vec<StoredMessage>]) -> Vec<StoredMessage> {
    let mut scores: HashMap<i64, (StoredMessage, f64)> = HashMap::new();
    for list in lists {
        for (rank, m) in list.iter().enumerate() {
            let e = scores.entry(m.id).or_insert_with(|| (m.clone(), 0.0));
            e.1 += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut v: Vec<(StoredMessage, f64)> = scores.into_values().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
    v.into_iter().map(|(m, _)| m).collect()
}

/// Text that stands for a whole exchange in the vector index: the question and
/// the answer's strongest sentences (chosen by the same extractive scorer as
/// the summary, so no model call). A question about the exchange can match it
/// even when neither message alone resembles the question.
pub fn round_key(question: &str, answer: &str) -> String {
    let points = key_points(answer, 3);
    if points.is_empty() {
        return format!("Question: {}
Answer: {}", flat(question, 600), flat(answer, 400));
    }
    format!("Question: {}
Key points: {}", flat(question, 600), points.join(" "))
}

/// When an exchange happened, from its stored message time.
fn pair_date(pair: &Pair) -> String {
    pair.user
        .as_ref()
        .or(pair.assistant.as_ref())
        .map(|m| m.timestamp.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "date unknown".to_string())
}

/// Position of an exchange in its conversation, for listing oldest first.
fn pair_order(pair: &Pair) -> i32 {
    pair.user
        .as_ref()
        .map(|m| m.message_index)
        .or_else(|| pair.assistant.as_ref().map(|m| m.message_index - 1))
        .unwrap_or(i32::MAX)
}

fn ordinal_suffix(n: usize) -> &'static str {
    match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    }
}

// ── Recall of this conversation's dropped turns ──────────────────────────────

/// Search the turns of THIS conversation that are no longer in the prompt
/// (message index below `cutoff`) and return them as question-and-answer
/// pairs, plus the exchange an ordinal question points at. `current_index`
/// is the position of the question being answered now.
pub async fn recall_dropped(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    session_id: &str,
    query: &str,
    cutoff: i32,
    current_index: i32,
    budget_chars: usize,
) -> Option<String> {
    if budget_chars < 300 {
        return None;
    }
    let terms = crate::memory_db::fts::content_tokens(query);
    let users: Vec<StoredMessage> = db
        .conversations
        .get_user_messages(session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.message_index < current_index)
        .collect();
    let turn_of: HashMap<i32, usize> = users.iter().enumerate().map(|(i, m)| (m.message_index, i + 1)).collect();

    // The exchange an ordinal question names, when it has left the prompt.
    let mut ordered: Vec<StoredMessage> = Vec::new();
    let mut ordinal_target: Option<i32> = None;
    if let Some(ord) = detect_ordinal(query) {
        if let Some(i) = resolve_ordinal(users.len(), ord) {
            let target = &users[i];
            if target.message_index < cutoff {
                ordinal_target = Some(target.message_index);
                ordered.push(target.clone());
            }
        }
    }

    if cutoff > 1 {
        let keyword = db
            .conversations
            .search_session_messages_fulltext(session_id, query, cutoff, IN_SESSION_CANDIDATES)
            .unwrap_or_default();
        let only: HashSet<String> = [session_id.to_string()].into_iter().collect();
        let semantic: Vec<StoredMessage> = crate::rag::search::semantic_message_hits(
            db, rt, query, Some(&only), "", None, IN_SESSION_CANDIDATES as usize, IN_SESSION_MIN_SIMILARITY,
        )
        .await
        .into_iter()
        .map(|(m, _)| m)
        .filter(|m| m.message_index < cutoff)
        .collect();
        for m in fuse(&[keyword, semantic]) {
            if Some(m.message_index) != ordinal_target {
                ordered.push(m);
            }
        }
    }
    if ordered.is_empty() {
        return None;
    }

    // Choose by relevance within the budget, then list oldest first with
    // dates, so questions about order, timing and changed facts can be
    // answered from the list itself.
    let room = budget_chars.saturating_sub(320);
    let mut chosen: Vec<(i32, String)> = Vec::new();
    let mut used = 0usize;
    for pair in pairs_for(db, &ordered) {
        let turn = pair
            .user
            .as_ref()
            .and_then(|u| turn_of.get(&u.message_index).copied());
        let is_target = ordinal_target.is_some()
            && pair.user.as_ref().map(|u| Some(u.message_index) == ordinal_target).unwrap_or(false);
        let when = pair_date(&pair);
        let label = match (turn, is_target) {
            (Some(t), true) => format!("Turn {}, {} (your {}{} question, the one asked about)", t, when, t, ordinal_suffix(t)),
            (Some(t), false) => format!("Turn {}, {} (your {}{} question)", t, when, t, ordinal_suffix(t)),
            (None, _) => format!("An earlier turn, {}", when),
        };
        let entry = pair.render(&label, &terms);
        if used + entry.len() > room {
            if chosen.is_empty() {
                chosen.push((pair_order(&pair), entry.chars().take(room).collect()));
                break;
            }
            // A smaller exchange further down may still fit.
            continue;
        }
        used += entry.len();
        chosen.push((pair_order(&pair), entry));
        if chosen.len() >= MAX_RECALL_PAIRS {
            break;
        }
    }
    chosen.sort_by_key(|(order, _)| *order);
    let shown = chosen.len();
    let body: String = chosen.into_iter().map(|(_, e)| e).collect();
    if shown == 0 {
        return None;
    }
    debug!("Recalled {} earlier exchange(s) of session {}", shown, session_id);
    Some(format!(
        "{} - retrieved from the saved record of this chat]\n\
         These exchanges happened earlier in THIS conversation and are no longer in view. \
         They are listed oldest first with the date each happened; today is {}. \
         Answer from them: when asked what was said, quote them as written, and where \
         they disagree, the most recent one is current.\n{}",
        RECALL_HEADING,
        chrono::Utc::now().format("%Y-%m-%d"),
        body
    ))
}

// ── Extractive rolling summary ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SummaryEntry {
    pub turn: usize,
    pub question: String,
    #[serde(default)]
    pub points: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
}

fn strip_markdown(s: &str) -> String {
    s.trim()
        .trim_start_matches(|c: char| matches!(c, '#' | '>' | '-' | '*' | '+' | ' ' | '|'))
        .replace("**", "")
        .replace('`', "")
}

fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = strip_markdown(line);
        let mut start = 0;
        let bytes: Vec<(usize, char)> = line.char_indices().collect();
        for (i, (pos, ch)) in bytes.iter().enumerate() {
            let next_space = bytes.get(i + 1).map(|(_, c)| c.is_whitespace()).unwrap_or(true);
            if matches!(ch, '.' | '?' | '!' | ';' | '。') && next_space {
                let end = pos + ch.len_utf8();
                out.push(line[start..end].trim().to_string());
                start = end;
            }
        }
        if start < line.len() {
            out.push(line[start..].trim().to_string());
        }
    }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

/// The sentences that carry the substance of an answer: citations, numbers,
/// decisions and obligations score highest. Original order is kept.
fn key_points(answer: &str, max: usize) -> Vec<String> {
    let mut scored: Vec<(usize, i32, String)> = sentences(answer)
        .into_iter()
        .enumerate()
        .filter(|(_, s)| (25..=400).contains(&s.chars().count()))
        .map(|(i, s)| {
            let mut score = 0;
            if CITE.is_match(&s) {
                score += 3;
            }
            if s.chars().any(|c| c.is_ascii_digit()) {
                score += 2;
            }
            score += 2 * DECISION.find_iter(&s).take(2).count() as i32;
            if s.contains('"') || s.contains('“') {
                score += 1;
            }
            (i, score, s)
        })
        .collect();
    // The first sentence usually states the answer; never rank it last.
    if let Some(first) = scored.first_mut() {
        first.1 += 2;
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut top: Vec<(usize, String)> = scored.into_iter().take(max).map(|(i, _, s)| (i, flat(&s, 220))).collect();
    top.sort_by_key(|(i, _)| *i);
    top.into_iter().map(|(_, s)| s).collect()
}

fn sources_of(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in CITE.find_iter(text) {
        let t = m.as_str();
        if (FILENAME.is_match(t) || t.contains("p.") || t.contains("Section") || t.contains("Sheet:"))
            && !out.iter().any(|o| o == t)
        {
            out.push(t.to_string());
        }
    }
    for m in FILENAME.find_iter(text) {
        let t = m.as_str().trim().to_string();
        if !out.iter().any(|o| o.contains(&t)) {
            out.push(t);
        }
    }
    out.truncate(3);
    out
}

/// Summary entries for `messages[from..to]` (positions in the request list).
/// Turn numbers count the user messages before each question.
pub fn extract_entries(messages: &[Message], from: usize, to: usize) -> Vec<SummaryEntry> {
    let to = to.min(messages.len());
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        let m = &messages[i];
        if m.role != "user" {
            i += 1;
            continue;
        }
        let turn = messages[..=i].iter().filter(|x| x.role == "user").count();
        let answer = messages.get(i + 1).filter(|n| n.role == "assistant" && i + 1 < to);
        let (points, sources) = match answer {
            Some(a) => (key_points(&a.content, 2), sources_of(&a.content)),
            None => (Vec::new(), Vec::new()),
        };
        out.push(SummaryEntry { turn, question: flat(&m.content, 200), points, sources });
        i += if answer.is_some() { 2 } else { 1 };
    }
    out
}

/// Merge new entries into the stored list, bounded in size.
pub fn merge_entries(mut stored: Vec<SummaryEntry>, new: Vec<SummaryEntry>) -> Vec<SummaryEntry> {
    for e in new {
        if !stored.iter().any(|s| s.turn == e.turn) {
            stored.push(e);
        }
    }
    stored.sort_by_key(|e| e.turn);
    if stored.len() > MAX_STORED_ENTRIES {
        let over = stored.len() - MAX_STORED_ENTRIES;
        for e in stored.iter_mut().take(over) {
            e.points.clear();
            e.sources.clear();
        }
    }
    stored
}

fn entry_line(e: &SummaryEntry, with_points: bool) -> String {
    let mut s = format!("- Turn {}: You asked: {}", e.turn, e.question);
    if with_points && !e.points.is_empty() {
        s.push_str(&format!(" | Key points: {}", e.points.join(" ")));
    }
    if with_points && !e.sources.is_empty() {
        s.push_str(&format!(" | Sources: {}", e.sources.join(", ")));
    }
    s.push('\n');
    s
}

/// The summary block, within `cap_chars`. Older entries lose their key points
/// first, then whole entries are folded into a count.
pub fn render_summary(entries: &[SummaryEntry], cap_chars: usize) -> Option<String> {
    if entries.is_empty() || cap_chars < 200 {
        return None;
    }
    let last_turn = entries.last().map(|e| e.turn).unwrap_or(0);
    let header = format!(
        "{}: turns 1-{} - extracted from the saved record of this chat]\n\
         The earlier part of this conversation, condensed. The exact wording of any of these \
         turns is retrieved automatically when a question needs it.\n",
        SUMMARY_HEADING, last_turn
    );
    let n = entries.len();
    let mut with_points = vec![true; n];
    let mut first_shown = 0usize;
    let render = |first: usize, wp: &Vec<bool>| -> String {
        let mut s = header.clone();
        if first > 0 {
            s.push_str(&format!("- ({} earliest turns not listed here; ask about them to retrieve them)\n", first));
        }
        for (i, e) in entries.iter().enumerate().skip(first) {
            s.push_str(&entry_line(e, wp[i]));
        }
        s
    };
    let mut text = render(first_shown, &with_points);
    let mut drop_points = 0usize;
    while text.len() > cap_chars {
        if drop_points < n.saturating_sub(1) {
            with_points[drop_points] = false;
            drop_points += 1;
        } else if first_shown < n.saturating_sub(1) {
            first_shown += 1;
        } else {
            break;
        }
        text = render(first_shown, &with_points);
    }
    if text.len() > cap_chars {
        text = text.chars().take(cap_chars).collect();
    }
    Some(text)
}

pub fn load_entries(db: &MemoryDatabase, session_id: &str) -> (usize, Vec<SummaryEntry>) {
    match db.conversations.get_memory(session_id) {
        Ok(Some((covered, json))) => {
            let entries = serde_json::from_str::<Vec<SummaryEntry>>(&json).unwrap_or_default();
            (covered.max(0) as usize, entries)
        }
        _ => (0, Vec::new()),
    }
}

pub fn save_entries(db: &MemoryDatabase, session_id: &str, covered: usize, entries: &[SummaryEntry]) {
    match serde_json::to_string(entries) {
        Ok(json) => {
            if let Err(e) = db.conversations.set_memory(session_id, covered as i64, &json) {
                tracing::warn!("Could not save the conversation summary for {}: {}", session_id, e);
            } else {
                info!(
                    "Conversation {} compacted: first {} messages condensed into {} summary entries",
                    session_id, covered, entries.len()
                );
            }
        }
        Err(e) => tracing::warn!("Could not encode the conversation summary: {}", e),
    }
}

// ── Other conversations, without a trigger phrase ────────────────────────────

/// Check the user's other conversations for exchanges closely related to this
/// question, even when it does not say "earlier". Keyword matches must cover
/// most of the question's words; meaning matches must clear a high
/// similarity bar and are only used while the embedding model is already
/// loaded (no model is started just for this).
pub async fn related_other_conversations(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    session_id: &str,
    query: &str,
    budget_chars: usize,
) -> Option<String> {
    if budget_chars < 400 || detect_ordinal(query).is_some() {
        return None;
    }
    let tokens = crate::memory_db::fts::content_tokens(query);
    if tokens.len() < 2 {
        return None;
    }
    let allowed = crate::rag::scope::allowed_sessions(db, session_id);
    let need = ((tokens.len() as f64) * 0.6).ceil().max(2.0) as usize;
    let keyword: Vec<StoredMessage> = db
        .conversations
        .search_messages_fulltext_scoped(query, 8, Some(session_id), None, allowed.as_ref())
        .unwrap_or_default()
        .into_iter()
        .map(|(m, _)| m)
        .filter(|m| {
            let lower = m.content.to_lowercase();
            tokens.iter().filter(|t| lower.contains(t.as_str())).count() >= need
        })
        .collect();
    // Topic memory: conversations about the same matter, even when worded
    // differently from this question.
    let topical: Vec<StoredMessage> = {
        let sessions = super::topic_memory::matching_sessions(db, query, session_id, allowed.as_ref(), 3);
        if sessions.is_empty() {
            Vec::new()
        } else {
            let set: std::collections::HashSet<String> = sessions.into_iter().collect();
            db.conversations
                .search_messages_fulltext_scoped(query, 6, Some(session_id), None, Some(&set))
                .unwrap_or_default()
                .into_iter()
                .map(|(m, _)| m)
                .collect()
        }
    };
    // A cold embedding model is started in the background, so the next
    // question has meaning-based recall too; this one uses keywords and topics.
    if rt.embedder.available() && !rt.embedder.warm() {
        tokio::spawn(async {
            let _ = crate::rag::runtime().embedder.embed_query("warm up").await;
        });
    }
    let semantic: Vec<StoredMessage> = if rt.embedder.available() && rt.embedder.warm() {
        crate::rag::search::semantic_message_hits(db, rt, query, allowed.as_ref(), session_id, None, 6, CROSS_MIN_SIMILARITY)
            .await
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    } else {
        Vec::new()
    };
    let hits: Vec<StoredMessage> = fuse(&[keyword, topical, semantic]).into_iter().take(6).collect();
    if hits.is_empty() {
        return None;
    }
    let titles: HashMap<String, String> = db
        .conversations
        .get_all_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id, s.metadata.title.unwrap_or_else(|| "Untitled conversation".to_string())))
        .collect();
    let mut body = String::new();
    let mut shown = 0usize;
    let current = super::duplicates::Current::load(db, session_id, query);
    for pair in super::duplicates::filter_pairs(db, &current, pairs_for(db, &hits)) {
        let when = pair
            .user
            .as_ref()
            .or(pair.assistant.as_ref())
            .map(|m| m.timestamp.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let title = titles.get(&pair.session_id).map(String::as_str).unwrap_or("Untitled conversation");
        let entry = pair.render(&format!("Conversation \"{}\" ({})", title, when), &tokens);
        if body.len() + entry.len() > budget_chars.saturating_sub(300) {
            break;
        }
        body.push_str(&entry);
        shown += 1;
        if shown >= 3 {
            break;
        }
    }
    if shown == 0 {
        return None;
    }
    Some(format!(
        "{}]\n\
         These exchanges from the user's other conversations look related to this question. \
         Use them only if they bear on it, and say they come from an earlier conversation. \
         If they do not help, ignore them and do not mention them.\n{}",
        RELATED_HEADING, body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message { role: role.to_string(), content: content.to_string() }
    }

    #[test]
    fn ordinals_are_recognised() {
        assert_eq!(detect_ordinal("What was my first question?"), Some(Ordinal::Nth(1)));
        assert_eq!(detect_ordinal("what did I originally ask"), Some(Ordinal::Nth(1)));
        assert_eq!(detect_ordinal("repeat the 3rd question"), Some(Ordinal::Nth(3)));
        assert_eq!(detect_ordinal("what was my last question"), Some(Ordinal::Last));
        assert_eq!(detect_ordinal("the question I asked two questions ago"), Some(Ordinal::Ago(2)));
        assert_eq!(detect_ordinal("show me question #12"), Some(Ordinal::Nth(12)));
        assert_eq!(detect_ordinal("What is the notice period in section 4?"), None);
    }

    #[test]
    fn ordinals_resolve_against_earlier_questions() {
        assert_eq!(resolve_ordinal(10, Ordinal::Nth(1)), Some(0));
        assert_eq!(resolve_ordinal(10, Ordinal::Last), Some(9));
        assert_eq!(resolve_ordinal(10, Ordinal::Ago(1)), Some(9));
        assert_eq!(resolve_ordinal(10, Ordinal::Ago(3)), Some(7));
        assert_eq!(resolve_ordinal(2, Ordinal::Nth(5)), None);
        assert_eq!(resolve_ordinal(0, Ordinal::Last), None);
    }

    #[test]
    fn entries_carry_turn_numbers_and_key_points() {
        let m = vec![
            msg("system", "sys"),
            msg("user", "What is the notice period for termination?"),
            msg("assistant", "Hello. The tenant must give 60 days written notice before terminating [lease.pdf, p.4]. Other text that is filler and not important."),
            msg("user", "And the deposit?"),
            msg("assistant", "The deposit is three months of rent."),
            msg("user", "current"),
        ];
        let e = extract_entries(&m, 1, 5);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].turn, 1);
        assert_eq!(e[1].turn, 2);
        assert!(e[0].points.iter().any(|p| p.contains("60 days")));
        assert!(e[0].sources.iter().any(|s| s.contains("lease.pdf")));
    }

    #[test]
    fn summary_respects_its_cap() {
        let entries: Vec<SummaryEntry> = (1..=50)
            .map(|t| SummaryEntry {
                turn: t,
                question: format!("Question number {} about the lease and its clauses", t),
                points: vec!["The landlord must repair the roof within 30 days of notice.".into()],
                sources: vec!["[lease.pdf, p.2]".into()],
            })
            .collect();
        let s = render_summary(&entries, 2_000).unwrap();
        assert!(s.len() <= 2_000);
        assert!(s.contains("Turn 50"));
        assert!(s.starts_with(SUMMARY_HEADING));
    }

    #[test]
    fn merge_keeps_turns_unique_and_ordered() {
        let a = vec![SummaryEntry { turn: 1, question: "a".into(), points: vec![], sources: vec![] }];
        let b = vec![
            SummaryEntry { turn: 2, question: "b".into(), points: vec![], sources: vec![] },
            SummaryEntry { turn: 1, question: "dup".into(), points: vec![], sources: vec![] },
        ];
        let m = merge_entries(a, b);
        assert_eq!(m.iter().map(|e| e.turn).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(m[0].question, "a");
    }
}

#[cfg(test)]
mod excerpt_tests {
    use super::*;

    fn terms(q: &str) -> Vec<String> {
        crate::memory_db::fts::content_tokens(q)
    }

    #[test]
    fn short_text_is_returned_whole() {
        assert_eq!(excerpt("The deposit is two months rent.", &terms("deposit"), 700), "The deposit is two months rent.");
    }

    #[test]
    fn evidence_late_in_a_long_message_survives_the_cut() {
        let filler = "This sentence talks about something unrelated to the question at hand. ".repeat(30);
        let text = format!("{}I named my Spotify playlist Summer Vibes and it has chill tracks. {}", filler, filler);
        let cut = excerpt(&text, &terms("What is the name of the playlist I created on Spotify?"), 700);
        assert!(cut.contains("Summer Vibes"), "{}", cut);
        assert!(cut.chars().count() <= 700 + 8, "{} chars", cut.chars().count());
        assert!(cut.starts_with('…'), "a cut start is marked: {}", cut);
        // The old behaviour, a prefix cut, loses it.
        assert!(!flat(&text, 700).contains("Summer Vibes"));
    }

    #[test]
    fn without_any_match_the_start_is_kept() {
        let text = "Alpha beta gamma delta. ".repeat(60);
        assert_eq!(excerpt(&text, &terms("unrelated zebra question"), 100), flat(&text, 100));
    }

    #[test]
    fn chosen_sentences_keep_their_original_order() {
        let pad = "Padding sentence without the words. ".repeat(20);
        let text = format!("First the lease starts in March. {}Later the lease deposit is refunded. {}", pad, pad);
        let cut = excerpt(&text, &terms("lease deposit"), 200);
        let a = cut.find("lease starts").unwrap_or(usize::MAX);
        let b = cut.find("deposit is refunded").expect("best sentence kept");
        assert!(a == usize::MAX || a < b, "{}", cut);
    }
}
