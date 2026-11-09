//! Removing prompt scaffolding that a model printed straight back.
//!
//! Retrieval folds blocks into the user's message so the model can read them:
//! document excerpts, matter material, earlier conversations, workspace
//! matches. A capable model reads them and answers. A small one often prints
//! them back first, so the reply opens with `[DOCUMENT EXCERPTS]`, a guidance
//! line written for the model and every `--- [file, p.3, ...] ---` tag before
//! any answer at all — the user is handed back the prompt.
//!
//! `RETRIEVAL_INSTRUCTIONS` forbids that, which is the real fix. This is the
//! guard for when it is ignored.
//!
//! # Two strippers, and why
//!
//! [`strip_scaffold`] is the exact one and the one to prefer. It is given the
//! blocks that were ACTUALLY folded in, and removes leading lines of the answer
//! that are literally those lines. It knows nothing about documents, pages,
//! slides or sheets, so it behaves the same for a PDF, a Word file, a deck, a
//! workbook or a text file, and it covers every block type equally — nothing
//! here needs updating when a new kind of block is added.
//!
//! [`strip_reprinted_headings`] is the fallback for when the blocks are not to
//! hand: a chat saved before this existed, or the live stream on the client.
//! It recognises the headings this codebase writes and cuts the run beneath.
//!
//! Both only ever cut from the START, and only while every line so far is
//! scaffolding. An answer that mentions a document, or quotes a line from one
//! and then says something about it, keeps everything from its first real
//! sentence on.

use std::collections::HashSet;

/// Every heading this codebase puts at the top of a folded block.
///
/// Matched as PREFIXES: three of them carry a trailing clause explaining why
/// the block was retrieved, and that wording is not worth pinning here.
const BLOCK_HEADINGS: [&str; 9] = [
    "[DOCUMENT EXCERPTS]",
    "[POSSIBLY RELEVANT MATERIAL FROM THIS MATTER'S DOCUMENTS]",
    "[MATERIAL FROM EARLIER",
    "[YOUR EARLIER CONVERSATIONS",
    "[Workspace matches for this question]",
    "[Note: no attached document is relevant",
    "[EARLIER IN THIS CONVERSATION",
    "[CONVERSATION SUMMARY",
    "[POSSIBLY RELATED FROM YOUR OTHER CONVERSATIONS",
];

fn is_heading(line: &str) -> bool {
    let t = line.trim();
    BLOCK_HEADINGS.iter().any(|h| t.starts_with(h))
}

/// One excerpt's tag line, e.g. `--- [SPA.pdf, p.47, Section 12.3] ---` or,
/// for a document with other versions in scope,
/// `--- [SPA.pdf, p.47] (v2, uploaded 3 May) ---`.
///
/// The tail is NOT required to be `] ---`. `format_passages` appends the
/// version label after the closing bracket, so testing for that missed every
/// versioned document — the one case where a reprint is longest and least
/// wanted.
fn is_tag_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("--- [") && t.ends_with("---") && t.len() > 8
}

/// Lines the passage formatter emits around an excerpt that are not the
/// excerpt itself.
fn is_formatter_line(line: &str) -> bool {
    matches!(
        line.trim(),
        "[excerpt shortened to fit]" | "[Further retrieved context omitted to fit the context window.]"
    )
}

/// Remove a reprint of the blocks that were folded into this turn.
///
/// `blocks` is what was actually given to the model. A leading line of the
/// answer is dropped when it is blank, a heading, a tag line, a formatter line,
/// or a line that appears verbatim in one of the blocks. The first line that is
/// none of those ends the cut, and everything from there is kept.
///
/// Nothing happens unless the answer OPENS with a block heading, so an answer
/// that repeats a sentence from a document mid-flow is untouched.
pub fn strip_scaffold(answer: &str, blocks: &[String]) -> String {
    let trimmed = answer.trim_start();
    if !trimmed.lines().next().map(is_heading).unwrap_or(false) {
        return answer.to_string();
    }

    // Every non-blank line the model was shown, trimmed, for exact matching.
    // A set rather than a substring search: a line either was in the prompt or
    // it was not, and "block contains line" would let one long block swallow
    // real prose that happened to appear inside it.
    let scaffold: HashSet<&str> = blocks
        .iter()
        .flat_map(|b| b.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let lines: Vec<&str> = trimmed.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty()
            || is_heading(t)
            || is_tag_line(t)
            || is_formatter_line(t)
            || scaffold.contains(t)
        {
            i += 1;
            continue;
        }
        break;
    }

    // Consuming the heading and nothing else is not a cut, it is damage: it
    // means there was no scaffolding to match against (no blocks were passed)
    // or the model paraphrased the block rather than copying it. Either way the
    // answer is returned whole, so `strip_reprinted_headings` still sees the
    // heading it needs to recognise the reprint by. Swallowing it here and
    // leaving the excerpts behind is the worst of both.
    if i <= 1 {
        return answer.to_string();
    }

    lines[i..].join("\n").trim().to_string()
}

