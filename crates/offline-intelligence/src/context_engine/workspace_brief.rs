//! What the Agent knows about the workspace.
//!
//! The Agent answers questions about the user's own material: "what matters
//! am I working on", "which draft mentions the indemnity cap", "what did we
//! decide about Acme last week". Its thread's own history and the page it is
//! opened on cannot answer those, so every Agent request carries a brief,
//! assembled here from the database, in two parts:
//!
//! - **Inventory**: counts plus the most recent matters, chats, drafts and
//!   vault files. It goes in the SYSTEM message and is cached for a few
//!   seconds, so across turns it is byte-identical and llama-server's prompt
//!   cache reuses its KV instead of re-prefilling it every turn.
//! - **Hits**: full-text matches for THIS question across chat messages,
//!   draft bodies and vault file names. Folded into the latest message, after
//!   the cacheable prefix, because it changes with every question.
//!
//! Everything is SQLite reads on indexed tables (FTS5 for text), bounded by
//! LIMIT and by a character budget carved from the Agent's own share of the
//! context window. No embedding or model call is made here, so the brief
//! costs milliseconds and no GPU time.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::memory_db::MemoryDatabase;
use crate::shared_state::ASSISTANT_SESSION_PREFIX;

/// How long the inventory is reused. Long enough that consecutive turns share
/// one cached prefix, short enough that a chat or draft created a moment ago
/// shows up in the next question.
const INVENTORY_TTL: Duration = Duration::from_secs(20);

/// Rows per inventory section, before the character budget trims further.
const SECTION_ROWS: usize = 12;
const MESSAGE_HITS: i64 = 6;
const DRAFT_HITS: usize = 4;
const VAULT_HITS: usize = 6;
/// Longest excerpt quoted from one message.
const EXCERPT_CHARS: usize = 320;

pub struct WorkspaceBrief {
    /// Stable inventory, for the system message.
    pub inventory: String,
    /// Question-specific matches, for the latest message. None when nothing
    /// in the workspace matches.
    pub hits: Option<String>,
}

impl WorkspaceBrief {
    pub fn chars(&self) -> usize {
        self.inventory.len() + self.hits.as_ref().map_or(0, |h| h.len())
    }
}

/// The global Agent (opened anywhere but inside a draft) is the one that
/// answers about the whole workspace; an Agent opened on a draft is about
/// that draft first.
pub fn is_global_agent(session_id: &str) -> bool {
    session_id
        .strip_prefix(ASSISTANT_SESSION_PREFIX)
        .map_or(false, |rest| rest.parse::<i64>().is_err())
}

/// Build the brief within `budget_chars`: at most 55% for the inventory, the
/// rest for hits.
pub fn build(db: &MemoryDatabase, session_id: &str, query: Option<&str>, budget_chars: usize) -> WorkspaceBrief {
    let inventory_budget = budget_chars * 55 / 100;
    let inventory = cached_inventory(db, inventory_budget);
    let hits_budget = budget_chars.saturating_sub(inventory.len());
    let hits = query
        .filter(|q| !q.trim().is_empty())
        .and_then(|q| build_hits(db, session_id, q, hits_budget));
    WorkspaceBrief { inventory, hits }
}

fn cached_inventory(db: &MemoryDatabase, budget: usize) -> String {
    static CACHE: Mutex<Option<(Instant, usize, String)>> = Mutex::new(None);
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, b, text)) = guard.as_ref() {
            if *b == budget && at.elapsed() < INVENTORY_TTL {
                return text.clone();
            }
        }
    }
    let text = build_inventory(db, budget);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((Instant::now(), budget, text.clone()));
    }
    text
}

/// Append `line` only while it fits the budget; returns false once full.
fn push_line(out: &mut String, line: &str, budget: usize) -> bool {
    if out.len() + line.len() + 1 > budget {
        return false;
    }
    out.push_str(line);
    out.push('\n');
    true
}

