//! Optional reading step (MEMORY_READING_NOTES, default off).
//!
//! A small model handed pages of recalled excerpts often answers from the
//! wrong one or not at all. With this step the model first writes short,
//! dated notes of the facts that bear on the question, and the answer is then
//! generated from those notes instead of the raw excerpts. It costs one extra
//! model call per question that has retrieved material, so it is a setting.

use crate::context_engine::document_memory::QUESTION_SEPARATOR;
use crate::memory::Message;

/// Longest note text kept (characters); notes past this are cut.
pub const MAX_NOTE_CHARS: usize = 3000;
/// Tokens the note-taking call may generate.
pub const NOTE_TOKENS: u32 = 384;

const NOTES_HEADING: &str = "[NOTES FROM EARLIER MATERIAL - the facts in the saved record that bear on this question]";

pub fn enabled() -> bool {
    crate::tuning::flag_or("MEMORY_READING_NOTES", false)
}

/// The note-taking request for a newest message that carries retrieved
/// material ahead of the question; None when it carries none.
pub fn notes_request(latest: &str) -> Option<Vec<Message>> {
    let at = latest.rfind(QUESTION_SEPARATOR)?;
    let material = latest[..at].trim();
    let question = latest[at + QUESTION_SEPARATOR.len()..].trim();
    if material.is_empty() || question.is_empty() {
        return None;
    }
    Some(vec![Message {
        role: "user".to_string(),
        content: format!(
            "Below is material from earlier conversations and documents, then a question.\n\
             Write short notes listing every fact in the material that helps answer the question, \
             one per line, each with its date or turn when one is given. Copy names, numbers and \
             dates exactly. Do not answer the question. If nothing in the material helps, write only: none\n\n\
             {}\n\nQuestion: {}",
            material, question
        ),
    }])
}

/// The newest message rebuilt around the notes: notes first, then the
/// question. None (keep the original) when the notes are empty or say none.
pub fn with_notes(latest: &str, notes: &str) -> Option<String> {
    let at = latest.rfind(QUESTION_SEPARATOR)?;
    let notes = notes.trim();
    let lowered = notes.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    if notes.is_empty() || lowered == "none" {
        return None;
    }
    let notes: String = notes.chars().take(MAX_NOTE_CHARS).collect();
    Some(format!("{}\n{}{}", NOTES_HEADING, notes, &latest[at..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::document_memory::fold_into_latest_message;

    fn folded(question: &str, block: &str) -> String {
        let mut m = vec![Message { role: "user".to_string(), content: question.to_string() }];
        fold_into_latest_message(&mut m, block);
        m.pop().unwrap().content
    }

    #[test]
    fn a_message_without_material_needs_no_notes() {
        assert!(notes_request("What is the deposit?").is_none());
    }

    #[test]
    fn the_request_carries_the_material_and_the_question() {
        let latest = folded("Where was the lease signed?", "--- [Turn 3, 2023-05-02] --- You asked: ... Answer: signed in Porto");
        let req = notes_request(&latest).unwrap();
        assert_eq!(req.len(), 1);
        assert!(req[0].content.contains("signed in Porto"));
        assert!(req[0].content.trim_end().ends_with("Question: Where was the lease signed?"));
    }

    #[test]
    fn notes_replace_the_material_and_the_question_stays_last() {
        let latest = folded("Where was the lease signed?", "LONG RAW EXCERPTS");
        let out = with_notes(&latest, "- Turn 3 (2023-05-02): the lease was signed in Porto").unwrap();
        assert!(out.starts_with(NOTES_HEADING));
        assert!(!out.contains("LONG RAW EXCERPTS"));
        assert!(out.ends_with("Where was the lease signed?"));
    }

    #[test]
    fn empty_or_none_notes_keep_the_original_material() {
        let latest = folded("Where was the lease signed?", "RAW");
        assert!(with_notes(&latest, "").is_none());
        assert!(with_notes(&latest, " None. ").is_none());
    }
}
