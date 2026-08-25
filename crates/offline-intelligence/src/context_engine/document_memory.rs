//! Backend-owned document memory — the system, not the model, remembers.
//!
//! Every document ever attached to a session (paperclip or Local Storage) is
//! persisted once in `documents`/`session_documents`/`document_chunks`
//! (memory_db::documents_store). On EVERY turn, regardless of whether an
//! attachment arrived this turn, this module reloads the session's full
//! document set from the database and folds it into the prompt — so turn 1,
//! turn 27, or a chat reopened after a restart all see the same documents.
//!
//! Retrieval is purely structural: there is no embedding model in this system,
//! so nothing here ranks content against the user's question. A session's
//! documents are included whole when they fit the active model's context
//! budget, and otherwise truncated to an ANNOUNCED head excerpt per document
//! (see `head_excerpt`) — never silently. The chunk provenance written at
//! extraction time (`utils::doc_context::chunk_text`) is unaffected, so
//! `[[page:N]]` / `[[slide:N]]` / `[[sheet:Name]]` markers survive into the
//! prompt and the model can still cite a location back to the user.
//!
//! A consequence worth keeping: because selection no longer depends on the
//! query, the block this module produces is byte-identical from turn to turn
//! for a given document set. It therefore stays inside llama-server's
//! cacheable prompt prefix, which query-dependent retrieval could never do.
//!
//! The output is TEXT to fold into EXISTING message content, never a new
//! message entry — chat templates with strict role alternation (verified
//! live against gemma-3/b8037) hard-error on an extra system-role message.

use tracing::{info, warn};

use crate::memory_db::{DocumentRecord, MemoryDatabase};

/// Human-readable file-type label from a filename's extension, so the model
/// is told what KIND of content it's looking at instead of inferring it (or
/// failing to) from the filename alone. Matters most when several documents
/// of different formats are attached in the same turn - without this, a
/// screenshot's OCR text and a resume's PDF text look identical in shape,
/// and a weak model has no signal to tell them apart.
///
/// Kept SHORT and purely nominal. This is a type NAME, printed next to a
/// filename - it is not the place for guidance. The image variant used to be
/// a 17-word sentence ("Image - the text below was read from the picture via
/// OCR, it is NOT a document the user wrote"), which a small model read as a
/// statement about the payload rather than a label, and which was repeated
/// for every image attached. How to read each format now lives once, in the
/// header's "Reading the formats" line.
fn file_type_label(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => "PDF",
        "doc" | "docx" => "Word document",
        "xls" | "xlsx" | "ods" => "Spreadsheet",
        "ppt" | "pptx" => "Presentation",
        "odt" => "OpenDocument text",
        "rtf" => "Rich Text document",
        "html" | "htm" => "Web page",
        "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "gif" => "Image (OCR text)",
        "csv" => "CSV data",
        "json" | "yaml" | "yml" | "xml" => "Structured data",
        "md" => "Markdown",
        "txt" | "log" => "Plain text",
        "js" | "ts" | "jsx" | "tsx" | "py" | "java" | "cpp" | "c" | "cs" | "go" | "rs" | "php"
        | "rb" | "swift" | "kt" | "scala" | "sql" | "sh" | "bat" | "ps1" => "Source code",
        _ => "file",
    }
}

/// Instructions that accompany every attached document, on every turn.
///
/// This replaced a version whose only stated use cases were "a
/// summary/description/explanation" and "describing THAT file's actual
/// content". The words quote, verbatim, exact and reproduce appeared
/// nowhere - so the model was never told that showing the user the actual
/// text was permitted, and reliably summarised even when asked outright for
/// exact wording. Every intent the product supports is now named.
///
/// Length is a real cost: this is subtracted from the same budget the
/// document body draws on (see assemble_session_block), so every sentence
/// must earn its place. Per-format reading notes are collapsed into one
/// line rather than repeated per file.
const DOCUMENT_INSTRUCTIONS: &str = "\
[ATTACHED DOCUMENTS - THE AUTHORITATIVE SOURCE FOR THIS ANSWER]\n\
The file(s) below are attached to this conversation and stay available for \
all of it; they never need re-attaching. Each is labelled with its type - \
trust that label rather than guessing from how the text looks.\n\
Serve the request the user actually made:\n\
- Show / display / quote / \"exact wording\" / \"full text\" -> reproduce the \
text below exactly as written, word for word, preserving its order. Do not \
paraphrase, condense, tidy, or swap any part of it for a description or a \
summary. This is an explicitly supported request, not something to decline.\n\
- Summarise / outline / key points -> summarise.\n\
- A specific question -> answer it directly, then cite where it came from.\n\
Always:\n\
- Answer only from the content below. Not from general knowledge, not from \
what a file with that name usually contains, not from earlier topics in this \
conversation. If a file was just attached, treat it as the subject.\n\
- Never invent document content. If what the user asked for is not below, \
say so plainly and state what you do have.\n\
- Cite the tag that opens an excerpt, e.g. [p.12, Section 4.2(b)] or \
[Slide 3], rather than describing the location in your own words.\n\
- Reading the formats: text from images and scanned pages was read by OCR and \
may carry recognition errors, and is not the user's own writing; spreadsheet \
cells are tab-separated under [[sheet:Name]]; slides appear as [[slide:N]]; \
pages as [[page:N]].\n";

