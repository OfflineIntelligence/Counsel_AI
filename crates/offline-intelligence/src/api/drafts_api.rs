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
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::document_workspace::view_model::{DraftPatch, DraftViewModel};
use crate::document_workspace::{DraftManager, UndoStack};
use crate::memory_db::drafts_store::{DraftFolderRecord, DraftRecord, DraftVersionRecord};
use crate::memory_db::library_store::{
    ClauseRecord, LibraryItemRecord, NewLibraryItem, LIBRARY_KINDS,
};
use crate::memory_db::matters_store::{MatterFields, MatterRecord, DRAFT_STATUSES, MATTER_STATUSES};
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

pub(crate) fn manager() -> DraftManager {
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
    /// The folder this draft is in; `null` is the root.
    pub folder_id: Option<i64>,
    /// The matter this draft is filed under; `null` is Unfiled.
    pub matter_id: Option<i64>,
    /// draft | in_review | completed
    pub status: String,
    pub starred: bool,
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
            folder_id: d.folder_id,
            matter_id: d.matter_id,
            status: d.status,
            starred: d.starred,
            created_at: d.created_at.to_rfc3339(),
            updated_at: d.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FolderSummary {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<DraftFolderRecord> for FolderSummary {
    fn from(f: DraftFolderRecord) -> Self {
        Self {
            id: f.id,
            name: f.name,
            parent_id: f.parent_id,
            created_at: f.created_at.to_rfc3339(),
            updated_at: f.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderBody {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateFolderBody {
    #[serde(default)]
    pub name: Option<String>,
    /// Present-and-null moves the folder to the root; absent leaves it alone.
    /// `Option<Option<_>>` is what distinguishes those two cases — a plain
    /// `Option` would make "move to root" indistinguishable from "don't move".
    #[serde(default, deserialize_with = "double_option")]
    pub parent_id: Option<Option<i64>>,
}

#[derive(Debug, Deserialize)]
pub struct MoveDraftBody {
    #[serde(default)]
    pub folder_id: Option<i64>,
}

/// Deserialize a present-but-null field as `Some(None)`.
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(de).map(Some)
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
    /// Edits that Undo can take back, and that Redo can reapply.
    pub undo_depth: usize,
    pub redo_depth: usize,
}

fn patch_result(id: i64, version: i64, model: DraftViewModel) -> PatchResult {
    let (undo_depth, redo_depth) = manager().undo_depth(id);
    PatchResult { version, model, undo_depth, redo_depth }
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub scale: Option<f32>,
}

// ---------------------------------------------------------------- compare

/// One side of a comparison.
///
/// Only drafts for now, and that covers both real uses without a second kind:
/// the Vault original of a draft IS its version 1, and the copy that came back
/// from the other side is imported as a draft before anyone compares it.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DocRef {
    Draft {
        id: i64,
        /// Omitted means the current version.
        #[serde(default)]
        version: Option<i64>,
    },
}

#[derive(Debug, Deserialize)]
pub struct CompareBody {
    pub left: DocRef,
    pub right: DocRef,
}

/// POST /compare
pub async fn compare_documents(
    State(state): State<UnifiedAppState>,
    Json(body): Json<CompareBody>,
) -> Response {
    let db = state.shared_state.database_pool.clone();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mgr = manager();

        let mut resolve = |r: &DocRef| -> anyhow::Result<(DraftViewModel, String, i64)> {
            match r {
                DocRef::Draft { id, version } => {
                    let draft = db
                        .drafts
                        .get_draft(*id)?
                        .ok_or_else(|| anyhow::anyhow!("Draft {} does not exist.", id))?;
                    let v = version.unwrap_or(draft.current_version);
                    let model = mgr.view_model_at(&db, *id, v)?;
                    // A PDF's model holds geometry, not text; compare its
                    // text layer instead, one line per extracted line with a
                    // page marker so the redline says where a change sits.
                    let model = match model {
                        DraftViewModel::Pdf { .. } => {
                            let bytes = mgr.read_version(&db, *id, v)?;
                            let pages = crate::document_workspace::pdf_workspace::page_texts(&bytes)?;
                            if pages.iter().all(|p| p.trim().is_empty()) {
                                anyhow::bail!(
                                    "'{}' has no text layer (it is probably a scan), so there is no text to compare.",
                                    draft.title
                                );
                            }
                            let content = pages
                                .iter()
                                .enumerate()
                                .map(|(i, text)| format!("— Page {} —\n{}", i + 1, text.trim()))
                                .collect::<Vec<_>>()
                                .join("\n");
                            DraftViewModel::Txt {
                                version: v,
                                content,
                                encoding: "UTF-8".into(),
                                line_ending: "LF".into(),
                                bom: false,
                            }
                        }
                        other => other,
                    };
                    Ok((model, draft.title, v))
                }
            }
        };

        let (left, left_title, left_version) = resolve(&body.left)?;
        let (right, right_title, right_version) = resolve(&body.right)?;

        let diff = crate::document_workspace::compare::compare(&left, &right)?;
        Ok((diff, left_title, left_version, right_title, right_version))
    })
    .await;

    match result {
        Ok(Ok((diff, lt, lv, rt, rv))) => (
            StatusCode::OK,
            Json(json!({
                "ops": diff.ops,
                "summary": diff.summary,
                "left":  { "title": lt, "version": lv },
                "right": { "title": rt, "version": rv },
            })),
        )
            .into_response(),
        // A refusal here is almost always "these two cannot be compared"
        // (a PDF, or two different formats), which is the caller asking for
        // something impossible rather than a fault on our side.
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "compare_unsupported",
            &full(&e),
            "Choose two documents of the same kind.",
        ),
        Err(e) => server_error("compare task panicked", e),
    }
}

// ---------------------------------------------------------------- matters

#[derive(Debug, Serialize)]
pub struct MatterSummary {
    pub id: i64,
    pub name: String,
    pub client: Option<String>,
    pub matter_number: Option<String>,
    pub jurisdiction: Option<String>,
    pub language: Option<String>,
    pub practice_area: Option<String>,
    pub status: String,
    pub opened_at: String,
    pub closed_at: Option<String>,
    pub updated_at: String,
}

impl From<MatterRecord> for MatterSummary {
    fn from(m: MatterRecord) -> Self {
        Self {
            id: m.id,
            name: m.name,
            client: m.client,
            matter_number: m.matter_number,
            jurisdiction: m.jurisdiction,
            language: m.language,
            practice_area: m.practice_area,
            status: m.status,
            opened_at: m.opened_at.to_rfc3339(),
            closed_at: m.closed_at.map(|d| d.to_rfc3339()),
            updated_at: m.updated_at.to_rfc3339(),
        }
    }
}

/// Every nullable field uses `Option<Option<_>>` so present-and-null ("clear
/// the client name") stays distinguishable from absent ("leave it alone").
#[derive(Debug, Deserialize)]
pub struct MatterBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub client: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub matter_number: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub jurisdiction: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub language: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub practice_area: Option<Option<String>>,
    #[serde(default)]
    pub status: Option<String>,
}

