//! Never recall a duplicate.
//!
//! Recall from other conversations (past retrieval, related exchanges, topic
//! memory) must bring the model something it does not already have. An
//! exchange whose question is the one being asked, or one already asked in
//! this conversation, adds nothing: the model copies it ("Your first question
//! was ...") instead of answering. A whole conversation that repeats this
//! one's questions is a copy of it, not earlier material.
//!
//! Two rules, applied to every cross-conversation source:
//! - an exchange is dropped when its question nearly duplicates the current
//!   question or any user message of the current conversation;
//! - a conversation is skipped entirely when at least half of its questions
//!   nearly duplicate questions of the current one.
//!
//! "Nearly duplicates" compares content words (stop words removed): Jaccard
//! overlap of at least 0.6, or one containing at least 85% of the other.

use std::collections::{HashMap, HashSet};

use crate::memory_db::MemoryDatabase;

const JACCARD_MIN: f64 = 0.6;
const CONTAINMENT_MIN: f64 = 0.85;
const MIN_WORDS: usize = 3;

fn words(text: &str) -> HashSet<String> {
    crate::memory_db::fts::content_tokens(text).into_iter().collect()
}

fn similar(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if inter / union >= JACCARD_MIN {
        return true;
    }
    let small = a.len().min(b.len());
    small >= MIN_WORDS && inter / small as f64 >= CONTAINMENT_MIN
}

/// Do two questions ask the same thing in nearly the same words?
pub fn near_duplicate(a: &str, b: &str) -> bool {
    let (wa, wb) = (words(a), words(b));
    // Identical text with no content words (e.g. "and?") still duplicates.
    if wa.is_empty() && wb.is_empty() {
        return a.trim().eq_ignore_ascii_case(b.trim()) && !a.trim().is_empty();
    }
    similar(&wa, &wb)
}

/// The questions of the current conversation plus the one being asked.
pub struct Current {
    questions: Vec<HashSet<String>>,
    raw: Vec<String>,
}

impl Current {
    pub fn load(db: &MemoryDatabase, session_id: &str, query: &str) -> Self {
        let mut raw: Vec<String> = db
            .conversations
            .get_user_messages(session_id)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.content)
            .collect();
        raw.push(query.to_string());
        Self::from_questions(raw)
    }

    pub fn from_questions(raw: Vec<String>) -> Self {
        Current { questions: raw.iter().map(|q| words(q)).collect(), raw }
    }

    /// Is `question` already asked here (or being asked now)?
    pub fn has(&self, question: &str) -> bool {
        let w = words(question);
        if w.is_empty() {
            return self.raw.iter().any(|r| near_duplicate(r, question));
        }
        self.questions.iter().any(|q| similar(q, &w))
    }

    /// Sessions among `candidates` that repeat this conversation: at least
    /// half of their questions are already asked here.
    pub fn duplicate_sessions<'a>(
        &self,
        db: &MemoryDatabase,
        candidates: impl IntoIterator<Item = &'a String>,
    ) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut seen: HashMap<&String, ()> = HashMap::new();
        for s in candidates {
            if seen.insert(s, ()).is_some() {
                continue;
            }
            let qs: Vec<String> = db
                .conversations
                .get_user_messages(s)
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.content)
                .collect();
            if qs.is_empty() {
                continue;
            }
            // Either direction: half of its questions are asked here, or this
            // conversation's questions (the new one included) were already
            // asked there - all of them, or at least two and half.
            let theirs = qs.iter().filter(|q| self.has(q)).count();
            let their_sets: Vec<HashSet<String>> = qs.iter().map(|q| words(q)).collect();
            let ours = self
                .questions
                .iter()
                .zip(&self.raw)
                .filter(|(q, r)| {
                    their_sets.iter().zip(&qs).any(|(t, tr)| similar(q, t) || (q.is_empty() && near_duplicate(r, tr)))
                })
                .count();
            let ours_dup = ours == self.raw.len() || (ours >= 2 && ours * 2 >= self.raw.len());
            if theirs * 2 >= qs.len() || ours_dup {
                out.insert(s.clone());
            }
        }
        out
    }
}

/// Drop recalled exchanges that duplicate the current conversation, and every
/// exchange of a conversation that duplicates it.
pub fn filter_pairs(
    db: &MemoryDatabase,
    current: &Current,
    pairs: Vec<super::conversation_memory::Pair>,
) -> Vec<super::conversation_memory::Pair> {
    let dup_sessions = current.duplicate_sessions(db, pairs.iter().map(|p| &p.session_id));
    let mut kept: Vec<super::conversation_memory::Pair> = Vec::new();
    for p in pairs {
        if dup_sessions.contains(&p.session_id) {
            continue;
        }
        let question = p.user.as_ref().map(|u| u.content.as_str());
        if question.map_or(false, |q| current.has(q)) {
            continue;
        }
        // Results never repeat one another: the same exchange held by two
        // conversations comes back once, from the first (best ranked).
        let repeats = kept.iter().any(|k| match (k.user.as_ref(), question) {
            (Some(ku), Some(q)) => near_duplicate(&ku.content, q),
            _ => false,
        });
        if !repeats {
            kept.push(p);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewordings_of_the_same_question_are_duplicates() {
        assert!(near_duplicate("What was my first question?", "what was my first question"));
        assert!(near_duplicate(
            "What notice period applies to the warehouse lease at Rua das Flores?",
            "Which notice period applies to the Rua das Flores warehouse lease?"
        ));
        assert!(!near_duplicate(
            "What notice period applies to the warehouse lease?",
            "Can the landlord terminate the warehouse lease early?"
        ));
        assert!(!near_duplicate("Draft a cover letter", "What notice period applies to the lease?"));
    }

    #[test]
    fn a_conversation_that_repeats_this_one_is_skipped_and_others_are_kept() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        for id in ["here", "copy", "other"] {
            db.conversations.create_session_with_id(id, None).unwrap();
        }
        let q1 = "My client's warehouse is at 14 Rua das Flores. What notice period applies?";
        let q2 = "Does Section 14.2 apply to the landlord too?";
        let store = |s: &str, i: i32, c: &str| {
            db.conversations.store_messages_batch(s, &[("user".to_string(), c.to_string(), i, 0, 0.5)]).unwrap();
        };
        store("here", 1, q1);
        store("copy", 1, q1);
        store("copy", 3, q2);
        store("other", 1, "What remedies does the buyer have for late delivery?");
        store("other", 3, q1);
        store("other", 5, "Is the liquidated damages clause enforceable?");
        let current = Current::load(&db, "here", q2);
        let candidates: Vec<String> = vec!["copy".into(), "other".into()];
        let dups = current.duplicate_sessions(&db, candidates.iter());
        assert!(dups.contains("copy"), "every question of 'copy' is asked here");
        assert!(!dups.contains("other"), "one shared question in three is not a copy");
        assert!(current.has(q1) && current.has(q2));
        assert!(!current.has("Is the liquidated damages clause enforceable?"));

        // A brand-new chat whose one question was already asked in 'other'
        // duplicates it, so 'other' is skipped too.
        db.conversations.create_session_with_id("new", None).unwrap();
        let fresh = Current::load(&db, "new", q1);
        let dups = fresh.duplicate_sessions(&db, candidates.iter());
        assert!(dups.contains("other") && dups.contains("copy"), "{:?}", dups);
    }
}