pub struct DocumentContextBlocks {
    /// Folds into the SYSTEM message (position 0). Stable/append-only across
    /// turns for a given session — this session's documents don't change
    /// unless a new one is attached, and selection is query-independent, so
    /// this text is byte-identical turn to turn, keeping it part of the
    /// cacheable prompt prefix.
    pub session_block: Option<String>,
    /// Character cost of the block, for the caller to subtract from the
    /// conversation-history budget it hands to the orchestrator.
    pub consumed_chars: usize,
}

/// Build the document-memory text block for this turn.
///
/// The whole budget goes to this session's own documents: what you attached
/// here, you can always ask about here. There is no cross-session pass and no
/// query-dependent ranking — see the module docs for why.
pub async fn build_document_context(
    database: &MemoryDatabase,
    session_id: &str,
    total_budget_chars: usize,
) -> DocumentContextBlocks {
    let session_docs = match database.documents.get_session_documents(session_id) {
        Ok(docs) => docs,
        Err(e) => {
            warn!("Failed to load session documents for {}: {}", session_id, e);
            Vec::new()
        }
    };

    let session_block = if session_docs.is_empty() {
        None
    } else {
        Some(assemble_session_block(&session_docs, total_budget_chars))
    };

    let consumed_chars = session_block.as_ref().map(|s| s.len()).unwrap_or(0);

    if let Some(ref block) = session_block {
        info!(
            "Document memory for session {}: {} in-session doc(s), session_block={} chars",
            session_id,
            session_docs.len(),
            block.len(),
        );
    }

    DocumentContextBlocks { session_block, consumed_chars }
}