impl MatterBody {
    fn into_fields(self) -> MatterFields {
        MatterFields {
            name: self.name.map(|n| n.trim().to_string()),
            client: self.client,
            matter_number: self.matter_number,
            jurisdiction: self.jurisdiction,
            language: self.language,
            practice_area: self.practice_area,
            status: self.status,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AssignMatterBody {
    #[serde(default)]
    pub matter_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StatusBody {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct StarBody {
    pub starred: bool,
}

fn bad_status(given: &str, allowed: &[&str]) -> Response {
    fail(
        StatusCode::BAD_REQUEST,
        "invalid_status",
        format!("\"{}\" is not a valid status.", given),
        &format!("Use one of: {}.", allowed.join(", ")),
    )
}

/// GET /matters
pub async fn list_matters(State(state): State<UnifiedAppState>) -> Response {
    match state.shared_state.database_pool.matters.list() {
        Ok(rows) => {
            let matters: Vec<MatterSummary> = rows.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(json!({ "matters": matters }))).into_response()
        }
        Err(e) => server_error("listing matters failed", e),
    }
}

/// POST /matters
pub async fn create_matter(
    State(state): State<UnifiedAppState>,
    Json(body): Json<MatterBody>,
) -> Response {
    let fields = body.into_fields();

    let name = fields.name.clone().unwrap_or_default();
    if name.is_empty() {
        return fail(
            StatusCode::BAD_REQUEST,
            "matter_name_empty",
            "A matter needs a name.",
            "Name it after the client or the dispute.",
        );
    }
    if let Some(s) = &fields.status {
        if !MATTER_STATUSES.contains(&s.as_str()) {
            return bad_status(s, &MATTER_STATUSES);
        }
    }

    match state.shared_state.database_pool.matters.create(fields) {
        Ok(m) => (StatusCode::CREATED, Json(MatterSummary::from(m))).into_response(),
        // A duplicate reference is the user's filing mistake, not a fault of
        // ours, so it must not arrive as a 500 telling them to try again.
        Err(e) if e.to_string().contains("matter number") => fail(
            StatusCode::CONFLICT,
            "matter_number_taken",
            &full(&e),
            "Use a different reference, or leave it blank.",
        ),
        Err(e) => server_error("creating a matter failed", e),
    }
}

/// PATCH /matters/:id
pub async fn update_matter(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<MatterBody>,
) -> Response {
    let store = &state.shared_state.database_pool.matters;
    let fields = body.into_fields();

    if let Some(n) = &fields.name {
        if n.is_empty() {
            return fail(
                StatusCode::BAD_REQUEST,
                "matter_name_empty",
                "A matter needs a name.",
                "Type a name and try again.",
            );
        }
    }
    if let Some(s) = &fields.status {
        if !MATTER_STATUSES.contains(&s.as_str()) {
            return bad_status(s, &MATTER_STATUSES);
        }
    }

    if let Err(e) = store.update(id, fields) {
        let text = e.to_string();
        return if text.contains("matter number") {
            fail(
                StatusCode::CONFLICT,
                "matter_number_taken",
                &full(&e),
                "Use a different reference, or leave it blank.",
            )
        } else if text.contains("does not exist") {
            fail(StatusCode::NOT_FOUND, "matter_not_found", &full(&e), "Refresh the workspace.")
        } else {
            server_error("updating a matter failed", e)
        };
    }

    match store.get(id) {
        Ok(Some(m)) => (StatusCode::OK, Json(MatterSummary::from(m))).into_response(),
        Ok(None) => fail(
            StatusCode::NOT_FOUND,
            "matter_not_found",
            format!("Matter {} does not exist.", id),
            "Refresh the workspace.",
        ),
        Err(e) => server_error("reading a matter failed", e),
    }
}

/// DELETE /matters/:id
///
/// Removes the label, not the work: documents and folders inside return to
/// Unfiled, and the response says how many, so the UI can tell the user where
/// their files went rather than leaving them to find out.
pub async fn delete_matter(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    match state.shared_state.database_pool.matters.delete(id) {
        Ok(moved) => (
            StatusCode::OK,
            Json(json!({ "deleted": id, "drafts_moved_to_unfiled": moved })),
        )
            .into_response(),
        Err(e) if e.to_string().contains("does not exist") => fail(
            StatusCode::NOT_FOUND,
            "matter_not_found",
            &full(&e),
            "Refresh the workspace.",
        ),
        Err(e) => server_error("deleting a matter failed", e),
    }
}

/// PATCH /drafts/:id/matter — file a document under a matter.
pub async fn assign_draft_matter(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<AssignMatterBody>,
) -> Response {
    let db = &state.shared_state.database_pool;
    match db.matters.assign_draft(id, body.matter_id) {
        Ok(()) => match db.drafts.get_draft(id) {
            Ok(Some(d)) => {
                if let (Some(m), Some(doc)) = (d.matter_id, d.source_document_id) {
                    if let Err(e) = db.rag.link_document_to_matter(m, doc) {
                        warn!("Draft {} source document {} could not be linked to matter {}: {}", id, doc, m, e);
                    }
                }
                (StatusCode::OK, Json(DraftSummary::from(d))).into_response()
            }
            Ok(None) => not_found(id),
            Err(e) => server_error("reading a draft failed", e),
        },
        Err(e) if e.to_string().contains("does not exist") => fail(
            StatusCode::NOT_FOUND,
            "file_target_missing",
            &full(&e),
            "Refresh the workspace and try again.",
        ),
        Err(e) => server_error("filing a draft failed", e),
    }
}

/// PATCH /drafts/:id/status
pub async fn set_draft_status(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<StatusBody>,
) -> Response {
    let db = &state.shared_state.database_pool;
    match db.matters.set_draft_status(id, &body.status) {
        Ok(()) => match db.drafts.get_draft(id) {
            Ok(Some(d)) => (StatusCode::OK, Json(DraftSummary::from(d))).into_response(),
            Ok(None) => not_found(id),
            Err(e) => server_error("reading a draft failed", e),
        },
        Err(e) if e.to_string().contains("is not a document status") => {
            bad_status(&body.status, &DRAFT_STATUSES)
        }
        Err(e) if e.to_string().contains("does not exist") => not_found(id),
        Err(e) => server_error("setting document status failed", e),
    }
}

/// PATCH /drafts/:id/star
pub async fn set_draft_starred(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<StarBody>,
) -> Response {
    let db = &state.shared_state.database_pool;
    match db.drafts.set_draft_starred(id, body.starred) {
        Ok(()) => match db.drafts.get_draft(id) {
            Ok(Some(d)) => (StatusCode::OK, Json(DraftSummary::from(d))).into_response(),
            Ok(None) => not_found(id),
            Err(e) => server_error("reading a draft failed", e),
        },
        Err(e) if e.to_string().contains("does not exist") => not_found(id),
        Err(e) => server_error("starring a document failed", e),
    }
}

// ---------------------------------------------------------------- search

/// How many hits one request returns.
///
/// A person scans a result list; they do not page through 500 of them. If the
/// answer is not near the top the query needs narrowing, not more rows.
const SEARCH_LIMIT: usize = 40;

/// How many stale drafts one backfill pass repairs.
///
/// Bounded because each one parses a whole document. A workspace with 800
/// unindexed drafts must not spend a minute of CPU on the first search — it
/// returns what it has, repairs a slice, and converges over subsequent calls.
const BACKFILL_BATCH: usize = 12;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
}

/// GET /workspace/search?q=
pub async fn search_workspace(
    State(state): State<UnifiedAppState>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let query = params.q.trim().to_string();
    if query.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({ "hits": [], "pending_index": 0 })),
        )
            .into_response();
    }

    let db = state.shared_state.database_pool.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        // Opportunistic repair, before the query rather than after: a draft
        // indexed on this pass can be found on this pass. Failures are
        // swallowed per draft so one unreadable document cannot stop search
        // working for every other one.
        let stale = db.search.stale_draft_ids(BACKFILL_BATCH).unwrap_or_default();
        if !stale.is_empty() {
            let mgr = manager();
            for id in &stale {
                mgr.reindex(&db, *id);
            }
            debug!("Search backfilled {} draft(s)", stale.len());
        }

        let hits = db.search.search(&query, SEARCH_LIMIT)?;
        // What is still missing, so the UI can say results are incomplete
        // instead of implying the workspace has been fully searched.
        let pending = db.search.stale_draft_ids(1000).map(|v| v.len()).unwrap_or(0);
        Ok((hits, pending))
    })
    .await;

    match result {
        Ok(Ok((hits, pending))) => (
            StatusCode::OK,
            Json(json!({ "hits": hits, "pending_index": pending })),
        )
            .into_response(),
        Ok(Err(e)) => server_error("search failed", e),
        Err(e) => server_error("search task panicked", e),
    }
}

// ---------------------------------------------------------------- folders

/// GET /draft-folders — the whole tree, flat.
pub async fn list_folders(State(state): State<UnifiedAppState>) -> Response {
    match state.shared_state.database_pool.drafts.list_folders() {
        Ok(rows) => {
            let folders: Vec<FolderSummary> = rows.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(json!({ "folders": folders }))).into_response()
        }
        Err(e) => server_error("listing folders failed", e),
    }
}

/// POST /draft-folders
pub async fn create_folder(
    State(state): State<UnifiedAppState>,
    Json(body): Json<CreateFolderBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if let Some(rejection) = reject_bad_folder_name(&name) {
        return rejection;
    }

    match state
        .shared_state
        .database_pool
        .drafts
        .create_folder(&name, body.parent_id)
    {
        Ok(f) => (StatusCode::CREATED, Json(FolderSummary::from(f))).into_response(),
        // A duplicate name is the user's doing, not a server fault, so it must
        // not come back as a 500 with "try again" — that advice would be wrong.
        Err(e) if e.to_string().contains("already here") => fail(
            StatusCode::CONFLICT,
            "folder_exists",
            &full(&e),
            "Choose a different name.",
        ),
        Err(e) => server_error("creating a folder failed", e),
    }
}

/// PATCH /draft-folders/:id — rename and/or re-parent.
pub async fn update_folder(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateFolderBody>,
) -> Response {
    let store = &state.shared_state.database_pool.drafts;

    if let Some(name) = &body.name {
        let name = name.trim().to_string();
        if let Some(rejection) = reject_bad_folder_name(&name) {
            return rejection;
        }
        if let Err(e) = store.rename_folder(id, &name) {
            return if e.to_string().contains("already here") {
                fail(StatusCode::CONFLICT, "folder_exists", &full(&e), "Choose a different name.")
            } else {
                server_error("renaming a folder failed", e)
            };
        }
    }

    if let Some(parent) = body.parent_id {
        if let Err(e) = store.move_folder(id, parent) {
            // Both cycle refusals are the caller asking for something
            // impossible, not a failure of ours.
            return fail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "folder_move_refused",
                &full(&e),
                "Move it somewhere outside itself.",
            );
        }
    }

    match store.get_folder(id) {
        Ok(Some(f)) => (StatusCode::OK, Json(FolderSummary::from(f))).into_response(),
        Ok(None) => fail(
            StatusCode::NOT_FOUND,
            "folder_not_found",
            format!("Folder {} does not exist.", id),
            "Refresh the workspace.",
        ),
        Err(e) => server_error("reading a folder failed", e),
    }
}

