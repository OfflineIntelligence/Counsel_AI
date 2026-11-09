//! Thread-based server implementation
//!
//! This module provides the server startup that uses thread-based
//! shared memory architecture. All API handlers access state through
//! Arc-wrapped shared memory (UnifiedAppState) â€” zero network hops
//! between components. The only network call is to the localhost llama-server.

use std::sync::Arc;
use tracing::{info, warn, error};

use crate::{
    config::Config,
    shared_state::{SharedState, UnifiedAppState},
    memory_db::MemoryDatabase,
    model_management::ModelManager,
};

/// Run server with thread-based architecture
/// 
/// # Arguments
/// * `cfg` - Server configuration
/// * `port_tx` - Optional channel to communicate the bound port back to the Tauri main thread.
///               The server prefers cfg.api_port (8888) and falls back to a free port if it stays taken.
pub async fn run_thread_server(cfg: Config, port_tx: Option<std::sync::mpsc::Sender<u16>>) -> anyhow::Result<()> {
    crate::telemetry::init_tracing();
    crate::metrics::init_metrics();
    crate::generation_queue::configure(&cfg);
    cfg.print_config();

    info!("Starting thread-based server architecture");

    // Bind the TCP socket and notify the Tauri main thread FIRST, before any
    // initialisation that can be slow (hardware probes, engine scan, model
    // catalog) or fail. The main thread waits on this port with a timeout;
    // anything ahead of the bind turns a slow disk or antivirus scan into a
    // "failed to bind" startup error. axum::serve accepts connections only
    // after all init is done.
    let (listener, selected_port) = bind_and_notify_port(
        &cfg.api_host,
        cfg.api_port,
        port_tx,          // consumed here; no second send below
    ).await?;
    info!("Socket reserved on port {}; starting initialisation", selected_port);

    // Kill any llama-server left behind by a previous session (force-killed app,
    // pre-job-object build). Runs BEFORE any engine verification or runtime
    // spawn so it can never race against this session's own child processes.
    crate::model_runtime::process_util::kill_orphaned_llama_servers();

    // The chat engine's port must be free BEFORE the runtime starts, or the
    // model load fails. An orphan just killed above releases it within a
    // moment; anything else holding it (another program) is worked around by
    // moving the engine to a free port. The engine is only ever reached
    // through cfg.backend_url, so nothing else needs to know.
    let mut cfg = cfg;
    if !wait_for_free_port(&cfg.llama_host, cfg.llama_port, std::time::Duration::from_secs(2)).await {
        match std::net::TcpListener::bind((cfg.llama_host.as_str(), 0)).and_then(|l| l.local_addr()) {
            Ok(addr) => {
                warn!("Engine port {} is in use by another program; using {} instead", cfg.llama_port, addr.port());
                cfg.llama_port = addr.port();
                cfg.backend_url = format!("http://{}:{}", cfg.llama_host, cfg.llama_port);
            }
            Err(e) => warn!("Engine port {} is in use and no free port was found: {}", cfg.llama_port, e),
        }
    }

    // Initialize database - use canonical app data directory for persistence across updates
    // This ensures data survives app updates and works on Windows where Program Files is read-only
    let memory_db_path = crate::config::get_app_data_dir()
        .join("data")
        .join("memory.db");

    // Ensure the data directory exists
    if let Some(parent) = memory_db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("Failed to create data directory {:?}: {}", parent, e);
        } else {
            info!("Created data directory: {:?}", parent);
        }
    }

    let memory_database = match MemoryDatabase::new(&memory_db_path) {
        Ok(db) => {
            info!("Memory database initialized at: {}", memory_db_path.display());
            Arc::new(db)
        }
        Err(e) => {
            warn!("Failed to initialize memory database at {}: {}. Falling back to in-memory.", memory_db_path.display(), e);
            Arc::new(MemoryDatabase::new_in_memory()?)
        }
    };

    // Initialize shared state (creates LLM worker internally with backend_url)
    let mut shared_state = SharedState::new(cfg.clone(), memory_database.clone())?;

    // Initialize Model Manager
    info!("ðŸ“¦ Initializing Model Manager");
    match ModelManager::new() {
        Ok(model_manager) => {
            let model_manager_arc = Arc::new(model_manager);
            // Initialize the model manager with hardware-aware compatibility scoring
            if let Err(e) = model_manager_arc.initialize(&cfg).await {
                warn!("âš ï¸  Model manager initialization failed: {}", e);
                // Still add the model manager even if initialization fails to have default catalog
                shared_state.model_manager = Some(model_manager_arc);
            } else {
                info!("âœ… Model manager initialized successfully");
                shared_state.model_manager = Some(model_manager_arc);
            }
        }
        Err(e) => {
            warn!("âš ï¸  Failed to create model manager: {}", e);
        }
    }

    // Initialize Engine Manager
    info!("âš™ï¸  Initializing Engine Manager");
    match crate::engine_management::EngineManager::new() {
        Ok(engine_manager) => {
            let engine_manager_arc = Arc::new(engine_manager);

            match engine_manager_arc.initialize(&cfg).await {
                Ok(true) => {
                    // Loud, not silent: a GPU-accelerated default engine with zero
                    // GPU offload (VRAM undetermined) runs at CPU speed. Named in
                    // the log here and in /healthz warnings for the UI.
                    if cfg.gpu_layers == 0 {
                        let reg = engine_manager_arc.registry.read().await;
                        if let Some(default) = reg.get_default_engine() {
                            if default.acceleration != crate::engine_management::AccelerationType::CPU {
                                error!(
                                    "GPU engine '{}' ({}) is the default but gpu_layers=0 — GPU memory \
                                     could not be determined, so inference will run WITHOUT GPU offload.",
                                    default.id, default.acceleration
                                );
                            }
                        }
                    }
                    info!("âœ… Engine manager initialized with engine ready");
                    shared_state.engine_manager = Some(engine_manager_arc.clone());
                }
                Ok(false) => {
                    info!("⚠️  No engine found — the NSIS installer should have placed one in AppData/engines/");
                    shared_state.engine_manager = Some(engine_manager_arc.clone());
                }
                Err(e) => {
                    warn!("âš ï¸  Engine manager scan failed: {}", e);
                    shared_state.engine_manager = Some(engine_manager_arc);
                }
            }
        }
        Err(e) => {
            error!("âŒ Failed to create engine manager: {}", e);
        }
    }

    let shared_state = Arc::new(shared_state);

    // Register the shared state with the vision extraction router: from here
    // on, standalone image extraction consults the live runtime and uses the
    // vision model when one is active (Windows OCR otherwise).
    crate::utils::vision_extraction::register_shared_state(shared_state.clone());

    // Store in shared_state so API handlers can report the real port
    if let Ok(mut guard) = shared_state.http_port.write() {
        *guard = selected_port;
    }

    // Initialize Runtime Manager for multi-format model support
    info!("ðŸš€ Initializing Runtime Manager for multi-format model support");
    let runtime_manager = Arc::new(crate::model_runtime::RuntimeManager::new());

    // CRITICAL: Wait for runtime initialization BEFORE starting HTTP server
    // This prevents 502 errors by ensuring llama-server is ready to accept requests
    info!("â³ Waiting for runtime initialization to complete...");
    
    // Build runtime config via the single source of truth on SharedSystemState
    let runtime_config = shared_state.build_runtime_config(
        std::path::PathBuf::from(&cfg.model_path),
        crate::model_runtime::ModelFormat::GGUF,
        if cfg.llama_bin.is_empty() { None } else { Some(std::path::PathBuf::from(&cfg.llama_bin)) },
    );

    // BLOCKING runtime initialization - wait for engine to be ready before starting HTTP server
    // This prevents race conditions and ensures llama-server is available when UI loads
    info!("â³ Waiting for runtime to be ready...");

    let runtime_initialized = false;

    // Always register the RuntimeManager in shared state so that POST /models/switch
    // can find it even when no engine is installed at startup.  The manager starts
    // idle (no child process running); it will be activated on the first switch_model call.
    if let Err(e) = shared_state.set_runtime_manager(runtime_manager.clone()) {
        error!("âŒ Failed to set runtime manager in shared state: {}", e);
    }
    shared_state.llm_worker.set_runtime_manager(runtime_manager.clone());
    info!("ðŸ”— RuntimeManager registered in shared state (idle until a model is activated)");

    if let Some(ref engine_manager) = shared_state.engine_manager {
        // Check if there's a default engine installed
        let registry = engine_manager.registry.read().await;
        if let Some(default_engine) = registry.get_default_engine_binary_path() {
            drop(registry); // Release the read lock
            info!("âœ… Default engine found: {}", default_engine.display());
            crate::rag::embedder::set_engine_binary(default_engine.clone());

            // Try to load last used model instead of config model (which is often empty)
            let should_auto_load = if cfg.model_path.is_empty() {
                // Try to load last used model from persistent storage
                {
                    let last_model_path = crate::config::get_app_data_dir().join("last_model.txt");
                    if let Ok(last_model_id) = std::fs::read_to_string(&last_model_path) {
                        let last_model_id = last_model_id.trim();
                        info!("ðŸ”„ Found last used model: {}", last_model_id);

                        // Attempt to load this model automatically
                        if let Some(ref model_manager) = shared_state.model_manager {
                            // Get model info from registry
                            let registry = model_manager.registry.read().await;
                            if let Some(model_info) = registry.get_model(last_model_id) {
                                // Check if model is installed
                                if model_info.status == crate::model_management::registry::ModelStatus::Installed {
                                    // Get model path
                                    if let Some(ref filename) = model_info.filename {
                                        let model_path_for_runtime = model_manager.storage.model_path(last_model_id, filename);

                                        if model_path_for_runtime.exists() {
                                            info!("âœ… Auto-loading last used model from: {}", model_path_for_runtime.display());

                                            // VISION: the last-used model may carry a multimodal
                                            // projector. Resolve it from the registry exactly like
                                            // POST /models/switch does — a vision model must never
                                            // silently restart text-only after an app relaunch.
                                            let auto_load_mmproj: Option<std::path::PathBuf> =
                                                match model_info.mmproj_filename {
                                                    Some(ref mmproj_filename) => {
                                                        let p = model_manager
                                                            .storage
                                                            .model_path(last_model_id, mmproj_filename);
                                                        if p.exists() {
                                                            info!(
                                                                "✅ Vision model: projector found at {}",
                                                                p.display()
                                                            );
                                                            Some(p)
                                                        } else {
                                                            error!(
                                                                "Vision model {} is missing its projector \
                                                                 '{}' — NOT auto-loading it text-only. \
                                                                 Reinstall the model to restore vision.",
                                                                last_model_id, mmproj_filename
                                                            );
                                                            None
                                                        }
                                                    }
                                                    None => None,
                                                };
                                            let vision_projector_missing =
                                                model_info.mmproj_filename.is_some()
                                                    && auto_load_mmproj.is_none();
                                            drop(registry); // Release lock before async operations

                                            if vision_projector_missing {
                                                // Loud skip: the user activates a model manually and
                                                // gets the full structured "reinstall" error there.
                                                false
                                            } else {

                                            // Update runtime config with the last used model
                                            let mut updated_config = runtime_config.clone();
                                            updated_config.model_path = model_path_for_runtime;
                                            updated_config.runtime_binary = Some(default_engine.clone());
                                            updated_config.mmproj_path = auto_load_mmproj;

                                            // Initialize with last used model
                                            // Lazy startup: choose the model but load nothing.
                                            // The first chat or Aurora question (or a prewarm)
                                            // loads it; see model_runtime::lifecycle.
                                            match runtime_manager.park(updated_config).await {
                                                Ok(()) => info!("Last used model chosen; it loads on first use"),
                                                Err(e) => warn!("Could not prepare the last used model: {}", e),
                                            }

                                            // Skip the manual load block below
                                            false
                                            } // end vision_projector_missing else
                                        } else {
                                            drop(registry);
                                            warn!("âš ï¸  Last used model file not found: {}", model_path_for_runtime.display());
                                            false
                                        }
                                    } else {
                                        drop(registry);
                                        warn!("âš ï¸  Last used model has no filename in registry");
                                        false
                                    }
                                } else {
                                    drop(registry);
                                    info!("â„¹ï¸  Last used model is not installed - user will need to activate a model");
                                    false
                                }
                            } else {
                                drop(registry);
                                warn!("âš ï¸  Last used model not found in registry: {}", last_model_id);
                                false
                            }
                        } else {
                            info!("â„¹ï¸  Model manager not available - skipping auto-load");
                            false
                        }
                    } else {
                        info!("â„¹ï¸  No last used model found - user will need to activate a model");
                        false
                    }
                }
            } else {
                // Config has a model path - try to use it
                true
            };

            if !should_auto_load {
                info!("â© Skipping manual load - either auto-loaded or will wait for user activation");
                // Don't initialize runtime - either already done via auto-load or waiting for user
            } else {
                // Update the runtime config to use the default engine binary
                let mut updated_config = runtime_config.clone();
                updated_config.runtime_binary = Some(default_engine);
                // Development pairing: MMPROJ_PATH from .env belongs to the
                // MODEL_PATH model being loaded here (production models get
                // their projector from the registry in the auto-load path).
                if !cfg.mmproj_path.is_empty() {
                    info!("Vision (dev override): --mmproj {}", cfg.mmproj_path);
                    updated_config.mmproj_path =
                        Some(std::path::PathBuf::from(&cfg.mmproj_path));
                }

                // BLOCKING initialization with 120 second timeout for llama-server health check
                // Lazy startup, as above: chosen now, loaded on first use.
                match runtime_manager.park(updated_config).await {
                    Ok(()) => info!("MODEL_PATH model chosen; it loads on first use"),
                    Err(e) => warn!("Could not prepare the MODEL_PATH model: {}", e),
                }
            } // End of should_auto_load else block
        } else {
            drop(registry); // Release the read lock
            info!("â³ No engine found - users can download an engine from the Engines panel");
            info!("   Users can download an engine from the Engines panel");
        }
    } else {
        info!("â³ Engine manager not available - local inference unavailable");
    }

    // Mark initialization complete now that runtime check is done
    shared_state.mark_initialization_complete();

    if runtime_initialized {
        info!("✅ Backend initialization complete with runtime ready");
    } else {
        info!("✅ Backend initialization complete (no model loaded yet)");
    }

    // Initialize workers
    let _llm_worker = shared_state.llm_worker.clone();

    // Initialize context orchestrator
    // Context budget follows the REAL context window (ctx_size is detected
    // per machine/model), reserving room for generation. The old hardcoded
    // 4000 forced context rewriting - and prompt-cache busting - on every
    // document conversation.
    let ctx_budget = (cfg.ctx_size as usize).saturating_sub(2048).max(2048);
    let context_orchestrator = match crate::context_engine::create_default_orchestrator(
        memory_database.clone(),
        ctx_budget,
    ).await {
        Ok(orchestrator) => {
            info!("Context orchestrator initialized");
            Some(orchestrator)
        }
        Err(e) => {
            warn!("Failed to initialize context orchestrator: {}. Memory features disabled.", e);
            None
        }
    };

    // Set context orchestrator (tokio RwLock for async access from handlers)
    {
        let mut orch_guard = shared_state.context_orchestrator.write().await;
        *orch_guard = context_orchestrator;
    }

    // Build unified app state and start serving on the already-bound socket
    let unified_state = UnifiedAppState::new(shared_state.clone());

    // Finish any Local Storage extraction that a previous run left unfinished.
    // Background extraction dies with the process, so a file uploaded moments
    // before the app closed has bytes but no searchable content. Spawned (not
    // awaited) so it never delays the server coming up, and sequential inside
    // so OCR does not compete with the first conversation.
    // Unload the model after a stretch without chat or Aurora use. Warm start
    // is now saved on each unload and restored on each wake (lifecycle.rs).
    crate::model_runtime::lifecycle::spawn_idle_reaper(shared_state.clone());

    crate::api::files_api::spawn_startup_backfill(unified_state.clone());

    // Install and index the legal reference documents shipped with the app.
    crate::knowledge::spawn_startup_sync(unified_state.clone());
    // Topic memory for conversations that predate it (bounded, background).
    {
        let db = shared_state.database_pool.clone();
        tokio::task::spawn_blocking(move || {
            let n = crate::context_engine::topic_memory::backfill(&db, 500);
            if n > 0 {
                info!("Topic memory built for {} earlier conversation(s)", n);
            }
        });
    }

    // Bring disk usage under the configured budget once at startup.
    //
    // Spawned rather than awaited: measuring the app-data tree walks every
    // model and engine file, which on a large install is slow enough to be
    // felt as a delayed launch, and nothing about serving requests depends on
    // it having finished. A pass also runs on every PUT /settings/storage, so
    // this is the "limit was lowered while the app was closed, or files grew
    // between runs" case rather than the only enforcement point.
    // Background retrieval indexer: chunks and keyword-indexes every stored
    // document (including ones from before this version), then embeds them
    // once the embedding model is available.
    crate::rag::indexer::spawn(unified_state.shared_state.database_pool.clone());

    {
        let eviction_state = unified_state.clone();
        tokio::spawn(async move {
            let outcome = crate::api::settings_api::run_eviction(&eviction_state).await;
            if outcome.ran {
                info!("Startup storage check: {}", outcome.reason);
            }
        });
    }

    info!("âœ… Backend initialisation complete â€” starting HTTP server on port {}", selected_port);
    let app = build_compatible_router(unified_state);
    spawn_metrics_listener(&cfg);

    if let Err(e) = axum::serve(listener, app).await {
        error!("Axum server error: {}", e);
    }

    info!("Axum server stopped");
    Ok(())
}

