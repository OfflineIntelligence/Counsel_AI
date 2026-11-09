//! Which documents a chat may retrieve from.
//!
//! Scope is resolved server-side on every request and applied before any
//! ranking, so a document of one matter can never surface in another
//! matter's chat unless that chat explicitly searches all matters.

use std::collections::HashSet;

use crate::memory_db::MemoryDatabase;
use crate::shared_state::ASSISTANT_SESSION_PREFIX;

#[derive(Debug, Clone)]
pub struct Scope {
    /// Documents attached to this chat (the chat's subject).
    pub session_documents: Vec<i64>,
    /// Matter the chat belongs to (None = General).
    pub matter_id: Option<i64>,
    /// All documents the chat may retrieve from; None = every document.
    pub allowed: Option<HashSet<i64>>,
}

impl Scope {
    pub fn allows(&self, document_id: i64) -> bool {
        self.allowed.as_ref().map(|a| a.contains(&document_id)).unwrap_or(true)
    }

    /// The allowed set minus the chat's own attachments.
    pub fn beyond_session(&self, db: &MemoryDatabase) -> Option<HashSet<i64>> {
        let session: HashSet<i64> = self.session_documents.iter().copied().collect();
        match &self.allowed {
            Some(a) => Some(a.difference(&session).copied().collect()),
            None => {
                let all: HashSet<i64> = db.rag.all_document_ids().unwrap_or_default().into_iter().collect();
                Some(all.difference(&session).copied().collect())
            }
        }
    }
}

/// Matter and "search all matters" flag of a session.
pub fn session_matter(db: &MemoryDatabase, session_id: &str) -> (Option<i64>, bool) {
    if let Some(rest) = session_id.strip_prefix(ASSISTANT_SESSION_PREFIX) {
        return match rest.parse::<i64>() {
            // An Agent opened on a draft works inside that draft's matter.
            Ok(draft_id) => {
                let matter = db.drafts.get_draft(draft_id).ok().flatten().and_then(|d| d.matter_id);
                (matter, false)
            }
            // The global Agent answers about the whole workspace, so it
            // searches every matter's documents and every past chat.
            Err(_) => (None, true),
        };
    }
    // Chats have no matter of their own (the chat matter picker was
    // removed): every chat is General.
    (None, false)
}

pub fn resolve(db: &MemoryDatabase, session_id: &str) -> Scope {
    let session_documents: Vec<i64> = db
        .documents
        .get_session_documents(session_id)
        .map(|d| d.into_iter().map(|d| d.id).collect())
        .unwrap_or_default();
    let (matter_id, search_all) = session_matter(db, session_id);

    if search_all {
        return Scope { session_documents, matter_id, allowed: None };
    }

    let mut allowed: HashSet<i64> = session_documents.iter().copied().collect();
    let pool = match matter_id {
        Some(m) => db.rag.matter_document_ids(m),
        None => db.rag.unassigned_document_ids(),
    };
    allowed.extend(pool.unwrap_or_default());
    allowed.extend(db.rag.knowledge_document_ids().unwrap_or_default());

    Scope { session_documents, matter_id, allowed: Some(allowed) }
}

/// Chats whose messages this chat may recall: chats of the same matter (or,
/// for a General chat, other General chats). `None` means every chat (the
/// chat searches all matters).
pub fn allowed_sessions(db: &MemoryDatabase, session_id: &str) -> Option<HashSet<String>> {
    let (matter_id, search_all) = session_matter(db, session_id);
    if search_all {
        return None;
    }
    let sessions = db.conversations.get_all_sessions().unwrap_or_default();
    let mut allowed: HashSet<String> = HashSet::new();
    for s in sessions {
        let session_matter = match s
            .id
            .strip_prefix(ASSISTANT_SESSION_PREFIX)
            .and_then(|rest| rest.parse::<i64>().ok())
        {
            Some(draft_id) => db.drafts.get_draft(draft_id).ok().flatten().and_then(|d| d.matter_id),
            None => None,
        };
        if session_matter == matter_id {
            allowed.insert(s.id);
        }
    }
    allowed.insert(session_id.to_string());
    Some(allowed)
}

/// Link a chat's documents to its matter (after attaching, or after the chat
/// is moved to a matter).
pub fn link_session_documents(db: &MemoryDatabase, session_id: &str) {
    let (matter_id, _) = session_matter(db, session_id);
    let Some(matter) = matter_id else { return };
    if let Ok(docs) = db.documents.get_session_documents(session_id) {
        for d in docs {
            if d.source_kind == "knowledge" {
                continue;
            }
            if let Err(e) = db.rag.link_document_to_matter(matter, d.id) {
                tracing::warn!("Could not link document {} to matter {}: {}", d.id, matter, e);
            }
        }
    }
}
