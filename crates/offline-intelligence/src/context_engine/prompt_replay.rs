//! Prompt replay: send history exactly as the engine saw and produced it.
//!
//! Each turn folds blocks onto the user's question (document excerpts,
//! earlier conversations, recalled turns) and the reply is cleaned before it
//! is saved (citation note appended, echoed blocks stripped). The client sends
//! back the stored versions, so without replay the next prompt diverges from
//! the engine's saved KV state at the previous question, and the previous
//! question, its blocks and the previous reply are all computed again.
//!
//! Replaying the recorded versions makes every prompt an exact extension of
//! the last one: the KV state restored for the previous turn covers all of it
//! and only the new question and its blocks are computed. A recorded version
//! is used only while the stored message is unchanged (an edited or deleted
//! turn falls back to the client's text, and is computed from there).

use std::collections::HashMap;

use crate::memory::Message;

/// What replay produced.
#[derive(Debug, Default)]
pub struct Replayed {
    pub messages: Vec<Message>,
    /// Blocks carried by replayed user turns (the part after the question):
    /// material the model has in view, so citations taken from it are valid.
    pub carried_blocks: Vec<String>,
    /// Positions replayed.
    pub replayed: usize,
}

/// `messages` is the request (positions equal stored message_index); the last
/// one is the new question and is never replaced. `recorded` maps a position
/// to (role, stored content, content as the engine saw or produced it).
pub fn replay(messages: &[Message], recorded: &HashMap<i32, (String, String, String)>) -> Replayed {
    let mut out = Replayed { messages: messages.to_vec(), ..Default::default() };
    let last = messages.len().saturating_sub(1);
    for (i, m) in out.messages.iter_mut().enumerate().take(last) {
        let Some((role, stored, prompt)) = recorded.get(&(i as i32)) else { continue };
        if role != &m.role || stored.trim() != m.content.trim() || prompt == &m.content {
            continue;
        }
        if m.role == "user" {
            // Folded as: material, QUESTION_SEPARATOR, the user's own words
            // (document_memory::fold_into_latest_message).
            let sep = crate::context_engine::document_memory::QUESTION_SEPARATOR;
            if let Some(at) = prompt.rfind(sep) {
                let material = prompt[..at].trim();
                if !material.is_empty() {
                    out.carried_blocks.push(material.to_string());
                }
            }
        }
        m.content = prompt.clone();
        out.replayed += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, c: &str) -> Message {
        Message { role: role.into(), content: c.into() }
    }

    #[test]
    fn unchanged_turns_are_replayed_as_sent_and_the_new_question_is_left_alone() {
        let msgs = vec![m("system", "S"), m("user", "Q1"), m("assistant", "A1 clean"), m("user", "Q2")];
        let mut rec = HashMap::new();
        rec.insert(1, ("user".into(), "Q1".into(), "[EXCERPTS] p.4\n\n[QUESTION]\nQ1".into()));
        rec.insert(2, ("assistant".into(), "A1 clean".into(), "A1 raw".into()));
        rec.insert(3, ("user".into(), "Q2".into(), "Q2 with blocks".into()));
        let r = replay(&msgs, &rec);
        assert_eq!(r.messages[1].content, "[EXCERPTS] p.4\n\n[QUESTION]\nQ1");
        assert_eq!(r.messages[2].content, "A1 raw");
        assert_eq!(r.messages[3].content, "Q2", "the new question is assembled this turn");
        assert_eq!(r.carried_blocks, vec!["[EXCERPTS] p.4".to_string()]);
        assert_eq!(r.replayed, 2);
    }

    #[test]
    fn an_edited_turn_is_sent_as_the_client_has_it() {
        let msgs = vec![m("system", "S"), m("user", "Q1 edited"), m("user", "Q2")];
        let mut rec = HashMap::new();
        rec.insert(1, ("user".into(), "Q1".into(), "Q1 + blocks".into()));
        let r = replay(&msgs, &rec);
        assert_eq!(r.messages[1].content, "Q1 edited");
        assert_eq!(r.replayed, 0);
    }
}