/// Remove a reprint using only the headings this codebase writes.
///
/// The fallback for when the folded blocks are not available. Cuts from a
/// leading heading to the end of the run beneath it: further headings, tag
/// lines, and the body under the last tag. Less precise than
/// [`strip_scaffold`] — it cannot tell an excerpt body from prose except by
/// position — so it is only reached when the exact pass cannot run.
pub fn strip_reprinted_headings(answer: &str) -> String {
    let trimmed = answer.trim_start();
    if !trimmed.lines().next().map(is_heading).unwrap_or(false) {
        return answer.to_string();
    }

    let lines: Vec<&str> = trimmed.lines().collect();
    let last_tag = lines.iter().rposition(|l| is_tag_line(l));

    let start = match last_tag {
        // Headings with no excerpts under them — the earlier-conversations and
        // workspace blocks, and a mid-stream reprint caught before its first
        // tag has arrived. Such a block is its heading plus its guidance, which
        // run to the first blank line.
        None => lines
            .iter()
            .position(|l| l.trim().is_empty())
            .map(|i| i + 1)
            .unwrap_or(lines.len()),
        Some(tag) => {
            // The body under the last tag runs to a blank line that is NOT
            // followed by another excerpt or another block.
            let mut i = tag + 1;
            while i < lines.len() {
                if lines[i].trim().is_empty() {
                    match lines[i + 1..].iter().find(|l| !l.trim().is_empty()) {
                        Some(l) if is_tag_line(l) || is_heading(l) => {}
                        _ => {
                            i += 1;
                            break;
                        }
                    }
                }
                i += 1;
            }
            i
        }
    };

    lines[start.min(lines.len())..].join("\n").trim().to_string()
}

/// Remove reprinted excerpt blocks ANYWHERE in the answer, not just at it.
///
/// A small model sometimes restates the question — or writes a sentence —
/// BEFORE echoing the folded context, so the block sits mid-answer where the
/// start-anchored strippers above never engage. A span is excised only when it
/// opens with a block heading AND contains at least one `--- [...] ---` tag
/// line. The tag requirement is what keeps real prose safe: a quoted sentence
/// or a mere mention of a heading never has one.
///
/// Between the heading and the first tag, blank lines, formatter lines and
/// scaffold-member lines are consumed — plus, when no blocks are to hand, up
/// to two guidance lines of unknown wording. After a tag, the run continues
/// through blanks, tags, formatter lines and scaffold members; without blocks,
/// unknown body lines are consumed to the first blank line not followed by
/// another tag or heading (the same rule as [`strip_reprinted_headings`]).
/// A fresh heading always ends the span and starts a new candidate, so two
/// reprinted blocks in one answer are both removed.
pub fn strip_embedded_blocks(answer: &str, blocks: &[String]) -> String {
    let scaffold: HashSet<&str> = blocks
        .iter()
        .flat_map(|b| b.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let lines: Vec<&str> = answer.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut cut_any = false;
    while i < lines.len() {
        if is_heading(lines[i]) {
            if let Some(end) = embedded_span_end(&lines, i, &scaffold) {
                cut_any = true;
                i = end;
                continue;
            }
        }
        out.push(lines[i]);
        i += 1;
    }
    if !cut_any {
        return answer.to_string();
    }
    // Excising a span can leave three or more newlines stacked where a block
    // sat between paragraphs. Squeeze those back to one blank line.
    let mut collapsed = String::with_capacity(answer.len());
    let mut blanks = 0;
    for line in out.join("\n").lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                collapsed.push('\n');
            }
        } else {
            blanks = 0;
            collapsed.push_str(line);
            collapsed.push('\n');
        }
    }
    collapsed.trim().to_string()
}

