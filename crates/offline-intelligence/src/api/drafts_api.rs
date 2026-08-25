//! Document workspace API — drafts, versions, patches and rendered pages.
//!
//! # The Vault is read-only from here
//!
//! Creating a draft from a Vault document COPIES its bytes. Nothing in this
//! module writes to `documents` or `local_files` except `publish`, which the
//! user asks for by name. A draft going wrong cannot damage the library.
//!
//! # Optimistic concurrency
//!
//! Every patch carries `base_version`. If it does not match the draft's current
//! version the request is refused with 409 and the current view model, so the
//! client can rebase rather than silently double-apply. Two workspace tabs, or
//! a retry after a response was lost to a suspended network, both land here.

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::document_workspace::view_model::{DraftPatch, DraftViewModel};
use crate::document_workspace::DraftManager;
use crate::memory_db::drafts_store::{DraftRecord, DraftVersionRecord};
use crate::shared_state::UnifiedAppState;

/// How long edits keep amending the current version instead of cutting a new
/// one.
///
/// Debounced autosave fires every few seconds; without this a ten-minute
/// editing session would leave a hundred versions and make the history useless
/// as an undo surface. Five minutes matches how a person actually thinks about
/// "a change" — see `document_workspace::DraftManager::apply_patches`.
const AMEND_WINDOW_SECS: i64 = 300;

/// Test-only redirection of where draft files live, so tests never write into
/// the developer's real `AppData\Local\Offline Counsel AI\drafts`.
///
/// A `RwLock` rather than a `OnceLock` because each test needs its OWN
/// directory, not one shared for the whole process. Draft ids come from an
/// in-memory database that restarts at 1 for every test, so a shared directory
/// means every test writes to `drafts/1/` — and one test deleting its draft
/// deletes another's files out from under it.
///
/// In production this is never written, so the read is one uncontended
/// `RwLock` acquisition per call and the value is always `None`.
static APP_DATA_OVERRIDE: std::sync::RwLock<Option<std::path::PathBuf>> =
    std::sync::RwLock::new(None);

fn manager() -> DraftManager {
    let override_dir = APP_DATA_OVERRIDE
        .read()
        .ok()
        .and_then(|guard| guard.clone());
    DraftManager::new(override_dir.unwrap_or_else(crate::config::get_app_data_dir))
}

#[cfg(test)]
fn set_app_data_override(path: std::path::PathBuf) {
    if let Ok(mut guard) = APP_DATA_OVERRIDE.write() {
        *guard = Some(path);
    }
}

/// Structured error, matching the shape `/models/switch` established.
///
/// A bare status code tells the UI nothing it can show a user. `action` is the
/// one sentence the user can actually act on.
fn fail(status: StatusCode, code: &str, detail: impl std::fmt::Display, action: &str) -> Response {
    (
        status,
        Json(json!({ "error": code, "detail": detail.to_string(), "action": action })),
    )
        .into_response()
}

/// Render an `anyhow` error with its whole cause chain.
///
/// `Display` on an `anyhow::Error` shows ONLY the outermost context, so
/// "'sealed.docx' cannot be opened for editing" would reach the user while the
/// sentence telling them what to do about it — "if it is password-protected,
/// remove the protection" — stayed buried one level down. The `{:#}` form joins
/// the chain, and every user-facing message here goes through it.
fn full(e: &anyhow::Error) -> String {
    format!("{:#}", e)
}

fn server_error(context: &str, e: impl std::fmt::Display) -> Response {
    error!("{}: {}", context, e);
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "draft_operation_failed",
        e,
        "The draft was not changed. Try again, or reopen it from the drafts list.",
    )
}

fn not_found(id: i64) -> Response {
    fail(
        StatusCode::NOT_FOUND,
        "draft_not_found",
        format!("Draft {} does not exist.", id),
        "It may have been deleted. Return to the drafts list.",
    )
}

// ---------------------------------------------------------------- shapes

