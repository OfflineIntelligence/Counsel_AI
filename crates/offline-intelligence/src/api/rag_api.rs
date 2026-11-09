//! Retrieval, embedding model and matter-scoping endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tracing::info;

use crate::rag::{self, model, search::RetrieveOptions};
use crate::shared_state::UnifiedAppState;

fn internal(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

/// GET /embeddings/status — the active model and the index.
pub async fn embeddings_status(State(state): State<UnifiedAppState>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let totals = tokio::task::spawn_blocking(move || db.rag.totals()).await;
    let totals = match totals {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let rt = rag::runtime();
    let spec = model::active();
    Json(serde_json::json!({
        "model": {
            "id": spec.id,
            "display_name": spec.display_name,
            "file_name": spec.file_name,
            "size_bytes": spec.size_bytes,
            "path": model::model_path_of(spec),
        },
        "installed": model::is_installed_spec(spec),
        "download": model::download_status_of(spec),
        "server_running": rt.embedder.warm(),
        "index": totals,
    }))
    .into_response()
}

/// GET /embeddings/models — every embedding model with its state.
pub async fn embeddings_models() -> Response {
    let active = model::active().id;
    let list: Vec<serde_json::Value> = model::CATALOG
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "display_name": m.display_name,
                "description": m.description,
                "size_bytes": m.size_bytes,
                "license": m.license,
                "active": m.id == active,
                "installed": model::is_installed_spec(m),
                "download": model::download_status_of(m),
            })
        })
        .collect();
    Json(serde_json::json!({ "active": active, "models": list })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct ModelIdBody {
    #[serde(default)]
    pub model_id: Option<String>,
}

fn spec_of(body: &Option<Json<ModelIdBody>>) -> Result<&'static model::EmbeddingModelSpec, Response> {
    match body.as_ref().and_then(|b| b.model_id.clone()) {
        Some(id) => model::find(&id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("Unknown embedding model '{}'", id)).into_response()),
        None => Ok(model::active()),
    }
}

/// POST /embeddings/install {model_id?} — start or resume a download.
pub async fn embeddings_install(body: Option<Json<ModelIdBody>>) -> Response {
    let spec = match spec_of(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let started = model::start_download_of(spec);
    Json(serde_json::json!({ "started": started, "model_id": spec.id, "download": model::download_status_of(spec) })).into_response()
}

/// POST /embeddings/pause — pause the running download (resumable).
pub async fn embeddings_pause() -> Response {
    Json(serde_json::json!({ "paused": model::pause_download() })).into_response()
}

/// POST /embeddings/cancel {model_id?} — stop and discard a download.
pub async fn embeddings_cancel(body: Option<Json<ModelIdBody>>) -> Response {
    let spec = match spec_of(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    Json(serde_json::json!({ "cancelled": model::cancel_download_of(spec), "model_id": spec.id })).into_response()
}

/// POST /embeddings/select {model_id} — use another installed model. Every
/// document is re-embedded with it in the background.
pub async fn embeddings_select(Json(body): Json<ModelIdBody>) -> Response {
    let Some(id) = body.model_id else {
        return (StatusCode::BAD_REQUEST, "model_id is required").into_response();
    };
    let Some(spec) = model::find(&id) else {
        return (StatusCode::NOT_FOUND, format!("Unknown embedding model '{}'", id)).into_response();
    };
    if !model::is_installed_spec(spec) {
        return (StatusCode::CONFLICT, format!("'{}' is not installed yet; download it first", spec.display_name)).into_response();
    }
    match model::set_active(spec.id) {
        Ok(_) => {
            if let Ok(mut idx) = rag::runtime().index.write() {
                idx.loaded = false;
            }
            Json(serde_json::json!({ "success": true, "active": spec.id })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// DELETE /embeddings/models/:id — remove a model that is not active.
pub async fn embeddings_remove(Path(id): Path<String>) -> Response {
    match model::remove(&id) {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

/// POST /rag/reindex — rebuild every chunk index and embedding.
pub async fn reindex(State(state): State<UnifiedAppState>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = db.rag.conn()?;
        conn.execute("UPDATE rag_documents SET chunker_version = 0", [])?;
        db.rag.clear_all_vectors()?;
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => {
            if let Ok(mut idx) = rag::runtime().index.write() {
                idx.loaded = false;
            }
            rag::signal_index_work();
            info!("Full retrieval re-index requested");
            Json(serde_json::json!({ "success": true })).into_response()
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub budget_bytes: Option<usize>,
}

/// POST /rag/search — retrieval as the chat would see it for a session
/// (or across everything when no session is given).
pub async fn search(State(state): State<UnifiedAppState>, Json(req): Json<SearchRequest>) -> Response {
    let db = state.shared_state.database_pool.clone();
    let rt = rag::runtime();
    let (scope, prefer) = match req.session_id.as_deref() {
        Some(sid) => {
            let s = rag::scope::resolve(&db, sid);
            (s.allowed.clone(), s.session_documents.clone())
        }
        None => (None, Vec::new()),
    };
    let opts = RetrieveOptions {
        budget_bytes: req.budget_bytes.unwrap_or(16_000).clamp(1_000, 200_000),
        ..Default::default()
    };
    let result = rag::search::retrieve(&db, &rt, &req.query, scope.as_ref(), &prefer, &opts).await;
    Json(result).into_response()
}

/// GET /documents/:id/index
pub async fn document_index_state(State(state): State<UnifiedAppState>, Path(id): Path<i64>) -> Response {
    match state.shared_state.database_pool.rag.document_state(id) {
        Ok(Some(s)) => Json(s).into_response(),
        Ok(None) => Json(serde_json::json!({ "document_id": id, "status": "pending" })).into_response(),
        Err(e) => internal(e),
    }
}

/// GET /matters/:id/documents
pub async fn list_matter_documents(State(state): State<UnifiedAppState>, Path(matter_id): Path<i64>) -> Response {
    let db = &state.shared_state.database_pool;
    let ids = match db.rag.matter_document_ids(matter_id) {
        Ok(ids) => ids,
        Err(e) => return internal(e),
    };
    let docs: Vec<serde_json::Value> = ids
        .into_iter()
        .filter_map(|id| db.documents.get_document(id).ok())
        .map(|d| {
            let index = db.rag.document_state(d.id).ok().flatten();
            serde_json::json!({
                "id": d.id,
                "original_filename": d.original_filename,
                "char_count": d.char_count,
                "extraction_status": d.extraction_status,
                "last_referenced_at": d.last_referenced_at,
                "index": index,
            })
        })
        .collect();
    Json(docs).into_response()
}

#[derive(Debug, Deserialize)]
pub struct MatterDocumentRequest {
    pub document_id: i64,
}

/// POST /matters/:id/documents
pub async fn add_matter_document(
    State(state): State<UnifiedAppState>,
    Path(matter_id): Path<i64>,
    Json(req): Json<MatterDocumentRequest>,
) -> Response {
    match state.shared_state.database_pool.rag.link_document_to_matter(matter_id, req.document_id) {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE /matters/:id/documents/:document_id
pub async fn remove_matter_document(
    State(state): State<UnifiedAppState>,
    Path((matter_id, document_id)): Path<(i64, i64)>,
) -> Response {
    match state.shared_state.database_pool.rag.unlink_document_from_matter(matter_id, document_id) {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => internal(e),
    }
}