/// PROMETHEUS_PORT: a second loopback listener serving only GET /metrics, for
/// a scraper that should not reach the rest of the API. 0 or the API port
/// itself turns it off (GET /metrics on the API stays available either way).
fn spawn_metrics_listener(cfg: &Config) {
    let port = cfg.prometheus_port;
    if port == 0 || port == cfg.api_port {
        return;
    }
    let addr = format!("{}:{}", cfg.api_host, port);
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                info!("Prometheus metrics on http://{}/metrics", addr);
                let app = axum::Router::new().route("/metrics", axum::routing::get(crate::metrics::get_metrics));
                if let Err(e) = axum::serve(listener, app).await {
                    warn!("Metrics listener stopped: {}", e);
                }
            }
            Err(e) => warn!("Metrics port {} unavailable ({}); /metrics stays on the API port", addr, e),
        }
    });
}

/// Bind to the configured port, store the result in shared_state, and notify the main
/// thread immediately via port_tx.  Returns the bound TcpListener and selected port so
/// axum::serve can start later (after all heavy init is done).
/// Falls back to a free port if the configured one stays in use.
async fn bind_and_notify_port(
    host: &str,
    configured_port: u16,
    port_tx: Option<std::sync::mpsc::Sender<u16>>,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    // The configured port first. A previous session that is still closing
    // can hold it for a moment, so keep trying briefly before giving up on it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let listener = loop {
        match try_bind_port(host, configured_port).await {
            Ok(listener) => break Some(listener),
            Err(e) if std::time::Instant::now() >= deadline => {
                warn!("{}; falling back to a free port", e);
                break None;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    };
    // Port taken by another program: any free loopback port works, because
    // the UI asks the Tauri shell for the real port (get_backend_port) rather
    // than assuming 8888.
    let listener = match listener {
        Some(listener) => listener,
        None => try_bind_port(host, 0).await?,
    };
    let selected_port = listener.local_addr()?.port();
    info!("HTTP server socket bound to {}:{}", host, selected_port);

    // Notify main thread â€” exactly once
    if let Some(tx) = port_tx {
        if tx.send(selected_port).is_err() {
            warn!("Port notification channel closed before port could be sent");
        } else {
            info!("âœ… Port {} communicated to Tauri main thread", selected_port);
        }
    }

    Ok((listener, selected_port))
}

/// True once `port` can be bound, polling until `wait` has passed.
async fn wait_for_free_port(host: &str, port: u16, wait: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if std::net::TcpListener::bind((host, port)).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Try to bind to a specific port, returning the listener if successful
async fn try_bind_port(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let addr = format!("{}:{}", host, port);
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            Err(anyhow::anyhow!("Port {} is already in use", port))
        }
        Err(e) => Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e)),
    }
}