/// Whole text when the session's documents fit; otherwise an announced head
/// excerpt per document that exceeds its share of the budget.
fn assemble_session_block(
    session_docs: &[DocumentRecord],
    budget_chars: usize,
) -> String {
    // Oldest-attached first: this is what makes the block APPEND-ONLY (and
    // therefore prefix-stable) as new documents are attached over the
    // session's lifetime — get_session_documents returns newest-first.
    let mut docs: Vec<&DocumentRecord> = session_docs.iter().collect();
    docs.reverse();

    let ok_docs: Vec<&DocumentRecord> = docs
        .iter()
        .copied()
        .filter(|d| d.extraction_status == "ok" && !d.extracted_text.trim().is_empty())
        .collect();
    let failed_docs: Vec<&DocumentRecord> = docs
        .iter()
        .copied()
        .filter(|d| d.extraction_status != "ok" || d.extracted_text.trim().is_empty())
        .collect();

    let doc_count = ok_docs.len() + failed_docs.len();
    let mut header = String::from(DOCUMENT_INSTRUCTIONS);
    if doc_count > 1 {
        header.push_str("Files attached to this conversation:\n");
        for doc in failed_docs.iter().chain(ok_docs.iter()) {
            header.push_str(&format!(
                "  - {} ({})\n",
                doc.original_filename,
                file_type_label(&doc.original_filename)
            ));
        }
    }
    // Named, not just counted: a silent "N files omitted" note is exactly
    // the kind of failure that's impossible to diagnose from the outside.
    for doc in &failed_docs {
        let reason = doc.extraction_error.as_deref().unwrap_or("no reason recorded");
        warn!(
            "Document '{}' (session doc, id {}) has no usable content - status='{}', reason: {}",
            doc.original_filename, doc.id, doc.extraction_status, reason
        );
        header.push_str(&format!(
            "[Could not read '{}' ({}): {}]\n",
            doc.original_filename, file_type_label(&doc.original_filename), reason
        ));
    }
    for doc in &ok_docs {
        info!(
            "Document '{}' (session doc, id {}) has {} extracted chars - will be injected",
            doc.original_filename, doc.id, doc.extracted_text.len()
        );
    }

    let total_chars: usize = ok_docs.iter().map(|d| d.extracted_text.len()).sum();
    let mut body = String::new();
    // Whether EVERY ok document below is present in full. This decides which
    // completeness statement the model is given, and it matters most for the
    // request that motivated all of this: asked to reproduce exact wording,
    // the model must know whether it is holding the whole document or a
    // selection, so it can either comply fully or say precisely what it is
    // missing - rather than quietly summarising, which is what it did before.
    let mut all_complete = true;

    if total_chars <= budget_chars.saturating_sub(header.len()) {
        for doc in &ok_docs {
            body.push_str(&format!(
                "\n--- Document: {} [{}] ---\n{}\n--- End of document ---\n",
                doc.original_filename, file_type_label(&doc.original_filename), doc.extracted_text
            ));
        }
    } else {
        // Doesn't fit whole: give each document a proportional share of the
        // budget and truncate the oversized ones to their opening text. With
        // no embedding model there is nothing to rank passages against, so
        // this is a head excerpt, not a selection - and it says so, both here
        // and inside head_excerpt, rather than letting a partial reading pass
        // for the whole document.
        let per_doc_budget = (budget_chars.saturating_sub(header.len()) / ok_docs.len().max(1)).max(400);
        for doc in &ok_docs {
            if doc.extracted_text.len() <= per_doc_budget {
                body.push_str(&format!(
                    "\n--- Document: {} [{}] ---\n{}\n--- End of document ---\n",
                    doc.original_filename, file_type_label(&doc.original_filename), doc.extracted_text
                ));
                continue;
            }
            let excerpt = head_excerpt(
                &doc.extracted_text,
                per_doc_budget,
                "the document is larger than the context budget available to it",
            );
            all_complete = false;
            body.push_str(&format!(
                "\n--- Document: {} [{}] (PARTIAL - {} characters in the original; only the opening \
                 portion below is available) ---\n{}\n--- End of document ---\n",
                doc.original_filename, file_type_label(&doc.original_filename), doc.extracted_text.len(), excerpt
            ));
        }
    }

    // State completeness explicitly, either way. Silence here is what let the
    // model treat a partial selection as if it were the whole file.
    //
    // "Included in full" is a claim about the WHOLE attachment set, so it
    // cannot be made while any file failed extraction - those are listed
    // above as "[Could not read ...]" and are genuinely absent. Saying
    // "every document below is included IN FULL" alongside them would be the
    // exact overstatement this block exists to prevent.
    if all_complete && failed_docs.is_empty() {
        header.push_str(
            "Every document below is included IN FULL - nothing has been omitted, so a \
             request for the complete or exact text can be answered directly from it.\n",
        );
    } else if all_complete {
        header.push_str(
            "The documents below are included in full, but the file(s) marked \
             'Could not read' above yielded no usable content and are NOT available to \
             you. Answer from what is present, and if the user asks about a file that \
             could not be read, say so rather than guessing at its contents.\n",
        );
    } else {
        header.push_str(
            "One or more documents below are marked PARTIAL: only the opening portion of \
             the file is shown, and the rest is not available to you at all. For those, \
             answer from what is shown, state plainly that you were given the beginning of \
             the document rather than all of it, and never present a partial reading as the \
             complete text. If the user asks about something that would appear later in \
             such a file, say it is beyond the portion you were given instead of guessing.\n",
        );
    }

    header.push_str(&body);
    header
}

fn head_excerpt(text: &str, budget_chars: usize, reason: &str) -> String {
    let mut end = budget_chars.min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    format!(
        "[Showing the first {} of {} characters only ({}); the remainder is not visible to the model.]\n{}",
        end, text.len(), reason, &text[..end]
    )
}

/// Fold `block` into the system message (position 0), creating one if none
/// exists. Never adds a message anywhere else — see module docs for why.
pub fn fold_into_system_message(messages: &mut Vec<crate::memory::Message>, block: &str) {
    if block.trim().is_empty() {
        return;
    }
    if let Some(first) = messages.first_mut() {
        if first.role == "system" {
            first.content.push_str("\n\n");
            first.content.push_str(block);
            return;
        }
    }
    messages.insert(0, crate::memory::Message { role: "system".to_string(), content: block.to_string() });
}