/// End index (exclusive) of the reprinted span opening at heading `h`, or
/// `None` when the heading is not followed by a tag — prose that merely
/// mentions a block name.
fn embedded_span_end(lines: &[&str], h: usize, scaffold: &HashSet<&str>) -> Option<usize> {
    let mut j = h + 1;
    let mut seen_tag = false;
    // Guidance lines of unknown wording, only spendable with no blocks to
    // check against. Every real block carries one or two; a heading followed
    // by prose and no tag soon after is not a reprint.
    let mut guidance_left = 2;
    while j < lines.len() {
        let t = lines[j].trim();
        if t.is_empty() || is_formatter_line(t) {
            j += 1;
            continue;
        }
        if is_heading(t) {
            break;
        }
        if is_tag_line(t) {
            seen_tag = true;
            j += 1;
            break;
        }
        if scaffold.contains(t) {
            j += 1;
            continue;
        }
        if scaffold.is_empty() && guidance_left > 0 {
            guidance_left -= 1;
            j += 1;
            continue;
        }
        break;
    }
    if !seen_tag {
        return None;
    }
    loop {
        if j >= lines.len() {
            break;
        }
        let t = lines[j].trim();
        if is_tag_line(t) || is_formatter_line(t) || scaffold.contains(t) {
            j += 1;
            continue;
        }
        if is_heading(t) {
            break;
        }
        if t.is_empty() {
            if scaffold.is_empty() {
                // No blocks to match against: a blank ends the body unless
                // another excerpt or block follows it.
                let next = lines[j + 1..].iter().find(|l| !l.trim().is_empty());
                match next {
                    Some(l) if is_tag_line(l) || is_heading(l) => {
                        j += 1;
                        continue;
                    }
                    _ => break,
                }
            }
            j += 1;
            continue;
        }
        if scaffold.is_empty() {
            // Fallback body line (see above).
            j += 1;
            continue;
        }
        break;
    }
    Some(j)
}