/// Health response structure with detailed runtime status
#[derive(serde::Serialize)]
struct HealthResponse {
    status: String,  // "ready", "initializing", "degraded"
    runtime_ready: bool,
    message: Option<String>,
    /// Explicit engine state (ready / not_installed / corrupted), computed live
    /// from the engine registry. `null` only if the engine manager itself could
    /// not be created — which `engine_manager_available: false` makes explicit.
    engine: Option<crate::engine_management::EngineState>,
    engine_manager_available: bool,
    /// Non-fatal but user-relevant conditions (e.g. GPU engine active with zero
    /// GPU offload because VRAM could not be measured). Never empty silently —
    /// anything degraded-but-running is named here.
    warnings: Vec<String>,
}

/// Health check handler that verifies backend is fully initialized AND runtime is ready
async fn health_check(axum::extract::State(state): axum::extract::State<UnifiedAppState>) -> axum::response::Response {
    use axum::Json;
    use axum::response::IntoResponse;

    let (engine, engine_manager_available) = match state.shared_state.engine_manager {
        Some(ref em) => (Some(em.current_state().await), true),
        None => (None, false),
    };

    // Surface degraded-but-running conditions by name — never silently.
    // The effective offload is the ACTIVE runtime's per-model value (dynamic
    // GGUF-aware calculation); the static config value is only the pre-load
    // fallback shown before any model is running.
    let effective_gpu_layers = {
        let rt = state.shared_state.runtime_manager.read().ok().and_then(|g| g.clone());
        match rt {
            Some(rt) => rt.get_current_config().await.map(|c| c.gpu_layers),
            None => None,
        }
    }
    .unwrap_or(state.shared_state.config.gpu_layers);

    let mut warnings: Vec<String> = Vec::new();
    if let Some(crate::engine_management::EngineState::Ready { ref acceleration, ref engine_id, .. }) = engine {
        if acceleration != "CPU" && effective_gpu_layers == 0 {
            warnings.push(format!(
                "GPU engine '{}' ({}) is active but 0 layers are offloaded to the GPU \
                 (VRAM too small for this model, or GPU memory could not be measured) — \
                 inference is running at CPU speed.",
                engine_id, acceleration
            ));
        }
    }
    // Check if backend initialization is complete
    if !state.shared_state.is_initialization_complete() {
        return Json(HealthResponse {
            status: "initializing".to_string(),
            runtime_ready: false,
            message: Some("Backend initializing...".to_string()),
            engine,
            engine_manager_available,
            warnings,
        })
        .into_response();
    }

    // Check if runtime is actually ready for inference
    let runtime_ready = state.shared_state.llm_worker.is_runtime_ready().await;

    let runtime_state = state.shared_state.llm_worker.get_runtime_state();

    let (status, message) = if runtime_ready {
        ("ready", None)
    } else if matches!(runtime_state, crate::model_runtime::RuntimeState::Switching { .. }) {
        let model_name = match runtime_state {
            crate::model_runtime::RuntimeState::Switching { ref model_name } => model_name.clone(),
            _ => "unknown".to_string(),
        };
        ("switching", Some(format!("Switching to model: {}", model_name)))
    } else if matches!(runtime_state, crate::model_runtime::RuntimeState::Restarting) {
        ("switching", Some("Model server is restarting, please wait...".to_string()))
    } else if let crate::model_runtime::RuntimeState::Sleeping { ref model_name } = runtime_state {
        // Reported as "ready": nothing is wrong, the model loads on demand.
        ("ready", Some(format!("Model {} is unloaded to save memory; it loads on the next question", model_name)))
    } else {
        let msg = match engine {
            Some(crate::engine_management::EngineState::NotInstalled) => {
                "Inference engine is not installed. Install it from the engine setup screen.".to_string()
            }
            Some(crate::engine_management::EngineState::Corrupted { ref engine_id, ref reason }) => {
                format!("Inference engine '{}' failed verification: {}", engine_id, reason)
            }
            _ => "No model loaded. Please activate a model from the Models page.".to_string(),
        };
        ("degraded", Some(msg))
    };

    Json(HealthResponse {
        status: status.to_string(),
        runtime_ready,
        message,
        engine,
        engine_manager_available,
        warnings,
    })
    .into_response()
}

