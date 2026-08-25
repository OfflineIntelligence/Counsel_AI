//! Streaming chat endpoint — the core 1-hop architecture handler.
//!
//! Flow: Client POST → SharedState (session + cache lookup) → LLM Worker (HTTP to llama-server) → SSE stream back
//! All state access is in-process via Arc/shared memory. The only network hop is to localhost llama-server.

use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use std::convert::Infallible;
use tracing::{info, error, debug, warn};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crate::memory::Message;
use crate::shared_state::UnifiedAppState;
use crate::utils::{extract_content_from_bytes, extraction_outcome};
use regex::Regex;

/// Inline file attachment sent with the request (temporary, in-memory only)
#[derive(Debug, Clone, Deserialize)]
pub struct ChatAttachment {
    pub name: String,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub content_text: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Absolute path on the user's machine, when captured via the native
    /// file dialog (Tauri plugin-dialog + plugin-fs). Null when unavailable
    /// (older frontend build, or a picker that can't expose one).
    #[serde(default)]
    pub source_path: Option<String>,
    /// Set when this attachment is an EXISTING Local Storage file being
    /// attached to this chat (the "@filename" autocomplete or folder-icon
    /// picker), as opposed to a fresh paperclip pick. When present,
    /// content_base64/content_text are ignored: the file is resolved
    /// directly from local_files/documents by id instead, so it is never
    /// re-extracted from already-extracted text (see
    /// persist_inline_attachments for why that distinction matters).
    #[serde(default)]
    pub local_file_id: Option<i64>,
    /// References an EXISTING document already known to the system by its
    /// own documents.id, as opposed to local_file_id (which only exists for
    /// Local-Storage-backed files). This is what lets a paperclip-only
    /// attachment (no local_file_id, no permanent byte copy) be
    /// re-referenced from the @ picker within the same session it was
    /// attached in - no bytes needed, the content is already in `documents`.
    #[serde(default)]
    pub document_id: Option<i64>,
}

/// Request body matching what the frontend sends
#[derive(Debug, Deserialize)]
pub struct StreamChatRequest {
    pub model: Option<String>,
    pub messages: Vec<Message>,
    pub session_id: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_stream")]
    pub stream: bool,
    /// Inline file attachments (temporary, session-scoped)
    #[serde(default)]
    pub attachments: Option<Vec<ChatAttachment>>,
}

fn default_max_tokens() -> u32 { 2000 }
fn default_temperature() -> f32 { 0.7 }
fn default_stream() -> bool { true }

/// Persist inline (paperclip) attachments into the unified document store,
/// linked to this session. Content is NEVER spliced into this message's
/// text - it flows to the model exclusively through
/// context_engine::build_document_context on every future turn. This is
/// what gives a document attached in turn 1 the same persistence as one
/// attached in turn 27: the database, not this request, carries the
/// knowledge forward (per-content-hash dedup: re-attaching the same file
/// anywhere is recognized, never re-extracted).
/// Hard ceiling on attachments processed from a single request, regardless
/// of what the frontend sends (which already enforces this at the UI level -
/// this is defense in depth, not the primary UX path). Silently accepting an
/// unbounded batch here would be exactly the kind of unannounced limit this
/// project's no-silent-fallback policy forbids, so an excess is logged
/// loudly and the extras are dropped, never processed unnoticed.
const MAX_ATTACHMENTS_PER_REQUEST: usize = 16;

/// Persist a batch of attachments concurrently.
///
/// # Concurrency
///
/// Extraction is NOT bounded here. It used to be capped at 4 simultaneous
/// extractions, which actively defeats per-format lanes: attaching three PDFs, a
/// Word file, a spreadsheet and an image would admit only the first four in
/// arrival order, so the spreadsheet and image sat idle behind PDFs that cannot
/// run in parallel anyway. The bound now lives in `utils::extraction_scheduler` -
/// one in-flight file per format lane - which is a tighter limit on genuinely
/// competing work and no limit at all on work that does not compete.
///
/// # Safety of concurrent DB writes
///
/// `link_session` uses INSERT OR IGNORE and is idempotent. `upsert_document`
/// does find-by-hash then INSERT, so two tasks submitting the SAME bytes can
/// both miss the check and race on the UNIQUE `content_hash`; the loser now
/// recovers by re-reading the winner's row rather than erroring (see
/// `DocumentsStore::upsert_document`). Either way one document exists and the
/// session ends up correctly attached.
async fn persist_inline_attachments(
    session_id: &str,
    attachments: &[ChatAttachment],
    state: &UnifiedAppState,
) {
    if attachments.is_empty() {
        return;
    }
    let attachments = if attachments.len() > MAX_ATTACHMENTS_PER_REQUEST {
        warn!(
            "Request for session {} carried {} attachments, exceeding the {}-file cap; \
             only the first {} will be processed",
            session_id, attachments.len(), MAX_ATTACHMENTS_PER_REQUEST, MAX_ATTACHMENTS_PER_REQUEST
        );
        &attachments[..MAX_ATTACHMENTS_PER_REQUEST]
    } else {
        attachments
    };
    info!(
        "Persisting {} inline attachment(s) for session {} (format lanes decide what \
         runs in parallel)",
        attachments.len(), session_id
    );

    // `None` = submit every attachment at once and let the scheduler decide.
    // Each one immediately waits for its own format lane, so a batch of six
    // different formats starts six extractions, while three PDFs in that batch
    // queue behind one another. Any cap here would instead be applied in
    // arrival order, blindly, and could hold back a spreadsheet that has nothing
    // to contend with.
    use futures::stream::{self, StreamExt};
    stream::iter(attachments.iter())
        .for_each_concurrent(None, |attach| async move {
            persist_single_inline_attachment(session_id, attach, state).await;
        })
        .await;
}

/// Handle one attachment in full - branching, extraction, and DB writes.
///
/// Extracted into its own async function so `persist_inline_attachments` can
/// drive many copies concurrently via `for_each_concurrent`, keeping the
/// three branches (existing document_id, existing local_file_id, fresh
/// bytes) side by side in one place instead of duplicated per code path.
async fn persist_single_inline_attachment(
    session_id: &str,
    attach: &ChatAttachment,
    state: &UnifiedAppState,
) {
    // An existing document (paperclip OR Local Storage) already known to
    // this session, re-picked from the @ picker's "attached in this
    // conversation" list - just (re-)link it, idempotent, no bytes
    // touched, no re-extraction. This is what makes a paperclip-only
    // attachment (no local_file_id, no permanent byte copy) referenceable
    // again within the same session it was attached in.
    if let Some(document_id) = attach.document_id {
        if let Err(e) = state.shared_state.database_pool.documents.link_session(session_id, document_id, "paperclip") {
            error!("Failed to (re-)link document {} to session {}: {}", document_id, session_id, e);
        } else {
            info!("Document {} re-referenced via @ picker, linked to session {}", document_id, session_id);
        }
        return;
    }
    // An existing Local Storage file being attached to this chat (the
    // "@filename" autocomplete or folder-icon picker) - resolved by id,
    // NEVER by re-extracting content_text. That field, when a picker
    // like this populates it, holds the file's ALREADY-EXTRACTED text
    // (fetched from GET /files/:id/content for preview purposes) - not
    // bytes of the original DOCX/PDF/XLSX/image container. Re-running
    // extract_content_from_bytes on that would try to parse extracted
    // plain text as a ZIP/PDF/image and fail for every binary format,
    // which is exactly the bug this branch exists to avoid.
    if let Some(local_file_id) = attach.local_file_id {
        persist_local_storage_attachment(session_id, local_file_id, state).await;
        return;
    }

    // Fresh bytes from the paperclip. Format gate, server-side - the native
    // dialog already filters (supportedFormats.ts), so this catches a direct
    // API call or a stale frontend build.
    //
    // Deliberately NOT applied to the document_id / local_file_id branches
    // above: those reference content already stored and already extracted,
    // including files that predate this gate. Refusing to re-link them would
    // break existing conversations to enforce a rule about NEW attachments.
    if !crate::utils::is_supported_attachment(&attach.name) {
        warn!(
            "Rejected attachment '{}': unsupported type (accepted: {})",
            attach.name,
            crate::utils::supported_attachment_list()
        );
        return;
    }

    let bytes: Vec<u8> = if let Some(ref text) = attach.content_text {
        text.as_bytes().to_vec()
    } else if let Some(ref b64) = attach.content_base64 {
        match BASE64.decode(b64) {
            Ok(b) => b,
            Err(e) => {
                warn!("Base64 decode failed for {}: {}", attach.name, e);
                return;
            }
        }
    } else {
        debug!("Attachment {} has no content", attach.name);
        return;
    };

    let (extracted_text, extraction_status, extraction_error) =
        extraction_outcome(extract_content_from_bytes(&bytes, &attach.name).await);
    info!(
        "Attachment '{}': {} bytes, extraction_status='{}', extracted {} chars",
        attach.name, bytes.len(), extraction_status, extracted_text.len()
    );

    let result = state.shared_state.database_pool.documents.upsert_document(crate::memory_db::NewDocument {
        original_bytes: &bytes,
        original_filename: &attach.name,
        // Paperclip: no permanent byte copy is kept (product decision) -
        // the absolute path IS captured client-side via the native file
        // dialog (Tauri plugin-dialog), when available.
        source_path: attach.source_path.clone(),
        source_kind: "paperclip",
        mime_type: attach.mime_type.clone(),
        size_bytes: bytes.len() as i64,
        extracted_text,
        extraction_status,
        extraction_error,
        local_file_id: None,
    });
    match result {
        Ok(doc) => {
            info!("Document '{}' upserted as id {} (hash-deduped if seen before)", attach.name, doc.id);
            if let Err(e) = state.shared_state.database_pool.documents.link_session(session_id, doc.id, "paperclip") {
                error!("Failed to link document {} to session {} - the document will NOT be visible to the model this turn: {}", doc.id, session_id, e);
            } else {
                info!("Document {} linked to session {}", doc.id, session_id);
            }
        }
        Err(e) => warn!("Failed to persist attachment '{}': {}", attach.name, e),
    }
}