#[derive(Debug, Serialize)]
pub struct DraftSummary {
    pub id: i64,
    pub title: String,
    pub format: String,
    pub origin_kind: String,
    pub source_document_id: Option<i64>,
    pub current_version: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<DraftRecord> for DraftSummary {
    fn from(d: DraftRecord) -> Self {
        Self {
            id: d.id,
            title: d.title,
            format: d.format,
            origin_kind: d.origin_kind,
            source_document_id: d.source_document_id,
            current_version: d.current_version,
            created_at: d.created_at.to_rfc3339(),
            updated_at: d.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VersionSummary {
    pub version_no: i64,
    pub byte_size: i64,
    pub label: Option<String>,
    pub created_at: String,
    pub is_current: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateFromVault {
    pub document_id: i64,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenameBody {
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchBody {
    pub patches: Vec<DraftPatch>,
    /// The version the client's view model was built from.
    pub base_version: i64,
    /// True on an explicit save (Ctrl-S, closing, switching sheet), which cuts
    /// a new version regardless of the amend window.
    #[serde(default)]
    pub explicit_save: bool,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PatchResult {
    pub version: i64,
    pub model: DraftViewModel,
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub scale: Option<f32>,
}

// ---------------------------------------------------------------- lifecycle

/// POST /drafts — from a Vault document.
pub async fn create_from_vault(
    State(state): State<UnifiedAppState>,
    Json(body): Json<CreateFromVault>,
) -> Response {
    let db = &state.shared_state.database_pool;

    let doc = match db.documents.get_document(body.document_id) {
        Ok(d) => d,
        Err(e) => {
            return fail(
                StatusCode::NOT_FOUND,
                "source_not_found",
                format!("Document {} could not be read: {}", body.document_id, e),
                "Choose a different file from the Vault.",
            )
        }
    };

    // Read the stored bytes the same way the document viewer does.
    let bytes = match read_document_bytes(&state, &doc) {
        Ok(b) => b,
        Err(e) => {
            return fail(
                StatusCode::NOT_FOUND,
                "source_unreadable",
                &full(&e),
                "Only the extracted text of this file is available, so it cannot be edited.",
            )
        }
    };

    let title = body.title.unwrap_or_else(|| strip_extension(&doc.original_filename));
    let created = tokio::task::spawn_blocking({
        let db = state.shared_state.database_pool.clone();
        let filename = doc.original_filename.clone();
        let title = title.clone();
        move || {
            manager().create(
                &db,
                &title,
                &filename,
                &bytes,
                "vault",
                Some(body.document_id),
                doc.local_file_id,
            )
        }
    })
    .await;

    match created {
        Ok(Ok(draft)) => (StatusCode::CREATED, Json(DraftSummary::from(draft))).into_response(),
        // A refusal here is a policy decision the user can act on (legacy
        // format, too large, password-protected), not a server fault.
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_editable",
            &full(&e),
            "Open it in the document viewer instead, or convert it and try again.",
        ),
        Err(e) => server_error("draft creation task panicked", e),
    }
}

/// POST /drafts/upload — from a file picked in the workspace.
pub async fn create_from_upload(
    State(state): State<UnifiedAppState>,
    mut multipart: Multipart,
) -> Response {
    let mut file: Option<(String, Vec<u8>)> = None;

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.file_name().unwrap_or("untitled").to_string();
                match field.bytes().await {
                    Ok(b) => file = Some((name, b.to_vec())),
                    Err(e) => {
                        return fail(
                            StatusCode::BAD_REQUEST,
                            "upload_unreadable",
                            e,
                            "Try picking the file again.",
                        )
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    "upload_unreadable",
                    e,
                    "Try picking the file again.",
                )
            }
        }
    }

    let Some((filename, bytes)) = file else {
        return fail(
            StatusCode::BAD_REQUEST,
            "no_file",
            "The request carried no file.",
            "Pick a document to open.",
        );
    };

    let title = strip_extension(&filename);
    let db = state.shared_state.database_pool.clone();
    match tokio::task::spawn_blocking(move || {
        manager().create(&db, &title, &filename, &bytes, "upload", None, None)
    })
    .await
    {
        Ok(Ok(draft)) => (StatusCode::CREATED, Json(DraftSummary::from(draft))).into_response(),
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_editable",
            &full(&e),
            "Convert the file to a supported format and try again.",
        ),
        Err(e) => server_error("upload draft task panicked", e),
    }
}

/// POST /drafts/blank — a new empty document.
pub async fn create_blank(
    State(state): State<UnifiedAppState>,
    Json(body): Json<BlankBody>,
) -> Response {
    let format = body.format.to_lowercase();

    // Seed bytes for each format we can synthesise from nothing.
    //
    // Only text, for now. A blank DOCX/XLSX/PPTX is not an empty file — it is a
    // complete OOXML package with content types, relationships, styles and a
    // theme. Generating one belongs in a shipped template, not in a
    // constructor, and inventing a minimal one here would produce a document
    // Word opens with a repair prompt.
    //
    // The single newline is deliberate: `create` refuses empty bytes (an empty
    // file is almost always a failed read), so a blank note starts as one
    // empty line rather than as nothing at all.
    let seed: &[u8] = match format.as_str() {
        "txt" => b"\n",
        other => {
            return fail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "blank_unsupported",
                format!(
                    "A blank {} cannot be created yet — only plain text.",
                    other.to_uppercase()
                ),
                "Open an existing file of that type instead.",
            )
        }
    };

    let filename = format!("{}.{}", body.title, format);
    let db = state.shared_state.database_pool.clone();
    let title = body.title.clone();
    match tokio::task::spawn_blocking(move || {
        manager().create(&db, &title, &filename, seed, "blank", None, None)
    })
    .await
    {
        Ok(Ok(draft)) => (StatusCode::CREATED, Json(DraftSummary::from(draft))).into_response(),
        Ok(Err(e)) => server_error("blank draft creation failed", e),
        Err(e) => server_error("blank draft task panicked", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct BlankBody {
    pub title: String,
    pub format: String,
}

/// GET /drafts
pub async fn list_drafts(State(state): State<UnifiedAppState>) -> Response {
    match state.shared_state.database_pool.drafts.list_drafts() {
        Ok(drafts) => {
            let out: Vec<DraftSummary> = drafts.into_iter().map(Into::into).collect();
            Json(json!({ "drafts": out })).into_response()
        }
        Err(e) => server_error("listing drafts failed", e),
    }
}

/// GET /drafts/:id
pub async fn get_draft(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    match state.shared_state.database_pool.drafts.get_draft(id) {
        Ok(Some(d)) => Json(DraftSummary::from(d)).into_response(),
        Ok(None) => not_found(id),
        Err(e) => server_error("reading a draft failed", e),
    }
}

/// PATCH /drafts/:id — rename.
pub async fn rename_draft(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameBody>,
) -> Response {
    let title = body.title.trim();
    if title.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            "empty_title",
            "A draft needs a name.",
            "Type a name and try again.",
        );
    }
    match state.shared_state.database_pool.drafts.rename_draft(id, title) {
        Ok(()) => Json(json!({ "ok": true, "title": title })).into_response(),
        Err(e) => server_error("renaming a draft failed", e),
    }
}

/// DELETE /drafts/:id — draft, versions and every byte it owns.
pub async fn delete_draft(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    match tokio::task::spawn_blocking(move || manager().delete(&db, id)).await {
        Ok(Ok(())) => {
            info!("Deleted draft {}", id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => server_error("deleting a draft failed", e),
        Err(e) => server_error("draft deletion task panicked", e),
    }
}

// ---------------------------------------------------------------- editing

/// GET /drafts/:id/content — the format-specific view model.
pub async fn get_content(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    // Checked before building the model so a draft that does not exist reports
    // 404 rather than "unreadable" — the two mean very different things to the
    // UI, which recovers from one and offers a version restore for the other.
    match db.drafts.get_draft(id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    }
    match tokio::task::spawn_blocking(move || manager().view_model(&db, id)).await {
        Ok(Ok(model)) => Json(model).into_response(),
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "draft_unreadable",
            &full(&e),
            "The draft's file may be missing. Restore an earlier version from the history.",
        ),
        Err(e) => server_error("view model task panicked", e),
    }
}

/// POST /drafts/:id/patch — apply edits and return the refreshed model.
pub async fn apply_patch(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchBody>,
) -> Response {
    let db = state.shared_state.database_pool.clone();

    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };

    // The version guard. Refusing here is what stops a second tab, or a retried
    // request whose first response was lost, from applying the same edit twice.
    if body.base_version != draft.current_version {
        warn!(
            "Draft {} patch rejected: client at v{}, server at v{}",
            id, body.base_version, draft.current_version
        );
        let db2 = db.clone();
        let model = tokio::task::spawn_blocking(move || manager().view_model(&db2, id))
            .await
            .ok()
            .and_then(|r| r.ok());
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "stale_version",
                "detail": format!(
                    "This draft has moved on to version {} since you opened it (you have {}).",
                    draft.current_version, body.base_version
                ),
                "action": "Your edits were not applied. The current document is included here.",
                "current_version": draft.current_version,
                "model": model,
            })),
        )
            .into_response();
    }