/// DELETE /draft-folders/:id
///
/// Deletes the folder and its subfolders. Drafts inside are NOT deleted — they
/// return to the root, and the response says how many, so the UI can tell the
/// user where their documents went rather than leaving them to wonder.
pub async fn delete_folder(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Response {
    match state.shared_state.database_pool.drafts.delete_folder(id) {
        Ok(moved) => (
            StatusCode::OK,
            Json(json!({ "deleted": id, "drafts_moved_to_root": moved })),
        )
            .into_response(),
        Err(e) if e.to_string().contains("does not exist") => fail(
            StatusCode::NOT_FOUND,
            "folder_not_found",
            &full(&e),
            "Refresh the workspace.",
        ),
        Err(e) => server_error("deleting a folder failed", e),
    }
}

/// PATCH /drafts/:id/folder — move a draft between folders.
pub async fn move_draft(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<MoveDraftBody>,
) -> Response {
    let store = &state.shared_state.database_pool.drafts;
    match store.move_draft(id, body.folder_id) {
        Ok(()) => match store.get_draft(id) {
            Ok(Some(d)) => (StatusCode::OK, Json(DraftSummary::from(d))).into_response(),
            Ok(None) => not_found(id),
            Err(e) => server_error("reading a draft failed", e),
        },
        Err(e) if e.to_string().contains("does not exist") => fail(
            StatusCode::NOT_FOUND,
            "move_target_missing",
            &full(&e),
            "Refresh the workspace and try again.",
        ),
        Err(e) => server_error("moving a draft failed", e),
    }
}

/// Folder names are shown as path segments, so the characters a path separator
/// would swallow are refused up front rather than producing a breadcrumb that
/// reads as two folders.
fn reject_bad_folder_name(name: &str) -> Option<Response> {
    if name.is_empty() {
        return Some(fail(
            StatusCode::BAD_REQUEST,
            "folder_name_empty",
            "A folder needs a name.",
            "Type a name and try again.",
        ));
    }
    if name.len() > 120 {
        return Some(fail(
            StatusCode::BAD_REQUEST,
            "folder_name_too_long",
            "That folder name is too long.",
            "Use 120 characters or fewer.",
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Some(fail(
            StatusCode::BAD_REQUEST,
            "folder_name_invalid",
            "A folder name cannot contain \\ or /.",
            "Remove the slash from the name.",
        ));
    }
    None
}

// ---------------------------------------------------------------- previews

/// How much of a document a thumbnail can possibly show.
///
/// A tile is ~200px tall. Past roughly this much content the text is below the
/// fold of the thumbnail and shipping it would be pure waste — and this
/// endpoint is called once per visible tile, so the waste multiplies.
const PREVIEW_PARAGRAPHS: usize = 14;
const PREVIEW_ROWS: usize = 9;
const PREVIEW_COLS: usize = 6;
const PREVIEW_CHARS: usize = 140;

/// A document's first page, reduced to what a thumbnail can draw.
///
/// # Why this is not a rendered image
///
/// Only PDF can be rendered to pixels here — pdfium is the crate's sole
/// renderer, and producing a true raster of a Word page would need a full text
/// layout engine (line breaking, font metrics, pagination). What a thumbnail
/// is actually FOR is recognising your document in a grid, and the real first
/// lines of the real document do that. So DOCX/XLSX/PPTX return their genuine
/// first-page content as structured text and the client draws a miniature
/// page; PDF clients keep using `/drafts/:id/page/0`, which is a real render.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DraftPreview {
    /// Paragraphs, in order, with headings flagged so the miniature can weight
    /// them the way the page does.
    Text { lines: Vec<PreviewLine> },
    /// The top-left corner of the first sheet, as a grid.
    Grid { sheet: String, rows: Vec<Vec<String>> },
    /// The first slide's text, largest placeholder first.
    Slide { lines: Vec<PreviewLine>, total: usize },
    /// PDF renders for real; the client asks for page 0 instead.
    Raster { pages: usize },
    /// Nothing readable — a corrupt or empty document still gets a tile.
    Empty,
}

#[derive(Debug, Serialize)]
pub struct PreviewLine {
    pub text: String,
    pub heading: bool,
}

fn clip(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= PREVIEW_CHARS {
        return t.to_string();
    }
    t.chars().take(PREVIEW_CHARS).collect::<String>() + "…"
}

/// GET /drafts/:id/preview
pub async fn get_preview(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let model = match tokio::task::spawn_blocking(move || manager().view_model(&db, id)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            // A preview is decoration. A document that cannot be parsed must
            // still get a tile rather than turning the whole grid into an error
            // — the user can open it and see the real failure there.
            warn!("preview for draft {} unavailable: {}", id, e);
            return (StatusCode::OK, Json(DraftPreview::Empty)).into_response();
        }
        Err(e) => return server_error("preview task panicked", e),
    };

    let preview = match model {
        DraftViewModel::Docx { blocks, .. } => {
            let lines = blocks
                .iter()
                .map(|b| PreviewLine {
                    text: clip(&b.runs.iter().map(|r| r.text.as_str()).collect::<String>()),
                    heading: b
                        .style
                        .as_deref()
                        .map(|s| s.to_lowercase().starts_with("heading"))
                        .unwrap_or(false),
                })
                // Leading empty paragraphs are common and would push all the
                // real content out of a 14-line budget.
                .skip_while(|l| l.text.is_empty())
                .take(PREVIEW_PARAGRAPHS)
                .collect::<Vec<_>>();
            if lines.is_empty() { DraftPreview::Empty } else { DraftPreview::Text { lines } }
        }

        DraftViewModel::Txt { content, .. } => {
            let lines = content
                .lines()
                .skip_while(|l| l.trim().is_empty())
                .take(PREVIEW_PARAGRAPHS)
                .map(|l| PreviewLine { text: clip(l), heading: false })
                .collect::<Vec<_>>();
            if lines.is_empty() { DraftPreview::Empty } else { DraftPreview::Text { lines } }
        }

        DraftViewModel::Xlsx { sheets, .. } => match sheets.first() {
            Some(sheet) => {
                let rows = (1..=PREVIEW_ROWS)
                    .map(|r| {
                        (0..PREVIEW_COLS)
                            .map(|c| {
                                // A1-style reference, which is how the cells are keyed.
                                let col = char::from(b'A' + c as u8);
                                sheet
                                    .cells
                                    .get(&format!("{col}{r}"))
                                    .map(|cell| clip(&cell.value))
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                DraftPreview::Grid { sheet: sheet.name.clone(), rows }
            }
            None => DraftPreview::Empty,
        },

        DraftViewModel::Pptx { slides, .. } => match slides.first() {
            Some(slide) => {
                let lines = slide
                    .shapes
                    .iter()
                    .flat_map(|s| s.paragraphs.iter())
                    .map(|p| PreviewLine { text: clip(&p.text), heading: p.level == 0 })
                    .filter(|l| !l.text.is_empty())
                    .take(PREVIEW_PARAGRAPHS)
                    .collect::<Vec<_>>();
                DraftPreview::Slide { lines, total: slides.len() }
            }
            None => DraftPreview::Empty,
        },

        DraftViewModel::Pdf { pages, .. } => DraftPreview::Raster { pages: pages.len() },
    };

    (StatusCode::OK, Json(preview)).into_response()
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
    // A blank DOCX/XLSX/PPTX is not an empty file — it is a complete OOXML
    // package with content types, relationships and (for PowerPoint) a master,
    // a layout and a theme. Those live in `ooxml::blank` as reviewable XML
    // rather than being improvised here; the tests there prove each one opens
    // in its own editor and keeps the parts Office refuses to repair without.
    //
    // The single newline for text is deliberate: `create` refuses empty bytes
    // (an empty file is almost always a failed read), so a blank note starts as
    // one empty line rather than as nothing at all.
    let seed: Vec<u8> = match format.as_str() {
        "txt" => b"\n".to_vec(),
        other => match crate::document_workspace::ooxml::blank::for_format(other) {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return server_error("blank template generation failed", e),
            None => {
                return fail(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "blank_unsupported",
                    format!("A blank {} cannot be created.", other.to_uppercase()),
                    "Word, Excel, PowerPoint and text documents can be started empty.",
                )
            }
        },
    };

    let filename = format!("{}.{}", body.title, format);
    let db = state.shared_state.database_pool.clone();
    let title = body.title.clone();
    match tokio::task::spawn_blocking(move || {
        manager().create(&db, &title, &filename, &seed, "blank", None, None)
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
    let doomed_documents = db.rag.saved_documents_of_draft(id).unwrap_or_default();
    let db_for_docs = db.clone();
    match tokio::task::spawn_blocking(move || manager().delete(&db, id)).await {
        Ok(Ok(())) => {
            if let Err(e) = db_for_docs.rag.delete_documents(&doomed_documents) {
                warn!("Deleted draft {} but its searchable copy remains: {}", id, e);
            }
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
            Ok(Ok(model)) => Json(patch_result(id, draft.current_version, model)).into_response(),
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
            Json(patch_result(id, version, model)).into_response()
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
            Json(patch_result(id, version, model)).into_response()
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

/// DELETE /drafts/:id/versions/:n
///
/// Removes one saved version from the history. The current version and
/// version 1 (the pristine original) are never removed, so the document and
/// its way back to the start are always intact.
pub async fn delete_version(
    State(state): State<UnifiedAppState>,
    Path((id, version_no)): Path<(i64, i64)>,
) -> Response {
    let db = state.shared_state.database_pool.clone();
    match tokio::task::spawn_blocking(move || {
        let draft = match db.drafts.get_draft(id)? {
            Some(d) => d,
            None => return anyhow::Ok(Err(StatusCode::NOT_FOUND)),
        };
        if version_no == 1 || version_no == draft.current_version {
            return anyhow::Ok(Err(StatusCode::CONFLICT));
        }
        let record = match db.drafts.get_version(id, version_no)? {
            Some(v) => v,
            None => return anyhow::Ok(Err(StatusCode::NOT_FOUND)),
        };
        let path = manager().absolute(&record.storage_path);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        db.drafts.delete_version(record.id)?;
        anyhow::Ok(Ok(()))
    })
    .await
    {
        Ok(Ok(Ok(()))) => {
            info!("Draft {} version {} deleted", id, version_no);
            Json(json!({ "deleted": version_no })).into_response()
        }
        Ok(Ok(Err(StatusCode::NOT_FOUND))) => not_found(id),
        Ok(Ok(Err(_))) => fail(
            StatusCode::CONFLICT,
            "version_protected",
            "The current version and the original cannot be deleted.",
            "Pick an older checkpoint.",
        ),
        Ok(Err(e)) => server_error("deleting a version failed", e),
        Err(e) => server_error("delete version task panicked", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct StepBody {
    /// The version the client is looking at; a mismatch is refused like a patch.
    #[serde(default)]
    pub base_version: Option<i64>,
}

/// GET /drafts/:id/undo — how many steps Undo and Redo can take.
pub async fn undo_state(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    match state.shared_state.database_pool.drafts.get_draft(id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    }
    let (undo_depth, redo_depth) = manager().undo_depth(id);
    Json(json!({ "undo_depth": undo_depth, "redo_depth": redo_depth })).into_response()
}

/// POST /drafts/:id/undo
pub async fn undo(state: State<UnifiedAppState>, id: Path<i64>, body: Json<StepBody>) -> Response {
    step(state, id, body, UndoStack::Undo).await
}

/// POST /drafts/:id/redo
pub async fn redo(state: State<UnifiedAppState>, id: Path<i64>, body: Json<StepBody>) -> Response {
    step(state, id, body, UndoStack::Redo).await
}

async fn step(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    Json(body): Json<StepBody>,
    direction: UndoStack,
) -> Response {
    let db = state.shared_state.database_pool.clone();
    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };
    if let Some(base) = body.base_version {
        if base != draft.current_version {
            return fail(
                StatusCode::CONFLICT,
                "stale_version",
                format!("This draft has moved on to version {} (you have {}).", draft.current_version, base),
                "Reopen the draft, then undo again.",
            );
        }
    }

    let db2 = db.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let m = manager();
        let stepped = m.step(&db2, id, direction)?;
        let model = m.view_model(&db2, id)?;
        anyhow::Ok((stepped, model))
    })
    .await;

    match outcome {
        Ok(Ok((stepped, model))) => {
            let version = stepped.unwrap_or(draft.current_version);
            Json(patch_result(id, version, model)).into_response()
        }
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "undo_failed",
            &full(&e),
            "Restore an earlier version from the history instead.",
        ),
        Err(e) => server_error("undo task panicked", e),
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

    match store_bytes_as_document(&state, &draft, &bytes, "published").await {
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
        if let Err(e) = store_bytes_as_document(&state, &draft, &bytes, "saved").await {
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
    kind: &str,
) -> anyhow::Result<i64> {
    let filename = format!("{}.{}", draft.title, draft.format);
    let document_id = crate::api::documents_api::ingest_bytes(state, &filename, bytes).await?;
    // A draft filed under a matter contributes its text to that matter's
    // retrieval pool. Re-read: the matter may have changed since `draft` was loaded.
    let db = &state.shared_state.database_pool;
    if let Err(e) = db.rag.link_draft_document(draft.id, document_id, kind) {
        warn!("Draft {} document {} could not be recorded: {}", draft.id, document_id, e);
    }
    let matter = db.drafts.get_draft(draft.id).ok().flatten().and_then(|d| d.matter_id).or(draft.matter_id);
    if let Some(m) = matter {
        if let Err(e) = db.rag.link_document_to_matter(m, document_id) {
            warn!("Draft {} document {} could not be linked to matter {}: {}", draft.id, document_id, m, e);
        }
    }
    Ok(document_id)
}

// =========================================================================
// The library — precedents, templates and clauses
// =========================================================================
//
// Two shapes, because they are used differently (see migration 018). A
// library item is a FILE you start a new document from; a clause is TEXT you
// drop into a document you are already writing.
//
// Bytes live in `AppData/library/{id}.{ext}`, outside `drafts/`, because the
// point of the library is that deleting the matter a precedent was drawn from
// must not reach into the firm's precedent bank.

fn library_dir() -> std::path::PathBuf {
    manager().app_data_dir().join("library")
}

fn library_relative(id: i64, format: &str) -> String {
    format!("library/{}.{}", id, format)
}

#[derive(Debug, Serialize)]
pub struct LibraryItemSummary {
    pub id: i64,
    pub title: String,
    pub kind: String,
    pub format: String,
    pub byte_size: i64,
    pub source_draft_id: Option<i64>,
    pub jurisdiction: Option<String>,
    pub practice_area: Option<String>,
    pub language: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Shipped with the app as part of the knowledge collection.
    pub bundled: bool,
}

impl From<LibraryItemRecord> for LibraryItemSummary {
    fn from(i: LibraryItemRecord) -> Self {
        // `storage_path` is deliberately not exposed: the frontend has no use
        // for a disk path it cannot open, and shipping one invites callers to
        // build their own file access around the API.
        Self {
            id: i.id,
            title: i.title,
            kind: i.kind,
            format: i.format,
            byte_size: i.byte_size,
            source_draft_id: i.source_draft_id,
            jurisdiction: i.jurisdiction,
            practice_area: i.practice_area,
            language: i.language,
            tags: i.tags,
            created_at: i.created_at.to_rfc3339(),
            updated_at: i.updated_at.to_rfc3339(),
            bundled: i.bundled_key.is_some(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ClauseSummary {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub jurisdiction: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<ClauseRecord> for ClauseSummary {
    fn from(c: ClauseRecord) -> Self {
        Self {
            id: c.id,
            title: c.title,
            body: c.body,
            jurisdiction: c.jurisdiction,
            created_at: c.created_at.to_rfc3339(),
            updated_at: c.updated_at.to_rfc3339(),
        }
    }
}

/// Shared by "upload a template" and "promote this draft", because the
/// classifying metadata is the same either way.
#[derive(Debug, Default, Deserialize)]
pub struct LibraryMeta {
    pub title: Option<String>,
    pub kind: Option<String>,
    pub jurisdiction: Option<String>,
    pub practice_area: Option<String>,
    pub language: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn bad_kind(given: &str) -> Response {
    fail(
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad_kind",
        format!("'{}' is not a library kind.", given),
        "Use 'template' for a blank house form or 'precedent' for a document that worked.",
    )
}

/// Writes the bytes, then records where they went.
///
/// The row is created first because the filename is the id — there is no
/// stable name to write under until the row exists. If the write fails the row
/// is removed again, so a library entry never points at a file that is not
/// there.
fn store_library_bytes(
    db: &crate::memory_db::MemoryDatabase,
    meta: &LibraryMeta,
    format: &str,
    bytes: &[u8],
    source_draft_id: Option<i64>,
    title: &str,
) -> anyhow::Result<LibraryItemRecord> {
    let kind = meta.kind.as_deref().unwrap_or("precedent");
    let item = db.library.create(NewLibraryItem {
        title,
        kind,
        format,
        byte_size: bytes.len() as i64,
        source_draft_id,
        jurisdiction: meta.jurisdiction.as_deref(),
        practice_area: meta.practice_area.as_deref(),
        language: meta.language.as_deref(),
        tags: meta.tags.clone(),
    })?;

    let relative = library_relative(item.id, format);
    let absolute = manager().absolute(&relative);
    let written = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(library_dir())?;
        std::fs::write(&absolute, bytes)
    })();

    if let Err(e) = written {
        // Roll the row back rather than leave a library entry whose file was
        // never written; the alternative is an item that fails to open
        // forever with no way to tell why.
        let _ = db.library.delete(item.id);
        return Err(anyhow::anyhow!("writing the library file failed: {}", e));
    }

    db.library.fix_storage_path(item.id, &relative)?;
    db.library
        .get(item.id)?
        .ok_or_else(|| anyhow::anyhow!("library item vanished immediately after being stored"))
}


/// Parse an optional JSON body without silently discarding a broken one.
///
/// `Option<Json<T>>` cannot tell "no body" from "a body I could not read": it
/// answers `None` to both. For these two endpoints that difference is the
/// whole metadata the caller sent — a mistyped `tags` would have been dropped
/// and answered 201, so the user would be told their precedent was filed with
/// tags it does not have. An empty body still means "defaults"; a malformed
/// one is now refused.
fn optional_json<T: serde::de::DeserializeOwned + Default>(body: &Bytes) -> Result<T, Response> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| {
        fail(
            StatusCode::BAD_REQUEST,
            "bad_request_body",
            e,
            "Check the details you sent and try again.",
        )
    })
}

/// GET /library
pub async fn list_library(State(state): State<UnifiedAppState>) -> Response {
    let db = state.shared_state.database_pool.clone();
    match db.library.list() {
        Ok(items) => {
            let out: Vec<LibraryItemSummary> =
                items.into_iter().map(LibraryItemSummary::from).collect();
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(e) => server_error("listing the library failed", e),
    }
}

/// POST /library — add a house template or precedent by uploading the file.
///
/// Multipart rather than JSON: this carries a document. The metadata rides
/// alongside as ordinary text fields.
pub async fn create_library_item(
    State(state): State<UnifiedAppState>,
    mut multipart: Multipart,
) -> Response {
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut meta = LibraryMeta::default();

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                let filename = field.file_name().map(str::to_string);
                match field.bytes().await {
                    Ok(b) => match filename {
                        Some(f) => file = Some((f, b.to_vec())),
                        None => {
                            let value = String::from_utf8_lossy(&b).trim().to_string();
                            if value.is_empty() {
                                continue;
                            }
                            match name.as_str() {
                                "title" => meta.title = Some(value),
                                "kind" => meta.kind = Some(value),
                                "jurisdiction" => meta.jurisdiction = Some(value),
                                "practice_area" => meta.practice_area = Some(value),
                                "language" => meta.language = Some(value),
                                // Repeated `tags` fields and one comma-joined
                                // field both work; multipart form encoders
                                // disagree about which to send.
                                "tags" => meta
                                    .tags
                                    .extend(value.split(',').map(|t| t.trim().to_string())),
                                _ => {}
                            }
                        }
                    },
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
            "Pick a document to add to the library.",
        );
    };

    if let Some(kind) = meta.kind.as_deref() {
        if !LIBRARY_KINDS.contains(&kind) {
            return bad_kind(kind);
        }
    }

    // Refused here rather than at first use: a library full of documents the
    // editor cannot open is worse than a rejected upload.
    let format = match crate::document_workspace::editable_format_of(&filename) {
        Ok(f) => f,
        Err(e) => {
            return fail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_editable",
                &full(&e),
                "Convert the file to a supported format and try again.",
            )
        }
    };

    let title = meta
        .title
        .clone()
        .unwrap_or_else(|| strip_extension(&filename));
    let db = state.shared_state.database_pool.clone();
    match tokio::task::spawn_blocking(move || {
        store_library_bytes(&db, &meta, &format, &bytes, None, &title)
    })
    .await
    {
        Ok(Ok(item)) => {
            (StatusCode::CREATED, Json(LibraryItemSummary::from(item))).into_response()
        }
        Ok(Err(e)) => server_error("adding a library item failed", full(&e)),
        Err(e) => server_error("library upload task panicked", e),
    }
}

/// DELETE /library/:id
pub async fn delete_library_item(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
) -> Response {
    let db = state.shared_state.database_pool.clone();

    // A bundled knowledge document is removed for good: its copy, its chat
    // index, and a record so app updates do not bring it back.
    if let Ok(Some(item)) = db.library.get(id) {
        if let Some(key) = item.bundled_key {
            return match crate::knowledge::remove_bundled(&state, &key) {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => server_error("removing a knowledge document failed", full(&e)),
            };
        }
    }

    let library_document = db.library.get(id).ok().flatten().and_then(|item| item.document_id);
    match db.library.delete(id) {
        Ok(path) => {
            if let Some(doc) = library_document {
                if let Err(e) = db.rag.delete_documents(&[doc]) {
                    warn!("Deleted library item {} but its searchable document remains: {}", id, e);
                }
            }
            if let Some(relative) = path {
                // Best effort: the row is already gone, and refusing to
                // acknowledge the delete because a file was missing would
                // leave the caller stuck on an item they can no longer see.
                if let Err(e) = std::fs::remove_file(manager().absolute(&relative)) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!("Library file {} could not be removed: {}", relative, e);
                    }
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => fail(
            StatusCode::NOT_FOUND,
            "library_item_not_found",
            &full(&e),
            "Refresh the library and try again.",
        ),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct NewFromPrecedent {
    pub title: Option<String>,
    pub matter_id: Option<i64>,
}

/// POST /library/:id/new-draft — the most common way a document begins.
///
/// A copy, not a link: editing the new draft must never change the precedent
/// everyone else starts from.
pub async fn new_draft_from_library(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    raw: Bytes,
) -> Response {
    let body: NewFromPrecedent = match optional_json(&raw) {
        Ok(b) => b,
        Err(response) => return response,
    };
    let db = state.shared_state.database_pool.clone();

    let item = match db.library.get(id) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return fail(
                StatusCode::NOT_FOUND,
                "library_item_not_found",
                format!("Library item {} does not exist.", id),
                "Refresh the library and try again.",
            )
        }
        Err(e) => return server_error("reading a library item failed", e),
    };

    let title = body.title.unwrap_or_else(|| item.title.clone());
    let filename = format!("{}.{}", title, item.format);
    let path = manager().absolute(&item.storage_path);
    let matter_id = body.matter_id;

    match tokio::task::spawn_blocking(move || -> anyhow::Result<DraftRecord> {
        let bytes = std::fs::read(&path).map_err(|e| {
            anyhow::anyhow!("the library file for '{}' could not be read: {}", title, e)
        })?;
        let draft = manager().create(&db, &title, &filename, &bytes, "library", None, None)?;
        if let Some(m) = matter_id {
            db.matters.assign_draft(draft.id, Some(m))?;
        }
        // Re-read so the response carries the matter that was just set rather
        // than the NULL the row was created with.
        Ok(db.drafts.get_draft(draft.id)?.unwrap_or(draft))
    })
    .await
    {
        Ok(Ok(draft)) => (StatusCode::CREATED, Json(DraftSummary::from(draft))).into_response(),
        Ok(Err(e)) => server_error("starting a draft from a precedent failed", full(&e)),
        Err(e) => server_error("new-from-precedent task panicked", e),
    }
}

/// Plain text of a document's bytes, for the AI Assistant.
fn text_of(format: &str, bytes: &[u8]) -> anyhow::Result<String> {
    match format {
        "pdf" => Ok(crate::document_workspace::pdf_workspace::page_texts(bytes)?
            .iter()
            .enumerate()
            .map(|(i, t)| format!("— Page {} —\n{}", i + 1, t.trim()))
            .collect::<Vec<_>>()
            .join("\n")),
        other => {
            let model = crate::document_workspace::view_model::build(other, bytes, 1)?;
            Ok(crate::document_workspace::view_model::plain_text(&model))
        }
    }
}

/// Longest text returned to the assistant; it trims further for the model.
const ASSISTANT_TEXT_LIMIT: usize = 60_000;

/// GET /library/:id/text — what a precedent says, so the AI Assistant can read
/// the item selected on the Library page without opening it as a draft.
pub async fn library_item_text(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let item = match db.library.get(id) {
        Ok(Some(i)) => i,
        Ok(None) => {
            return fail(
                StatusCode::NOT_FOUND,
                "library_item_not_found",
                format!("Library item {} does not exist.", id),
                "Refresh the library and try again.",
            )
        }
        Err(e) => return server_error("reading a library item failed", e),
    };
    let path = manager().absolute(&item.storage_path);
    let format = item.format.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("the library file could not be read: {}", e))?;
        text_of(&format, &bytes)
    })
    .await
    {
        Ok(Ok(mut text)) => {
            let truncated = text.len() > ASSISTANT_TEXT_LIMIT;
            if truncated {
                let cut = (0..=ASSISTANT_TEXT_LIMIT).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                text.truncate(cut);
            }
            Json(json!({ "id": id, "title": item.title, "format": item.format, "text": text, "truncated": truncated })).into_response()
        }
        Ok(Err(e)) => fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "library_text_unreadable",
            &full(&e),
            "The file may be damaged or password-protected.",
        ),
        Err(e) => server_error("library text task panicked", e),
    }
}

/// POST /drafts/:id/to-library — promote what was just written.
pub async fn draft_to_library(
    State(state): State<UnifiedAppState>,
    Path(id): Path<i64>,
    raw: Bytes,
) -> Response {
    let meta: LibraryMeta = match optional_json(&raw) {
        Ok(m) => m,
        Err(response) => return response,
    };

    if let Some(kind) = meta.kind.as_deref() {
        if !LIBRARY_KINDS.contains(&kind) {
            return bad_kind(kind);
        }
    }

    let db = state.shared_state.database_pool.clone();
    let draft = match db.drafts.get_draft(id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(id),
        Err(e) => return server_error("reading a draft failed", e),
    };

    let title = meta.title.clone().unwrap_or_else(|| draft.title.clone());
    let format = draft.format.clone();

    match tokio::task::spawn_blocking(move || -> anyhow::Result<LibraryItemRecord> {
        // The current version, not the original import: promoting a draft
        // means promoting the document as it now reads.
        let bytes = manager().read_current(&db, id)?;
        store_library_bytes(&db, &meta, &format, &bytes, Some(id), &title)
    })
    .await
    {
        Ok(Ok(item)) => {
            (StatusCode::CREATED, Json(LibraryItemSummary::from(item))).into_response()
        }
        Ok(Err(e)) => server_error("promoting a draft to the library failed", full(&e)),
        Err(e) => server_error("promote-to-library task panicked", e),
    }
}

/// GET /clauses
pub async fn list_clauses(State(state): State<UnifiedAppState>) -> Response {
    let db = state.shared_state.database_pool.clone();
    match db.library.list_clauses() {
        Ok(cs) => {
            let out: Vec<ClauseSummary> = cs.into_iter().map(ClauseSummary::from).collect();
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(e) => server_error("listing clauses failed", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ClauseBody {
    pub title: String,
    pub body: String,
    pub jurisdiction: Option<String>,
}

/// POST /clauses
pub async fn create_clause(
    State(state): State<UnifiedAppState>,
    Json(body): Json<ClauseBody>,
) -> Response {
    let title = body.title.trim();
    let text = body.body.trim();
    if title.is_empty() || text.is_empty() {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "clause_incomplete",
            "A clause needs both a name and its text.",
            "Give the clause a name and paste the provision.",
        );
    }

    let db = state.shared_state.database_pool.clone();
    match db
        .library
        .create_clause(title, text, body.jurisdiction.as_deref())
    {
        Ok(c) => (StatusCode::CREATED, Json(ClauseSummary::from(c))).into_response(),
        Err(e) => server_error("saving a clause failed", e),
    }
}

/// DELETE /clauses/:id
pub async fn delete_clause(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    let db = state.shared_state.database_pool.clone();
    match db.library.delete_clause(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(
            StatusCode::NOT_FOUND,
            "clause_not_found",
            &full(&e),
            "Refresh the clause list and try again.",
        ),
    }
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
    async fn undo_and_redo_step_through_edits_and_a_new_edit_clears_redo() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;
        let text = |body: &serde_json::Value| body["model"]["blocks"][0]["runs"][0]["text"].as_str().unwrap_or("").to_string();

        let (_, original) = call(state, "GET", &format!("/drafts/{}/content", id), None).await;
        let original_text = original["blocks"][0]["runs"][0]["text"].as_str().unwrap().to_string();

        let edit = |version: i64, t: &'static str| {
            json!({ "base_version": version, "patches": [{ "op": "SetRunText", "addr": "body/p[0]/r[0]", "text": t }] })
        };
        let (_, one) = call(state, "POST", &format!("/drafts/{}/patch", id), Some(edit(1, "One"))).await;
        let v = one["version"].as_i64().unwrap();
        let (_, two) = call(state, "POST", &format!("/drafts/{}/patch", id), Some(edit(v, "Two"))).await;
        assert_eq!(two["undo_depth"], 2);
        assert_eq!(two["redo_depth"], 0);

        let (status, undone) = call(state, "POST", &format!("/drafts/{}/undo", id), Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{:?}", undone);
        assert_eq!(text(&undone), "One");
        assert_eq!(undone["redo_depth"], 1);

        let (_, back) = call(state, "POST", &format!("/drafts/{}/undo", id), Some(json!({}))).await;
        assert_eq!(text(&back), original_text, "undo reaches the original");
        assert_eq!(back["undo_depth"], 0);

        let (_, redone) = call(state, "POST", &format!("/drafts/{}/redo", id), Some(json!({}))).await;
        assert_eq!(text(&redone), "One");

        let v = redone["version"].as_i64().unwrap();
        let (_, fresh) = call(state, "POST", &format!("/drafts/{}/patch", id), Some(edit(v, "Fresh"))).await;
        assert_eq!(fresh["redo_depth"], 0, "a new edit makes the undone future unreachable");

        let (_, depth) = call(state, "GET", &format!("/drafts/{}/undo", id), None).await;
        assert_eq!(depth["undo_depth"], 2);

        // Nothing to redo is not an error: the document is returned unchanged.
        let (status, same) = call(state, "POST", &format!("/drafts/{}/redo", id), Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(text(&same), "Fresh");
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
    async fn a_blank_document_can_be_created_in_every_format_that_has_a_template() {
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

        // Word, Excel and PowerPoint can now be started empty too, from the
        // templates in `ooxml::blank`. Each has to come back as a draft that
        // OPENS — a blank package that cannot be read back would be worse than
        // the refusal this replaced.
        for (format, title) in [("docx", "Advice"), ("xlsx", "Damages model"), ("pptx", "Pitch")] {
            let (status, body) = call(
                state,
                "POST",
                "/drafts/blank",
                Some(json!({ "title": title, "format": format })),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "blank {format}: {body:?}");

            let (status, model) =
                call(state, "GET", &format!("/drafts/{}/content", body["id"]), None).await;
            assert_eq!(status, StatusCode::OK, "blank {format} does not open: {model:?}");
            assert_eq!(model["format"], format);
        }

        // A format with no template is still refused, and says so.
        let (status, body) = call(
            state,
            "POST",
            "/drafts/blank",
            Some(json!({ "title": "Scan", "format": "pdf" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "blank_unsupported");
    }

    #[tokio::test]
    async fn comparing_two_versions_of_a_draft_shows_what_changed() {
        let world = test_state().await;
        let state = &world.state;

        let (_, draft) = upload(
            state,
            "clause.txt",
            b"The Receiving Party shall keep the Information secret.",
        )
        .await;
        let id = draft["id"].as_i64().unwrap();

        let (status, _) = call(
            state,
            "POST",
            &format!("/drafts/{id}/patch"),
            Some(json!({
                "patches": [{ "op": "SetText",
                              "content": "The Receiving Party shall keep the Information confidential." }],
                "base_version": 1,
                "explicit_save": true
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            state,
            "POST",
            "/compare",
            Some(json!({
                "left":  { "kind": "draft", "id": id, "version": 1 },
                "right": { "kind": "draft", "id": id }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");

        // One word changed: it must read as an EDIT, not as the whole clause
        // being removed and a new one appearing.
        assert_eq!(body["summary"]["changes"], 1, "{body:?}");
        assert_eq!(body["summary"]["deletions"], 0);
        assert_eq!(body["summary"]["identical"], false);
        assert_eq!(body["left"]["version"], 1);
        assert_eq!(body["right"]["version"], 2);

        let words = body["ops"][0]["words"].as_array().unwrap();
        assert!(words.iter().any(|w| w["op"] == "delete" && w["text"].as_str().unwrap().contains("secret")));
        assert!(words.iter().any(|w| w["op"] == "insert" && w["text"].as_str().unwrap().contains("confidential")));
    }

    #[tokio::test]
    async fn comparing_a_draft_with_itself_reports_identical() {
        let world = test_state().await;
        let state = &world.state;
        let (_, draft) = upload(state, "notes.txt", b"Unchanged text.").await;
        let id = draft["id"].as_i64().unwrap();

        let (status, body) = call(
            state,
            "POST",
            "/compare",
            Some(json!({
                "left":  { "kind": "draft", "id": id },
                "right": { "kind": "draft", "id": id }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        assert_eq!(body["summary"]["identical"], true, "{body:?}");
    }

    #[tokio::test]
    async fn comparing_something_that_cannot_be_compared_says_why() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        // A draft that does not exist, and a missing version, are both the
        // caller asking for something impossible rather than a server fault.
        let (status, body) = call(
            state,
            "POST",
            "/compare",
            Some(json!({
                "left":  { "kind": "draft", "id": id },
                "right": { "kind": "draft", "id": 999_999 }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body:?}");
        assert_eq!(body["error"], "compare_unsupported");
    }

    #[tokio::test]
    async fn a_document_is_searchable_over_http_as_soon_as_it_is_imported() {
        let world = test_state().await;
        let state = &world.state;

        let (status, draft) = upload(
            state,
            "advice.txt",
            "The indemnity cap is limited to fees paid under this engagement.".as_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{draft:?}");

        // Indexed on import, not on first edit: a document nobody has opened
        // yet is exactly the one search has to be able to find.
        let (status, body) = call(state, "GET", "/workspace/search?q=indemnity", None).await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        let hits = body["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{body:?}");
        assert_eq!(hits[0]["draft_id"], draft["id"]);
        assert!(
            hits[0]["snippet"].as_str().unwrap().contains('\u{2}'),
            "the snippet marks the match: {:?}",
            hits[0]["snippet"]
        );
    }

    #[tokio::test]
    async fn search_follows_an_edit_and_forgets_a_deleted_document() {
        let world = test_state().await;
        let state = &world.state;

        let (_, draft) = upload(state, "notes.txt", b"arbitration in Singapore").await;
        let id = draft["id"].as_i64().unwrap();

        let (status, _) = call(
            state,
            "POST",
            &format!("/drafts/{id}/patch"),
            Some(json!({
                "patches": [{ "op": "SetText", "content": "mediation in Geneva" }],
                "base_version": 1,
                "explicit_save": true
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, stale) = call(state, "GET", "/workspace/search?q=arbitration", None).await;
        assert!(stale["hits"].as_array().unwrap().is_empty(), "old text is gone");
        let (_, fresh) = call(state, "GET", "/workspace/search?q=mediation", None).await;
        assert_eq!(fresh["hits"].as_array().unwrap().len(), 1, "new text is found");

        call(state, "DELETE", &format!("/drafts/{id}"), None).await;

        // A deleted document must not stay searchable: that would both offer a
        // result that cannot be opened and leak its text after deletion.
        let (_, after) = call(state, "GET", "/workspace/search?q=mediation", None).await;
        assert!(after["hits"].as_array().unwrap().is_empty(), "{after:?}");
    }

    #[tokio::test]
    async fn an_empty_or_punctuation_only_search_is_answered_not_refused() {
        let world = test_state().await;
        let state = &world.state;
        upload(state, "notes.txt", b"some text").await;

        for q in ["", "()*", "AND"] {
            let (status, body) =
                call(state, "GET", &format!("/workspace/search?q={q}"), None).await;
            assert_eq!(status, StatusCode::OK, "query {q:?} must not error: {body:?}");
            assert!(body["hits"].is_array(), "query {q:?}: {body:?}");
        }
    }

    #[tokio::test]
    async fn a_preview_returns_the_documents_real_first_lines() {
        let world = test_state().await;
        let state = &world.state;

        let (status, draft) = upload(
            state,
            "advice.txt",
            b"\n\nSubject: Summary of Complaint\n\nDear Client,\nWe have reviewed the filing.\n",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{draft:?}");

        let (status, preview) =
            call(state, "GET", &format!("/drafts/{}/preview", draft["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "{preview:?}");
        assert_eq!(preview["kind"], "text");

        let lines = preview["lines"].as_array().unwrap();
        // The leading blank lines must be skipped, or a thumbnail of a document
        // that starts with whitespace shows nothing at all.
        assert_eq!(lines[0]["text"], "Subject: Summary of Complaint");
        assert!(lines.iter().any(|l| l["text"] == "We have reviewed the filing."));
    }

    #[tokio::test]
    async fn a_word_preview_carries_real_paragraphs_and_flags_its_headings() {
        let world = test_state().await;
        let state = &world.state;
        let id = new_docx_draft(state).await;

        let (status, preview) = call(state, "GET", &format!("/drafts/{id}/preview"), None).await;
        assert_eq!(status, StatusCode::OK, "{preview:?}");
        assert_eq!(preview["kind"], "text");

        let lines = preview["lines"].as_array().unwrap();
        assert!(!lines.is_empty(), "a Word document previews as its own text");
        assert!(
            lines.iter().all(|l| l["heading"].is_boolean()),
            "every line says whether it is a heading, so the miniature can weight it",
        );
    }

    #[tokio::test]
    async fn a_blank_document_previews_without_erroring() {
        let world = test_state().await;
        let state = &world.state;

        // The empty case is the one a grid actually hits — a just-created blank
        // has no content at all, and must still yield a tile.
        for format in ["docx", "xlsx", "pptx", "txt"] {
            let (status, draft) = call(
                state,
                "POST",
                "/drafts/blank",
                Some(json!({ "title": format!("Empty {format}"), "format": format })),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{draft:?}");

            let (status, preview) =
                call(state, "GET", &format!("/drafts/{}/preview", draft["id"]), None).await;
            assert_eq!(status, StatusCode::OK, "blank {format} preview: {preview:?}");
            assert!(preview["kind"].is_string(), "blank {format} still gets a tile");
        }
    }

    #[tokio::test]
    async fn a_matter_can_be_opened_filed_into_and_closed_over_http() {
        let world = test_state().await;
        let state = &world.state;

        let (status, matter) = call(
            state,
            "POST",
            "/matters",
            Some(json!({
                "name": "Acme v. Initech",
                "client": "Acme Corp",
                "matter_number": "2026-0417",
                "jurisdiction": "GB-EAW",
                "language": "en-GB",
                "practice_area": "litigation"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{matter:?}");
        assert_eq!(matter["status"], "open");
        // Jurisdiction and language are what let one product serve a practice
        // spanning several countries, so they must survive the round trip.
        assert_eq!(matter["jurisdiction"], "GB-EAW");
        assert_eq!(matter["language"], "en-GB");

        let (_, draft) = upload(state, "notes.txt", b"Particulars of claim").await;
        let (status, filed) = call(
            state,
            "PATCH",
            &format!("/drafts/{}/matter", draft["id"]),
            Some(json!({ "matter_id": matter["id"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{filed:?}");
        assert_eq!(filed["matter_id"], matter["id"]);
        assert_eq!(filed["status"], "draft", "documents start as drafts");

        let (status, closed) = call(
            state,
            "PATCH",
            &format!("/matters/{}", matter["id"]),
            Some(json!({ "status": "closed" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{closed:?}");
        assert_eq!(closed["status"], "closed");
        assert!(!closed["closed_at"].is_null(), "closing records when");
    }

    #[tokio::test]
    async fn deleting_a_matter_returns_its_documents_to_unfiled_and_never_deletes_them() {
        let world = test_state().await;
        let state = &world.state;

        let (_, matter) = call(state, "POST", "/matters", Some(json!({ "name": "Acme" }))).await;
        let (_, draft) = upload(state, "notes.txt", b"Confidential settlement terms").await;
        call(
            state,
            "PATCH",
            &format!("/drafts/{}/matter", draft["id"]),
            Some(json!({ "matter_id": matter["id"] })),
        )
        .await;

        let (status, result) =
            call(state, "DELETE", &format!("/matters/{}", matter["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "{result:?}");
        assert_eq!(result["drafts_moved_to_unfiled"], 1);

        // Deleting a LABEL must never destroy work.
        let (status, after) = call(state, "GET", &format!("/drafts/{}", draft["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "the document outlives its matter: {after:?}");
        assert_eq!(after["matter_id"], serde_json::Value::Null);

        let (status, _) =
            call(state, "GET", &format!("/drafts/{}/content", draft["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "and still opens");
    }

    #[tokio::test]
    async fn a_document_moves_through_its_status_lifecycle_and_nonsense_is_refused() {
        let world = test_state().await;
        let state = &world.state;
        let (_, draft) = upload(state, "notes.txt", b"Engagement terms").await;
        let id = draft["id"].as_i64().unwrap();

        for s in ["in_review", "completed"] {
            let (status, body) = call(
                state,
                "PATCH",
                &format!("/drafts/{id}/status"),
                Some(json!({ "status": s })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "status {s}: {body:?}");
            assert_eq!(body["status"], s);
        }

        let (status, body) = call(
            state,
            "PATCH",
            &format!("/drafts/{id}/status"),
            Some(json!({ "status": "shredded" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
        assert_eq!(body["error"], "invalid_status");
        assert!(body["action"].as_str().unwrap().contains("completed"), "{body:?}");
    }

    #[tokio::test]
    async fn a_duplicate_matter_number_is_a_conflict_and_a_blank_name_is_refused() {
        let world = test_state().await;
        let state = &world.state;

        call(state, "POST", "/matters", Some(json!({ "name": "First", "matter_number": "2026-1" })))
            .await;

        let (status, dup) = call(
            state,
            "POST",
            "/matters",
            Some(json!({ "name": "Second", "matter_number": "2026-1" })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{dup:?}");
        assert_eq!(dup["error"], "matter_number_taken");

        for bad in ["", "   "] {
            let (status, body) =
                call(state, "POST", "/matters", Some(json!({ "name": bad }))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "name {bad:?}: {body:?}");
        }
    }

    #[tokio::test]
    async fn a_matter_patch_changes_only_what_it_names() {
        let world = test_state().await;
        let state = &world.state;
        let (_, matter) = call(
            state,
            "POST",
            "/matters",
            Some(json!({ "name": "Acme", "client": "Acme Corp", "jurisdiction": "SG" })),
        )
        .await;

        let (status, patched) = call(
            state,
            "PATCH",
            &format!("/matters/{}", matter["id"]),
            Some(json!({ "name": "Acme v. Initech" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{patched:?}");
        assert_eq!(patched["name"], "Acme v. Initech");
        assert_eq!(patched["client"], "Acme Corp", "untouched by the patch");
        assert_eq!(patched["jurisdiction"], "SG", "untouched by the patch");

        // Present-and-null means CLEAR, and must stay distinguishable from
        // absent, which means leave alone.
        let (status, cleared) = call(
            state,
            "PATCH",
            &format!("/matters/{}", matter["id"]),
            Some(json!({ "client": null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cleared:?}");
        assert_eq!(cleared["client"], serde_json::Value::Null);
        assert_eq!(cleared["jurisdiction"], "SG", "still untouched");
    }

    #[tokio::test]
    async fn folders_can_be_created_listed_renamed_and_nested_over_http() {
        let world = test_state().await;
        let state = &world.state;

        let (status, root) =
            call(state, "POST", "/draft-folders", Some(json!({ "name": "Matters" }))).await;
        assert_eq!(status, StatusCode::CREATED, "{root:?}");
        assert_eq!(root["parent_id"], serde_json::Value::Null, "top level");

        let (status, child) = call(
            state,
            "POST",
            "/draft-folders",
            Some(json!({ "name": "Acme", "parent_id": root["id"] })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{child:?}");
        assert_eq!(child["parent_id"], root["id"]);

        // A duplicate sibling is the user's mistake, so it must be a 409 with
        // actionable text — not a 500 telling them to try again.
        let (status, dup) =
            call(state, "POST", "/draft-folders", Some(json!({ "name": "Matters" }))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{dup:?}");
        assert_eq!(dup["error"], "folder_exists");

        let (status, renamed) = call(
            state,
            "PATCH",
            &format!("/draft-folders/{}", child["id"]),
            Some(json!({ "name": "Acme Corp" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(renamed["name"], "Acme Corp");

        let (status, listed) = call(state, "GET", "/draft-folders", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["folders"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_folder_cannot_be_moved_into_its_own_subtree() {
        let world = test_state().await;
        let state = &world.state;

        let (_, parent) =
            call(state, "POST", "/draft-folders", Some(json!({ "name": "Matters" }))).await;
        let (_, child) = call(
            state,
            "POST",
            "/draft-folders",
            Some(json!({ "name": "Acme", "parent_id": parent["id"] })),
        )
        .await;

        let (status, body) = call(
            state,
            "PATCH",
            &format!("/draft-folders/{}", parent["id"]),
            Some(json!({ "parent_id": child["id"] })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body:?}");
        assert_eq!(body["error"], "folder_move_refused");
    }

    #[tokio::test]
    async fn deleting_a_folder_moves_its_drafts_to_the_root_and_never_deletes_them() {
        let world = test_state().await;
        let state = &world.state;

        let (status, draft) = upload(state, "notes.txt", b"Line one\n").await;
        assert_eq!(status, StatusCode::CREATED, "{draft:?}");
        let (_, folder) =
            call(state, "POST", "/draft-folders", Some(json!({ "name": "Matters" }))).await;

        let (status, moved) = call(
            state,
            "PATCH",
            &format!("/drafts/{}/folder", draft["id"]),
            Some(json!({ "folder_id": folder["id"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{moved:?}");
        assert_eq!(moved["folder_id"], folder["id"]);

        let (status, result) =
            call(state, "DELETE", &format!("/draft-folders/{}", folder["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "{result:?}");
        assert_eq!(result["drafts_moved_to_root"], 1);

        // The document itself must be untouched and still openable — deleting a
        // label is not deleting a file.
        let (status, after) =
            call(state, "GET", &format!("/drafts/{}", draft["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "the draft outlives its folder: {after:?}");
        assert_eq!(after["folder_id"], serde_json::Value::Null);

        let (status, _) =
            call(state, "GET", &format!("/drafts/{}/content", draft["id"]), None).await;
        assert_eq!(status, StatusCode::OK, "and still opens");
    }

    #[tokio::test]
    async fn a_draft_can_be_sent_back_to_the_root_and_bad_names_are_refused() {
        let world = test_state().await;
        let state = &world.state;

        let (_, draft) = upload(state, "notes.txt", b"Line one\n").await;
        let (_, folder) =
            call(state, "POST", "/draft-folders", Some(json!({ "name": "Matters" }))).await;
        call(
            state,
            "PATCH",
            &format!("/drafts/{}/folder", draft["id"]),
            Some(json!({ "folder_id": folder["id"] })),
        )
        .await;

        // Present-and-null must mean "move to the root", not "leave it alone".
        let (status, back) = call(
            state,
            "PATCH",
            &format!("/drafts/{}/folder", draft["id"]),
            Some(json!({ "folder_id": null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{back:?}");
        assert_eq!(back["folder_id"], serde_json::Value::Null);

        for bad in ["", "   ", "Client/Acme", "Client\\Acme"] {
            let (status, body) =
                call(state, "POST", "/draft-folders", Some(json!({ "name": bad }))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "name {bad:?} must be refused: {body:?}");
        }
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

    // ---------------------------------------------------------- the library

    /// Add a file to the library over real multipart HTTP, metadata included.
    async fn library_upload(
        state: &UnifiedAppState,
        filename: &str,
        bytes: &[u8],
        fields: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let boundary = "----OCALibraryBoundary42";
        let mut body: Vec<u8> = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
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
            .uri("/library")
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

    #[tokio::test]
    async fn a_template_can_be_added_to_the_library_and_listed() {
        let world = test_state().await;
        let state = &world.state;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();

        let (status, body) = library_upload(
            state,
            "Mutual NDA.docx",
            &bytes,
            &[
                ("kind", "template"),
                ("jurisdiction", "GB-EAW"),
                ("practice_area", "corporate"),
                ("tags", "NDA, Confidentiality"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", body);
        assert_eq!(body["title"], "Mutual NDA");
        assert_eq!(body["kind"], "template");
        assert_eq!(body["jurisdiction"], "GB-EAW");
        assert_eq!(body["byte_size"].as_i64().unwrap(), bytes.len() as i64);
        assert_eq!(
            body["tags"],
            serde_json::json!(["confidentiality", "nda"]),
            "tags come back folded and sorted"
        );
        assert!(body.get("storage_path").is_none(), "disk paths stay server-side");

        let (status, list) = call(state, "GET", "/library", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn models_can_be_starred_and_unstarred() {
        let world = test_state().await;
        let state = &world.state;
        let star = |id: &str, on: bool| json!({ "model_id": id, "starred": on });

        let (status, body) = call(state, "POST", "/models/marks/star", Some(star("gemma-4-12b", true))).await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        call(state, "POST", "/models/marks/star", Some(star("qwen3-8b", true))).await;
        // Starring twice does not duplicate.
        call(state, "POST", "/models/marks/star", Some(star("qwen3-8b", true))).await;
        let (_, marks) = call(state, "GET", "/models/marks", None).await;
        assert_eq!(marks["starred"], json!(["gemma-4-12b", "qwen3-8b"]));

        let (_, after) = call(state, "POST", "/models/marks/star", Some(star("gemma-4-12b", false))).await;
        assert_eq!(after["starred"], json!(["qwen3-8b"]));

        let (status, _) = call(state, "POST", "/models/marks/star", Some(star("  ", true))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_library_item_can_be_read_as_text_for_the_assistant() {
        let world = test_state().await;
        let state = &world.state;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (_, item) = library_upload(state, "MSA.docx", &bytes, &[("kind", "precedent")]).await;
        let id = item["id"].as_i64().unwrap();

        let (status, body) = call(state, "GET", &format!("/library/{}/text", id), None).await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        assert!(body["text"].as_str().unwrap().contains("Master Services Agreement"), "{:?}", body);
        assert_eq!(body["truncated"], false);

        let (status, _) = call(state, "GET", "/library/9999/text", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_library_upload_the_editor_cannot_open_is_refused() {
        let world = test_state().await;
        let (status, body) =
            library_upload(&world.state, "scan.jpg", b"not a document", &[]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", body);
        assert_eq!(body["error"], "not_editable");

        let (_, list) = call(&world.state, "GET", "/library", None).await;
        assert!(
            list.as_array().unwrap().is_empty(),
            "a refused upload leaves no row behind"
        );
    }

    #[tokio::test]
    async fn a_library_item_must_be_a_template_or_a_precedent() {
        let world = test_state().await;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (status, body) =
            library_upload(&world.state, "Thing.docx", &bytes, &[("kind", "favourite")]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", body);
        assert_eq!(body["error"], "bad_kind");
    }

    #[tokio::test]
    async fn a_draft_can_be_promoted_to_the_library_and_started_from_again() {
        let world = test_state().await;
        let state = &world.state;
        let draft_id = new_docx_draft(state).await;

        let (status, item) = call(
            state,
            "POST",
            &format!("/drafts/{}/to-library", draft_id),
            Some(serde_json::json!({
                "title": "Engagement Letter (house)",
                "kind": "precedent",
                "tags": ["engagement"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", item);
        assert_eq!(item["source_draft_id"], draft_id, "provenance is recorded");
        // Asserted because it was once silently dropped: the body extractor
        // answered "no metadata" to a body it could not read.
        assert_eq!(item["title"], "Engagement Letter (house)", "the title sent was used");
        assert_eq!(item["tags"], serde_json::json!(["engagement"]));
        let item_id = item["id"].as_i64().unwrap();

        // Starting from a precedent gives a new, independent draft.
        let (status, new_draft) = call(
            state,
            "POST",
            &format!("/library/{}/new-draft", item_id),
            Some(serde_json::json!({ "title": "Acme engagement" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", new_draft);
        assert_eq!(new_draft["title"], "Acme engagement");
        assert_eq!(new_draft["origin_kind"], "library");
        assert_ne!(new_draft["id"], draft_id, "a copy, not the original");

        // Both documents are real and openable.
        let (status, _) = call(
            state,
            "GET",
            &format!("/drafts/{}/content", new_draft["id"].as_i64().unwrap()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Deleting the source matter's draft must not touch the library.
        let (status, _) = call(state, "DELETE", &format!("/drafts/{}", draft_id), None).await;
        assert!(status.is_success());
        let (_, list) = call(state, "GET", "/library", None).await;
        assert_eq!(
            list.as_array().unwrap().len(),
            1,
            "the precedent survives the draft it came from"
        );
    }

    #[tokio::test]
    async fn a_draft_started_from_a_precedent_can_be_filed_to_a_matter_at_once() {
        let world = test_state().await;
        let state = &world.state;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (_, item) = library_upload(state, "Claim Form.docx", &bytes, &[]).await;

        let (_, matter) = call(
            state,
            "POST",
            "/matters",
            Some(serde_json::json!({ "name": "Acme v Brown" })),
        )
        .await;
        let matter_id = matter["id"].as_i64().unwrap();

        let (status, draft) = call(
            state,
            "POST",
            &format!("/library/{}/new-draft", item["id"].as_i64().unwrap()),
            Some(serde_json::json!({ "matter_id": matter_id })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", draft);
        assert_eq!(
            draft["matter_id"], matter_id,
            "the response carries the matter, not the NULL it was created with"
        );
    }

    #[tokio::test]
    async fn deleting_a_library_item_takes_its_file_with_it() {
        let world = test_state().await;
        let state = &world.state;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (_, item) = library_upload(state, "Old Form.docx", &bytes, &[]).await;
        let id = item["id"].as_i64().unwrap();

        let on_disk = world._dir.path().join(format!("library/{}.docx", id));
        assert!(on_disk.exists(), "the bytes were written");

        let (status, _) = call(state, "DELETE", &format!("/library/{}", id), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!on_disk.exists(), "and removed again");

        let (status, _) = call(state, "DELETE", &format!("/library/{}", id), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "deleting it twice is not silent");
    }

    #[tokio::test]
    async fn starting_from_a_precedent_that_does_not_exist_is_a_clean_404() {
        let world = test_state().await;
        let (status, body) = call(&world.state, "POST", "/library/999/new-draft", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{:?}", body);
        assert_eq!(body["error"], "library_item_not_found");
    }

    #[tokio::test]
    async fn a_promotion_whose_details_cannot_be_read_is_refused_rather_than_filed_bare() {
        let world = test_state().await;
        let state = &world.state;
        let draft_id = new_docx_draft(state).await;

        // `tags` as a string rather than a list. Accepting this and filing the
        // precedent with no tags would tell the user their document was
        // classified when it was not.
        let (status, body) = call(
            state,
            "POST",
            &format!("/drafts/{}/to-library", draft_id),
            Some(serde_json::json!({ "title": "Bare", "tags": "engagement" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}", body);
        assert_eq!(body["error"], "bad_request_body");

        let (_, list) = call(state, "GET", "/library", None).await;
        assert!(list.as_array().unwrap().is_empty(), "and nothing was filed");
    }

    #[tokio::test]
    async fn starting_from_a_precedent_needs_no_body_at_all() {
        let world = test_state().await;
        let state = &world.state;
        let bytes = crate::document_workspace::ooxml::package::fixtures::docx();
        let (_, item) = library_upload(state, "Notice.docx", &bytes, &[]).await;

        // An empty body means "use the defaults", which is what a plain
        // "start from this one" click sends.
        let (status, draft) = call(
            state,
            "POST",
            &format!("/library/{}/new-draft", item["id"].as_i64().unwrap()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", draft);
        assert_eq!(draft["title"], "Notice", "it inherits the precedent's name");
    }

    #[tokio::test]
    async fn clauses_are_stored_as_text_and_can_be_listed_and_removed() {
        let world = test_state().await;
        let state = &world.state;

        let (status, clause) = call(
            state,
            "POST",
            "/clauses",
            Some(serde_json::json!({
                "title": "Governing law — England & Wales",
                "body": "This agreement is governed by the laws of England and Wales.",
                "jurisdiction": "GB-EAW"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{:?}", clause);
        assert!(
            clause["body"].as_str().unwrap().contains("England and Wales"),
            "the text comes back in the row, not as a file to open"
        );

        let (status, list) = call(state, "GET", "/clauses", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        let id = clause["id"].as_i64().unwrap();
        let (status, _) = call(state, "DELETE", &format!("/clauses/{}", id), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, list) = call(state, "GET", "/clauses", None).await;
        assert!(list.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_clause_without_its_text_is_refused() {
        let world = test_state().await;
        let (status, body) = call(
            &world.state,
            "POST",
            "/clauses",
            Some(serde_json::json!({ "title": "Liability cap", "body": "   " })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", body);
        assert_eq!(body["error"], "clause_incomplete");
    }
}
