//! Unified document store API - browsing, metadata, and raw content for the
//! document viewer. Backs the "system knows what document is what, across
//! every session" browsing surface, plus raw bytes for rendering.

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::shared_state::UnifiedAppState;

#[derive(Debug, Deserialize)]
pub struct DocumentSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// POST /documents/attach — ingest a paperclip file the MOMENT it is picked.
///
/// This is what makes attachment processing independent of the send button.
/// Previously a paperclip file sat in the browser until the user typed a
/// message and pressed send, and only then was it read, extracted and stored -
/// so the wait for a scanned PDF's OCR landed squarely between "send" and the
/// first token. Now the work starts while the user is still typing, and by the
/// time they send, the content is usually already in the database.
///
/// # What this deliberately does NOT do
///
/// It does not link the document to a session. At attach time there may not BE
/// a session: a brand-new chat has no `sessions` row until the first message is
/// persisted, and `session_documents` carries a foreign key to it. Linking here
/// would fail that constraint silently for exactly the most common case - a user
/// opening a fresh chat and immediately attaching a file. (That precise bug
/// shipped once before; see stream_api's ordering of create_session_with_id.)
///
/// So the split is: this endpoint makes the document EXIST and be extracted,
/// globally and permanently; `stream_api`'s existing `document_id` branch links
/// it to the conversation when the message is actually sent. A file attached and
/// then removed is still in the library - a deliberate product decision, matching
/// how Local Storage already behaves.
///
/// Extraction runs through `utils::extraction_scheduler`'s format lanes, so
/// several files picked at once process in parallel across formats and in series
/// within one.
pub async fn attach_document(
    State(state): State<UnifiedAppState>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, StatusCode> {
    // PHASE 1 - drain the multipart stream.
    //
    // Sequential of necessity: multipart is a stream and its parts can only be
    // read in order. That is cheap (a memory copy per file) and is NOT where the
    // time goes.
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        error!("Error reading attachment field: {}", e);
        StatusCode::BAD_REQUEST
    })? {
        let file_name = field.file_name().unwrap_or("unknown_filename").to_string();

        // Same gate as every other entry point, applied before the body is read
        // so an unsupported file is never pulled into memory.
        if !crate::utils::is_supported_attachment(&file_name) {
            warn!(
                "Rejected attachment '{}': unsupported type (accepted: {})",
                file_name,
                crate::utils::supported_attachment_list()
            );
            rejected.push(file_name);
            continue;
        }

        let data = field.bytes().await.map_err(|e| {
            error!("Error reading attachment '{}': {}", file_name, e);
            StatusCode::BAD_REQUEST
        })?;
        pending.push((file_name, data.to_vec()));
    }

    // PHASE 2 - extract them all CONCURRENTLY.
    //
    // This split is the whole point. Extraction used to happen inside the loop
    // above, awaited per field, which made a 16-file attach run 16 extractions
    // strictly one after another - the format lanes were never contended and so
    // did nothing at all. Reading the parts first and then submitting the whole
    // batch is what lets the scheduler see every file at once and run one per
    // format lane in parallel.
    //
    // `None` = no cap here. The real limit is the lanes: one in-flight file per
    // format, which bounds genuinely competing work without throttling work that
    // does not compete.
    info!(
        "Processing {} attachment(s) at attach time ({} rejected); format lanes decide          what runs in parallel",
        pending.len(),
        rejected.len()
    );
    let results: Vec<serde_json::Value> = futures::future::join_all(
        pending.into_iter().map(|(file_name, data)| {
            let state = state.clone();
            async move { process_one_attachment(&state, file_name, data, "paperclip").await }
        }),
    )
    .await
    .into_iter()
    .flatten()
    .collect();

    if results.is_empty() && !rejected.is_empty() {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    Ok(Json(serde_json::json!({
        "documents": results,
        "rejected": rejected,
        "supported_types": crate::utils::SUPPORTED_ATTACHMENT_EXTENSIONS,
    })))
}

