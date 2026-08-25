// Server/src/admin.rs
// Graceful-shutdown endpoint for the 1-hop architecture.
// (The Tauri shell POSTs /admin/shutdown on app exit.)

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use crate::metrics;
use tracing::info;

pub async fn stop_backend(
    State(state): State<crate::shared_state::UnifiedAppState>,
) -> impl IntoResponse {
    info!("Graceful shutdown initiated");

    // Clone runtime manager Arc outside the lock
    let rt_mgr = state.shared_state.runtime_manager.read()
        .ok()
        .and_then(|guard| guard.clone());

    if let Some(rt_mgr) = rt_mgr {
        // Save the active conversation's KV state BEFORE the runtime goes
        // down: the slot only exists while llama-server is running, so a save
        // attempted after shutdown has nothing to read. Best-effort by design
        // - a warm start is an optimisation, and failing to write one must
        // never delay or block the user closing the app.
        save_warm_start(&state, &rt_mgr).await;

        info!("Shutting down runtime manager...");
        if let Err(e) = rt_mgr.shutdown().await {
            info!("Runtime shutdown: {}", e);
        } else {
            info!("Runtime manager shut down");
        }
    }

    metrics::inc_request("admin_stop", "ok");
    (StatusCode::OK, "System shutdown initiated".to_string())
}

/// Persist the active conversation's slot for a warm start next launch.
///
/// Gathers the identity the restore path will check against - which model,
/// which engine, what context size - from the RUNNING runtime rather than from
/// configuration, so a saved blob is described by what actually produced it.
///
/// Every early return is a normal condition (nothing generated yet, no engine
/// registered, runtime already gone), so they are logged at debug and the
/// shutdown continues.
async fn save_warm_start(
    state: &crate::shared_state::UnifiedAppState,
    rt_mgr: &std::sync::Arc<crate::model_runtime::RuntimeManager>,
) {
    use tracing::debug;

    let session_id = match state.shared_state.last_active_session.read() {
        Ok(guard) => match guard.clone() {
            Some(id) => id,
            None => {
                debug!("No warm start saved: no conversation generated during this run");
                return;
            }
        },
        Err(_) => return,
    };

    let base_url = match rt_mgr.get_base_url().await {
        Some(url) => url,
        None => {
            debug!("No warm start saved: the runtime is not serving");
            return;
        }
    };

    let runtime_config = match rt_mgr.get_current_config().await {
        Some(c) => c,
        None => {
            debug!("No warm start saved: no active runtime configuration");
            return;
        }
    };

    // The runtime knows a model PATH; its directory is named after the
    // sanitised model id, which is the identity the restore check compares.
    let model_id = runtime_config
        .model_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let engine_id = match state.shared_state.engine_manager {
        Some(ref em) => {
            let registry = em.registry.read().await;
            registry.get_default_engine().map(|e| e.id.clone()).unwrap_or_default()
        }
        None => String::new(),
    };

    // The context size the slot actually holds, read back from the running
    // server where available - the requested value can be clamped at load.
    let ctx_size = rt_mgr
        .get_effective_context()
        .await
        .unwrap_or(runtime_config.context_size);

    crate::model_runtime::slot_cache::save_active_slot(
        &state.shared_state.database_pool,
        &base_url,
        &session_id,
        &model_id,
        &engine_id,
        ctx_size,
    )
    .await;
}
