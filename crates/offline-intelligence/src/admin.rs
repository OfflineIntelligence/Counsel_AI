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

    // Pause downloads first, so each writes what it has and stops cleanly.
    // Their pending records stay on disk and the next launch offers to resume
    // them from the partial files.
    if let Some(mm) = state.shared_state.model_manager.as_ref() {
        let paused = mm.downloader.progress_tracker().pause_all_active().await;
        if paused > 0 {
            info!("Paused {} download(s) for shutdown; they will be offered for resume next launch", paused);
            // One chunk interval is enough for every loop to see the pause
            // and flush.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    }

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
        if crate::model_runtime::kv_store::enabled() {
            // Waits for a reply that is still streaming, but never long.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::model_runtime::kv_store::save_active(&state.shared_state),
            )
            .await;
        } else {
            // Same 5 s cap: closing the app must never wait on a slow save.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::model_runtime::lifecycle::save_warm_start(&state.shared_state, &rt_mgr),
            )
            .await;
        }

        if let Some(llama) = crate::rag::runtime().llama.clone() {
            llama.shutdown().await;
        }

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