    if body.patches.is_empty() {
        // Not an error: an autosave can fire with nothing queued.
        let db2 = db.clone();
        return match tokio::task::spawn_blocking(move || manager().view_model(&db2, id)).await {
            Ok(Ok(model)) => Json(PatchResult { version: draft.current_version, model }).into_response(),
            Ok(Err(e)) => server_error("view model failed", e),
            Err(e) => server_error("view model task panicked", e),
        };
    }

    // A new version is cut on an explicit save, or when the current version has
    // been sitting long enough that the next edit is a separate thought.
    let age = (chrono::Utc::now() - draft.updated_at).num_seconds();
    let cut_new_version = body.explicit_save || age > AMEND_WINDOW_SECS;
    debug!(
        "Draft {} patch: {} edits, {} (current version is {}s old)",
        id,
        body.patches.len(),
        if cut_new_version { "cutting a new version" } else { "amending" },
        age
    );

    let label = body.label.clone();
    let patches = body.patches.clone();
    let db2 = db.clone();
    let applied = tokio::task::spawn_blocking(move || {
        let m = manager();
        let version = m.apply_patches(&db2, id, &patches, cut_new_version, label.as_deref())?;
        let model = m.view_model(&db2, id)?;
        anyhow::Ok((version, model))
    })
    .await;