/// Fold `block` into the LATEST message's content.
///
/// For content that is inherently query-dependent — retrieved past documents and
/// messages, which differ every turn because the question does. Such a block can
/// never be part of a stable prompt prefix regardless of where it goes, so it is
/// placed on the one message that already changes every turn, leaving the prefix
/// before it byte-identical and still cacheable.
///
/// Appends to existing content rather than inserting a message: chat templates
/// with strict role alternation (verified live against gemma-3/b8037) hard-error
/// on an extra system-role message mid-array.
pub fn fold_into_latest_message(messages: &mut [crate::memory::Message], block: &str) {
    if block.trim().is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        last.content.push_str("\n\n");
        last.content.push_str(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Message;

    #[test]
    fn fold_into_system_creates_one_when_absent() {
        let mut messages = vec![Message { role: "user".to_string(), content: "hi".to_string() }];
        fold_into_system_message(&mut messages, "doc content");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "doc content");
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn fold_into_system_appends_when_present() {
        let mut messages = vec![
            Message { role: "system".to_string(), content: "base prompt".to_string() },
            Message { role: "user".to_string(), content: "hi".to_string() },
        ];
        fold_into_system_message(&mut messages, "doc content");
        assert_eq!(messages.len(), 2, "must not add a message when one already exists");
        assert!(messages[0].content.starts_with("base prompt"));
        assert!(messages[0].content.contains("doc content"));
    }

    #[test]
    fn head_excerpt_names_truncation_and_reason() {
        let text = "x".repeat(1000);
        let out = head_excerpt(&text, 50, "test reason");
        assert!(out.contains("test reason"));
        assert!(out.contains("not visible to the model"));
    }

    #[tokio::test]
    async fn empty_session_yields_no_blocks() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        let blocks = build_document_context(&db, "no-such-session", 4000).await;
        assert!(blocks.session_block.is_none());
        assert_eq!(blocks.consumed_chars, 0);
    }

    /// Regression test born from a user report ("only one of my two attached
    /// documents seems to come through"). Investigated with the user's
    /// actual real PDFs (~4400 chars each, near-duplicate legal agreements)
    /// at multiple realistic budgets - both documents' full, distinguishing
    /// content was present every time, conclusively ruling out a backend
    /// content-assembly bug. This test keeps that proof permanent using
    /// synthetic stand-ins of the same size/similarity shape (two ~4000-char
    /// near-duplicate documents differing only in a few key phrases) instead
    /// of the user's real private text.
    #[tokio::test]
    async fn two_similar_realistically_sized_documents_both_survive_combination() {
        // Distinguishing detail placed EARLY (Section 1), matching how real
        // legal documents front-load identifying terms - a fair analogue of
        // the real user PDFs, where the compensation figures that actually
        // differed between the two files appeared well before the midpoint.
        fn make_doc(unique_marker: &str) -> String {
            let filler = "This clause restates the standing terms of the arrangement between the parties and carries no additional obligation beyond what is already described above. ".repeat(24);
            format!(
                "ADVISORY AGREEMENT\nSection 1: {unique_marker}\nSection 2: Compensation.\n{filler}\nSection 3: General.\n{filler}"
            )
        }
        let text0 = make_doc("The retainer for document ZERO is five thousand dollars monthly");
        let text1 = make_doc("The retainer for document ONE is negotiated separately each quarter");
        assert!(text0.len() > 2000 && text1.len() > 2000, "stand-ins must be realistically sized");

        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("diag", None).ok();
        let d0 = db.documents.upsert_document(crate::memory_db::NewDocument {
            original_bytes: text0.as_bytes(),
            original_filename: "Agreement.pdf",
            source_path: None, source_kind: "paperclip", mime_type: None,
            size_bytes: text0.len() as i64,
            extracted_text: text0.clone(), extraction_status: "ok", extraction_error: None,
            local_file_id: None,
        }).unwrap();
        db.documents.link_session("diag", d0.id, "paperclip").unwrap();
        let d1 = db.documents.upsert_document(crate::memory_db::NewDocument {
            original_bytes: text1.as_bytes(),
            original_filename: "Unofficial Agreement.pdf",
            source_path: None, source_kind: "paperclip", mime_type: None,
            size_bytes: text1.len() as i64,
            extracted_text: text1.clone(), extraction_status: "ok", extraction_error: None,
            local_file_id: None,
        }).unwrap();
        db.documents.link_session("diag", d1.id, "paperclip").unwrap();

        // Budgets spanning small (2048 tokens) through generous (16384) -
        // covers the realistic range for the model context sizes this
        // product auto-detects.
        for ctx_budget_tokens in [2048usize, 6144, 16384] {
            let doc_budget_chars = ctx_budget_tokens * 2;
            let blocks = build_document_context(&db, "diag", doc_budget_chars).await;
            let block = blocks.session_block.clone().unwrap_or_default();
            assert!(
                block.contains("Agreement.pdf") && block.contains("Unofficial Agreement.pdf"),
                "both documents must be named in the block at budget {}: {}",
                ctx_budget_tokens, block
            );
            // The contract: EITHER the distinguishing detail survives, OR its
            // absence is honestly announced (truncation/excerpting notice) -
            // never silently dropped with no trace. Never both-silent.
            for (needle, label) in [
                ("five thousand dollars monthly", "document ZERO"),
                ("negotiated separately each quarter", "document ONE"),
            ] {
                let present = block.contains(needle);
                let announced = block.contains("not visible to the model")
                    || block.contains("showing the most relevant parts")
                    || block.contains("[...]");
                assert!(
                    present || announced,
                    "{}'s distinguishing content is missing AND not announced as truncated/excerpted at budget {} - this would be SILENT loss: {}",
                    label, ctx_budget_tokens, block
                );
            }
        }
    }

    /// A document too large to include whole must be ANNOUNCED as partial,
    /// and whatever it does contain must be in document order.
    ///
    /// This is the central guarantee of the truncation path: the model is told
    /// PARTIAL and told not to present a partial reading as the complete text.
    /// Without that signal it answers "show me the exact text" from a fragment
    /// as though it were the whole file - the failure this block exists to
    /// prevent.
    ///
    /// The ordering assertion is cheap but not redundant: it pins that
    /// truncation takes the document's OPENING text and keeps it sequential,
    /// rather than emitting whatever pieces happen to be reachable in whatever
    /// order - the property that makes the announcement ("only the opening
    /// portion") an accurate description of what was actually sent.
    #[tokio::test]
    async fn oversized_document_is_announced_as_partial_and_stays_in_order() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("ordered", None).ok();

        // 12 numbered sections, each large enough that they cannot all fit -
        // forcing the excerpt path rather than the whole-document path.
        let body = "This clause sets out the parties' obligations in detail and \
                    continues at length so that the section occupies a meaningful \
                    share of the document budget under test. ";
        let mut text = String::new();
        for n in 1..=12 {
            text.push_str(&format!(
                "Section {}: Clause Heading {}\n{}\n\n",
                n,
                n,
                body.repeat(6)
            ));
        }

        let doc = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: text.as_bytes(),
                original_filename: "ordered-agreement.pdf",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: text.len() as i64,
                extracted_text: text.clone(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap();
        db.documents.link_session("ordered", doc.id, "paperclip").unwrap();

        let chunk_count = db.documents.get_chunks(doc.id).map(|c| c.len()).unwrap_or(0);
        assert!(chunk_count > 3, "fixture must produce several chunks, got {}", chunk_count);

        // Budget deliberately far below the document size, so excerpting runs.
        let blocks = build_document_context(&db, "ordered", 3_000).await;
        let block = blocks.session_block.expect("session block must be present");

        // The document was too large to include whole - that must be stated,
        // not left for the model to assume either way.
        assert!(
            block.contains("PARTIAL"),
            "an excerpted document must be marked PARTIAL:\n{}",
            block
        );
        assert!(
            block.contains("never present a partial reading as the complete text"),
            "the model must be told not to pass a selection off as the whole document:\n{}",
            block
        );
        assert!(
            !block.contains("Every document below is included IN FULL"),
            "must not claim completeness for an excerpted document:\n{}",
            block
        );

        // Whatever sections did make it in must be in ascending order.
        let re = regex::Regex::new(r"Section (\d+):").unwrap();
        let order: Vec<i32> = re
            .captures_iter(&block)
            .filter_map(|c| c.get(1))
            .filter_map(|m| m.as_str().parse::<i32>().ok())
            .collect();
        assert!(!order.is_empty(), "expected some document content:\n{}", block);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(
            order, sorted,
            "sections must appear in ascending document order, got {:?}",
            order
        );
    }

    /// The mirror case: when everything fits, the model must be told so - it
    /// is what licenses a direct, complete answer to "show me the full text".
    #[tokio::test]
    async fn fully_included_documents_are_announced_as_complete() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("whole", None).ok();
        let text = "SECTION 1: TERM\nThis agreement runs for twelve months from the effective date.";
        let doc = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: text.as_bytes(),
                original_filename: "short.docx",
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
        db.documents.link_session("whole", doc.id, "paperclip").unwrap();

        let blocks =
            build_document_context(&db, "whole", 100_000).await;
        let block = blocks.session_block.expect("session block must be present");

        assert!(
            block.contains("Every document below is included IN FULL"),
            "a document that fits must be announced as complete:\n{}",
            block
        );
        assert!(!block.contains("PARTIAL"), "nothing was excerpted:\n{}", block);
        assert!(block.contains("runs for twelve months"), "content must be present:\n{}", block);
    }

    /// The regression this whole change exists for: the instructions must
    /// name verbatim reproduction as a supported request. The previous
    /// wording enumerated only "summary/description/explanation", so the
    /// model was never told it could show the user the actual text.
    #[tokio::test]
    async fn instructions_authorise_verbatim_reproduction_not_only_summarising() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("verbatim", None).ok();
        let doc = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: b"clause text",
                original_filename: "deed.pdf",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: 11,
                extracted_text: "The purchase price is one pound sterling.".to_string(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap();
        db.documents.link_session("verbatim", doc.id, "paperclip").unwrap();

        let blocks =
            build_document_context(&db, "verbatim", 100_000).await;
        let block = blocks.session_block.expect("session block must be present");

        for required in [
            "reproduce the text below exactly as written",
            "Do not paraphrase",
            "explicitly supported request",
            "Summarise / outline / key points",
            "answer it directly",
        ] {
            assert!(
                block.contains(required),
                "instructions must contain {:?} so every supported intent is named:\n{}",
                required, block
            );
        }
    }

    #[tokio::test]
    async fn session_block_contains_whole_document_when_it_fits() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("s1", None).ok();
        let doc = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: b"contract bytes",
                original_filename: "contract.pdf",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: 14,
                extracted_text: "This contract caps liability at one million dollars.".to_string(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap();
        db.documents.link_session("s1", doc.id, "paperclip").unwrap();

        let blocks = build_document_context(&db, "s1", 100_000).await;
        let block = blocks.session_block.expect("session has a document, block must be present");
        assert!(block.contains("contract.pdf"));
        assert!(block.contains("caps liability at one million"));
    }
    /// The instructions are subtracted from the same budget the document body
    /// draws on, so their size is a product decision, not an accident. This
    /// pins it: if the block grows past the ceiling, that is a deliberate
    /// trade against document room and this test should be updated knowingly.
    #[test]
    fn instruction_block_stays_within_its_char_budget() {
        let len = DOCUMENT_INSTRUCTIONS.len();
        println!("DOCUMENT_INSTRUCTIONS = {} chars", len);
        assert!(
            len <= 1800,
            "instruction block is {} chars - every char here is one fewer of the user's \
             document the model gets to see",
            len
        );
    }

    /// A file that failed extraction is genuinely absent, so the block must
    /// NOT tell the model everything is present in full. Caught auditing the
    /// completeness statement added earlier in this same change - the first
    /// version keyed only on whether excerpting ran, and would happily claim
    /// full inclusion while sitting next to a "[Could not read ...]" line.
    #[tokio::test]
    async fn completeness_is_not_overstated_when_a_file_failed_extraction() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("mixed", None).ok();

        let good = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: b"readable bytes",
                original_filename: "readable.txt",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: 14,
                extracted_text: "The deposit is refundable within fourteen days.".to_string(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap();
        db.documents.link_session("mixed", good.id, "paperclip").unwrap();

        let bad = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: b"unreadable bytes",
                original_filename: "scan.pdf",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: 16,
                extracted_text: String::new(),
                extraction_status: "failed",
                extraction_error: Some("[PDF 'scan.pdf' contains no extractable text.]".to_string()),
                local_file_id: None,
            })
            .unwrap();
        db.documents.link_session("mixed", bad.id, "paperclip").unwrap();

        let blocks =
            build_document_context(&db, "mixed", 100_000).await;
        let block = blocks.session_block.expect("session block must be present");

        assert!(
            block.contains("Could not read 'scan.pdf'"),
            "the unreadable file must still be named:\n{}",
            block
        );
        assert!(
            !block.contains("Every document below is included IN FULL"),
            "must not claim full inclusion while a file failed extraction:\n{}",
            block
        );
        assert!(
            block.contains("NOT available to you"),
            "the model must be told the failed file's content is absent:\n{}",
            block
        );
        assert!(
            block.contains("refundable within fourteen days"),
            "the readable document must still be present in full:\n{}",
            block
        );
    }
}