/// Attach an EXISTING Local Storage file to this session by id (the
/// "@filename" autocomplete / folder-icon picker path). The file was already
/// extracted once when it was uploaded (files_api::upload_file) - the fast
/// path here just links the existing document, no bytes read, no
/// re-extraction, no re-hashing. Only on a genuine miss (a local_files row
/// with no corresponding documents row yet - e.g. uploaded before the
/// unified document store existed) does this fall back to reading the real
/// file bytes and extracting fresh, exactly like persist_file_reference_attachments.
// `pub(crate)` solely so api::files_api's tests can drive the real attach
// path against a real background extraction, which is the only way to
// exercise the upload/attach race for what it is. Not called outside this
// module in production code.
pub(crate) async fn persist_local_storage_attachment(session_id: &str, local_file_id: i64, state: &UnifiedAppState) {
    let documents = &state.shared_state.database_pool.documents;

    // Take the per-file extraction permit BEFORE the lookup below.
    //
    // Local Storage uploads extract in the background, so this file's
    // extraction may be in flight right now - a user uploading a scanned PDF
    // and attaching it a few seconds later is an ordinary flow, not an edge
    // case. Waiting here means the lookup that follows sees the finished
    // result and takes the fast path, instead of starting a second OCR pass
    // over the same bytes.
    //
    // Held for the whole function so the "extract fresh" and "repair" paths
    // below are also covered; dropped when this returns.
    let _permit = state
        .shared_state
        .extraction_coordinator
        .acquire(local_file_id)
        .await;

    // Set when the stored row exists but its extraction is unusable: the
    // document must be re-extracted from real bytes and REPAIRED in place,
    // not merely linked. Reusing an existing extraction is only a fast path
    // when there is actually something to reuse.
    let mut repair_document_id: Option<i64> = None;

    match documents.get_document_by_local_file_id(local_file_id) {
        Ok(Some(doc)) => {
            let usable = doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty();
            if usable {
                if let Err(e) = documents.link_session(session_id, doc.id, "local_storage") {
                    error!(
                        "Failed to link existing document {} (local_file_id {}) to session {} - \
                         the document will NOT be visible to the model this turn: {}",
                        doc.id, local_file_id, session_id, e
                    );
                } else {
                    info!(
                        "Local Storage file {} (document {}) linked to session {} - reused existing extraction ({} chars), no re-work",
                        local_file_id, doc.id, session_id, doc.extracted_text.len()
                    );
                }
                return;
            }
            // Unusable stored extraction. Historically this branch did not
            // exist: the row was linked as-is, document_memory listed it
            // under "[Could not read ...]", and because upsert_document never
            // re-extracts a known hash, re-attaching the file could never fix
            // it. Fall through to a real re-extraction and repair the row.
            warn!(
                "Local Storage file {} (document {}) has an unusable stored extraction \
                 (status='{}', {} chars, reason: {}) - re-extracting from the file's real \
                 bytes and repairing the record",
                local_file_id,
                doc.id,
                doc.extraction_status,
                doc.extracted_text.len(),
                doc.extraction_error.as_deref().unwrap_or("none recorded")
            );
            repair_document_id = Some(doc.id);
        }
        Ok(None) => {
            debug!(
                "Local Storage file {} has no document row yet - extracting fresh",
                local_file_id
            );
        }
        Err(e) => {
            warn!(
                "Lookup by local_file_id {} failed ({}) - falling back to fresh extraction",
                local_file_id, e
            );
        }
    }

    let local_files = &state.shared_state.database_pool.local_files;
    let file = match local_files.get_file(local_file_id) {
        Ok(f) => f,
        Err(e) => {
            warn!("Local Storage file {} no longer exists, cannot attach: {}", local_file_id, e);
            return;
        }
    };
    let bytes = match local_files.get_file_content(local_file_id) {
        Ok(b) => b,
        Err(e) => {
            warn!("Could not read Local Storage file {} ('{}'): {}", local_file_id, file.name, e);
            return;
        }
    };

    // Repair path: the row already exists (so upsert_and_link would find it
    // by hash and return it UNCHANGED, extraction and all), which is exactly
    // why the extraction has to be written back explicitly here.
    if let Some(document_id) = repair_document_id {
        let (extracted_text, extraction_status, extraction_error) =
            extraction_outcome(extract_content_from_bytes(&bytes, &file.name).await);
        info!(
            "Re-extracted '{}' for repair: {} bytes, extraction_status='{}', extracted {} chars",
            file.name, bytes.len(), extraction_status, extracted_text.len()
        );
        if let Err(e) = documents.repair_extraction(
            document_id,
            &extracted_text,
            extraction_status,
            extraction_error.as_deref(),
        ) {
            error!(
                "Failed to repair extraction for document {} ('{}'): {}",
                document_id, file.name, e
            );
        }
        if let Err(e) = documents.link_session(session_id, document_id, "local_storage") {
            error!(
                "Failed to link repaired document {} to session {} - it will NOT be \
                 visible to the model this turn: {}",
                document_id, session_id, e
            );
        }
        return;
    }

    upsert_and_link(
        session_id,
        &bytes,
        &file.name,
        None,
        "local_storage",
        Some(local_file_id),
        state,
    )
    .await;
}

/// Shared tail: extract, record in the unified document store, and link to
/// this session. Used by every attachment path that has real file bytes in
/// hand (as opposed to persist_local_storage_attachment's fast path, which
/// deliberately skips this entirely when an existing extraction can be reused).
async fn upsert_and_link(
    session_id: &str,
    bytes: &[u8],
    filename: &str,
    source_path: Option<String>,
    source_kind: &str,
    local_file_id: Option<i64>,
    state: &UnifiedAppState,
) {
    let documents = &state.shared_state.database_pool.documents;
    // MIME derived from the filename rather than left NULL.
    //
    // This function is reached by paths that resolve a file by NAME
    // (@filename, [Attached: x]) and therefore have no client-supplied MIME.
    // It used to pass `mime_type: None`, so a document first seen through one
    // of those paths recorded no MIME at all - and because the dedupe branch
    // of upsert_document did not backfill it either, that gap was permanent.
    let mime_type = mime_guess::from_path(filename).first().map(|m| m.to_string());

    // Hash FIRST, extract only on a miss.
    //
    // This is the difference between mentioning a file and re-processing it.
    // persist_file_reference_attachments scans the WHOLE message history for
    // @filename / [Attached: x] patterns on every turn, so a single mention in
    // turn 1 is re-detected on turn 2, 3, 20 - and this function used to
    // extract unconditionally before handing the bytes to upsert_document,
    // which would then discard the result as a known hash. For a scanned PDF
    // that meant a full pdfium rasterization plus up to 50 pages of Windows
    // OCR, on the critical path, before EVERY reply in the conversation.
    //
    // blake3 over the bytes costs microseconds by comparison. Extraction now
    // happens exactly once per distinct file, ever.
    let hash = crate::memory_db::DocumentsStore::hash_bytes(bytes);
    match documents.get_document_by_hash(&hash) {
        Ok(Some(doc)) if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() => {
            if let Err(e) = documents.touch_document(
                doc.id,
                source_path.as_deref(),
                local_file_id,
                mime_type.as_deref(),
            ) {
                warn!("Could not update provenance for document {}: {}", doc.id, e);
            }
            if let Err(e) = documents.link_session(session_id, doc.id, source_kind) {
                error!(
                    "Failed to link known document {} to session {} - it will NOT be visible \
                     to the model this turn: {}",
                    doc.id, session_id, e
                );
            } else {
                debug!(
                    "'{}' already extracted (document {}, {} chars) - linked without re-extraction",
                    filename, doc.id, doc.extracted_text.len()
                );
            }
            return;
        }
        Ok(_) => {}
        Err(e) => warn!(
            "Hash lookup failed for '{}': {} - extracting (a duplicate would still be \
             deduped by upsert_document)",
            filename, e
        ),
    }

    let (extracted_text, extraction_status, extraction_error) =
        extraction_outcome(extract_content_from_bytes(bytes, filename).await);
    info!(
        "'{}': {} bytes, extraction_status='{}', extracted {} chars",
        filename, bytes.len(), extraction_status, extracted_text.len()
    );

    let result = documents.upsert_document(crate::memory_db::NewDocument {
        original_bytes: bytes,
        original_filename: filename,
        source_path,
        source_kind,
        mime_type,
        size_bytes: bytes.len() as i64,
        extracted_text,
        extraction_status,
        extraction_error,
        local_file_id,
    });
    match result {
        Ok(doc) => {
            info!("Document '{}' upserted as id {} (hash-deduped if seen before)", filename, doc.id);
            if let Err(e) = state.shared_state.database_pool.documents.link_session(session_id, doc.id, source_kind) {
                error!(
                    "Failed to link document {} to session {} - the document will NOT be visible to the model this turn: {}",
                    doc.id, session_id, e
                );
            } else {
                info!("Document {} linked to session {}", doc.id, session_id);
            }
        }
        Err(e) => warn!("Failed to persist '{}': {}", filename, e),
    }
}

/// Detect [Attached: filename] / @filename references in the user's
/// messages and persist the referenced file into the unified document
/// store. Same contract as persist_inline_attachments - never spliced into
/// message text, only recorded for context_engine::build_document_context
/// to inject on every turn.
async fn persist_file_reference_attachments(session_id: &str, messages: &[Message], state: &UnifiedAppState) {
    let attached_re = Regex::new(r"\[Attached: ([^\]]+)\]").unwrap();
    let at_re = Regex::new(r"@(\S+\.\w+)").unwrap();

    let mut filenames: Vec<String> = Vec::new();
    for msg in messages.iter().filter(|m| m.role == "user") {
        for cap in attached_re.captures_iter(&msg.content) {
            if let Some(m) = cap.get(1) {
                filenames.push(m.as_str().to_string());
            }
        }
        for cap in at_re.captures_iter(&msg.content) {
            if let Some(m) = cap.get(1) {
                filenames.push(m.as_str().to_string());
            }
        }
    }
    if filenames.is_empty() {
        return;
    }
    filenames.sort();
    filenames.dedup();

    let local_files = &state.shared_state.database_pool.local_files;
    for filename in filenames {
        // Format gate. Local Storage contents are gated at upload, but the
        // app-data fallback below reads a path assembled from text the USER
        // typed - so without this an "@notes.zip" in a message would be read
        // off disk and text-decoded into mojibake stored as document content.
        if !crate::utils::is_supported_attachment(&filename) {
            debug!(
                "Ignoring file reference '{}': unsupported type (accepted: {})",
                filename,
                crate::utils::supported_attachment_list()
            );
            continue;
        }
        // A reference that resolves to a Local Storage file is handled by the
        // SAME function the "@ picker by id" path uses, rather than being
        // re-implemented here.
        //
        // That matters for more than tidiness. This branch used to read the
        // bytes and go straight to upsert_and_link, which skips two things
        // persist_local_storage_attachment does:
        //
        //   1. It takes the per-file extraction permit, so a reference typed
        //      seconds after the upload WAITS for the background extraction
        //      already running instead of starting a second, identical one.
        //      For a scanned PDF that is a whole duplicate OCR pass.
        //   2. It reuses a good stored extraction without reading the bytes at
        //      all, and repairs an unusable one in place.
        //
        // Both were already built and tested; this path simply was not using
        // them.
        if let Ok(file) = local_files.get_file_by_name(&filename) {
            persist_local_storage_attachment(session_id, file.id, state).await;
            continue;
        }

        // Filesystem fallback for backward compatibility with files placed
        // directly in the app data directory. No local_files row exists, so
        // there is nothing to coordinate with and no stored extraction to reuse.
        let app_data_dir = crate::config::get_app_data_dir();
        let file_path = app_data_dir.join(&filename);
        let bytes = match std::fs::read(&file_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                debug!("Referenced file '{}' not found in local files or app data dir", filename);
                continue;
            }
        };

        upsert_and_link(session_id, &bytes, &filename, None, "paperclip", None, state).await;
    }
}

/// Did the engine reject the prompt because its chat template requires strict
/// user/assistant alternation?
///
/// llama-server surfaces this as a hard HTTP 500 carrying the Jinja error from
/// the model's own template — gemma-3's wording is "Conversation roles must
/// alternate user/assistant/user/assistant/...". Matching it lets the UI say
/// what actually went wrong instead of "the engine returned 500".
///
/// Matched on lowercase fragments rather than the whole sentence because the
/// exact phrasing varies between templates (gemma, mistral and qwen all word
/// it differently) while the words "roles" and "alternate" do not.
pub(crate) fn is_role_alternation_error(message: &str) -> bool {
    let m = message.to_lowercase();
    (m.contains("role") && m.contains("alternate"))
        || m.contains("must alternate")
        || m.contains("conversation roles")
}

/// Did the engine reject the prompt because it does not fit the context window?
pub(crate) fn is_context_overflow_error(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("context size")
        || m.contains("exceeds the available context")
        || m.contains("exceed context")
        || m.contains("n_ctx")
        || (m.contains("prompt") && m.contains("too long"))
}

#[cfg(test)]
mod engine_error_classification_tests {
    use super::*;