/// Extract and store one attachment. Returns the JSON summary for it, or None
/// if it could not be stored at all.
///
/// Split out of `attach_document` so the whole batch can be driven concurrently
/// - as one `async` unit per file, each of which independently waits for its own
/// format lane.
/// `source_kind` records where the bytes came from ("paperclip", "draft"), and
/// is the only thing that differs between an attached file and a saved draft -
/// both otherwise want identical treatment: hash dedup, the format lanes, the
/// same `documents` row, the same FTS index.
async fn process_one_attachment(
    state: &UnifiedAppState,
    file_name: String,
    data: Vec<u8>,
    source_kind: &str,
) -> Option<serde_json::Value> {
    let mime_type = mime_guess::from_path(&file_name).first().map(|m| m.to_string());

    // Hash first: a file already known needs no extraction at all, however
    // large. Re-attaching the same contract is then free.
    let hash = crate::memory_db::DocumentsStore::hash_bytes(&data);
    if let Ok(Some(existing)) = state.shared_state.database_pool.documents.get_document_by_hash(&hash) {
        if existing.extraction_status == "ok" && !existing.extracted_text.trim().is_empty() {
            let _ = state.shared_state.database_pool.documents.touch_document(
                existing.id,
                None,
                None,
                mime_type.as_deref(),
            );
            info!(
                "Attachment '{}' already known as document {} ({} chars) - no re-extraction",
                file_name, existing.id, existing.extracted_text.len()
            );
            return Some(serde_json::json!({
                "document_id": existing.id,
                "filename": existing.original_filename,
                "extraction_status": existing.extraction_status,
                "char_count": existing.char_count,
                "reused": true,
                // Which engine read this document, classified from the stored
                // provenance header: "vision_model" | "windows_ocr" | "native".
                // The UI flags windows_ocr images ("use a vision model for
                // handwritten content").
                "extraction_engine":
                    crate::utils::vision_extraction::extraction_engine_of(&existing.extracted_text),
            }));
        }
    }

    let (extracted_text, extraction_status, extraction_error) =
        crate::utils::extraction_outcome(
            crate::utils::extract_content_from_bytes(&data, &file_name).await,
        );
    let char_count = extracted_text.chars().count() as i64;
    let extraction_engine =
        crate::utils::vision_extraction::extraction_engine_of(&extracted_text);

    match state.shared_state.database_pool.documents.upsert_document(
        crate::memory_db::NewDocument {
            original_bytes: &data,
            original_filename: &file_name,
            source_path: None,
            source_kind,
            mime_type,
            size_bytes: data.len() as i64,
            extracted_text,
            extraction_status,
            extraction_error: extraction_error.clone(),
            local_file_id: None,
        },
    ) {
        Ok(doc) => {
            info!(
                "Attachment '{}' processed at attach time as document {}:                  extraction_status='{}', {} chars",
                file_name, doc.id, extraction_status, char_count
            );
            Some(serde_json::json!({
                "document_id": doc.id,
                "filename": doc.original_filename,
                // Surfaced so the UI can show a file that could not be read as
                // failed AT ATTACH TIME, instead of the user discovering it from
                // the model's answer.
                "extraction_status": extraction_status,
                "extraction_error": extraction_error,
                "char_count": doc.char_count,
                "reused": false,
                // "vision_model" | "windows_ocr" | "native" — see the reused
                // branch above for what the UI does with this.
                "extraction_engine": extraction_engine,
            }))
        }
        Err(e) => {
            error!("Failed to store attachment '{}': {}", file_name, e);
            None
        }
    }
}