/// Build router for 1-hop architecture
/// `pub(crate)` so API tests can drive real requests through the ACTUAL route
/// table instead of calling handler functions directly - the routing,
/// extractor and middleware wiring is precisely what a function-level test
/// cannot check.
pub(crate) fn build_compatible_router(mut state: UnifiedAppState) -> axum::Router {
    use axum::{
        Router,
        routing::{get, post, put, delete},
        extract::DefaultBodyLimit,
    };
    use tower_http::{
        cors::{Any, CorsLayer},
        trace::TraceLayer,
        timeout::TimeoutLayer,
    };
    use std::time::Duration;

    // Only the app's own pages may call the API from a browser context: the
    // Tauri webview (tauri://localhost on macOS/Linux, http(s)://tauri.localhost
    // on Windows) and local dev servers. `Any` let every website open in the
    // user's browser read their Vault, chats and settings off 127.0.0.1.
    // Non-browser callers send no Origin and are unaffected.
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::predicate(|origin, _| {
            let o = origin.to_str().unwrap_or("");
            o == "tauri://localhost"
                || o == "http://tauri.localhost"
                || o == "https://tauri.localhost"
                || o.starts_with("http://localhost:")
                || o.starts_with("http://127.0.0.1:")
                || o == "http://localhost"
                || o == "http://127.0.0.1"
        }))
        // PATCH is load-bearing and was missing: renaming a draft, moving one
        // between folders, filing it under a matter and changing its status
        // are all PATCH, and every one of them failed in the browser as an
        // unexplained "Failed to fetch" while working perfectly over curl.
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
        ])
        .allow_headers(Any);

    // Get JWT secret from environment or generate a default
    // JWT_SECRET if set; otherwise a random secret created once per install and
    // kept in the app's data folder, so no two installs share a signing key.
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| install_secret());

    // Get users store from database
    let users_store = state.shared_state.database_pool.users.clone();
    
    // Create and set auth state
    state.auth_state = Some(Arc::new(crate::api::auth_api::AuthState {
        users: users_store,
        jwt_secret,
    }));

    Router::new()
        // Auth routes (email/password with SMTP verification)
        .route("/auth/signup", post(crate::api::auth_api::signup))
        .route("/auth/login", post(crate::api::auth_api::login))
        .route("/auth/verify-email", post(crate::api::auth_api::verify_email))
        .route("/auth/me", post(crate::api::auth_api::get_current_user))
        // E-mail verified accounts: one-time code, then user name + password
        .route("/auth/status", get(crate::api::account_api::status))
        .route("/auth/register/start", post(crate::api::account_api::register_start))
        .route("/auth/register/verify", post(crate::api::account_api::register_verify))
        .route("/auth/signin", post(crate::api::account_api::sign_in))
        // Passwordless sign-in by e-mailed code, and Google / Microsoft
        .route("/auth/age", post(crate::api::account_api::confirm_age))
        .route("/auth/otp/start", post(crate::api::account_api::otp_start))
        .route("/auth/otp/verify", post(crate::api::account_api::otp_verify))
        .route("/auth/providers", get(crate::api::oauth_api::providers))
        .route("/auth/oauth/start", post(crate::api::oauth_api::start))
        .route("/auth/oauth/callback", get(crate::api::oauth_api::callback))
        .route("/auth/oauth/result", get(crate::api::oauth_api::result))
        // Core 1-hop streaming endpoint
        .route("/generate/stream", post(crate::api::stream_api::generate_stream))
        // Conversation CRUD via shared memory -> database
        .route("/conversations", get(crate::api::conversation_api::get_conversations).post(crate::api::conversation_api::create_conversation))
        .route("/conversations/db-stats", get(crate::api::conversation_api::get_conversations_db_stats))
        .route("/conversations/:id", get(crate::api::conversation_api::get_conversation))
        .route("/conversations/:id/title", put(crate::api::conversation_api::update_conversation_title))
        .route("/conversations/:id/pinned", post(crate::api::conversation_api::update_conversation_pinned))
        .route("/conversations/:id", delete(crate::api::conversation_api::delete_conversation))
        // Model management endpoints
        .route("/models", get(crate::api::model_api::list_models))
        .route("/context/window", get(crate::api::model_api::context_window))
        .route("/models/by-mode", get(crate::api::model_api::list_models_by_mode))
        .route("/models/active", get(crate::api::model_api::get_active_model))
        .route("/models/readiness", get(crate::api::model_api::get_model_readiness))
        .route("/models/search", get(crate::api::model_api::search_models))
        .route("/models/install", post(crate::api::model_api::install_model))
        .route("/models/remove", delete(crate::api::model_api::remove_model))
        .route("/models/progress", get(crate::api::model_api::get_download_progress))
        .route("/models/downloads", get(crate::api::model_api::get_active_downloads))
        .route("/models/downloads/cancel", post(crate::api::model_api::cancel_download))
        .route("/models/downloads/pause", post(crate::api::model_api::pause_download))
        .route("/models/downloads/resume", post(crate::api::model_api::resume_download))
        .route("/models/downloads/interrupted", get(crate::api::model_api::get_interrupted_downloads))
        .route("/models/downloads/interrupted/discard", post(crate::api::model_api::discard_interrupted_download))
        .route("/models/recommendations", get(crate::api::model_api::get_recommended_models))
        .route("/models/preferences", post(crate::api::model_api::update_preferences))
        .route("/models/marks", get(crate::api::model_prefs_api::get_marks))
        .route("/models/marks/star", post(crate::api::model_prefs_api::set_star))
        .route("/models/refresh", post(crate::api::model_api::refresh_models))
        .route("/models/switch", post(crate::api::model_api::switch_model))
        .route("/models/prewarm", post(crate::api::model_api::prewarm_model))
        .route("/runtime/focus", post(crate::api::model_api::set_window_focus))
        .route("/hardware/recommendations", get(crate::api::model_api::get_hardware_recommendations))
        .route("/hardware/info", get(crate::api::model_api::get_hardware_info))
        // Engine management endpoints
        // Unified document store (browsing, metadata, raw content for the viewer)
        .route("/documents", get(crate::api::documents_api::list_documents))
        .route("/documents/attach", post(crate::api::documents_api::attach_document))
        .route("/documents/session/:session_id", get(crate::api::documents_api::get_session_documents))
        .route("/documents/by-local-file/:local_file_id", get(crate::api::documents_api::get_or_create_document_for_local_file))
        .route("/documents/:id", get(crate::api::documents_api::get_document))
        .route("/documents/:id/raw", get(crate::api::documents_api::get_document_raw))

        // Document workspace (Drafts). Localhost-only like everything else.
        //
        // Ordering note: axum matches literal segments before `:params`, so
        // /drafts/upload and /drafts/blank coexist with /drafts/:id without
        // ambiguity. `router_builds_without_route_conflicts` is what proves it.
        .route("/drafts", get(crate::api::drafts_api::list_drafts).post(crate::api::drafts_api::create_from_vault))
        .route("/drafts/upload", post(crate::api::drafts_api::create_from_upload))
        .route("/drafts/blank", post(crate::api::drafts_api::create_blank))
        .route(
            "/workspace/search",
            get(crate::api::drafts_api::search_workspace),
        )
        .route("/compare", post(crate::api::drafts_api::compare_documents))
        .route(
            "/matters",
            get(crate::api::drafts_api::list_matters).post(crate::api::drafts_api::create_matter),
        )
        .route(
            "/matters/:id",
            axum::routing::patch(crate::api::drafts_api::update_matter)
                .delete(crate::api::drafts_api::delete_matter),
        )
        // The library sits beside /drafts rather than under it: a precedent
        // outlives the matter it came from, so it is not a draft.
        .route(
            "/library",
            get(crate::api::drafts_api::list_library)
                .post(crate::api::drafts_api::create_library_item),
        )
        // Literal segments before /library/:id, same reason as /drafts/upload.
        .route(
            "/library/:id/text",
            get(crate::api::drafts_api::library_item_text),
        )
        .route(
            "/library/:id/new-draft",
            post(crate::api::drafts_api::new_draft_from_library),
        )
        .route(
            "/library/:id",
            axum::routing::delete(crate::api::drafts_api::delete_library_item),
        )
        .route(
            "/drafts/:id/to-library",
            post(crate::api::drafts_api::draft_to_library),
        )
        .route(
            "/clauses",
            get(crate::api::drafts_api::list_clauses)
                .post(crate::api::drafts_api::create_clause),
        )
        .route(
            "/clauses/:id",
            axum::routing::delete(crate::api::drafts_api::delete_clause),
        )
        .route(
            "/drafts/:id/matter",
            axum::routing::patch(crate::api::drafts_api::assign_draft_matter),
        )
        .route(
            "/drafts/:id/status",
            axum::routing::patch(crate::api::drafts_api::set_draft_status),
        )
        .route(
            "/drafts/:id/star",
            axum::routing::patch(crate::api::drafts_api::set_draft_starred),
        )
        .route(
            "/draft-folders",
            get(crate::api::drafts_api::list_folders).post(crate::api::drafts_api::create_folder),
        )
        .route(
            "/draft-folders/:id",
            axum::routing::patch(crate::api::drafts_api::update_folder)
                .delete(crate::api::drafts_api::delete_folder),
        )
        .route(
            "/drafts/:id/folder",
            axum::routing::patch(crate::api::drafts_api::move_draft),
        )
        .route(
            "/drafts/:id",
            get(crate::api::drafts_api::get_draft)
                .patch(crate::api::drafts_api::rename_draft)
                .delete(crate::api::drafts_api::delete_draft),
        )
        .route("/drafts/:id/content", get(crate::api::drafts_api::get_content))
        .route("/drafts/:id/preview", get(crate::api::drafts_api::get_preview))
        .route("/drafts/:id/patch", post(crate::api::drafts_api::apply_patch))
        .route("/drafts/:id/undo", get(crate::api::drafts_api::undo_state).post(crate::api::drafts_api::undo))
        .route("/drafts/:id/redo", post(crate::api::drafts_api::redo))
        .route("/drafts/:id/raw", get(crate::api::drafts_api::get_raw))
        .route("/drafts/:id/publish", post(crate::api::drafts_api::publish))
        .route("/drafts/:id/page/:page", get(crate::api::drafts_api::get_page))
        .route(
            "/drafts/:id/versions",
            get(crate::api::drafts_api::list_versions).post(crate::api::drafts_api::checkpoint),
        )
        .route("/drafts/:id/versions/:n/restore", post(crate::api::drafts_api::restore_version))
        .route("/drafts/:id/versions/:n", delete(crate::api::drafts_api::delete_version))
        .route("/engines", get(crate::api::model_api::list_available_engines))
        .route("/engines/install", post(crate::api::model_api::install_engine))
        .route("/engines/progress", get(crate::api::model_api::get_engine_download_progress))
        .route("/engines/cancel", post(crate::api::model_api::cancel_engine_download))
        .route("/metrics/system", get(crate::api::model_api::get_system_metrics))
        .route("/storage/metadata", get(crate::api::model_api::get_storage_metadata))
        // User-adjustable settings (disk budget)
        .route("/settings/storage", get(crate::api::settings_api::get_storage_settings)
                                   .put(crate::api::settings_api::update_storage_settings))
        // API keys and cloud models: a signed-in session once an account exists.
        .merge(
            Router::new()
                .route("/api-keys", post(crate::api::api_keys_api::save_api_key))
                .route("/api-keys", get(crate::api::api_keys_api::get_api_key))
                .route("/api-keys/all", get(crate::api::api_keys_api::get_all_api_keys))
                .route("/api-keys", delete(crate::api::api_keys_api::delete_api_key))
                .route("/api-keys/mark-used", post(crate::api::api_keys_api::mark_key_used))
                .route("/cloud/providers", get(crate::api::cloud_api::list_providers))
                .route("/cloud/models", get(crate::api::cloud_api::list_models))
                .route("/cloud/active", get(crate::api::cloud_api::get_active).post(crate::api::cloud_api::set_active))
                .route_layer(axum::middleware::from_fn_with_state(state.clone(), crate::api::authz::require_session)),
        )
        // Files API endpoints (database-backed with nested folder support)
        .route("/files", get(crate::api::files_api::get_files))
        .route("/files/all", get(crate::api::files_api::get_all_files))
        .route("/files/search", get(crate::api::files_api::search_files))
        .route("/files/folder", post(crate::api::files_api::create_folder))
        .route("/files/upload", post(crate::api::files_api::upload_file))
        .route("/files/sync", post(crate::api::files_api::sync_files))
        .route("/files/resync", post(crate::api::files_api::resync_files))
        .route("/files/:id", get(crate::api::files_api::get_file_by_id))
        .route("/files/:id/content", get(crate::api::files_api::get_file_content))
        .route("/files/:id", delete(crate::api::files_api::delete_file_by_id))
        .route("/files", delete(crate::api::files_api::delete_file))
        // Feedback endpoint
        .route("/feedback", post(crate::api::feedback_api::submit_feedback))
        .route(
            "/announcements/:id/seen",
            get(crate::api::announcements_api::get_seen),
        )
        .route(
            "/announcements/seen",
            post(crate::api::announcements_api::mark_seen),
        )
        // Login notification endpoint
        .route("/notify-login", post(crate::api::login_notification_api::notify_user_login))
        // Metrics endpoint
        .route("/metrics", get(crate::metrics::get_metrics))
        .route("/healthz", get(health_check))
        .route("/admin/shutdown", post(crate::admin::stop_backend))
        .route("/admin/health", get(crate::api::admin_api::health))
        .route("/admin/db-stats", get(crate::api::admin_api::db_stats))
        .route("/admin/maintenance", post(crate::api::admin_api::maintenance))
        .route("/memory/optimize", post(crate::api::memory_api::memory_optimize))
        .route("/memory/stats/:session_id", get(crate::api::memory_api::memory_stats))
        .route("/memory/cleanup", post(crate::api::memory_api::memory_cleanup))
        // Retrieval (RAG), embedding model and matter scoping
        .route("/embeddings/status", get(crate::api::rag_api::embeddings_status))
        .route("/embeddings/install", post(crate::api::rag_api::embeddings_install))
        .route("/embeddings/models", get(crate::api::rag_api::embeddings_models))
        .route("/embeddings/models/:id", axum::routing::delete(crate::api::rag_api::embeddings_remove))
        .route("/embeddings/select", post(crate::api::rag_api::embeddings_select))
        .route("/embeddings/pause", post(crate::api::rag_api::embeddings_pause))
        .route("/embeddings/cancel", post(crate::api::rag_api::embeddings_cancel))
        .route("/rag/reindex", post(crate::api::rag_api::reindex))
        .route("/rag/search", post(crate::api::rag_api::search))
        .route("/documents/:id/index", get(crate::api::rag_api::document_index_state))
        .route(
            "/matters/:id/documents",
            get(crate::api::rag_api::list_matter_documents).post(crate::api::rag_api::add_matter_document),
        )
        .route(
            "/matters/:id/documents/:document_id",
            axum::routing::delete(crate::api::rag_api::remove_matter_document),
        )
.layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(Duration::from_secs(crate::tuning::parse_or("HTTP_REQUEST_TIMEOUT_SECONDS", 600u64).max(30))))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

/// Router-wide request-body ceiling, and therefore the hard limit on every
/// upload route (`/documents/attach`, `/files/upload`).
///
/// Exceeding it is rejected by the `DefaultBodyLimit` layer with HTTP 413
/// BEFORE any handler runs, so no handler can produce a useful message about
/// it — the client has to gate below this number to say anything helpful. That
/// makes this value a shared contract rather than a local tuning knob.
///
/// The frontend mirrors it as `SERVER_BODY_LIMIT_BYTES` in
/// `apps/desktop/src/uploadLimits.ts`, and `frontend_upload_limit_mirrors_the_server_limit`
/// below fails the build if the two drift. Change both together.
pub const MAX_REQUEST_BODY_BYTES: usize = 50 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// axum's Router::route() panics IMMEDIATELY at registration time if two
    /// routes are ambiguous (e.g. a literal segment colliding with a
    /// wildcard at the same position). None of the unit tests elsewhere
    /// exercise the full router, so a genuine conflict among the new
    /// /documents/* routes (added alongside /documents/:id and
    /// /documents/:id/raw) would only surface here - or at real app
    /// startup. This proves it doesn't.
    #[tokio::test]
    async fn router_builds_without_route_conflicts() {
        let cfg = Config::from_env().expect("Config::from_env should succeed with defaults");
        let database = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        let shared_state = Arc::new(SharedState::new(cfg, database).expect("SharedState::new"));
        let unified_state = UnifiedAppState::new(shared_state);

        // Panics on any route conflict - reaching this line proves the full
        // route table registers cleanly. That now includes the twelve
        // /drafts/* paths, where the risk is concrete: /drafts/upload and
        // /drafts/blank are literal segments sharing a position with
        // /drafts/:id, and /drafts/:id/versions/:n/restore nests two
        // parameters. axum resolves all of these, but only this test proves it
        // before the app tries to start.
        let _router = build_compatible_router(unified_state);
    }

    /// Cross-language invariant: the frontend's upload budget must be derived
    /// from the SAME body limit this router enforces.
    ///
    /// Enforced by PARSING the real .ts file rather than restating its value
    /// here — a restated copy would itself be a third number free to drift.
    /// Same technique, and same reason, as
    /// `utils::file_processor::supported_formats_match_the_frontend_list`.
    ///
    /// Why this needs pinning: the limit is invisible from the client's side.
    /// `DefaultBodyLimit` rejects an oversized request with a bare 413 before
    /// any handler runs, so the only way a user gets a comprehensible message
    /// is if the frontend refuses the batch first — which it can only do
    /// correctly while its mirror of this number is accurate. If the server
    /// limit were lowered and the mirror left behind, uploads inside the
    /// frontend's stale budget would start failing with a raw 413 again, which
    /// is precisely the failure `uploadLimits.ts` was written to remove.
    #[test]
    fn frontend_upload_limit_mirrors_the_server_limit() {
        let ts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src/uploadLimits.ts");
        let source = std::fs::read_to_string(&ts_path).unwrap_or_else(|e| {
            panic!(
                "cannot read the frontend upload-limit policy at {}: {} - if the file \
                 moved, update this test rather than deleting it; it is the only thing \
                 keeping the client's budget in agreement with DefaultBodyLimit",
                ts_path.display(),
                e
            )
        });

        // Matches `export const SERVER_BODY_LIMIT_BYTES = 50 * 1024 * 1024`,
        // evaluating the product so the .ts stays readable rather than being
        // forced to spell out a magic byte count.
        let expr = source
            .split_once("export const SERVER_BODY_LIMIT_BYTES =")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once('\n'))
            .map(|(line, _)| line.trim().trim_end_matches(';').to_string())
            .expect("SERVER_BODY_LIMIT_BYTES declaration not found in uploadLimits.ts");

        let mirrored: usize = expr
            .split('*')
            .map(|factor| {
                factor.trim().parse::<usize>().unwrap_or_else(|_| {
                    panic!(
                        "SERVER_BODY_LIMIT_BYTES must be a product of plain integers so this \
                         test can evaluate it, got {:?}",
                        expr
                    )
                })
            })
            .product();

        assert_eq!(
            mirrored, MAX_REQUEST_BODY_BYTES,
            "uploadLimits.ts mirrors the body limit as {} bytes but this router enforces \
             {} - the frontend would gate against the wrong ceiling. Update both.",
            mirrored, MAX_REQUEST_BODY_BYTES
        );
    }
}

/// The per-install signing secret: read from `auth.secret` in the app's data
/// folder, or generated there on first use.
fn install_secret() -> String {
    use crate::utils::secret_box;
    use rand::Rng;
    // Stored encrypted (AES-256-GCM, DPAPI-wrapped key) in the app data folder.
    // An older plaintext file is read once and rewritten encrypted.
    let path = crate::config::get_app_data_dir().join("auth.secret");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        let plain = if secret_box::is_encrypted(existing) {
            secret_box::decrypt(existing).ok()
        } else {
            Some(existing.to_string())
        };
        if let Some(plain) = plain.filter(|p| p.len() >= 32) {
            if !secret_box::is_encrypted(existing) {
                if let Ok(enc) = secret_box::encrypt(&plain) {
                    let _ = std::fs::write(&path, enc);
                }
            }
            return plain;
        }
    }
    let bytes: [u8; 32] = rand::thread_rng().gen();
    let secret = hex::encode(bytes);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match secret_box::encrypt(&secret) {
        Ok(enc) => {
            let _ = std::fs::write(&path, enc);
        }
        Err(e) => tracing::warn!("JWT signing key not persisted: {}", e),
    }
    secret
}