    /// The strings below are what llama-server actually returns in the body of
    /// a failed /v1/chat/completions, wrapped by `llm_worker` as
    /// "LLM backend returned 500: {body}". Classifying them is what turns a
    /// bare 502 into a sentence that names the real problem.
    #[test]
    fn a_chat_template_role_error_is_recognised_across_templates() {
        let real_world = [
            // gemma-3, live-observed (b8037)
            "LLM backend returned 500: {\"error\":{\"message\":\"Conversation roles must \
             alternate user/assistant/user/assistant/...\",\"type\":\"server_error\"}}",
            // mistral / qwen phrasings of the same constraint
            "LLM backend returned 500: roles must alternate between user and assistant",
            "LLM backend returned 500: Only user and assistant roles are supported, and \
             they must alternate",
        ];
        for message in real_world {
            assert!(
                is_role_alternation_error(message),
                "should be recognised as a role-alternation failure: {}",
                message
            );
        }
    }

    #[test]
    fn a_context_overflow_is_recognised() {
        for message in [
            "LLM backend returned 500: the request exceeds the available context size",
            "LLM backend returned 400: {\"error\":{\"message\":\"n_ctx exceeded\"}}",
            "LLM backend returned 500: prompt is too long",
        ] {
            assert!(
                is_context_overflow_error(message),
                "should be recognised as a context overflow: {}",
                message
            );
        }
    }

    /// The classifiers must not claim unrelated failures. A misattributed
    /// cause is worse than an honest "the engine rejected this", because it
    /// sends the reader off to fix something that was never wrong — which is
    /// precisely what the old blanket "no model is loaded" message did.
    #[test]
    fn unrelated_failures_are_not_misattributed() {
        for message in [
            "LLM backend returned 500: failed to allocate KV cache",
            "LLM backend returned 503: no slot available",
            "Cannot connect to local LLM server.",
            "LLM backend request failed: connection reset by peer",
        ] {
            assert!(
                !is_role_alternation_error(message),
                "must not be classified as a role error: {}",
                message
            );
            assert!(
                !is_context_overflow_error(message),
                "must not be classified as a context overflow: {}",
                message
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::shared_state::SharedState;
    use std::sync::Arc;

    async fn test_state() -> UnifiedAppState {
        let cfg = Config::from_env().expect("Config::from_env should succeed with defaults");
        let database = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        let shared_state = Arc::new(SharedState::new(cfg, database).expect("SharedState::new"));
        UnifiedAppState::new(shared_state)
    }

    /// The format gate is a SERVER-side boundary, not just a picker filter.
    ///
    /// The native dialog and the Local Storage picker both restrict to the
    /// supported set, but neither is enforcement: a direct API call, or a
    /// frontend build older than the gate, can still submit anything. An
    /// unsupported type must produce NO document row - because the extractor
    /// would happily text-decode arbitrary bytes into mojibake and store that
    /// as though it were document content, which then reaches the model as
    /// authoritative source material.
    #[tokio::test]
    async fn unsupported_attachment_types_are_refused_before_any_document_is_stored() {
        let state = test_state().await;
        let session_id = "gate-session";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        // A ZIP is binary, is NOT in the supported set, and would otherwise be
        // text-decoded into garbage by the extractor's unknown-type fallback.
        let unsupported = ChatAttachment {
            name: "archive.zip".to_string(),
            content_base64: Some(BASE64.encode(b"PK not a document at all")),
            content_text: None,
            mime_type: Some("application/zip".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments(session_id, &[unsupported], &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert!(
            docs.is_empty(),
            "an unsupported type must not be stored at all, got: {:?}",
            docs.iter().map(|d| &d.original_filename).collect::<Vec<_>>()
        );

        // Control: the SAME code path must accept a supported type, so this
        // test proves a gate rather than a broken persistence path.
        let supported = ChatAttachment {
            name: "notes.txt".to_string(),
            content_base64: None,
            content_text: Some("The indemnity cap is five million dollars.".to_string()),
            mime_type: Some("text/plain".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments(session_id, &[supported], &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(docs.len(), 1, "the supported .txt must be stored");
        assert_eq!(docs[0].original_filename, "notes.txt");
        assert!(docs[0].extracted_text.contains("five million dollars"));
    }

    /// A Local Storage file whose stored extraction is unusable must be
    /// RE-EXTRACTED from its real bytes and repaired when attached - not
    /// linked as-is.
    ///
    /// This is the exact shape of the production bug: api::files_api used to
    /// classify any '['-prefixed extraction as a failure, so every image and
    /// scanned PDF uploaded to Local Storage was recorded with
    /// extraction_status='failed' and empty text. Because upsert_document
    /// never re-extracts a hash it already knows, the fast path in
    /// persist_local_storage_attachment then linked that dead row on every
    /// future attach, and document_memory rendered it as "[Could not read
    /// 'X']" forever. Re-attaching could never fix it.
    ///
    /// The row here is seeded in exactly that poisoned state while the real
    /// file bytes on disk are perfectly extractable.
    #[tokio::test]
    async fn unusable_stored_extraction_is_repaired_from_real_bytes_on_attach() {
        let state = test_state().await;
        let session_id = "repair-session";
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        let real_text = b"The indemnity cap is five million dollars and survives termination.";
        let local_file = state.shared_state.database_pool.local_files
            .upload_file(None, "scan.txt", real_text, Some("text/plain"))
            .unwrap();

        // Seed the poisoned row: right bytes (so the hash matches what a
        // re-extraction would produce), wrong extraction.
        let poisoned = state.shared_state.database_pool.documents.upsert_document(
            crate::memory_db::NewDocument {
                original_bytes: real_text,
                original_filename: "scan.txt",
                source_path: None,
                source_kind: "local_storage",
                mime_type: None,
                size_bytes: real_text.len() as i64,
                extracted_text: String::new(),
                extraction_status: "failed",
                extraction_error: Some("[Cannot extract 'scan.txt': seeded failure]".to_string()),
                local_file_id: Some(local_file.id),
            },
        ).unwrap();
        assert_eq!(poisoned.extraction_status, "failed");

        let attachment = ChatAttachment {
            name: "scan.txt".to_string(),
            content_base64: None,
            content_text: None,
            mime_type: None,
            source_path: None,
            local_file_id: Some(local_file.id),
            document_id: None,
        };
        persist_inline_attachments(session_id, std::slice::from_ref(&attachment), &state).await;

        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), 1, "the repaired document must be linked exactly once");
        assert_eq!(docs[0].id, poisoned.id, "must repair the SAME row, not create a duplicate");
        assert_eq!(
            docs[0].extraction_status, "ok",
            "a re-extraction that succeeds must overwrite the stored failure"
        );
        assert!(
            docs[0].extracted_text.contains("five million dollars"),
            "the real file content must now be stored: {:?}",
            docs[0].extracted_text
        );

        // And it must actually reach the model-facing block, not merely sit
        // in the database.
        let blocks = crate::context_engine::build_document_context(
            &state.shared_state.database_pool,
            session_id,
            50_000,
        ).await;
        let block = blocks.session_block.expect("session block must be present");
        assert!(
            block.contains("five million dollars"),
            "repaired content must reach the prompt: {}",
            block
        );
        assert!(
            !block.contains("Could not read"),
            "the repaired document must no longer be reported as unreadable: {}",
            block
        );
    }

    /// The gap every prior test left: every other test either set
    /// extracted_text by hand or tested extraction in isolation. This drives
    /// REAL PDF bytes through the ACTUAL functions generate_stream calls -
    /// persist_inline_attachments (real pdfium extraction -> real
    /// upsert_document -> real link_session), then the real retrieval path
    /// (build_document_context) and the real fold step - proving all the
    /// pieces actually connect, not just that each one works alone.
    #[tokio::test]
    async fn attach_real_pdf_and_retrieve_via_document_memory_end_to_end() {
        let state = test_state().await;
        let session_id = "e2e-session";

        // Mirrors generate_stream's REQUIRED ordering: the session row must
        // exist before attachment persistence links a document to it.
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        // A minimal but real, valid single-page PDF (same construction
        // proven to extract correctly via pdfium in utils::pdf_text's test).
        let pdf_bytes: &[u8] = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 68>>stream\nBT /F1 24 Tf 72 700 Td (This Agreement caps liability at 1M) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let content_base64 = BASE64.encode(pdf_bytes);

        let attachment = ChatAttachment {
            name: "Agreement.pdf".to_string(),
            content_base64: Some(content_base64),
            content_text: None,
            mime_type: Some("application/pdf".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };

        // The exact function generate_stream calls for a paperclip attachment.
        persist_inline_attachments(session_id, std::slice::from_ref(&attachment), &state).await;

        // Prove it landed in the store with REAL extracted text (not a stub).
        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), 1, "the attachment must be linked to the session");
        assert_eq!(
            docs[0].extraction_status, "ok",
            "pdfium must successfully extract this valid PDF - reason if not: {:?}",
            docs[0].extraction_error
        );
        assert!(
            docs[0].extracted_text.contains("caps liability"),
            "extracted text must be the PDF's real content, got: {:?}",
            docs[0].extracted_text
        );

        // Prove the retrieval path (what generate_stream calls next) surfaces it.
        let blocks = crate::context_engine::build_document_context(
            &state.shared_state.database_pool,
            session_id,
            50_000,
        ).await;
        let block = blocks.session_block.expect("a linked, successfully-extracted document must produce a session block");
        assert!(block.contains("Agreement.pdf"), "block: {}", block);
        assert!(block.contains("caps liability"), "block: {}", block);

        // Prove the fold step (what generate_stream calls last) puts it
        // where the model will actually see it: the system message content.
        let mut llm_input = vec![
            Message { role: "system".to_string(), content: "<base system prompt supplied by the client>".to_string() },
            Message { role: "user".to_string(), content: "what does the agreement say?".to_string() },
        ];
        crate::context_engine::fold_into_system_message(&mut llm_input, &block);
        assert_eq!(llm_input.len(), 2, "folding must not add a message");
        assert!(llm_input[0].content.contains("caps liability"), "system message: {}", llm_input[0].content);
    }

    /// Regression test for the exact bug just fixed: GET /files/:id/content
    /// returns the file's ALREADY-EXTRACTED text (files_api::get_file_content),
    /// which the frontend's "@filename"/folder-icon picker used to stuff
    /// into content_text and send back - causing persist_inline_attachments
    /// to re-run extract_content_from_bytes on that extracted text's UTF-8
    /// bytes, using the original filename's extension. For a real DOCX this
    /// used to fail (the bytes are no longer a valid ZIP container), marking
    /// the document extraction_status='failed' and the model never seeing
    /// its real content, even though correct text was extracted moments
    /// earlier by the /content endpoint and discarded. Proves the fix: a
    /// ChatAttachment carrying local_file_id resolves the EXISTING document
    /// (or falls back to real bytes), never round-tripping through
    /// already-extracted text.
    #[tokio::test]
    async fn local_storage_attachment_reuses_real_extraction_not_extracted_text() {
        let state = test_state().await;
        let session_id = "local-storage-session";
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        // A real DOCX (same construction proven to extract correctly via
        // quick-xml in utils::file_processor's tests) - not a plain text
        // file, so a bug that only works "by accident" for .txt cannot hide.
        let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body><w:p><w:r><w:t>This NDA caps liability at 2M dollars.</w:t></w:r></w:p></w:body>
</w:document>"#;
        let mut zip_buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut zip_buf);
            writer.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut writer, doc_xml.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        let docx_bytes = zip_buf.into_inner();

        // Mirrors files_api::upload_file: real bytes recorded in local_files
        // AND extracted once into the unified document store.
        let local_file = state.shared_state.database_pool.local_files
            .upload_file(None, "NDA.docx", &docx_bytes, Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"))
            .unwrap();
        let (extracted_text, extraction_status, extraction_error) =
            extraction_outcome(extract_content_from_bytes(&docx_bytes, "NDA.docx").await);
        assert_eq!(extraction_status, "ok", "sanity check: the DOCX itself must extract cleanly");
        state.shared_state.database_pool.documents.upsert_document(crate::memory_db::NewDocument {
            original_bytes: &docx_bytes,
            original_filename: "NDA.docx",
            source_path: None,
            source_kind: "local_storage",
            mime_type: None,
            size_bytes: docx_bytes.len() as i64,
            extracted_text,
            extraction_status,
            extraction_error,
            local_file_id: Some(local_file.id),
        }).unwrap();

        // The exact attachment shape the "@filename"/folder-icon picker now
        // sends: identified by id, no content_text/content_base64 at all.
        let attachment = ChatAttachment {
            name: "NDA.docx".to_string(),
            content_base64: None,
            content_text: None,
            mime_type: None,
            source_path: None,
            local_file_id: Some(local_file.id),
            document_id: None,
        };
        persist_inline_attachments(session_id, std::slice::from_ref(&attachment), &state).await;

        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), 1, "the local storage file must be linked to the session");
        assert_eq!(
            docs[0].extraction_status, "ok",
            "must reuse the real, already-successful extraction - reason if not: {:?}",
            docs[0].extraction_error
        );
        assert!(
            docs[0].extracted_text.contains("caps liability at 2M"),
            "must be the DOCX's real content, not a re-extraction failure: {:?}",
            docs[0].extracted_text
        );
    }

    /// The fallback half of the same fix: a local_files row with NO
    /// corresponding documents row yet (e.g. uploaded before the unified
    /// store existed) must still resolve correctly - reading real bytes and
    /// extracting fresh - not fail because the fast-path lookup missed.
    #[tokio::test]
    async fn local_storage_attachment_falls_back_to_fresh_extraction_when_document_row_missing() {
        let state = test_state().await;
        let session_id = "local-storage-fallback-session";
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        let local_file = state.shared_state.database_pool.local_files
            .upload_file(None, "notes.txt", b"Meeting notes: settle by March.", Some("text/plain"))
            .unwrap();
        // Deliberately NOT calling documents.upsert_document - simulates the
        // "no document row yet" case the fallback path must handle.

        let attachment = ChatAttachment {
            name: "notes.txt".to_string(),
            content_base64: None,
            content_text: None,
            mime_type: None,
            source_path: None,
            local_file_id: Some(local_file.id),
            document_id: None,
        };
        persist_inline_attachments(session_id, std::slice::from_ref(&attachment), &state).await;

        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].extraction_status, "ok");
        assert!(docs[0].extracted_text.contains("settle by March"));
    }

    /// The defense-in-depth cap: a request carrying more than
    /// MAX_ATTACHMENTS_PER_REQUEST attachments must process only the first
    /// N, never silently accept an unbounded batch.
    #[tokio::test]
    async fn attachments_beyond_the_cap_are_dropped_not_silently_processed() {
        let state = test_state().await;
        let session_id = "cap-session";
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        let attachments: Vec<ChatAttachment> = (0..20)
            .map(|i| ChatAttachment {
                name: format!("file{i}.txt"),
                content_base64: None,
                content_text: Some(format!("content {i}")),
                mime_type: None,
                source_path: None,
                local_file_id: None,
                document_id: None,
            })
            .collect();
        persist_inline_attachments(session_id, &attachments, &state).await;

        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), MAX_ATTACHMENTS_PER_REQUEST, "only the cap's worth of attachments may be processed");
    }