/// Both passes: the exact one, then the headings one for anything it left.
pub fn strip_all(answer: &str, blocks: &[String]) -> String {
    strip_embedded_blocks(
        &strip_reprinted_headings(&strip_scaffold(answer, blocks)),
        blocks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_block() -> String {
        "[DOCUMENT EXCERPTS]
Retrieved from the attached documents for this question. Cite the tag heading each excerpt.

--- [IDMS.pdf, p.3, 4.2 Text, Style, and Image Recognition] ---
4.2 Text, Style, and Image Recognition
Beyond extracting raw text, the system understands text styles.

--- [IDMS.pdf, pp.3-4, 4.3 Digitization] ---
4.3 Digitization
The system converts image-based documents."
            .to_string()
    }

    /// A deck, a workbook, a Word file and a text file cite by slide, sheet and
    /// paragraph rather than by page. The tag WRAPPER is identical for all of
    /// them, which is the whole reason neither stripper knows about formats.
    fn mixed_format_block() -> String {
        "[DOCUMENT EXCERPTS]
Retrieved from the attached documents for this question. Cite the tag heading each excerpt.

--- [Pitch Deck.pptx, Slide 4] ---
Offline by design.

--- [Q1 Model.xlsx, Sheet: Revenue] ---
Total revenue 1,204,880.

--- [Advisory Agreement.docx, P12-18, Section 4: Fees] ---
The Adviser shall be paid monthly in arrears.

--- [Memory Safety.txt, P3] ---
Rust moves the check to compile time."
            .to_string()
    }

    fn versioned_block() -> String {
        "[DOCUMENT EXCERPTS]
Retrieved from the attached documents for this question. Cite the tag heading each excerpt.

--- [SPA.pdf, p.47, Section 12.3] (v2, uploaded 3 May) ---
The Seller shall indemnify the Buyer."
            .to_string()
    }

    #[test]
    fn strips_a_pure_reprint_exactly() {
        let b = pdf_block();
        assert_eq!(strip_scaffold(&b, std::slice::from_ref(&b)), "");
    }

    #[test]
    fn strips_a_reprint_for_every_format_not_just_pdf() {
        let b = mixed_format_block();
        assert_eq!(strip_scaffold(&b, std::slice::from_ref(&b)), "");
        // ...and, without the blocks to hand, through the fallback too.
        assert_eq!(strip_reprinted_headings(&b), "");
    }

    #[test]
    fn strips_a_versioned_tag_the_old_suffix_test_missed() {
        let b = versioned_block();
        assert_eq!(strip_reprinted_headings(&b), "");
        assert_eq!(strip_scaffold(&b, std::slice::from_ref(&b)), "");
    }

    #[test]
    fn keeps_the_answer_that_follows_a_reprint() {
        let b = mixed_format_block();
        let text = format!("{}\n\nIn short, the deck says it runs offline.", b);
        assert_eq!(
            strip_all(&text, std::slice::from_ref(&b)),
            "In short, the deck says it runs offline."
        );
    }

    #[test]
    fn strips_the_non_document_blocks_too() {
        for block in [
            "[MATERIAL FROM EARLIER, retrieved because this question refers back to it]
This is SUPPLEMENTARY context from previous conversations.",
            "[Workspace matches for this question]
Chat: Fee review - what did we agree on fees?",
            "[Note: no attached document is relevant to this message. Answer it normally.]",
        ] {
            let b = block.to_string();
            let text = format!("{}\n\nHere is the actual answer.", b);
            assert_eq!(
                strip_all(&text, std::slice::from_ref(&b)),
                "Here is the actual answer.",
                "block: {}",
                block
            );
        }
    }

    #[test]
    fn leaves_an_ordinary_answer_alone() {
        let a = "The system recognises headings and bold text [IDMS.pdf, p.3].";
        assert_eq!(strip_all(a, &[pdf_block()]), a);
    }

    #[test]
    fn leaves_an_answer_that_merely_mentions_the_block_alone() {
        let a = "I could not find it in the [DOCUMENT EXCERPTS] you gave me.";
        assert_eq!(strip_all(a, &[pdf_block()]), a);
    }

    #[test]
    fn keeps_prose_that_happens_to_quote_one_line() {
        // Does not open with a heading, so neither stripper engages.
        let a = "The deck puts it plainly: 'Offline by design.' That is the claim.";
        assert_eq!(strip_all(a, &[mixed_format_block()]), a);
    }

    #[test]
    fn no_blocks_means_no_exact_pass_but_the_fallback_still_runs() {
        let b = pdf_block();
        assert_eq!(strip_all(&b, &[]), "");
    }

    /// The exact pass must not half-cut. Given a block it cannot match, it has
    /// to leave the answer alone rather than swallow the heading and strand the
    /// excerpts under it - the fallback recognises a reprint BY that heading.
    #[test]
    fn a_block_it_cannot_match_is_left_for_the_fallback() {
        let unrelated = "[Workspace matches for this question]\nsomething else entirely".to_string();
        assert_eq!(strip_scaffold(&pdf_block(), &[unrelated.clone()]), pdf_block());
        assert_eq!(strip_all(&pdf_block(), &[unrelated]), "");
    }

    /// The production case: the model restates the question first, THEN echoes
    /// the block. The start-anchored strippers never engage; the embedded pass
    /// removes the block and keeps the prose on both sides.
    #[test]
    fn strips_a_mid_answer_reprint_and_keeps_surrounding_prose() {
        let b = pdf_block();
        let text = format!(
            "What is the compensation structure?\n\n{}\n\nIn short, it runs offline.",
            b
        );
        assert_eq!(
            strip_all(&text, std::slice::from_ref(&b)),
            "What is the compensation structure?\n\nIn short, it runs offline."
        );
    }

    #[test]
    fn strips_a_mid_answer_reprint_without_blocks_to_hand() {
        let b = pdf_block();
        let text = format!(
            "What is the compensation structure?\n\n{}\n\nIn short, it runs offline.",
            b
        );
        assert_eq!(
            strip_all(&text, &[]),
            "What is the compensation structure?\n\nIn short, it runs offline."
        );
    }

    #[test]
    fn strips_two_embedded_blocks_in_one_answer() {
        let b = pdf_block();
        let text = format!("First thought.\n\n{}\n\nSecond thought.\n\n{}\n\nDone.", b, b);
        assert_eq!(
            strip_all(&text, std::slice::from_ref(&b)),
            "First thought.\n\nSecond thought.\n\nDone."
        );
    }

    #[test]
    fn leaves_a_heading_with_no_tag_alone_mid_answer() {
        // Prose that merely mentions a block name: no tag follows, so the
        // heading line is kept.
        let a = "I checked the [DOCUMENT EXCERPTS] idea and it does not apply.\n\nHere is why.";
        assert_eq!(strip_all(a, &[pdf_block()]), a);
    }

    #[test]
    fn leaves_a_lone_quoted_line_alone_mid_answer() {
        // A verbatim quote without its heading and tag is a citation, not a
        // reprint: the embedded pass only engages on a heading line.
        let a = "The deck puts it plainly: 'Offline by design.' That is the claim.";
        assert_eq!(strip_all(a, &[mixed_format_block()]), a);
    }
}