    match applied {
        Ok(Ok((version, model))) => {
            // A saved draft becomes referenceable in chat and searchable, which
            // is what stops the workspace being an island. Best-effort and
            // detached: a search-index failure must never fail the user's save.
            if cut_new_version {
                register_saved_draft(&state, id);
            }
            Json(PatchResult { version, model }).into_response()
        }
        // A rejected patch is a stale address or an unsupported edit — the
        // document is untouched, and the message says which.
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "patch_rejected",
            &full(&e),
            "Reopen the draft to pick up the current version, then try again.",
        ),
        Err(e) => server_error("patch task panicked", e),
    }
}

// ---------------------------------------------------------------- versions

/// GET /drafts/:id/versions
pub async fn list_versions(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = &state.shared_state.database_pool;
    let current = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d.current_version,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };

    match db.drafts.list_versions(id) {
        Ok(versions) => {
            let out: Vec<VersionSummary> = versions
                .into_iter()
                .map(|v: DraftVersionRecord| VersionSummary {
                    is_current: v.version_no == current,
                    version_no: v.version_no,
                    byte_size: v.byte_size,
                    label: v.label,
                    created_at: v.created_at.to_rfc3339(),
                })
                .collect();
            Json(json!({ "versions": out })).into_response()
        }
        Err(e) => server_error("listing versions failed", e),
    }
}

/// POST /drafts/:id/versions/:n/restore
///
/// Restoring writes the old bytes as a NEW version, so the state being restored
/// *from* is never destroyed. "Undo the undo" is always available.
pub async fn restore_version(
    State(state): State<UnifiedAppState>,
    Path((id, version_no)): Path<(i64, i64)>,
) -> Response {
    let db = state.shared_state.database_pool.clone();
    match tokio::task::spawn_blocking(move || {
        let m = manager();
        let created = m.restore_version(&db, id, version_no)?;
        let model = m.view_model(&db, id)?;
        anyhow::Ok((created, model))
    })
    .await
    {
        Ok(Ok((version, model))) => {
            info!("Draft {} restored v{} as v{}", id, version_no, version);
            Json(PatchResult { version, model }).into_response()
        }
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "restore_failed",
            &full(&e),
            "That version's file may be missing. Pick a different one.",
        ),
        Err(e) => server_error("restore task panicked", e),
    }
}

/// POST /drafts/:id/versions — cut a labelled checkpoint with no edits.
pub async fn checkpoint(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameBody>,
) -> Response {
    let db = state.shared_state.database_pool.clone();
    let label = body.title;
    match tokio::task::spawn_blocking(move || {
        manager().apply_patches(&db, id, &[], true, Some(&label))
    })
    .await
    {
        Ok(Ok(version)) => Json(json!({ "version": version })).into_response(),
        Ok(Err(e)) => server_error("creating a checkpoint failed", e),
        Err(e) => server_error("checkpoint task panicked", e),
    }
}

// ---------------------------------------------------------------- bytes

/// GET /drafts/:id/raw — the current bytes, for download and Save-as.
pub async fn get_raw(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };

    let db2 = db.clone();
    let bytes = match tokio::task::spawn_blocking(move || manager().read_current(&db2, id)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return server_error("reading draft bytes failed", e),
        Err(e) => return server_error("raw read task panicked", e),
    };

    let filename = format!("{}.{}", draft.title, draft.format);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_for(&draft.format).to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename.replace('"', "")),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// GET /drafts/:id/page/:n?scale=1.5 — a pdfium-rendered page image.