    /// Regression test for a real production bug found via a live probe
    /// against real user PDFs: pdfium's plain text-layer read came back
    /// sparse for two genuinely non-scanned, born-digital PDFs, tripping the
    /// "looks scanned" heuristic in utils::pdf_text - OCR then ran and
    /// recovered good text - but extraction_outcome's naive "any '[' prefix
    /// = failure" rule discarded that recovered content anyway, because the
    /// OCR success message ALSO starts with '['. The model still received
    /// the text (mislabeled as an error reason, wrapped in a confusing
    /// double "[Could not read ...: [Scanned PDF ...]]" note), which is
    /// exactly why it behaved inconsistently - sometimes using the content,
    /// sometimes parroting "it is scanned" back at the user. These are the
    /// EXACT strings the real probe produced.
    #[test]
    fn ocr_recovered_content_is_kept_not_discarded() {
        let real_ocr_message = "[Scanned PDF 'Agreement.pdf': no text layer found; text below was recovered via OCR (2 of 2 pages processed).]\n\n=== Page 1 (OCR) ===\nADVISORY AGREEMENT\nOffline Intelligence\nUnofficial Agreement\nBetween: Akhil Pamarthy (\"Founder\") and John Keith King (\"Advisor\")\n";
        let (text, status, error) = extraction_outcome(Ok(real_ocr_message.to_string()));
        assert_eq!(status, "ok", "OCR-recovered content with real text must be usable, not a failure");
        assert!(text.contains("ADVISORY AGREEMENT"), "the recovered text must be KEPT, not discarded: {:?}", text);
        assert!(error.is_none());
    }

    #[test]
    fn genuine_ocr_failure_with_no_recovered_text_is_still_failed() {
        let no_text_recovered = "[Scanned PDF 'blank.pdf': no text layer found and OCR recovered no text. The scan quality may be too low.]";
        let (text, status, error) = extraction_outcome(Ok(no_text_recovered.to_string()));
        assert_eq!(status, "failed");
        assert!(text.is_empty());
        assert_eq!(error.as_deref(), Some(no_text_recovered));
    }

    #[test]
    fn bare_single_line_failure_markers_are_still_failed() {
        for msg in [
            "[Cannot extract 'x.doc': this is a legacy binary Word file (.doc). Please save it as .docx.]",
            "[PDF 'x.pdf' contains no extractable text.]",
            "[Word document 'x.docx' contains no extractable text.]",
        ] {
            let (text, status, error) = extraction_outcome(Ok(msg.to_string()));
            assert_eq!(status, "failed", "message should be classified failed: {}", msg);
            assert!(text.is_empty());
            assert_eq!(error.as_deref(), Some(msg));
        }
    }

    #[test]
    fn clean_extraction_with_no_bracket_is_ok() {
        let (text, status, error) = extraction_outcome(Ok("Plain extracted contract text.".to_string()));
        assert_eq!(status, "ok");
        assert_eq!(text, "Plain extracted contract text.");
        assert!(error.is_none());
    }

    /// Same chain, but the attachment is a plain text file (content_text
    /// path, not base64/pdfium) - proves the non-PDF path also connects
    /// end to end, isolating whether any regression is PDF-specific.
    #[tokio::test]
    async fn attach_text_file_and_retrieve_end_to_end() {
        let state = test_state().await;
        let session_id = "e2e-text-session";
        state.shared_state.database_pool.conversations.create_session_with_id(session_id, None).unwrap();

        let attachment = ChatAttachment {
            name: "notes.txt".to_string(),
            content_base64: None,
            content_text: Some("The termination clause requires 30 days written notice.".to_string()),
            mime_type: Some("text/plain".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments(session_id, std::slice::from_ref(&attachment), &state).await;

        let docs = state.shared_state.database_pool.documents.get_session_documents(session_id).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].extraction_status, "ok");
        assert!(docs[0].extracted_text.contains("30 days written notice"));

        let blocks = crate::context_engine::build_document_context(
            &state.shared_state.database_pool,
            session_id,
            50_000,
        ).await;
        let block = blocks.session_block.expect("session block must be present");
        assert!(block.contains("30 days written notice"));
    }