/// Ingest raw bytes as a document, returning its id.
///
/// The document workspace's way into this pipeline. Deliberately the SAME code
/// path as a paperclip attachment rather than a second one writing the same
/// tables: a saved draft dedups by content hash, extracts through the format
/// lanes, and lands in FTS exactly like any other document, so it is
/// @-referenceable in chat the moment it is saved.
pub(crate) async fn ingest_bytes(
    state: &UnifiedAppState,
    file_name: &str,
    data: &[u8],
) -> anyhow::Result<i64> {
    let summary = process_one_attachment(state, file_name.to_string(), data.to_vec(), "draft")
        .await
        .ok_or_else(|| anyhow::anyhow!("'{}' could not be stored as a document", file_name))?;
    summary
        .get("document_id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("'{}' was stored without an id", file_name))
}

/// GET /documents?q=&limit= — list or search all known documents, globally,
/// across every session. This is the cross-session document awareness
/// surface: the system knows every document it has ever seen.
pub async fn list_documents(
    State(state): State<UnifiedAppState>,
    Query(query): Query<DocumentSearchQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let documents = &state.shared_state.database_pool.documents;
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);

    let result = match query.q.as_deref().filter(|q| !q.trim().is_empty()) {
        Some(q) => documents.search_documents(q, limit),
        None => documents.all_documents(limit),
    };

    match result {
        Ok(docs) => Ok(Json(docs)),
        Err(e) => {
            error!("Failed to list documents: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// GET /documents/session/:session_id — every document ever attached to a
/// specific session, most recently attached first.
pub async fn get_session_documents(
    State(state): State<UnifiedAppState>,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    match state.shared_state.database_pool.documents.get_session_documents(&session_id) {
        Ok(docs) => Ok(Json(docs)),
        Err(e) => {
            error!("Failed to list documents for session {}: {}", session_id, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// GET /documents/by-local-file/:local_file_id — resolve (or lazily create)
/// the unified document record for a Local Storage file. Files uploaded
/// before this store existed have no document row yet; this creates one
/// on-demand so every Local Storage file is previewable, not just new ones.
pub async fn get_or_create_document_for_local_file(
    State(state): State<UnifiedAppState>,
    Path(local_file_id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let documents = &state.shared_state.database_pool.documents;

    if let Ok(Some(doc)) = documents.get_document_by_local_file_id(local_file_id) {
        return Ok(Json(doc));
    }

    let local_files = &state.shared_state.database_pool.local_files;
    let file = local_files.get_file(local_file_id).map_err(|e| {
        warn!("Local file {} not found for document backfill: {}", local_file_id, e);
        StatusCode::NOT_FOUND
    })?;
    let bytes = local_files.get_file_content(local_file_id).map_err(|e| {
        error!("Could not read local file {} for document backfill: {}", local_file_id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Shared classifier - see utils::extraction_outcome. The local naive
    // version this replaces recorded OCR-recovered image/scanned-PDF text as
    // a failure with empty content.
    let (extracted_text, extraction_status, extraction_error) = crate::utils::extraction_outcome(
        crate::utils::extract_content_from_bytes(&bytes, &file.name).await,
    );

    let doc = documents
        .upsert_document(crate::memory_db::NewDocument {
            original_bytes: &bytes,
            original_filename: &file.name,
            source_path: None,
            source_kind: "local_storage",
            mime_type: file.mime_type.clone(),
            size_bytes: bytes.len() as i64,
            extracted_text,
            extraction_status,
            extraction_error,
            local_file_id: Some(local_file_id),
        })
        .map_err(|e| {
            error!("Failed to backfill document for local file {}: {}", local_file_id, e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(doc))
}

/// GET /documents/:id — one document's metadata + extracted text.
pub async fn get_document(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    match state.shared_state.database_pool.documents.get_document(id) {
        Ok(doc) => Ok(Json(doc)),
        Err(e) => {
            warn!("Document {} not found: {}", id, e);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// GET /documents/:id/raw — original bytes, for the document viewer
/// (PDF/DOCX/image rendering). Resolution order, each an explicit outcome:
///   1. local_file_id set → read the on-disk copy Local Storage keeps.
///   2. source_path set → best-effort read from that path (paperclip
///      attachments keep no permanent copy; this can fail if the user moved
///      or deleted the original file, which is reported by name, not hidden).
///   3. Neither → explicit 404 naming the reason.
pub async fn get_document_raw(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Response {
    let documents = &state.shared_state.database_pool.documents;
    let doc = match documents.get_document(id) {
        Ok(d) => d,
        Err(e) => {
            warn!("Document {} not found: {}", id, e);
            return (StatusCode::NOT_FOUND, "document not found").into_response();
        }
    };

    let bytes: Vec<u8> = if let Some(local_file_id) = doc.local_file_id {
        match state.shared_state.database_pool.local_files.get_file_content(local_file_id) {
            Ok(b) => b,
            Err(e) => {
                error!("Document {} raw read failed (local_file_id {}): {}", id, local_file_id, e);
                return (
                    StatusCode::NOT_FOUND,
                    format!("Stored file for '{}' could not be read: {}", doc.original_filename, e),
                )
                    .into_response();
            }
        }
    } else if let Some(ref path) = doc.source_path {
        match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                warn!("Document {} raw read failed (source_path {}): {}", id, path, e);
                return (
                    StatusCode::NOT_FOUND,
                    format!(
                        "'{}' is not available for preview: the original file at '{}' \
                         could not be read ({}). It may have been moved or deleted.",
                        doc.original_filename, path, e
                    ),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "'{}' has no stored copy or known location, so it cannot be previewed. \
                 Only its extracted text is available.",
                doc.original_filename
            ),
        )
            .into_response();
    };

    let mime = doc
        .mime_type
        .clone()
        .or_else(|| mime_guess::from_path(&doc.original_filename).first().map(|m| m.to_string()))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", doc.original_filename)),
        ],
        bytes,
    )
        .into_response()
}