fn build_inventory(db: &MemoryDatabase, budget: usize) -> String {
    let matters = db.matters.list().unwrap_or_default();
    let matter_names: HashMap<i64, &str> = matters.iter().map(|m| (m.id, m.name.as_str())).collect();
    let matter_label = |id: Option<i64>| -> String {
        id.and_then(|i| matter_names.get(&i).copied())
            .map(|n| format!("matter \"{}\"", n))
            .unwrap_or_else(|| "General".to_string())
    };

    let chats: Vec<_> = db
        .conversations
        .get_all_sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.id.starts_with(ASSISTANT_SESSION_PREFIX))
        .collect();
    let mut drafts = db.drafts.list_drafts().unwrap_or_default();
    drafts.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut files: Vec<_> = db
        .local_files
        .get_all_files()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !f.is_directory)
        .collect();
    files.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "WORKSPACE (today is {}): {} chats, {} matters, {} Workspace drafts, {} Vault files.",
        chrono::Utc::now().format("%Y-%m-%d"),
        chats.len(),
        matters.len(),
        drafts.len(),
        files.len()
    );

    // Each section gets an equal share of what is left, so a long list of
    // chats can never push the matters out entirely.
    let section_budget = budget.saturating_sub(out.len()) / 4;

    let mut section = String::from("Matters:\n");
    for m in matters.iter().take(SECTION_ROWS) {
        let drafts_in = drafts.iter().filter(|d| d.matter_id == Some(m.id)).count();
        let mut line = format!("- {} [{}]", m.name, m.status);
        if let Some(c) = m.client.as_deref().filter(|c| !c.is_empty()) {
            let _ = write!(line, " client: {}", c);
        }
        if let Some(n) = m.matter_number.as_deref().filter(|n| !n.is_empty()) {
            let _ = write!(line, " no. {}", n);
        }
        let _ = write!(line, "; {} drafts", drafts_in);
        if !push_line(&mut section, &line, section_budget) {
            break;
        }
    }
    if matters.is_empty() {
        section.push_str("- (none)\n");
    }
    out.push_str(&section);

    let mut section = String::from("Recent chats:\n");
    for s in chats.iter().take(SECTION_ROWS) {
        let title = s.metadata.title.as_deref().unwrap_or("Untitled chat");
        let line = format!("- \"{}\" (last active {})", title, s.last_accessed.format("%Y-%m-%d"));
        if !push_line(&mut section, &line, section_budget) {
            break;
        }
    }
    if chats.is_empty() {
        section.push_str("- (none)\n");
    }
    out.push_str(&section);

    let mut section = String::from("Workspace drafts:\n");
    for d in drafts.iter().take(SECTION_ROWS) {
        let line = format!(
            "- \"{}\" ({}, version {}, {}; {}; updated {})",
            d.title,
            d.format,
            d.current_version,
            d.status,
            matter_label(d.matter_id),
            d.updated_at.format("%Y-%m-%d")
        );
        if !push_line(&mut section, &line, section_budget) {
            break;
        }
    }
    if drafts.is_empty() {
        section.push_str("- (none)\n");
    }
    out.push_str(&section);

    let mut section = String::from("Vault files:\n");
    for f in files.iter().take(SECTION_ROWS) {
        let line = format!(
            "- {} ({} KB, modified {})",
            f.path,
            (f.size_bytes.max(0) + 1023) / 1024,
            f.modified_at.format("%Y-%m-%d")
        );
        if !push_line(&mut section, &line, section_budget) {
            break;
        }
    }
    if files.is_empty() {
        section.push_str("- (none)\n");
    }
    out.push_str(&section);

    out.push_str(
        "Answer questions about the user's chats, matters, Workspace drafts and Vault from this \
         inventory and the matches given with the question. If something is not listed, say you \
         cannot see it rather than guessing.",
    );
    out
}

fn excerpt(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == crate::memory_db::search_store::MARK_OPEN || c == crate::memory_db::search_store::MARK_CLOSE { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.len() <= max {
        return flat;
    }
    let cut = (0..=max).rev().find(|&i| flat.is_char_boundary(i)).unwrap_or(0);
    format!("{}…", &flat[..cut])
}

fn build_hits(db: &MemoryDatabase, session_id: &str, query: &str, budget: usize) -> Option<String> {
    if budget < 200 {
        return None;
    }
    let mut out = String::from("[Workspace matches for this question]\n");
    let header_len = out.len();

    // Chat messages, across every chat except this Agent thread.
    let titles: HashMap<String, String> = db
        .conversations
        .get_all_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id, s.metadata.title.unwrap_or_else(|| "Untitled chat".to_string())))
        .collect();
    if let Ok(hits) = db
        .conversations
        .search_messages_fulltext(query, MESSAGE_HITS, Some(session_id), None)
    {
        for (m, _) in hits {
            let origin = if m.session_id.starts_with(ASSISTANT_SESSION_PREFIX) {
                "Agent".to_string()
            } else {
                format!("chat \"{}\"", titles.get(&m.session_id).map(String::as_str).unwrap_or("Untitled chat"))
            };
            let line = format!(
                "- {} ({}, {}): {}",
                origin,
                m.role,
                m.timestamp.format("%Y-%m-%d"),
                excerpt(&m.content, EXCERPT_CHARS)
            );
            if !push_line(&mut out, &line, budget) {
                break;
            }
        }
    }

    // Draft bodies and titles.
    if let Ok(hits) = db.search.search(query, DRAFT_HITS) {
        for h in hits {
            let line = format!("- draft \"{}\": {}", h.title, excerpt(&h.snippet, EXCERPT_CHARS));
            if !push_line(&mut out, &line, budget) {
                break;
            }
        }
    }

    // Vault file names containing a word of the question.
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.to_lowercase())
        .collect();
    if !words.is_empty() {
        let mut shown = 0;
        for f in db.local_files.get_all_files().unwrap_or_default() {
            if shown >= VAULT_HITS {
                break;
            }
            let name = f.name.to_lowercase();
            if !f.is_directory && words.iter().any(|w| name.contains(w.as_str())) {
                let line = format!("- Vault file {}", f.path);
                if !push_line(&mut out, &line, budget) {
                    break;
                }
                shown += 1;
            }
        }
    }

    (out.len() > header_len).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_agent_is_the_non_draft_thread() {
        assert!(is_global_agent("draft-assistant-global"));
        assert!(!is_global_agent("draft-assistant-42"));
        assert!(!is_global_agent("chat-123"));
    }

    #[test]
    fn excerpt_respects_char_boundaries() {
        let s = "é".repeat(400);
        let e = excerpt(&s, 101);
        assert!(e.ends_with('…'));
    }
}