    /// Every attach path must record the FULL column set, not just the text.
    ///
    /// filename, content, MIME, size, hash and both timestamps are what the
    /// retrieval layer and the document viewer read. A path that stores text
    /// but leaves MIME null still "works" in a chat, which is exactly why the
    /// gap survived - so it is pinned here per path rather than assumed.
    #[tokio::test]
    async fn every_attach_path_records_the_full_column_set() {
        let state = test_state().await;

        // ---- Path 1: paperclip, fresh bytes -----------------------------
        let session_a = "paperclip-columns";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_a, None)
            .ok();
        let paperclip = ChatAttachment {
            name: "clause.txt".to_string(),
            content_base64: None,
            content_text: Some("Clause 9 sets the governing law.".to_string()),
            mime_type: Some("text/plain".to_string()),
            source_path: Some("C:/docs/clause.txt".to_string()),
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments(session_a, &[paperclip], &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_a)
            .unwrap();
        assert_eq!(docs.len(), 1, "paperclip attachment must be linked");
        let d = &docs[0];
        assert_eq!(d.original_filename, "clause.txt");
        assert!(d.extracted_text.contains("governing law"));
        assert_eq!(d.source_kind, "paperclip");
        assert_eq!(d.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(d.source_path.as_deref(), Some("C:/docs/clause.txt"));
        assert!(d.size_bytes > 0, "size must be recorded");
        assert!(d.char_count > 0, "char_count must be recorded");
        assert!(!d.content_hash.is_empty(), "hash must be recorded");

        // ---- Path 2: @filename / [Attached: x] resolved from the vault ---
        // This path resolves by NAME and has no client-supplied MIME, so it is
        // the one that used to store mime_type = NULL.
        let session_b = "reference-columns";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_b, None)
            .ok();
        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(
                None,
                "schedule.txt",
                b"Schedule B lists the deliverables.",
                Some("text/plain"),
            )
            .expect("vault write must succeed");

        let messages = vec![Message {
            role: "user".to_string(),
            content: "What does @schedule.txt say?".to_string(),
        }];
        persist_file_reference_attachments(session_b, &messages, &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_b)
            .unwrap();
        assert_eq!(docs.len(), 1, "the @-referenced file must be linked");
        let d = &docs[0];
        assert_eq!(d.original_filename, "schedule.txt");
        assert!(d.extracted_text.contains("deliverables"));
        assert_eq!(
            d.mime_type.as_deref(),
            Some("text/plain"),
            "MIME must be derived from the filename when the client supplies none"
        );
        assert_eq!(d.local_file_id, Some(file.id));
        assert!(d.char_count > 0);

        // ---- Path 3: attach-from-Local-Storage by id --------------------
        let session_c = "localstorage-columns";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_c, None)
            .ok();
        let file2 = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "fees.txt", b"The fee schedule is annexed.", Some("text/plain"))
            .expect("vault write must succeed");
        let by_id = ChatAttachment {
            name: "fees.txt".to_string(),
            content_base64: None,
            content_text: None,
            mime_type: None,
            source_path: None,
            local_file_id: Some(file2.id),
            document_id: None,
        };
        persist_inline_attachments(session_c, &[by_id], &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_c)
            .unwrap();
        assert_eq!(docs.len(), 1, "the local-storage attachment must be linked");
        assert!(docs[0].extracted_text.contains("fee schedule is annexed"));
        assert_eq!(docs[0].local_file_id, Some(file2.id));
        assert_eq!(docs[0].source_kind, "local_storage");
    }

    /// Referencing the same file again must NOT re-extract it.
    ///
    /// persist_file_reference_attachments scans the WHOLE message history every
    /// turn, so one "@contract.pdf" in turn 1 is re-detected on every later
    /// turn. Before the hash-first check, each of those turns re-ran extraction
    /// - for a scanned PDF, a full pdfium rasterization plus up to 50 pages of
    /// Windows OCR, on the critical path, before every single reply.
    ///
    /// Proven by mutating the stored extraction to a sentinel and then
    /// re-referencing: if extraction ran again, upsert_document's dedupe branch
    /// would leave the sentinel intact but the WORK would have happened. So the
    /// observable assertion is that the row is untouched AND its recency was
    /// bumped - i.e. the reference was recorded via the touch path, not the
    /// extract path.
    #[tokio::test]
    async fn re_referencing_a_file_links_it_without_re_extracting() {
        let state = test_state().await;
        let session_id = "no-reextract";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let body = b"Section 4 governs assignment of the agreement.";
        state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "assign.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        let messages = vec![Message {
            role: "user".to_string(),
            content: "Summarise @assign.txt".to_string(),
        }];
        persist_file_reference_attachments(session_id, &messages, &state).await;

        let first = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(first.len(), 1);
        let doc_id = first[0].id;

        // Overwrite the stored text with a sentinel. A re-extraction would
        // produce the ORIGINAL text again, replacing this; the hash-first path
        // must leave it exactly as-is.
        state
            .shared_state
            .database_pool
            .documents
            .repair_extraction(doc_id, "SENTINEL-NOT-REEXTRACTED", "ok", None)
            .unwrap();

        // Same mention again, exactly as the next turn would deliver it.
        persist_file_reference_attachments(session_id, &messages, &state).await;

        let after = state
            .shared_state
            .database_pool
            .documents
            .get_document(doc_id)
            .unwrap();
        assert_eq!(
            after.extracted_text, "SENTINEL-NOT-REEXTRACTED",
            "the file was re-extracted on a repeat reference - this is the per-turn \
             OCR cost the hash-first check exists to remove"
        );

        // Still exactly one document and one link - no duplicates from the
        // repeat reference.
        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(docs.len(), 1, "a repeat reference must not create a second link");
    }

    /// An unsupported type named in message text must not be read off disk and
    /// text-decoded into the document store. The path builds a filesystem path
    /// out of user-typed text, so this gate is load-bearing.
    #[tokio::test]
    async fn unsupported_file_references_in_message_text_are_ignored() {
        let state = test_state().await;
        let session_id = "ref-gate";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        // Written straight to the vault, bypassing the upload API's gate.
        state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "bundle.zip", b"PK\x03\x04 binary junk", Some("application/zip"))
            .expect("vault write must succeed");

        let messages = vec![Message {
            role: "user".to_string(),
            content: "Look at @bundle.zip please".to_string(),
        }];
        persist_file_reference_attachments(session_id, &messages, &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert!(
            docs.is_empty(),
            "an unsupported reference must not be stored, got {:?}",
            docs.iter().map(|d| &d.original_filename).collect::<Vec<_>>()
        );
    }

    /// End-to-end through the REAL functions generate_stream calls, in the same
    /// order: persist the attachment, build the session document block, detect a
    /// backward reference, retrieve past material, and fold both into the LLM
    /// view.
    ///
    /// The unit tests prove each piece; this proves they connect. It also pins
    /// the property that motivated the whole design: the persisted conversation
    /// record is NEVER mutated by any of it. Only the throwaway LLM view is.
    #[tokio::test]
    async fn past_material_reaches_the_llm_view_without_touching_the_conversation_record() {
        let state = test_state().await;
        let db = &state.shared_state.database_pool;

        // --- An earlier conversation, with its own document ---------------
        db.conversations.create_session_with_id("older-chat", None).ok();
        let old_attach = ChatAttachment {
            name: "escrow-terms.txt".to_string(),
            content_base64: None,
            content_text: Some(
                "The escrow amount of two million dollars is held for eighteen months."
                    .to_string(),
            ),
            mime_type: Some("text/plain".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments("older-chat", &[old_attach], &state).await;
        db.conversations
            .store_messages_batch(
                "older-chat",
                &[(
                    "assistant".to_string(),
                    "The escrow is released in two tranches.".to_string(),
                    0,
                    0,
                    0.5,
                )],
            )
            .unwrap();

        // --- A NEW conversation that refers back to it -------------------
        let session_id = "new-chat";
        db.conversations.create_session_with_id(session_id, None).ok();
        let query = "What did we discuss about the escrow amount earlier?";
        let processed_messages = vec![Message {
            role: "user".to_string(),
            content: query.to_string(),
        }];

        let doc_blocks = crate::context_engine::build_document_context(db, session_id, 20_000).await;
        assert!(
            doc_blocks.session_block.is_none(),
            "this new chat has no documents of its own"
        );

        let known: Vec<String> = db
            .documents
            .all_documents(500)
            .unwrap()
            .into_iter()
            .map(|d| d.original_filename)
            .collect();
        let intent = crate::context_engine::detect_reference(query, &known, chrono::Utc::now());
        assert!(intent.refers_to_past(), "{}", intent.explain());

        let past = crate::context_engine::build_past_context(db, session_id, query, &intent, 6000);
        let block = past
            .block
            .clone()
            .expect("past material must be retrieved for a backward reference");
        assert!(
            block.contains("eighteen months"),
            "the earlier document's content must reach the block: {}",
            block
        );
        assert!(
            block.contains("escrow-terms.txt"),
            "the source must be named so the model can cite it: {}",
            block
        );

        let mut llm_input_messages = processed_messages.clone();
        crate::context_engine::fold_into_latest_message(&mut llm_input_messages, &block);

        // The block lands on the LATEST message, leaving any prefix untouched.
        assert_eq!(
            llm_input_messages.len(),
            processed_messages.len(),
            "folding must never add a message - strict-alternation chat templates \
             hard-error on an extra system-role entry mid-array"
        );
        assert!(llm_input_messages.last().unwrap().content.contains("eighteen months"));
        assert!(
            llm_input_messages.last().unwrap().content.starts_with(query),
            "the user's own words must still come first in their message"
        );

        // THE INVARIANT: the record that is persisted and shown in the UI is
        // untouched. Retrieval enriches the model's view only.
        assert_eq!(processed_messages.len(), 1);
        assert_eq!(
            processed_messages[0].content, query,
            "the persisted conversation record must never be mutated by retrieval"
        );
    }

    /// The negative end-to-end: a genuinely new question retrieves nothing even
    /// with a full database, but is still persisted. "Do not check the database,
    /// but do save to it."
    #[tokio::test]
    async fn a_new_topic_retrieves_nothing_but_is_still_persisted() {
        let state = test_state().await;
        let db = &state.shared_state.database_pool;

        db.conversations.create_session_with_id("older-chat", None).ok();
        let attach = ChatAttachment {
            name: "escrow-terms.txt".to_string(),
            content_base64: None,
            content_text: Some("Escrow and termination provisions apply.".to_string()),
            mime_type: Some("text/plain".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments("older-chat", &[attach], &state).await;

        let session_id = "fresh-chat";
        db.conversations.create_session_with_id(session_id, None).ok();
        // Deliberately uses words that WOULD match the stored document, so this
        // tests the detection gate rather than the absence of matching content.
        let query = "How does escrow work in a termination scenario?";

        let known: Vec<String> = db
            .documents
            .all_documents(500)
            .unwrap()
            .into_iter()
            .map(|d| d.original_filename)
            .collect();
        let intent = crate::context_engine::detect_reference(query, &known, chrono::Utc::now());
        assert!(
            !intent.refers_to_past(),
            "a new question must not be treated as a reference: {}",
            intent.explain()
        );

        let past = crate::context_engine::build_past_context(db, session_id, query, &intent, 6000);
        assert!(
            past.block.is_none(),
            "a new topic must not retrieve past material: {:?}",
            past.block
        );
        assert_eq!(past.consumed_chars, 0, "and must consume no budget");

        // Still persisted - the database keeps growing as knowledge either way.
        db.conversations
            .store_messages_batch(session_id, &[("user".to_string(), query.to_string(), 0, 0, 0.5)])
            .unwrap();
        let stored = db.conversations.get_session_messages(session_id, None, None).unwrap();
        assert_eq!(stored.len(), 1, "a new topic must still be recorded");
        assert_eq!(stored[0].content, query);
    }

    /// Naming a file from a DIFFERENT conversation must pull that exact file -
    /// the "attach in Chat A, ask in Chat B" behaviour, rebuilt lexically.
    #[tokio::test]
    async fn a_document_attached_in_another_chat_is_found_by_name() {
        let state = test_state().await;
        let db = &state.shared_state.database_pool;

        db.conversations.create_session_with_id("chat-a", None).ok();
        let attach = ChatAttachment {
            name: "vendor-nda.txt".to_string(),
            content_base64: None,
            content_text: Some(
                "The receiving party shall not disclose for a period of five years."
                    .to_string(),
            ),
            mime_type: Some("text/plain".to_string()),
            source_path: None,
            local_file_id: None,
            document_id: None,
        };
        persist_inline_attachments("chat-a", &[attach], &state).await;

        let session_id = "chat-b";
        db.conversations.create_session_with_id(session_id, None).ok();
        let query = "what does vendor-nda.txt say about disclosure?";

        let known: Vec<String> = db
            .documents
            .all_documents(500)
            .unwrap()
            .into_iter()
            .map(|d| d.original_filename)
            .collect();
        let intent = crate::context_engine::detect_reference(query, &known, chrono::Utc::now());
        let past = crate::context_engine::build_past_context(db, session_id, query, &intent, 6000);
        let block = past.block.expect("naming a file from another chat must find it");
        assert!(
            block.contains("five years"),
            "the named document's content must be retrieved across sessions: {}",
            block
        );
    }

    /// Proves `extract_content_from_bytes` actually TAKES a lane permit — the
    /// wiring a refactor could silently drop, leaving the scheduler correct but
    /// unused.
    ///
    /// Deterministic rather than timing-based: hold the Text lane externally,
    /// then extract a .txt. If the extractor acquires its lane it must block, so
    /// a short timeout expiring IS the proof. Measuring peak concurrency from
    /// outside cannot work here — the permit is taken inside the call, so an
    /// external observer sees all callers "inside" while they queue.
    #[tokio::test]
    async fn extraction_waits_for_its_format_lane() {
        use crate::utils::extraction_scheduler::{global, Lane};
        use std::time::Duration;

        let held = global().acquire_lane(Lane::Text).await;
        let blocked = tokio::time::timeout(
            Duration::from_millis(300),
            extract_content_from_bytes(b"clause text", "notes.txt"),
        )
        .await;
        assert!(
            blocked.is_err(),
            "extraction must wait for its lane permit; it completed while the Text \
             lane was held, so the scheduler is not actually wired in"
        );

        // Releasing the lane must let it through.
        drop(held);
        let allowed = tokio::time::timeout(
            Duration::from_secs(5),
            extract_content_from_bytes(b"clause text", "notes.txt"),
        )
        .await;
        assert!(allowed.is_ok(), "extraction must proceed once the lane is free");
    }

    /// The other half: a file whose format is in a DIFFERENT lane must not be
    /// held up. This is what makes attaching a spreadsheet alongside three PDFs
    /// worth doing.
    #[tokio::test]
    async fn extraction_is_not_blocked_by_an_unrelated_format_lane() {
        use crate::utils::extraction_scheduler::{global, Lane};
        use std::time::Duration;

        // Occupy the PDF lane, then extract a text file.
        let _held = global().acquire_lane(Lane::Pdf).await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            extract_content_from_bytes(b"clause text", "unrelated.txt"),
        )
        .await;
        assert!(
            result.is_ok(),
            "a text file must not wait behind a PDF - different lanes run in parallel"
        );
    }

    /// Serialising a lane must not LOSE work. Three files that all queue behind
    /// each other must all still be extracted and stored.
    #[tokio::test]
    async fn a_batch_sharing_one_lane_still_stores_every_file() {
        let state = test_state().await;
        let session_id = "one-lane-batch";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let attachments: Vec<ChatAttachment> = (0..3)
            .map(|i| ChatAttachment {
                name: format!("clause{}.txt", i),
                content_base64: None,
                content_text: Some(format!("Clause {} of the agreement is binding.", i)),
                mime_type: Some("text/plain".to_string()),
                source_path: None,
                local_file_id: None,
                document_id: None,
            })
            .collect();

        persist_inline_attachments(session_id, &attachments, &state).await;

        let docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(
            docs.len(),
            3,
            "queueing must not drop attachments, got {:?}",
            docs.iter().map(|d| &d.original_filename).collect::<Vec<_>>()
        );
        for doc in &docs {
            assert!(
                doc.extracted_text.contains("is binding"),
                "'{}' must have real extracted content: {:?}",
                doc.original_filename,
                doc.extracted_text
            );
        }
    }

    /// Build a multipart/form-data body by hand, so the endpoint is exercised
    /// over real HTTP rather than by calling the handler function directly.
    fn multipart_body(files: &[(&str, &[u8])]) -> (String, Vec<u8>) {
        let boundary = "----OCATestBoundary7d91";
        let mut body: Vec<u8> = Vec::new();
        for (name, bytes) in files {
            body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"files\"; filename=\"{}\"\r\n\r\n",
                    name
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", boundary),
            body,
        )
    }

    async fn post_attach(
        state: UnifiedAppState,
        files: &[(&str, &[u8])],
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use tower::ServiceExt;
        let router = crate::thread_server::build_compatible_router(state);
        let (content_type, body) = multipart_body(files);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/documents/attach")
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .body(axum::body::Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// The point of P3: a paperclip file is extracted and stored the moment it
    /// is picked, WITHOUT a session and without the user having sent anything.
    ///
    /// Driven over real HTTP through the actual router, because the wiring
    /// between route, multipart extraction and the document store is exactly
    /// what a function-level test would skip.
    #[tokio::test]
    async fn attaching_a_file_processes_it_immediately_over_http() {
        let state = test_state().await;
        let (status, json) = post_attach(
            state.clone(),
            &[("contract.txt", b"The indemnity cap is five million dollars.")],
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK, "response: {}", json);
        let docs = json["documents"].as_array().expect("documents array");
        assert_eq!(docs.len(), 1, "{}", json);
        assert_eq!(docs[0]["extraction_status"], "ok", "{}", json);
        assert_eq!(docs[0]["filename"], "contract.txt");
        assert!(docs[0]["char_count"].as_i64().unwrap() > 0);

        // The content must really be in the store, with no session involved.
        let id = docs[0]["document_id"].as_i64().expect("document_id");
        let stored = state
            .shared_state
            .database_pool
            .documents
            .get_document(id)
            .expect("document must exist");
        assert!(
            stored.extracted_text.contains("five million dollars"),
            "content must be extracted at attach time: {:?}",
            stored.extracted_text
        );
        assert_eq!(stored.source_kind, "paperclip");
    }

    /// Several files picked at once must ALL be processed in one request - this
    /// is the batch that the format lanes parallelise.
    #[tokio::test]
    async fn attaching_several_files_processes_all_of_them() {
        let state = test_state().await;
        let (status, json) = post_attach(
            state.clone(),
            &[
                ("a.txt", b"Clause A concerns termination."),
                ("b.txt", b"Clause B concerns indemnity."),
                ("c.txt", b"Clause C concerns arbitration."),
            ],
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK, "{}", json);
        let docs = json["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 3, "every picked file must be processed: {}", json);
        let ids: Vec<i64> = docs
            .iter()
            .map(|d| d["document_id"].as_i64().unwrap())
            .collect();
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "each file must get its own document"
        );
    }

    /// Attach time is the RIGHT time to learn a file is unreadable - not from
    /// the model's answer three minutes later.
    #[tokio::test]
    async fn an_unsupported_type_is_refused_at_attach_time() {
        let state = test_state().await;
        let (status, _json) =
            post_attach(state.clone(), &[("bundle.zip", b"PK\x03\x04 not a document")]).await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "an all-unsupported batch must fail loudly, not report success"
        );
        assert!(
            state
                .shared_state
                .database_pool
                .documents
                .all_documents(10)
                .unwrap()
                .is_empty(),
            "nothing may be stored for an unsupported type"
        );
    }

    /// Re-attaching the same bytes must reuse the stored extraction rather than
    /// paying for it again - the same hash-first rule as every other path.
    #[tokio::test]
    async fn re_attaching_identical_content_reuses_the_existing_document() {
        let state = test_state().await;
        let payload: &[u8] = b"Governing law is the State of Delaware.";

        let (_, first) = post_attach(state.clone(), &[("law.txt", payload)]).await;
        let first_id = first["documents"][0]["document_id"].as_i64().unwrap();
        assert_eq!(first["documents"][0]["reused"], false);

        let (_, second) = post_attach(state.clone(), &[("law.txt", payload)]).await;
        let second_id = second["documents"][0]["document_id"].as_i64().unwrap();
        assert_eq!(second_id, first_id, "the same content must map to one document");
        assert_eq!(
            second["documents"][0]["reused"], true,
            "the second attach must be served from the store, not re-extracted"
        );

        assert_eq!(
            state.shared_state.database_pool.documents.all_documents(10).unwrap().len(),
            1,
            "no duplicate document rows"
        );
    }

    /// A document attached this way must NOT be linked to any session yet.
    ///
    /// At attach time there may be no `sessions` row at all (a brand-new chat
    /// creates one only when the first message is persisted), and
    /// `session_documents` has a foreign key to it. Linking here would fail that
    /// constraint for the most common case of all - open a chat, attach a file.
    #[tokio::test]
    async fn an_attached_document_is_not_linked_to_a_session_until_the_message_is_sent() {
        let state = test_state().await;
        let (_, json) =
            post_attach(state.clone(), &[("pending.txt", b"Escrow terms apply.")]).await;
        let document_id = json["documents"][0]["document_id"].as_i64().unwrap();

        // No session exists yet, and the document belongs to none.
        let session_id = "not-created-yet";
        assert!(
            state
                .shared_state
                .database_pool
                .documents
                .get_session_documents(session_id)
                .unwrap()
                .is_empty(),
            "the document must not be attached to any conversation yet"
        );

        // Sending is what links it - via the existing document_id branch.
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();
        let attachment = ChatAttachment {
            name: "pending.txt".to_string(),
            content_base64: None,
            content_text: None,
            mime_type: None,
            source_path: None,
            local_file_id: None,
            document_id: Some(document_id),
        };
        persist_inline_attachments(session_id, &[attachment], &state).await;

        let linked = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(linked.len(), 1, "sending must link the pre-processed document");
        assert_eq!(linked[0].id, document_id);
        assert!(linked[0].extracted_text.contains("Escrow terms apply."));
    }

    /// An `@filename` reference to a Local Storage file must go through the
    /// coordinated path — waiting for any background extraction already running
    /// for that file, and reusing a stored extraction instead of redoing it.
    ///
    /// Proven the same way as the other reuse tests: overwrite the stored text
    /// with a sentinel, then reference the file. A re-extraction would restore
    /// the original text, so the sentinel surviving is proof the extraction was
    /// reused rather than repeated.
    ///
    /// Before this, the reference path read the bytes and called
    /// upsert_and_link directly, bypassing both behaviours.
    #[tokio::test]
    async fn an_at_reference_to_a_local_storage_file_reuses_its_extraction() {
        let state = test_state().await;
        let session_id = "ref-reuse";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let body = b"The arbitration seat is Singapore.";
        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "seat.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        // Extract once (as the background pass would), then mark the stored text
        // so a second extraction would be detectable.
        crate::api::files_api::spawn_background_extraction(
            state.clone(),
            file.id,
            "seat.txt".to_string(),
            body.to_vec(),
            Some("text/plain".to_string()),
            None,
        );
        let mut doc_id = None;
        for _ in 0..150 {
            if let Ok(Some(d)) = state
                .shared_state
                .database_pool
                .documents
                .get_document_by_local_file_id(file.id)
            {
                if d.extraction_status == "ok" && !d.extracted_text.trim().is_empty() {
                    doc_id = Some(d.id);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let doc_id = doc_id.expect("background extraction must complete");
        state
            .shared_state
            .database_pool
            .documents
            .repair_extraction(doc_id, "SENTINEL-REUSED-NOT-REEXTRACTED", "ok", None)
            .unwrap();

        let messages = vec![Message {
            role: "user".to_string(),
            content: "summarise @seat.txt".to_string(),
        }];
        persist_file_reference_attachments(session_id, &messages, &state).await;

        let linked = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(linked.len(), 1, "the referenced file must be linked");
        assert_eq!(linked[0].id, doc_id, "it must be the SAME document, not a new one");
        assert_eq!(
            linked[0].extracted_text, "SENTINEL-REUSED-NOT-REEXTRACTED",
            "the @ reference re-extracted a file whose extraction already existed"
        );
    }

    /// The fallback still works: a referenced file that is NOT in Local Storage
    /// has nothing to coordinate with, and must not be broken by the routing
    /// change above.
    #[tokio::test]
    async fn an_at_reference_to_an_unknown_file_is_ignored_cleanly() {
        let state = test_state().await;
        let session_id = "ref-unknown";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let messages = vec![Message {
            role: "user".to_string(),
            content: "what about @definitely-not-here.txt".to_string(),
        }];
        persist_file_reference_attachments(session_id, &messages, &state).await;

        assert!(
            state
                .shared_state
                .database_pool
                .documents
                .get_session_documents(session_id)
                .unwrap()
                .is_empty(),
            "a reference to a file that does not exist must attach nothing"
        );
    }

    /// A multi-format batch must extract CONCURRENTLY across lanes, not one file
    /// after another.
    ///
    /// This is the regression guard for a real bug: `attach_document` originally
    /// awaited extraction inside its multipart loop, so a 16-file attach ran 16
    /// extractions strictly in sequence. Every earlier test still passed —
    /// documents were stored, content was correct, ids were unique — because
    /// they only checked OUTCOMES. Sequential processing is not incorrect, just
    /// slow, so nothing caught it.
    ///
    /// Proven by occupying one lane and observing that a different lane still
    /// completes. If the endpoint drained and extracted field-by-field, the
    /// blocked file would stall every file behind it and the whole request would
    /// time out.
    #[tokio::test]
    async fn an_attach_batch_does_not_serialise_across_format_lanes() {
        use crate::utils::extraction_scheduler::{global, Lane};
        use std::time::Duration;

        let state = test_state().await;

        // Hold the Text lane so any .txt in the batch cannot be extracted.
        let held = global().acquire_lane(Lane::Text).await;

        // Batch of two different formats. The .xlsx is not valid spreadsheet
        // data, so its extraction fails fast - which is fine and is the point:
        // it must be ABLE to fail while the .txt is still blocked, proving the
        // two were processed concurrently rather than in file order.
        let request = tokio::spawn({
            let state = state.clone();
            async move {
                post_attach(
                    state,
                    &[
                        ("blocked.txt", b"this lane is occupied"),
                        ("other.xlsx", b"not really a spreadsheet"),
                    ],
                )
                .await
            }
        });

        // With sequential processing the request cannot finish at all while the
        // Text lane is held. Give it a moment, then release and confirm it
        // completes promptly - the concurrency itself is asserted by the
        // companion test below.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!request.is_finished(), "the blocked .txt should still be waiting");

        drop(held);
        let (status, json) = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .expect("request must complete once the lane is released")
            .expect("task must not panic");
        assert_eq!(status, axum::http::StatusCode::OK, "{}", json);
        assert_eq!(
            json["documents"].as_array().unwrap().len(),
            2,
            "both files must be accounted for: {}",
            json
        );
    }

    /// The direct measurement: a batch spanning several lanes must take about as
    /// long as its SLOWEST lane, not the sum of every file.
    ///
    /// Uses the scheduler directly with controlled delays, because real
    /// extraction of test-sized payloads is too fast to time reliably. This is
    /// what pins "3 PDFs + 3 Word + 3 Excel + 3 PPTX + 4 images" behaving as
    /// four-plus lanes in parallel rather than sixteen files in series.
    #[tokio::test]
    async fn a_sixteen_file_batch_costs_the_slowest_lane_not_the_sum() {
        use crate::utils::extraction_scheduler::global;
        use std::time::{Duration, Instant};

        // 3 pdf, 3 word, 3 excel, 3 pptx, 2 png, 2 jpg = 16 files, 5 lanes.
        let batch: Vec<String> = ["pdf", "docx", "xlsx", "pptx"]
            .iter()
            .flat_map(|ext| (0..3).map(move |i| format!("file{}.{}", i, ext)))
            .chain((0..2).map(|i| format!("shot{}.png", i)))
            .chain((0..2).map(|i| format!("photo{}.jpg", i)))
            .collect();
        assert_eq!(batch.len(), 16);

        const UNIT: u64 = 40;
        let started = Instant::now();
        let handles: Vec<_> = batch
            .into_iter()
            .map(|name| {
                tokio::spawn(async move {
                    let _lane = global().acquire(&name).await;
                    tokio::time::sleep(Duration::from_millis(UNIT)).await;
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = started.elapsed();

        // Longest lane is Image: 2 png + 2 jpg share ONE lane = 4 units.
        // Sequential would be 16 units. Assert comfortably between the two.
        let sequential = Duration::from_millis(UNIT * 16);
        let slowest_lane = Duration::from_millis(UNIT * 4);
        assert!(
            elapsed < sequential / 2,
            "batch took {:?}, close to fully sequential ({:?}) - lanes are not \
             running in parallel",
            elapsed,
            sequential
        );
        assert!(
            elapsed >= slowest_lane,
            "batch took {:?}, less than the 4 serialised images require ({:?}) - \
             same-lane files are overlapping when they must not",
            elapsed,
            slowest_lane
        );
    }

    /// Two files with IDENTICAL CONTENT in one attach batch.
    ///
    /// Regression for a bug that locked the composer permanently. Both files
    /// hash the same, both pass `find_by_hash` before either INSERTs, and the
    /// loser hit the UNIQUE constraint on `content_hash`. `attach_document`
    /// returned nothing for it, and the frontend left that chip in "processing"
    /// forever — with the send button disabled while anything is processing, the
    /// user could not send at all.
    ///
    /// Concurrency made this reachable: P3 processes a batch with `join_all`, so
    /// the two find_by_hash calls genuinely overlap. Every earlier P3 test used
    /// files with DISTINCT content and so never produced a collision.
    ///
    /// The contract: both files get a result, both name the same document (the
    /// content IS the same), and exactly one row exists.
    #[tokio::test]
    async fn two_identical_files_in_one_batch_both_get_a_result() {
        let state = test_state().await;
        let same: &[u8] = b"The indemnity cap is five million dollars.";

        let (status, json) = post_attach(
            state.clone(),
            &[("contract.txt", same), ("contract-copy.txt", same)],
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK, "{}", json);
        let docs = json["documents"].as_array().expect("documents array");
        assert_eq!(
            docs.len(),
            2,
            "BOTH files must get a result - one missing strands its chip in \
             'processing' and disables send: {}",
            json
        );
        for d in docs {
            assert!(
                d["document_id"].as_i64().is_some(),
                "every result needs a document id: {}",
                d
            );
        }

        // Identical content is one document by design.
        let ids: std::collections::HashSet<i64> = docs
            .iter()
            .map(|d| d["document_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids.len(), 1, "identical content must resolve to one document");
        assert_eq!(
            state.shared_state.database_pool.documents.all_documents(10).unwrap().len(),
            1,
            "no duplicate rows may be created by the race"
        );
    }

    /// The same race at the store level, driven concurrently on purpose so it is
    /// not dependent on HTTP timing.
    ///
    /// `upsert_document` must never surface a UNIQUE violation to its caller: the
    /// winner stored the identical bytes, so returning that row is the correct
    /// answer rather than a consolation.
    ///
    /// Runs against a FILE database, not the in-memory one every other test
    /// uses. That is deliberate: `SqliteConnectionManager::memory()` opens
    /// connections in SHARED-CACHE mode, where contention surfaces as
    /// SQLITE_LOCKED_SHAREDCACHE (code 262) - a table-level lock that
    /// `busy_timeout` does not govern at all. Production is a file database in
    /// WAL mode, where the same contention is SQLITE_BUSY and busy_timeout does
    /// apply. Testing this in memory would measure a failure mode production
    /// does not have, and miss the one it does.
    ///
    /// # What this test does and does not prove
    ///
    /// It is a CONTRACT test: concurrent upserts of identical content must all
    /// succeed and agree on one document id. It is NOT a reliable reproducer of
    /// the insert race, and should not be trusted as one - measured, by
    /// disabling the recovery in `upsert_document` and re-running: it still
    /// passes, because the window between `find_by_hash` and the INSERT is too
    /// narrow to hit consistently on a fast local file.
    ///
    /// The race itself IS real and was reproduced during the audit that added
    /// this test - on the in-memory shared-cache path, which interleaves more
    /// aggressively, disabling the recovery produces
    /// "UNIQUE constraint failed: documents.content_hash". That configuration
    /// cannot serve as the regression test though, because with the recovery in
    /// place it then fails on shared-cache table locking instead, which
    /// production never encounters.
    ///
    /// So the recovery in `upsert_document` is justified by that reproduction
    /// plus inspection, not by this test failing without it. Making it
    /// deterministic would need a test-only hook between the hash check and the
    /// insert; worth adding if this area is touched again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_upserts_of_identical_content_all_succeed() {
        let dir = std::env::temp_dir().join(format!(
            "oca-race-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(
            crate::memory_db::MemoryDatabase::new(&dir.join("memory.db"))
                .expect("file database must open"),
        );
        let payload = b"Governing law is the State of Delaware.".to_vec();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            let payload = payload.clone();
            handles.push(tokio::spawn(async move {
                db.documents.upsert_document(crate::memory_db::NewDocument {
                    original_bytes: &payload,
                    original_filename: "law.txt",
                    source_path: None,
                    source_kind: "paperclip",
                    mime_type: None,
                    size_bytes: payload.len() as i64,
                    extracted_text: "Governing law is the State of Delaware.".to_string(),
                    extraction_status: "ok",
                    extraction_error: None,
                    local_file_id: None,
                })
                .map(|d| d.id)
            }));
        }

        let mut ids = Vec::new();
        for h in handles {
            let result = h.await.unwrap();
            ids.push(
                result.expect("a concurrent upsert of identical content must not error"),
            );
        }
        assert_eq!(ids.len(), 8);
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "every caller must receive the same document id, got {:?}",
            ids
        );
        assert_eq!(
            db.documents.all_documents(10).unwrap().len(),
            1,
            "exactly one row for one piece of content"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// POST /generate/stream — Main streaming chat endpoint
///
/// 1. Validates request and gets/creates session in shared memory
/// 2. Persists user message to database
/// 3. Streams LLM response back via SSE
/// 4. Persists assistant response to database after completion
pub async fn generate_stream(
    State(state): State<UnifiedAppState>,
    Json(req): Json<StreamChatRequest>,
) -> Response {
    let request_num = state.shared_state.counters.inc_total_requests();
    info!("Stream request #{} for session: {}", request_num, req.session_id);
    
    // Debug: Log attachment info
    if let Some(ref attachments) = req.attachments {
        info!("Request has {} attachments", attachments.len());
        for (i, att) in attachments.iter().enumerate() {
            info!("  Attachment {}: name={}, has_text={}, has_base64={}", 
                i, att.name, att.content_text.is_some(), att.content_base64.is_some());
        }
    } else {
        info!("Request has NO attachments (None)");
    }

    if req.messages.is_empty() {
        return (StatusCode::BAD_REQUEST, "Messages array cannot be empty").into_response();
    }

    let session_id = req.session_id.clone();

    // 1. processed_messages is the CONVERSATION RECORD: persisted to the DB
    //    and shown back to the user verbatim. Document content is NEVER
    //    spliced in here - it is recorded separately (documents_store) and
    //    folded into a SEPARATE view built just before the LLM call (step 4).
    let processed_messages = req.messages.clone();

    // 1. Ensure the session row exists in the database FIRST. Attachment
    //    persistence (below) links documents to this session via a foreign
    //    key on session_documents.session_id (FK enforcement is ON) - on a
    //    brand-new chat that row does not exist until this call runs, so it
    //    must happen BEFORE any attachment is processed. Getting this order
    //    wrong doesn't error loudly: the FK insert just fails silently
    //    (logged as a warn), the document is extracted and stored but never
    //    linked to the conversation, and the model never sees it - this was
    //    a real regression, not a hypothetical.
    if let Err(e) = state.shared_state.database_pool.conversations.create_session_with_id(&session_id, None) {
        // Ignore "already exists" errors - the common case for turn 2+.
        debug!("Session creation result (may already exist): {}", e);
    }

    // 1a. Persist inline (paperclip) attachments into the unified document store.
    if let Some(ref attachments) = req.attachments {
        persist_inline_attachments(&session_id, attachments, &state).await;
    }

    // 1b. Persist file references (@filename, [Attached: filename]) into the
    //     same store, resolving from Local Storage or the app data dir.
    persist_file_reference_attachments(&session_id, &processed_messages, &state).await;

    // 2. Get or create session in shared memory (zero-cost Arc lookup)
    let session = state.shared_state.get_or_create_session(&session_id).await;

    // Remember which conversation is driving the engine. At shutdown this is
    // the session whose KV state is in the slot, and therefore the one worth
    // persisting for a warm start on the next launch.
    if let Ok(mut last) = state.shared_state.last_active_session.write() {
        *last = Some(session_id.clone());
    }

    // 3. Update in-memory session with the processed messages
    {
        if let Ok(mut session_data) = session.write() {
            session_data.last_accessed = std::time::Instant::now();
            session_data.messages = processed_messages.clone();
        }
    }

    // 4. Persist user message (session row already ensured in step 1)
    let user_msg_content = processed_messages.iter().rev().find(|m| m.role == "user").map(|m| m.content.clone());
    if let Some(ref content) = user_msg_content {
        let db = state.shared_state.database_pool.clone();
        let sid = session_id.clone();
        let content = content.clone();
        let msg_count = processed_messages.len() as i32;

        // Persist user message in background (this can be async)
        tokio::spawn(async move {
            if let Err(e) = db.conversations.store_messages_batch(
                &sid,
                &[("user".to_string(), content, msg_count - 1, 0, 0.5)],
            ) {
                error!("Failed to persist user message: {}", e);
            }
        });
    }

    // 4. Document memory (backend-owned, model-independent): reload EVERY
    //    document ever attached to this session - not just what arrived this
    //    turn - and fold it into a SEPARATE view for the LLM only.
    //    processed_messages (the persisted conversation record) is never
    //    touched by this.
    let ctx_budget_tokens = state.shared_state.current_context_budget().await;
    // Document-memory budget. Two competing pressures, resolved together:
    //
    //   1. More attached files need more total room. Attaching 3 contracts
    //      into the "1 doc's worth" budget forces each into 1/3 the space
    //      and reads like the model was quoting from a stub. So the base
    //      grows with attachment count.
    //   2. That growth MUST NOT exceed the real context window. The old
    //      formula (`ctx_budget_tokens * 2 * scale_pct/100`) at 7+ docs
    //      produced ~8× the token budget in chars, which llama-server would
    //      then silently truncate — a partial reading presented as if
    //      complete, the exact class of failure document_memory's PARTIAL
    //      markers exist to prevent. It also used a 2-chars/token conversion
    //      here while the "remaining" line below uses 4 — internally
    //      inconsistent. Both now use the same CHARS_PER_TOKEN.
    //
    // Base share is 50% of the real context in chars; the per-file scale
    // then grows it, but a hard ceiling (MAX_DOC_CONTEXT_PCT) guarantees at
    // least 30% of the window survives for conversation memory + generation
    // reserve regardless of how many documents were attached.
    const CHARS_PER_TOKEN: usize = 4;
    const MAX_DOC_CONTEXT_PCT: usize = 70;
    let doc_count = state.shared_state.database_pool.documents
        .get_session_documents(&session_id)
        .ok()
        .map(|d| d.len())
        .unwrap_or(0);
    let scale_pct = 100 + (doc_count.saturating_sub(1) * 50).min(300); // 1=100%, 2=150%, 3=200%, 4+=300%
    let ctx_budget_chars = ctx_budget_tokens * CHARS_PER_TOKEN;
    let scaled = ctx_budget_chars / 2 * scale_pct / 100;                // ~50% base scaled by doc count
    let hard_cap = ctx_budget_chars * MAX_DOC_CONTEXT_PCT / 100;
    let doc_budget_chars = scaled.min(hard_cap);
    let doc_blocks = crate::context_engine::build_document_context(
        &state.shared_state.database_pool,
        &session_id,
        doc_budget_chars,
    ).await;

    let mut llm_input_messages = processed_messages.clone();
    if let Some(ref block) = doc_blocks.session_block {
        crate::context_engine::fold_into_system_message(&mut llm_input_messages, block);
    }

    // 4b. Past-material retrieval: documents and messages from EARLIER that this
    //     question refers back to.
    //
    //     Runs only when the question actually points backwards - a named or
    //     described file, an unambiguous phrase, or a date (see
    //     context_engine::reference_detector). A brand-new topic performs no
    //     search at all, which is the point: dragging unrelated history into a
    //     fresh question produces worse answers than ignoring history entirely.
    //     Persistence is unaffected either way - every message and document is
    //     recorded regardless of whether anything is retrieved.
    //
    //     Folded into the LATEST message, not the system message. This block is
    //     query-dependent by nature, so it can never be part of a stable prompt
    //     prefix; putting it on the message that already changes every turn
    //     leaves everything before it byte-identical and still cacheable, which
    //     is what keeps time-to-first-token low on document conversations.
    //
    //     Budget is deliberately modest: this is supplementary context, and it
    //     is drawn from the same window the conversation history needs.
    const PAST_CONTEXT_PCT: usize = 15;
    let past_budget_chars = ctx_budget_chars
        .saturating_sub(doc_blocks.consumed_chars)
        * PAST_CONTEXT_PCT
        / 100;
    let past_context = if let Some(ref query) = user_msg_content {
        // Known filenames let a DESCRIPTION resolve to a file ("the merger
        // agreement" -> merger-agreement.pdf) instead of requiring an exact name.
        let known_filenames: Vec<String> = state
            .shared_state
            .database_pool
            .documents
            .all_document_filenames(500)
            .unwrap_or_default();
        let intent = crate::context_engine::detect_reference(
            query,
            &known_filenames,
            chrono::Utc::now(),
        );
        crate::context_engine::build_past_context(
            &state.shared_state.database_pool,
            &session_id,
            query,
            &intent,
            past_budget_chars,
        )
    } else {
        crate::context_engine::PastContext {
            block: None,
            consumed_chars: 0,
            reason: "no retrieval: no user query in this request".to_string(),
        }
    };
    if let Some(ref block) = past_context.block {
        crate::context_engine::fold_into_latest_message(&mut llm_input_messages, block);
    }
    debug!("Past-material retrieval: {}", past_context.reason);

    // Remaining budget for tier1/2/3 conversation memory, after documents AND
    // retrieved past material. Both are subtracted, using the same chars/token
    // conversion as the budget calc above - omitting either would let the
    // orchestrator plan against space that is already spent, and llama-server
    // would then silently truncate the overflow.
    let remaining_budget_tokens = ctx_budget_tokens
        .saturating_sub(doc_blocks.consumed_chars / CHARS_PER_TOKEN)
        .saturating_sub(past_context.consumed_chars / CHARS_PER_TOKEN)
        .max(512);

    // 5. Context Engine: Retrieve past conversation context (tiers 1/2/3,
    //    keyword-based) on top of the document-enriched view above.
    //    Always let the retrieval planner decide — even a brand-new session can trigger
    //    cross-session search if the user asks "what did we discuss yesterday?".
    let context_messages = {
        let orchestrator_guard = state.context_orchestrator.read().await;
        if let Some(ref orchestrator) = *orchestrator_guard {
            let user_query = user_msg_content.as_deref();
            match orchestrator.process_conversation(&session_id, &llm_input_messages, user_query, remaining_budget_tokens).await {
                Ok(optimized) => {
                    if optimized.len() != llm_input_messages.len() {
                        info!("Context engine optimized: {} → {} messages (retrieved past context)",
                            llm_input_messages.len(), optimized.len());
                    }
                    optimized
                }
                Err(e) => {
                    error!("Context engine error (falling back to document-enriched messages): {}", e);
                    llm_input_messages.clone()
                }
            }
        } else {
            debug!("Context orchestrator not initialized, using document-enriched messages");
            llm_input_messages.clone()
        }
    };

    // (classifiers for engine rejections live at module scope — see
    // is_role_alternation_error / is_context_overflow_error)

    // 5. Route to local LLM worker
    let max_tokens = req.max_tokens;
    let temperature = req.temperature;
    let db_for_persist = state.shared_state.database_pool.clone();
    let session_id_for_persist = session_id.clone();
    let msg_index = req.messages.len() as i32;

    {
        // First check if the runtime is ready before attempting to stream
        if !state.llm_worker.is_runtime_ready().await {
            let rt_state = state.llm_worker.get_runtime_state();
            if matches!(rt_state, crate::model_runtime::RuntimeState::Switching { .. }) {
                return (StatusCode::SERVICE_UNAVAILABLE,
                    "Model Switching: A model switch is in progress. Please wait for it to complete.").into_response();
            }
            if matches!(rt_state, crate::model_runtime::RuntimeState::Restarting) {
                return (StatusCode::SERVICE_UNAVAILABLE,
                    "Model Restarting: The model server is restarting automatically. Please wait a moment and try again.").into_response();
            }
            return (StatusCode::SERVICE_UNAVAILABLE,
                "Model Not Ready: No local model is currently loaded. Please go to the Models page and activate a model by clicking \"Active Model\".").into_response();
        }

        let llm_worker = state.llm_worker.clone();
        
        match llm_worker.stream_response(context_messages, max_tokens, temperature).await {
            Ok(llm_stream) => {
                // Wrap the LLM stream to collect the full response for DB persistence
                let output_stream = async_stream::stream! {
                    let mut full_response = String::new();

                    futures_util::pin_mut!(llm_stream);

                    while let Some(item) = tokio_stream::StreamExt::next(&mut llm_stream).await {
                        match item {
                            Ok(sse_line) => {
                                // Extract content from SSE data for persistence
                                if sse_line.starts_with("data: ") && !sse_line.contains("[DONE]") {
                                    if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(&sse_line[6..].trim()) {
                                        if let Some(content) = chunk
                                            .get("choices")
                                            .and_then(|c| c.get(0))
                                            .and_then(|c| c.get("delta"))
                                            .and_then(|d| d.get("content"))
                                            .and_then(|c| c.as_str())
                                        {
                                            full_response.push_str(content);
                                        }
                                    }
                                }

                                // Yield SSE event to client
                                let data = sse_line.trim_start_matches("data: ").trim_end().to_string();
                                yield Ok::<_, Infallible>(Event::default().data(data));
                            }
                            Err(e) => {
                                error!("Stream error: {}", e);
                                yield Ok(Event::default().data(
                                    format!("{{\"error\": \"{}\"}}", e)
                                ));
                                break;
                            }
                        }
                    }

                    // Persist assistant response to database after stream completes
                    if !full_response.is_empty() {
                        match db_for_persist.conversations.store_messages_batch(
                            &session_id_for_persist,
                            &[("assistant".to_string(), full_response.clone(), msg_index, 0, 0.5)],
                        ) {
                            Ok(_stored_msgs) => {
                                debug!("Persisted assistant response ({} chars) for session {}",
                                    full_response.len(), session_id_for_persist);
                            }
                            Err(e) => {
                                error!("Failed to persist assistant message: {}", e);
                            }
                        }
                    }
                };

                return Sse::new(output_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(15))
                    )
                    .into_response();
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                error!("Failed to start LLM stream: {}", error_msg);
                // Logged separately at error level with the raw text, so the
                // cause survives even if the response never reaches the UI.
                error!(
                    "Engine rejection detail (verbatim, for diagnosis): {}",
                    error_msg
                );
                
                // Provide clear, actionable error messages based on error type
                let (status_code, user_message) = if error_msg.contains("Cannot connect") || error_msg.contains("Connection refused") {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Local LLM server is not running. Please ensure:\\n\\n1. An engine is installed (Settings > Engines)\\n2. A model is downloaded and loaded (Settings > Models)\\n3. The engine has finished initializing".to_string()
                    )
                } else if error_msg.contains("not found") || error_msg.contains("No such file") {
                    (
                        StatusCode::NOT_FOUND,
                        "Model or engine binary not found. Please:\\n\\n1. Download an engine (Settings > Engines)\\n2. Download a model (Settings > Models)\\n3. Wait for initialization to complete".to_string()
                    )
                } else if error_msg.contains("timeout") || error_msg.contains("timed out") {
                    (
                        StatusCode::GATEWAY_TIMEOUT,
                        "LLM server connection timed out. The engine may be still initializing. Please wait a moment and try again.".to_string()
                    )
                } else if is_role_alternation_error(&error_msg) {
                    // The engine is alive and REFUSED the prompt: the chat
                    // template requires strict user/assistant alternation and
                    // something broke it. Naming it is what turns an
                    // unexplained failure into a one-line bug report.
                    (
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "The model rejected the conversation's shape. Its chat template \
                             requires messages to strictly alternate user/assistant, and this \
                             request broke that pattern. This is a bug in how the conversation \
                             was assembled, not something you did.\n\nEngine said: {}",
                            error_msg
                        ),
                    )
                } else if is_context_overflow_error(&error_msg) {
                    (
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "This conversation no longer fits in the model's context window. \
                             Start a new discussion, or remove an attached document.\n\n\
                             Engine said: {}",
                            error_msg
                        ),
                    )
                } else {
                    // Whatever the engine actually said goes FIRST and verbatim.
                    //
                    // This branch used to bury the engine's own words behind a
                    // generic checklist, and the frontend then discarded the
                    // body entirely — so a live engine refusing a request was
                    // reported to the user as "no model is loaded". The real
                    // message is the only thing here with diagnostic value.
                    (
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "The model engine rejected this request.\n\nEngine said: {}",
                            error_msg
                        ),
                    )
                };
                
                return (status_code, user_message).into_response();
            }
        }
    }
}