///
/// Cached on disk under the draft's own directory, keyed by version, page and
/// scale. The cache is what makes scrolling usable: pdfium is serialised
/// process-wide, so an uncached scroll would queue every page behind whatever
/// extraction is running.
pub async fn get_page(
    State(state): State<UnifiedAppState>,
    Path((id, page)): Path<(i64, usize)>,
    Query(q): Query<PageQuery>,
) -> Response {
    let scale = q.scale.unwrap_or(1.5).clamp(0.25, 4.0);
    let scale_x10 = (scale * 10.0).round() as u32;

    let db = state.shared_state.database_pool.clone();
    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };
    if draft.format != "pdf" {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_renderable",
            format!("Draft {} is a {} document, which has no rendered pages.", id, draft.format),
            "Use /drafts/:id/content for this format.",
        );
    }

    let version = draft.current_version;
    let cache_path = manager().page_cache_path(id, version, page, scale_x10);
    if let Ok(cached) = std::fs::read(&cache_path) {
        return png_response(cached);
    }

    let db2 = db.clone();
    let rendered = tokio::task::spawn_blocking(move || {
        let m = manager();
        let bytes = m.read_current(&db2, id)?;
        let png = crate::document_workspace::pdf_workspace::render_page(&bytes, page, scale)?;
        // Write-through. A cache write failure is not the user's problem —
        // the page still renders, it will just render again next time.
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&cache_path, &png) {
            warn!("Could not cache rendered page {} of draft {}: {}", page, id, e);
        }
        anyhow::Ok(png)
    })
    .await;

    match rendered {
        Ok(Ok(png)) => png_response(png),
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_failed",
            &full(&e),
            "Try a different page, or reopen the draft.",
        ),
        Err(e) => server_error("page render task panicked", e),
    }
}

fn png_response(png: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            // Safe to cache hard: the URL's version segment changes on save, so
            // a stale image can never be served for new content.
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        png,
    )
        .into_response()
}

/// POST /drafts/:id/publish — copy the current version back into the Vault.
///
/// The only operation in this module that writes to the document library, and
/// it is always user-initiated. It does not overwrite the source: it registers
/// the draft's bytes as their own document, so the original stays intact.
pub async fn publish(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };

    let db2 = db.clone();
    let bytes = match tokio::task::spawn_blocking(move || manager().read_current(&db2, id)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return server_error("reading draft bytes failed", e),
        Err(e) => return server_error("publish task panicked", e),
    };

    match store_bytes_as_document(&state, &draft, &bytes).await {
        Ok(document_id) => {
            info!("Published draft {} to the Vault as document {}", id, document_id);
            Json(json!({ "document_id": document_id })).into_response()
        }
        Err(e) => server_error("publishing a draft failed", e),
    }
}

// ---------------------------------------------------------------- helpers

fn strip_extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| filename.to_string())
        .trim()
        .to_string()
}

