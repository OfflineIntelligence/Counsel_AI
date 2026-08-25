//! Files API endpoints - Database-backed local file management
//!
//! Provides persistent file storage with metadata in SQLite and content in app data folder.
//! Supports nested folder hierarchy and 10MB file size limit.

use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::shared_state::UnifiedAppState;
use crate::memory_db::{LocalFile, LocalFileTree};

/// Response structure for file entries (compatible with frontend)
#[derive(Debug, Serialize)]
pub struct FileEntryResponse {
    pub id: i64,
    pub name: String,
    pub path: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
    pub size: i64,
    pub modified: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FileEntryResponse>>,
}

impl From<LocalFile> for FileEntryResponse {
    fn from(f: LocalFile) -> Self {
        Self {
            id: f.id,
            name: f.name,
            path: f.path,
            is_directory: f.is_directory,
            size: f.size_bytes,
            modified: f.modified_at.to_rfc3339(),
            children: None,
        }
    }
}

impl From<LocalFileTree> for FileEntryResponse {
    fn from(t: LocalFileTree) -> Self {
        Self {
            id: t.file.id,
            name: t.file.name,
            path: t.file.path,
            is_directory: t.file.is_directory,
            size: t.file.size_bytes,
            modified: t.file.modified_at.to_rfc3339(),
            children: t.children.map(|c| c.into_iter().map(FileEntryResponse::from).collect()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderRequest {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// Absolute path on the user's machine, when captured via the native
    /// file dialog (Tauri plugin-dialog). Recorded on the unified document
    /// so Local Storage files carry provenance the same as any other.
    #[serde(default)]
    pub source_path: Option<String>,
}

/// GET /files - Get all files as a nested tree
pub async fn get_files(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.get_file_tree() {
        Ok(tree) => {
            let response: Vec<FileEntryResponse> = tree.into_iter()
                .map(FileEntryResponse::from)
                .collect();
            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get files: {}", e);
            // Return empty array on error for compatibility
            Ok(Json(Vec::<FileEntryResponse>::new()))
        }
    }
}

/// GET /files/:id - Get single file metadata
pub async fn get_file_by_id(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.get_file(id) {
        Ok(file) => Ok(Json(FileEntryResponse::from(file))),
        Err(e) => {
            error!("Failed to get file {}: {}", id, e);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// GET /files/:id/content - Get file content for LLM processing
pub async fn get_file_content(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;

    let file = match local_files.get_file(id) {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to get file {}: {}", id, e);
            return Err(StatusCode::NOT_FOUND);
        }
    };

    // Prefer the STORED extraction. Uploads extract in the background into
    // `documents`, so by the time anything asks for content it is normally
    // already there - and re-running pdfium/OCR to reproduce a result the
    // database already holds would be pure waste (seconds, for a scan).
    //
    // Reading the stored copy also makes this endpoint agree with what the
    // model actually sees, since document_memory reads the same column. A
    // live re-extraction could differ from the stored text (a repaired row, a
    // different OCR pass) and quietly present the user a preview that is not
    // what the model was given.
    match state.shared_state.database_pool.documents.get_document_by_local_file_id(id) {
        Ok(Some(doc)) if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() => {
            debug!(
                "Serving stored extraction for file {} ('{}'): {} chars",
                id, file.name, doc.extracted_text.len()
            );
            return Ok(Json(serde_json::json!({
                "id": id,
                "content": doc.extracted_text,
                "source": "stored",
            })));
        }
        Ok(_) => {
            debug!(
                "No usable stored extraction for file {} ('{}') - extracting live (background \
                 extraction may still be running, or it failed)",
                id, file.name
            );
        }
        Err(e) => warn!(
            "Document lookup failed for file {} ('{}'): {} - extracting live",
            id, file.name, e
        ),
    }

    // Fall back to extracting on demand. Reached when the background pass has
    // not finished (or failed) - this endpoint must still answer rather than
    // report an empty document.
    match local_files.get_file_content(id) {
        Ok(bytes) => {
            match crate::utils::extract_content_from_bytes(&bytes, &file.name).await {
                Ok(content) => Ok(Json(serde_json::json!({
                    "id": id,
                    "content": content,
                    "source": "live",
                }))),
                Err(e) => {
                    error!("Extraction failed for file {} ({}): {}", id, file.name, e);
                    Ok(Json(serde_json::json!({
                        "id": id,
                        "content": format!("[Could not extract '{}': {}]", file.name, e),
                        "source": "live",
                    })))
                }
            }
        }
        Err(e) => {
            error!("Failed to get file content {}: {}", id, e);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// GET /files/search?q=... - Search files by name
pub async fn search_files(
    State(state): State<UnifiedAppState>,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.search_files(&query.q) {
        Ok(files) => {
            let response: Vec<FileEntryResponse> = files.into_iter()
                .map(FileEntryResponse::from)
                .collect();
            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to search files: {}", e);
            Ok(Json(Vec::<FileEntryResponse>::new()))
        }
    }
}

/// GET /files/all - Get flat list of all files (for @filename autocomplete)
pub async fn get_all_files(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.get_all_files() {
        Ok(files) => {
            let response: Vec<FileEntryResponse> = files.into_iter()
                .map(FileEntryResponse::from)
                .collect();
            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to get all files: {}", e);
            Ok(Json(Vec::<FileEntryResponse>::new()))
        }
    }
}

/// POST /files/folder - Create a new folder
pub async fn create_folder(
    State(state): State<UnifiedAppState>,
    Json(request): Json<CreateFolderRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    if request.name.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    match local_files.create_folder(request.parent_id, &request.name) {
        Ok(folder) => {
            info!("Created folder: {}", folder.path);
            Ok(Json(serde_json::json!({
                "message": "Folder created successfully",
                "id": folder.id,
                "path": folder.path
            })))
        }
        Err(e) => {
            error!("Failed to create folder '{}': {}", request.name, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Extract a just-uploaded Local Storage file's text and record it in the
/// unified document store, off the request path.
///
/// Runs under the per-file extraction permit and RE-CHECKS the database after
/// acquiring it, because the user may have attached this same file in the
/// meantime and the attach path may already have done the work. Without that
/// re-check the lock would merely delay a duplicate OCR pass rather than
/// prevent it (see utils::extraction_coordinator).
///
/// Deliberately fire-and-forget with no completion signal to the client: the
/// upload is already complete and durable at this point. Everything here is
/// recoverable work, so a failure is logged at `error!` with the consequence
/// spelled out rather than surfaced as a failed upload the user would have to
/// retry pointlessly.
// `pub(crate)` so stream_api's tests can drive a real background extraction
// against the real attach paths, which is the only way to exercise the
// upload/reference race for what it is.
pub(crate) fn spawn_background_extraction(
    state: UnifiedAppState,
    local_file_id: i64,
    file_name: String,
    data: Vec<u8>,
    mime_type: Option<String>,
    source_path: Option<String>,
) {
    tokio::spawn(async move {
        let documents = &state.shared_state.database_pool.documents;
        let _permit = state
            .shared_state
            .extraction_coordinator
            .acquire(local_file_id)
            .await;

        // Re-check under the permit: an attach may have beaten us to it.
        match documents.get_document_by_local_file_id(local_file_id) {
            Ok(Some(doc))
                if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() =>
            {
                debug!(
                    "Background extraction for '{}' (local_file_id {}) skipped - already \
                     extracted ({} chars), most likely by an attach that raced this task",
                    file_name, local_file_id, doc.extracted_text.len()
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    "Background extraction for '{}': document lookup failed ({}) - \
                     extracting anyway; a duplicate hash would be deduped by upsert",
                    file_name, e
                );
            }
        }

        // Shared classifier (utils::extraction_outcome), NOT a local
        // "starts with '[' means failure" rule: an image or scanned PDF is
        // extracted via OCR into a "[header]\n<real text>" message, and the
        // naive rule recorded those as failed with empty text - permanently,
        // since upsert_document never re-extracts a known hash. That made an
        // image uploaded to Local Storage invisible to the model forever,
        // while the identical file attached by paperclip worked fine.
        let (extracted_text, extraction_status, extraction_error) =
            crate::utils::extraction_outcome(
                crate::utils::extract_content_from_bytes(&data, &file_name).await,
            );
        let char_count = extracted_text.len();

        match documents.upsert_document(crate::memory_db::NewDocument {
            original_bytes: &data,
            original_filename: &file_name,
            source_path,
            source_kind: "local_storage",
            mime_type,
            size_bytes: data.len() as i64,
            extracted_text,
            extraction_status,
            extraction_error,
            local_file_id: Some(local_file_id),
        }) {
            Ok(doc) => info!(
                "Background extraction complete for '{}' (local_file_id {}, document {}): \
                 {} bytes, extraction_status='{}', {} chars indexed",
                file_name, local_file_id, doc.id, data.len(), extraction_status, char_count
            ),
            Err(e) => error!(
                "Background extraction FAILED to record '{}' (local_file_id {}): {} - the \
                 file is stored and safe, but will not be searchable until it is attached \
                 to a chat, which re-extracts it",
                file_name, local_file_id, e
            ),
        }
    });
}

/// How many files one startup backfill pass will process. A bound, not a
/// cap on eventual coverage: whatever is left is picked up by the next
/// startup, or by the attach-time path if the user gets there first. The
/// point is that a large unextracted backlog must not saturate the blocking
/// pool (OCR) while the user is trying to hold a conversation.
const BACKFILL_BATCH: i64 = 50;

/// Finish extraction for Local Storage files that never got it.
///
/// Called once at startup. Uploads extract in a background task, and a
/// background task dies with the process - so uploading a file and closing the
/// app moments later leaves bytes in the vault with no `documents` row. Nothing
/// would ever revisit that file unless the user happened to attach it, which
/// means an uploaded file could be permanently invisible to retrieval. This is
/// the pass that makes background extraction durable rather than best-effort.
///
/// Deliberately sequential. These files are, by definition, ones nobody is
/// waiting on; extracting them one at a time keeps OCR off the critical path
/// while the user chats. Each goes through the same permit as every other
/// extraction, so a file the user attaches mid-backfill is never
/// double-extracted.
pub fn spawn_startup_backfill(state: UnifiedAppState) {
    tokio::spawn(async move {
        let documents = &state.shared_state.database_pool.documents;
        let pending = match documents.local_files_needing_extraction(BACKFILL_BATCH) {
            Ok(p) => p,
            Err(e) => {
                warn!("Local Storage extraction backfill could not run: {}", e);
                return;
            }
        };
        if pending.is_empty() {
            debug!("Local Storage extraction backfill: nothing to do");
            return;
        }
        info!(
            "Local Storage extraction backfill: {} file(s) have no usable extraction - \
             processing sequentially in the background",
            pending.len()
        );

        let mut done = 0usize;
        let mut failed = 0usize;
        for (local_file_id, name) in pending {
            let bytes = match state
                .shared_state
                .database_pool
                .local_files
                .get_file_content(local_file_id)
            {
                Ok(b) => b,
                Err(e) => {
                    // The metadata row exists but the bytes do not. Real and
                    // worth naming: a vault file was removed out from under us.
                    warn!(
                        "Backfill skipped '{}' (local_file_id {}): content unreadable: {}",
                        name, local_file_id, e
                    );
                    failed += 1;
                    continue;
                }
            };
            let mime_type = mime_guess::from_path(&name).first().map(|m| m.to_string());

            let _permit = state
                .shared_state
                .extraction_coordinator
                .acquire(local_file_id)
                .await;
            // Re-check under the permit - an attach may have handled it while
            // this pass worked through earlier files.
            if let Ok(Some(doc)) = documents.get_document_by_local_file_id(local_file_id) {
                if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() {
                    debug!("Backfill: '{}' was extracted by an attach meanwhile", name);
                    continue;
                }
            }

            let (extracted_text, extraction_status, extraction_error) =
                crate::utils::extraction_outcome(
                    crate::utils::extract_content_from_bytes(&bytes, &name).await,
                );
            match documents.upsert_document(crate::memory_db::NewDocument {
                original_bytes: &bytes,
                original_filename: &name,
                source_path: None,
                source_kind: "local_storage",
                mime_type,
                size_bytes: bytes.len() as i64,
                extracted_text,
                extraction_status,
                extraction_error,
                local_file_id: Some(local_file_id),
            }) {
                Ok(_) if extraction_status == "ok" => done += 1,
                Ok(_) => {
                    // Recorded, but with nothing usable in it. Named here
                    // because it will be retried on every future startup
                    // otherwise silently.
                    warn!(
                        "Backfill extracted no usable text from '{}' (local_file_id {})",
                        name, local_file_id
                    );
                    failed += 1;
                }
                Err(e) => {
                    error!(
                        "Backfill failed to record '{}' (local_file_id {}): {}",
                        name, local_file_id, e
                    );
                    failed += 1;
                }
            }
        }
        info!(
            "Local Storage extraction backfill finished: {} indexed, {} could not be extracted",
            done, failed
        );
    });
}

/// POST /files/upload - Upload files
pub async fn upload_file(
    State(state): State<UnifiedAppState>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;

    let mut file_count = 0;
    // Files refused because their type is outside the product's supported
    // set. Collected and REPORTED rather than dropped: the frontend picker
    // filters too, so anything arriving here came via drag-and-drop, a folder
    // upload, or a direct API call - all cases where the user has no other
    // way to learn the file was ignored.
    let mut rejected: Vec<String> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        error!("Error reading multipart field: {}", e);
        StatusCode::BAD_REQUEST
    })? {
        let file_name = field.file_name().unwrap_or("unknown_filename").to_string();

        // Format gate, server-side. The picker is a convenience; THIS is the
        // boundary. Checked before reading the body so an unsupported 2 GB
        // file is not pulled into memory just to be discarded.
        if !crate::utils::is_supported_attachment(&file_name) {
            warn!(
                "Rejected Local Storage upload '{}': unsupported type (accepted: {})",
                file_name,
                crate::utils::supported_attachment_list()
            );
            rejected.push(file_name);
            continue;
        }

        // Get file data
        let data = field.bytes().await.map_err(|e| {
            error!("Error reading file {}: {}", file_name, e);
            StatusCode::BAD_REQUEST
        })?;

        // Determine MIME type
        let mime_type = mime_guess::from_path(&file_name)
            .first()
            .map(|m| m.to_string());

        // Upload file to database-backed storage
        match local_files.upload_file(query.parent_id, &file_name, &data, mime_type.as_deref()) {
            Ok(file) => {
                info!("Uploaded file: {} ({} bytes)", file.path, data.len());
                file_count += 1;

                // Local Storage is a VAULT: storing the bytes is the whole job
                // of this request, and it completes here. Text extraction is
                // dispatched to run AFTER the response, because it is the one
                // genuinely slow step in the system - a scanned PDF goes
                // through pdfium rasterization plus Windows OCR, up to 50
                // pages. Doing that inline (as this handler used to) meant a
                // multi-file upload of scans held the HTTP request open for
                // tens of seconds with no progress signal, and the whole batch
                // was serialized because this loop awaited each extraction
                // before reading the next multipart field.
                //
                // Extraction still HAPPENS, and its result is still stored -
                // that is what makes an uploaded file findable by the
                // retrieval layer without ever being attached to a chat. The
                // only thing that changed is that the user is not made to
                // wait for it.
                //
                // Correctness does not depend on this task completing: if the
                // process exits mid-extraction, or extraction fails, the
                // documents row is simply absent or unusable, and the
                // attach-time path in stream_api (persist_local_storage_attachment)
                // extracts and repairs it on first use. That backstop is the
                // guarantee; this task is the optimisation that usually makes
                // it unnecessary.
                spawn_background_extraction(
                    state.clone(),
                    file.id,
                    file_name.clone(),
                    data.to_vec(),
                    mime_type.clone(),
                    query.source_path.clone(),
                );
            }
            Err(e) => {
                error!("Failed to upload file {}: {}", file_name, e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    // A batch where EVERY file was refused is a failed request, not a
    // successful upload of zero files - returning 200 there would let the UI
    // report success for an empty result.
    if file_count == 0 && !rejected.is_empty() {
        error!(
            "Local Storage upload rejected entirely: {} unsupported file(s): {}",
            rejected.len(),
            rejected.join(", ")
        );
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    Ok(Json(serde_json::json!({
        "message": if rejected.is_empty() {
            format!("Successfully uploaded {} file(s)", file_count)
        } else {
            format!(
                "Uploaded {} file(s); skipped {} unsupported file(s). Supported types: {}",
                file_count, rejected.len(), crate::utils::supported_attachment_list()
            )
        },
        "count": file_count,
        // Named, not just counted - the UI can list exactly what was skipped.
        "rejected": rejected,
        "supported_types": crate::utils::SUPPORTED_ATTACHMENT_EXTENSIONS,
    })))
}

/// DELETE /files - Delete a file or folder (by id or path query param)
pub async fn delete_file(
    State(state): State<UnifiedAppState>,
    Query(query): Query<DeleteQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    // Get file ID either directly or by path lookup
    let file_id = if let Some(id) = query.id {
        id
    } else if let Some(path) = &query.path {
        match local_files.get_file_by_path(path) {
            Ok(file) => file.id,
            Err(e) => {
                error!("File not found at path {}: {}", path, e);
                return Err(StatusCode::NOT_FOUND);
            }
        }
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };
    
    match local_files.delete_file(file_id) {
        Ok(()) => {
            info!("Deleted file/folder with id {}", file_id);
            Ok(Json(serde_json::json!({
                "message": "File/directory deleted successfully"
            })))
        }
        Err(e) => {
            error!("Failed to delete file {}: {}", file_id, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// DELETE /files/:id - Delete a file or folder by ID
pub async fn delete_file_by_id(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.delete_file(id) {
        Ok(()) => {
            info!("Deleted file/folder with id {}", id);
            Ok(Json(serde_json::json!({
                "message": "File/directory deleted successfully"
            })))
        }
        Err(e) => {
            error!("Failed to delete file {}: {}", id, e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// POST /files/sync - Sync filesystem with database (import existing files)
pub async fn sync_files(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    match local_files.sync_from_filesystem() {
        Ok(count) => {
            info!("Synced {} files from filesystem", count);
            Ok(Json(serde_json::json!({
                "message": format!("Synced {} files from filesystem", count),
                "count": count
            })))
        }
        Err(e) => {
            error!("Failed to sync files: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// POST /files/resync - Clear database and resync from filesystem (fixes duplicates)
pub async fn resync_files(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let local_files = &state.shared_state.database_pool.local_files;
    
    // First clear all entries
    match local_files.clear_all() {
        Ok(cleared) => {
            info!("Cleared {} entries from local_files", cleared);
        }
        Err(e) => {
            error!("Failed to clear local_files: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    // Then resync from filesystem
    match local_files.sync_from_filesystem() {
        Ok(count) => {
            info!("Resynced {} files from filesystem", count);
            Ok(Json(serde_json::json!({
                "message": format!("Cleared and resynced {} files from filesystem", count),
                "count": count
            })))
        }
        Err(e) => {
            error!("Failed to sync files: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::shared_state::SharedState;
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> UnifiedAppState {
        let cfg = Config::from_env().expect("Config::from_env should succeed with defaults");
        let database = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        let shared_state = Arc::new(SharedState::new(cfg, database).expect("SharedState::new"));
        UnifiedAppState::new(shared_state)
    }

    /// Poll for the documents row a background extraction should produce.
    /// Bounded, so a genuine failure fails the test instead of hanging it.
    async fn await_document(
        state: &UnifiedAppState,
        local_file_id: i64,
    ) -> Option<crate::memory_db::DocumentRecord> {
        for _ in 0..150 {
            if let Ok(Some(doc)) = state
                .shared_state
                .database_pool
                .documents
                .get_document_by_local_file_id(local_file_id)
            {
                if doc.extraction_status == "ok" && !doc.extracted_text.trim().is_empty() {
                    return Some(doc);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    /// The F2 contract: storing bytes and extracting text are now SEPARATE
    /// steps. Upload records the file; a background pass fills in the
    /// searchable content afterwards, without the user waiting for it.
    ///
    /// This is what makes an uploaded-but-never-attached file findable by the
    /// retrieval layer, so it underpins the "database as knowledge" behaviour
    /// for Local Storage.
    #[tokio::test]
    async fn background_extraction_indexes_an_uploaded_file() {
        let state = test_state().await;
        let body = b"The indemnity cap under this agreement is five million dollars.";

        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "agreement.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        spawn_background_extraction(
            state.clone(),
            file.id,
            "agreement.txt".to_string(),
            body.to_vec(),
            Some("text/plain".to_string()),
            None,
        );

        let doc = await_document(&state, file.id)
            .await
            .expect("background extraction must eventually record the document");
        assert!(
            doc.extracted_text.contains("five million dollars"),
            "stored extraction must hold the real content: {:?}",
            doc.extracted_text
        );
        // The full column set the product promises to record.
        assert_eq!(doc.source_kind, "local_storage");
        assert_eq!(doc.local_file_id, Some(file.id));
        assert_eq!(doc.original_filename, "agreement.txt");
        assert_eq!(doc.size_bytes, body.len() as i64);
        assert!(doc.char_count > 0, "char_count must be recorded");
        assert!(!doc.content_hash.is_empty(), "content hash must be recorded");
    }

    /// The race F2 introduces, and one users hit constantly: upload a file,
    /// then attach it to a chat while the background extraction is still in
    /// flight.
    ///
    /// Both paths want to extract and both want to write. The requirement is
    /// exactly ONE document row, correct content, and a working session link -
    /// never a duplicate, never an empty row, never a lost attachment.
    #[tokio::test]
    async fn attaching_during_background_extraction_yields_one_correct_document() {
        let state = test_state().await;
        let session_id = "race-session";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let body = b"Section 7: termination requires thirty days written notice.";
        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "terms.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        // Kick off the background pass and attach in the same breath.
        spawn_background_extraction(
            state.clone(),
            file.id,
            "terms.txt".to_string(),
            body.to_vec(),
            Some("text/plain".to_string()),
            None,
        );
        crate::api::stream_api::persist_local_storage_attachment(session_id, file.id, &state).await;

        // Let the background task finish so a duplicate would be observable.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let matching: Vec<_> = state
            .shared_state
            .database_pool
            .documents
            .all_documents(100)
            .unwrap()
            .into_iter()
            .filter(|d| d.original_filename == "terms.txt")
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "exactly one document row expected, got ids {:?}",
            matching.iter().map(|d| d.id).collect::<Vec<_>>()
        );
        assert!(
            matching[0].extracted_text.contains("thirty days written notice"),
            "content must be present and correct: {:?}",
            matching[0].extracted_text
        );

        // And the attachment must actually be visible to the model this turn.
        let session_docs = state
            .shared_state
            .database_pool
            .documents
            .get_session_documents(session_id)
            .unwrap();
        assert_eq!(session_docs.len(), 1, "the file must be linked to the session");
        assert!(session_docs[0]
            .extracted_text
            .contains("thirty days written notice"));
    }

    /// A background extraction for a file ALREADY extracted by an attach must
    /// skip its own work rather than redo it. This is the
    /// double-checked-locking half of the coordinator contract - without the
    /// re-check, the permit would only delay a duplicate OCR pass.
    #[tokio::test]
    async fn background_extraction_skips_work_already_done_by_an_attach() {
        let state = test_state().await;
        let session_id = "already-done";
        state
            .shared_state
            .database_pool
            .conversations
            .create_session_with_id(session_id, None)
            .ok();

        let body = b"Governing law is the State of Delaware.";
        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "law.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        // Attach FIRST, so the document row exists and is usable.
        crate::api::stream_api::persist_local_storage_attachment(session_id, file.id, &state).await;
        let before = state
            .shared_state
            .database_pool
            .documents
            .get_document_by_local_file_id(file.id)
            .unwrap()
            .expect("attach must have created the document");

        // Now run the background pass that upload would have started.
        spawn_background_extraction(
            state.clone(),
            file.id,
            "law.txt".to_string(),
            body.to_vec(),
            Some("text/plain".to_string()),
            None,
        );
        tokio::time::sleep(Duration::from_millis(400)).await;

        let after = state
            .shared_state
            .database_pool
            .documents
            .get_document_by_local_file_id(file.id)
            .unwrap()
            .expect("the document must still be there");
        assert_eq!(before.id, after.id, "no second row may be created");
        assert_eq!(
            before.extracted_text, after.extracted_text,
            "the existing extraction must be left untouched"
        );
    }

    /// Once a usable extraction is stored it is the single source of truth for
    /// this file's content - the same column document_memory reads, so a
    /// preview cannot disagree with what the model was given.
    #[tokio::test]
    async fn stored_extraction_is_available_for_the_content_endpoint() {
        let state = test_state().await;
        let body = b"Exhibit A contains the fee schedule.";
        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "exhibit.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        spawn_background_extraction(
            state.clone(),
            file.id,
            "exhibit.txt".to_string(),
            body.to_vec(),
            Some("text/plain".to_string()),
            None,
        );
        let doc = await_document(&state, file.id)
            .await
            .expect("document must be indexed");

        assert_eq!(doc.extraction_status, "ok");
        assert!(doc.extracted_text.contains("fee schedule"));
    }

    /// Durability: an upload whose background extraction never ran (app closed
    /// moments after upload) must be recovered at startup.
    ///
    /// Without this pass such a file has bytes in the vault and no `documents`
    /// row, so it is invisible to retrieval FOREVER unless the user happens to
    /// attach it to a chat - which defeats the whole point of indexing
    /// uploaded-but-unattached files.
    ///
    /// Simulated exactly: the file is written to the vault and the background
    /// task is deliberately NOT started, which is precisely the state a killed
    /// process leaves behind.
    #[tokio::test]
    async fn startup_backfill_recovers_an_upload_whose_extraction_never_ran() {
        let state = test_state().await;
        let body = b"Clause 12 caps aggregate liability at the fees paid.";

        let file = state
            .shared_state
            .database_pool
            .local_files
            .upload_file(None, "liability.txt", body, Some("text/plain"))
            .expect("vault write must succeed");

        // Precondition: the file is stored but has NO extraction - the exact
        // state a process kill mid-extraction leaves behind.
        assert!(
            state
                .shared_state
                .database_pool
                .documents
                .get_document_by_local_file_id(file.id)
                .unwrap()
                .is_none(),
            "precondition: no document row yet"
        );
        let pending = state
            .shared_state
            .database_pool
            .documents
            .local_files_needing_extraction(50)
            .unwrap();
        assert!(
            pending.iter().any(|(id, _)| *id == file.id),
            "the un-extracted file must be reported as needing work: {:?}",
            pending
        );

        spawn_startup_backfill(state.clone());

        let doc = await_document(&state, file.id)
            .await
            .expect("startup backfill must index the orphaned upload");
        assert!(
            doc.extracted_text.contains("caps aggregate liability"),
            "backfill must store the real content: {:?}",
            doc.extracted_text
        );

        // And it must no longer be reported as needing work, or every startup
        // would redo it forever.
        let after = state
            .shared_state
            .database_pool
            .documents
            .local_files_needing_extraction(50)
            .unwrap();
        assert!(
            !after.iter().any(|(id, _)| *id == file.id),
            "an extracted file must not be re-queued on the next startup: {:?}",
            after
        );
    }

    /// The backfill must not pick up work that does not exist or cannot be
    /// done: folders have no content, and unsupported types must not be
    /// re-extracted into mojibake just because they predate the format gate.
    #[tokio::test]
    async fn backfill_ignores_folders_and_unsupported_legacy_files() {
        let state = test_state().await;
        let local_files = &state.shared_state.database_pool.local_files;

        let folder = local_files
            .create_folder(None, "Contracts")
            .expect("folder creation must succeed");
        // Written directly to the vault, bypassing the API's format gate - the
        // shape of a row uploaded before that gate existed.
        let legacy = local_files
            .upload_file(None, "old-archive.zip", b"PK\x03\x04 binary", Some("application/zip"))
            .expect("vault write must succeed");
        let supported = local_files
            .upload_file(None, "valid.txt", b"Real readable content here.", Some("text/plain"))
            .expect("vault write must succeed");

        let pending = state
            .shared_state
            .database_pool
            .documents
            .local_files_needing_extraction(50)
            .unwrap();

        assert!(
            pending.iter().any(|(id, _)| *id == supported.id),
            "the supported file must be queued: {:?}",
            pending
        );
        assert!(
            !pending.iter().any(|(id, _)| *id == folder.id),
            "a folder has nothing to extract: {:?}",
            pending
        );
        assert!(
            !pending.iter().any(|(id, _)| *id == legacy.id),
            "an unsupported legacy file must not be re-extracted into garbage: {:?}",
            pending
        );
    }
}