fn mime_for(format: &str) -> &'static str {
    match format {
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Read a Vault document's stored bytes, the same two ways the viewer does.
fn read_document_bytes(
    state: &UnifiedAppState,
    doc: &crate::memory_db::documents_store::DocumentRecord,
) -> anyhow::Result<Vec<u8>> {
    if let Some(local_file_id) = doc.local_file_id {
        return state
            .shared_state
            .database_pool
            .local_files
            .get_file_content(local_file_id)
            .map_err(|e| anyhow::anyhow!("stored copy could not be read: {}", e));
    }
    if let Some(ref path) = doc.source_path {
        return std::fs::read(path).map_err(|e| {
            anyhow::anyhow!("the original file at '{}' could not be read ({})", path, e)
        });
    }
    Err(anyhow::anyhow!(
        "'{}' has no stored copy or known location.",
        doc.original_filename
    ))
}

/// Make a saved draft visible to chat and search.
///
/// Detached and best-effort by design: the user's save has already succeeded on
/// disk, and failing it because an index write did not land would be the wrong
/// trade. A failure is logged at `warn`, not swallowed.
fn register_saved_draft(state: &UnifiedAppState, draft_id: i64) {
    let state = state.clone();
    tokio::spawn(async move {
        let db = state.shared_state.database_pool.clone();
        let draft = match db.drafts.get_draft(draft_id) {
            Ok(Some(d)) => d,
            _ => return,
        };
        let db2 = db.clone();
        let bytes =
            match tokio::task::spawn_blocking(move || manager().read_current(&db2, draft_id)).await {
                Ok(Ok(b)) => b,
                _ => return,
            };
        if let Err(e) = store_bytes_as_document(&state, &draft, &bytes).await {
            warn!(
                "Draft {} saved, but could not be indexed for chat and search: {}",
                draft_id, e
            );
        }
    });
}

/// Put draft bytes through the ordinary document pipeline.
///
/// Reuses `documents_api`'s ingestion wholesale — content-hash dedup, format
/// lanes, FTS — rather than adding a second path into the same tables.
async fn store_bytes_as_document(
    state: &UnifiedAppState,
    draft: &DraftRecord,
    bytes: &[u8],
) -> anyhow::Result<i64> {
    let filename = format!("{}.{}", draft.title, draft.format);
    crate::api::documents_api::ingest_bytes(state, &filename, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::shared_state::SharedState;

    /// Every draft API test runs against the real router with a real (in-memory)
    /// database and a temporary app-data directory. Calling the handlers
    /// directly would skip exactly the wiring — routing, extractors, status
    /// codes — that these tests exist to prove.
    /// Holds a test's isolated world open for its whole body.
    ///
    /// Both fields are load-bearing despite never being read: the guard keeps
    /// other tests out of the shared `APP_DATA_OVERRIDE` while this one runs,
    /// and the `TempDir` deletes the directory when the test ends. Dropping
    /// either early would let tests write over each other again.
    struct TestWorld {
        state: UnifiedAppState,
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    /// Serialises draft API tests.
    ///
    /// `APP_DATA_OVERRIDE` is process-wide, so two tests running at once would
    /// share one directory — and with each test's in-memory database numbering
    /// drafts from 1, they would share `drafts/1/` too. Taking this lock is
    /// what makes "my draft 1" mean one thing at a time.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn test_state() -> TestWorld {
        // Poisoning is irrelevant here: the guarded value is `()`, so a panic
        // in an earlier test leaves nothing inconsistent to protect against.
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        set_app_data_override(dir.path().to_path_buf());

        let cfg = Config::from_env().expect("Config::from_env should succeed with defaults");
        let database = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        let shared_state = Arc::new(SharedState::new(cfg, database).expect("SharedState::new"));

        TestWorld {
            state: UnifiedAppState::new(shared_state),
            _dir: dir,
            _guard: guard,
        }
    }

    async fn call(
        state: &UnifiedAppState,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let router = crate::thread_server::build_compatible_router(state.clone());
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => axum::body::Body::empty(),
        };
        let response = router.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Create a draft by uploading a file, over real multipart HTTP.
    async fn upload(
        state: &UnifiedAppState,
        filename: &str,
        bytes: &[u8],
    ) -> (StatusCode, serde_json::Value) {
        let boundary = "----OCADraftBoundary42";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n\r\n",
                filename
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

        let router = crate::thread_server::build_compatible_router(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/drafts/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(axum::body::Body::from(body))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let raw = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null))
    }

    async fn new_docx_draft(state: &UnifiedAppState) -> i64 {
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (status, body) = upload(state, "Engagement Letter.docx", &bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", body);
        body["id"].as_i64().unwrap()
    }

    #[tokio::test]
    async fn a_draft_can_be_created_listed_and_read_over_http() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let (status, body) = call(state, "GET", &format!("/drafts/{}", id), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["title"], "Engagement Letter");
        assert_eq!(body["format"], "docx");
        assert_eq!(body["origin_kind"], "upload");
        assert_eq!(body["current_version"], 1);

        let (status, body) = call(state, "GET", "/drafts", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["drafts"].as_array().unwrap().len(), 1);

        let (status, model) = call(state, "GET", &format!("/drafts/{}/content", id), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(model["format"], "docx");
        assert_eq!(model["blocks"][0]["runs"][0]["text"], "Master Services Agreement");
    }

    #[tokio::test]
    async fn an_edit_lands_and_returns_the_refreshed_model() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let (status, body) = call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "explicit_save": true,
                "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "Amended Agreement" }]
            })),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{:?}", body);
        assert_eq!(body["version"], 2, "an explicit save cuts a new version");
        assert_eq!(
            body["model"]["blocks"][0]["runs"][0]["text"],
            "Amended Agreement",
            "the response carries the document as it now is"
        );
    }

    /// The concurrency guard. Two tabs, or a retry after a lost response, must
    /// not double-apply.
    #[tokio::test]
    async fn a_stale_base_version_is_refused_with_409_and_the_current_document() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let edit = json!({
            "base_version": 1,
            "explicit_save": true,
            "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "First" }]
        });
        let (status, _) = call(state, "POST", &format!("/drafts/{}/patch", id), Some(edit.clone())).await;
        assert_eq!(status, StatusCode::OK);

        // The same request again — as a retry would send it.
        let (status, body) = call(state, "POST", &format!("/drafts/{}/patch", id), Some(edit)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "stale_version");
        assert_eq!(body["current_version"], 2);
        assert!(
            body["model"]["blocks"].is_array(),
            "the client needs the current document to rebase against: {:?}",
            body
        );
        assert_eq!(body["model"]["blocks"][0]["runs"][0]["text"], "First");
    }

    /// Autosave must not leave a version per keystroke.
    #[tokio::test]
    async fn autosaves_inside_the_amend_window_do_not_pile_up_versions() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        // The first edit always cuts v2: v1 is the pristine original and is
        // never amended.
        let (_, first) = call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "One" }]
            })),
        )
        .await;
        assert_eq!(first["version"], 2);

        for (n, text) in ["Two", "Three", "Four"].iter().enumerate() {
            let (status, body) = call(
                &state,
                "POST",
                &format!("/drafts/{}/patch", id),
                Some(json!({
                    "base_version": 2,
                    "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": text }]
                })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "autosave {} failed: {:?}", n, body);
            assert_eq!(body["version"], 2, "autosave must amend, not cut");
        }

        let (_, versions) = call(state, "GET", &format!("/drafts/{}/versions", id), None).await;
        assert_eq!(
            versions["versions"].as_array().unwrap().len(),
            2,
            "four edits inside the window must leave two versions, not five"
        );
    }

    #[tokio::test]
    async fn version_one_survives_editing_and_can_be_restored_without_losing_the_present() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "explicit_save": true,
                "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "Rewritten" }]
            })),
        )
        .await;

        let (status, body) = call(
            state,
            "POST",
            &format!("/drafts/{}/versions/1/restore", id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        assert_eq!(body["version"], 3, "restoring writes a NEW version");
        assert_eq!(
            body["model"]["blocks"][0]["runs"][0]["text"],
            "Master Services Agreement",
            "the original text is back"
        );

        // And version 2 still exists, so the restore is itself undoable.
        let (_, versions) = call(state, "GET", &format!("/drafts/{}/versions", id), None).await;
        let nums: Vec<i64> = versions["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version_no"].as_i64().unwrap())
            .collect();
        assert_eq!(nums, vec![3, 2, 1], "newest first, nothing destroyed");
    }

    #[tokio::test]
    async fn a_stale_address_is_refused_without_changing_the_document() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let (status, body) = call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "patches": [{ "op": "SetRunText", "addr": "body/p[99]/r[0]", "text": "x" }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "patch_rejected");
        assert!(body["detail"].as_str().unwrap().contains("no longer exists"));

        let (_, model) = call(state, "GET", &format!("/drafts/{}/content", id), None).await;
        assert_eq!(
            model["blocks"][0]["runs"][0]["text"], "Master Services Agreement",
            "a rejected patch must leave the document exactly as it was"
        );
    }

    #[tokio::test]
    async fn downloading_a_draft_returns_its_real_bytes_and_type() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let router = crate::thread_server::build_compatible_router(state.clone());
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/drafts/{}/raw", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response.headers()[header::CONTENT_TYPE].to_str().unwrap().to_string();
        assert!(content_type.contains("wordprocessingml"), "{}", content_type);
        let disposition = response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("Engagement Letter.docx"), "{}", disposition);

        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024).await.unwrap();
        // A real ZIP, i.e. a real .docx — not an error page with a 200 on it.
        assert_eq!(&bytes[..2], b"PK");
    }

    /// The format gate is a server-side boundary, not a picker filter.
    #[tokio::test]
    async fn a_legacy_binary_office_file_is_refused_with_actionable_advice() {
        let world = test_state().await;
        let state = &world.state;
        let (status, body) = upload(state, "contract.doc", b"\xD0\xCF\x11\xE0legacy").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "not_editable");
        assert!(body["detail"].as_str().unwrap().contains("legacy binary"), "{:?}", body);
    }

    #[tokio::test]
    async fn a_password_protected_package_is_refused_before_a_draft_row_exists() {
        let world = test_state().await;
        let state = &world.state;
        let (status, body) = upload(state, "sealed.docx", b"not a readable zip").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["detail"].as_str().unwrap().contains("password-protected"), "{:?}", body);

        let (_, list) = call(state, "GET", "/drafts", None).await;
        assert!(
            list["drafts"].as_array().unwrap().is_empty(),
            "a refused file must leave no half-made draft: {:?}",
            list
        );
    }

    #[tokio::test]
    async fn renaming_and_deleting_a_draft_work_over_http() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let (status, _) = call(
            state,
            "PATCH",
            &format!("/drafts/{}", id),
            Some(json!({ "title": "Engagement Letter (revised)" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(state, "GET", &format!("/drafts/{}", id), None).await;
        assert_eq!(body["title"], "Engagement Letter (revised)");

        let (status, _) = call(state, "PATCH", &format!("/drafts/{}", id), Some(json!({ "title": "   " }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "a blank name is refused");

        let router = crate::thread_server::build_compatible_router(state.clone());
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/drafts/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let (status, _) = call(state, "GET", &format!("/drafts/{}", id), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_draft_reports_404_rather_than_a_server_error() {
        let world = test_state().await;
        let state = &world.state;
        for uri in ["/drafts/999", "/drafts/999/content", "/drafts/999/versions"] {
            let (status, _) = call(state, "GET", uri, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{}", uri);
        }
    }

    /// Page rendering is a PDF-only surface; asking for it on a Word document
    /// must say so rather than 500.
    #[tokio::test]
    async fn asking_for_a_rendered_page_of_a_docx_is_refused_by_name() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;
        let (status, body) = call(state, "GET", &format!("/drafts/{}/page/0", id), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "not_renderable");
    }

    /// A rendered page must never outlive the bytes it was rendered from.
    ///
    /// The dangerous case is an autosave INSIDE the amend window: the version
    /// number does not change, so the cache key does not either, and a
    /// highlight added in that window would keep serving the pre-highlight
    /// image. This test plants a cache file and proves an edit clears it.
    #[tokio::test]
    async fn editing_a_draft_clears_its_rendered_page_cache_even_when_amending() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        // Move off version 1 so the next edit takes the amend path.
        call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "explicit_save": true,
                "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "One" }]
            })),
        )
        .await;

        let cached = manager().page_cache_path(id, 2, 0, 15);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"stale png").unwrap();
        assert!(cached.exists());

        // An autosave against the SAME version — the amend path.
        let (status, body) = call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 2,
                "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": "Two" }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        assert_eq!(body["version"], 2, "this must be the amend path, not a new version");

        assert!(
            !cached.exists(),
            "the cached page survived an edit to the bytes it was rendered from"
        );
    }

    #[tokio::test]
    async fn a_blank_note_can_be_created_and_a_blank_docx_is_refused_by_name() {
        let world = test_state().await;
        let state = &world.state;

        let (status, body) = call(
            state,
            "POST",
            "/drafts/blank",
            Some(json!({ "title": "Attendance note", "format": "txt" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", body);
        assert_eq!(body["origin_kind"], "blank");

        // It must be a real, openable draft from the first moment.
        let (status, model) =
            call(state, "GET", &format!("/drafts/{}/content", body["id"]), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(model["format"], "txt");

        let (status, body) = call(
            state,
            "POST",
            "/drafts/blank",
            Some(json!({ "title": "Advice", "format": "docx" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "blank_unsupported");
        assert!(body["detail"].as_str().unwrap().contains("only plain text"), "{:?}", body);
    }

    #[tokio::test]
    async fn a_text_draft_keeps_its_line_endings_across_an_edit() {
        let world = test_state().await;
        let state = &world.state;
        let (status, body) = upload(state, "notes.txt", b"Line one\r\nLine two\r\n").await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", body);
        let id = body["id"].as_i64().unwrap();

        let (_, model) = call(state, "GET", &format!("/drafts/{}/content", id), None).await;
        assert_eq!(model["line_ending"], "CRLF");
        assert_eq!(model["content"], "Line one\nLine two\n", "the editor sees \\n");

        call(
            state,
            "POST",
            &format!("/drafts/{}/patch", id),
            Some(json!({
                "base_version": 1,
                "explicit_save": true,
                "patches": [{ "op": "SetText", "content": "Line one\nLine two\nLine three\n" }]
            })),
        )
        .await;

        let router = crate::thread_server::build_compatible_router(state.clone());
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/drafts/{}/raw", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            &bytes[..], b"Line one\r\nLine two\r\nLine three\r\n",
            "CRLF must survive a round trip through the editor"
        );
    }

    #[test]
    fn a_filename_becomes_a_title_without_its_extension() {
        assert_eq!(strip_extension("Engagement Letter.docx"), "Engagement Letter");
        assert_eq!(strip_extension("report.final.pdf"), "report.final");
        assert_eq!(strip_extension("no-extension"), "no-extension");
    }

    #[test]
    fn every_editable_format_has_a_real_mime_type() {
        for format in crate::document_workspace::EDITABLE_FORMATS {
            assert_ne!(
                mime_for(format),
                "application/octet-stream",
                "'{}' should download with its real type, not as a blob",
                format
            );
        }
    }

    /// The amend window has to be long enough to cover a pause for thought and
    /// short enough that a session leaves a usable history.
    #[test]
    fn the_amend_window_is_in_a_sane_range() {
        assert!((60..=900).contains(&AMEND_WINDOW_SECS), "{}", AMEND_WINDOW_SECS);
    }

    #[test]
    fn a_patch_body_parses_with_its_version_guard() {
        let body: PatchBody = serde_json::from_str(
            r#"{"patches":[{"op":"SetRunText","addr":"body/p[0]/r[0]","text":"hi"}],
                "base_version":3}"#,
        )
        .unwrap();
        assert_eq!(body.base_version, 3);
        assert!(!body.explicit_save, "autosave is the default");
        assert_eq!(body.patches.len(), 1);
    }
}
