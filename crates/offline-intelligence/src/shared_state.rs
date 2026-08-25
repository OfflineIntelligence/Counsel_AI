//! Shared state management for thread-based architecture
//!
//! This module provides the core shared memory infrastructure that enables
//! efficient communication between worker threads while maintaining thread safety.

use std::sync::{Arc, RwLock, atomic::{AtomicUsize, AtomicBool, Ordering}};
use dashmap::DashMap;
use tracing::{info, warn, debug};

use crate::{
    config::Config,
    context_engine::ContextOrchestrator,
    memory_db::MemoryDatabase,
    worker_threads::LLMWorker,
    model_management::ModelManager,
    model_runtime::RuntimeManager,
};
use crate::engine_management::EngineManager;

/// Core shared system state container
pub struct SharedSystemState {
    /// Conversation data with hierarchical locking
    pub conversations: Arc<ConversationHierarchy>,

    /// Conversation that most recently generated a response.
    ///
    /// The backend has no notion of "the chat the user is looking at" - the UI
    /// owns that. What it can observe is which session last ran inference,
    /// which is the same conversation whose KV state is sitting in the engine's
    /// slot. That is exactly the one worth saving at shutdown for a warm start.
    pub last_active_session: Arc<RwLock<Option<String>>>,

    /// Database connection pool
    pub database_pool: Arc<MemoryDatabase>,

    /// Configuration (read-only after initialization)
    pub config: Arc<Config>,

    /// Atomic counters for performance tracking
    pub counters: Arc<AtomicCounters>,

    /// Context orchestrator for memory management (tokio RwLock for async access)
    pub context_orchestrator: Arc<tokio::sync::RwLock<Option<ContextOrchestrator>>>,

    /// LLM worker for inference operations
    pub llm_worker: Arc<LLMWorker>,

    /// Serializes text extraction per Local Storage file, so a background
    /// upload extraction and a user attaching that same file seconds later
    /// cannot extract the same bytes twice. Correctness does not depend on
    /// this (content_hash is UNIQUE and upsert_document is idempotent) - it
    /// exists to avoid paying for an OCR pass twice. See
    /// utils::extraction_coordinator.
    pub extraction_coordinator: Arc<crate::utils::ExtractionCoordinator>,

    /// Model management system
    pub model_manager: Option<Arc<ModelManager>>,

    /// Runtime management system
    pub runtime_manager: Arc<std::sync::RwLock<Option<Arc<RuntimeManager>>>>,

    /// Engine management system
    pub engine_manager: Option<Arc<EngineManager>>,

    /// Initialization completion flag - true when all components are ready
    pub initialization_complete: Arc<AtomicBool>,
    
    /// HTTP server port - may differ from config if original port was in use
    pub http_port: Arc<RwLock<u16>>,
}

/// Hierarchical conversation storage for reduced lock contention
pub struct ConversationHierarchy {
    /// Coarse-grained session-level locks
    pub sessions: DashMap<String, Arc<RwLock<SessionData>>>,

    /// Fine-grained message-level queues for hot paths
    pub message_queues: DashMap<String, Arc<crossbeam_queue::ArrayQueue<PendingMessage>>>,

    /// Lock-free counters for performance metrics
    pub counters: Arc<AtomicCounters>,
}

/// Session-level data structure
#[derive(Debug, Clone)]
pub struct SessionData {
    pub session_id: String,
    pub messages: Vec<crate::memory::Message>,
    pub last_accessed: std::time::Instant,
    pub pinned: bool,
}

/// Pending message for asynchronous processing
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub message: crate::memory::Message,
    pub timestamp: std::time::Instant,
}

/// Atomic counters for system metrics
pub struct AtomicCounters {
    pub total_requests: AtomicUsize,
    pub active_sessions: AtomicUsize,
    pub processed_messages: AtomicUsize,
    pub cache_hits: AtomicUsize,
    pub cache_misses: AtomicUsize,
}

impl AtomicCounters {
    pub fn new() -> Self {
        Self {
            total_requests: AtomicUsize::new(0),
            active_sessions: AtomicUsize::new(0),
            processed_messages: AtomicUsize::new(0),
            cache_hits: AtomicUsize::new(0),
            cache_misses: AtomicUsize::new(0),
        }
    }

    pub fn inc_total_requests(&self) -> usize {
        self.total_requests.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn inc_processed_messages(&self) -> usize {
        self.processed_messages.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn inc_cache_hit(&self) -> usize {
        self.cache_hits.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn inc_cache_miss(&self) -> usize {
        self.cache_misses.fetch_add(1, Ordering::Relaxed) + 1
    }
}

impl SharedSystemState {
    pub fn new(config: Config, database: Arc<MemoryDatabase>) -> anyhow::Result<Self> {
        info!("Initializing shared system state");

        let conversations = Arc::new(ConversationHierarchy {
            sessions: DashMap::new(),
            message_queues: DashMap::new(),
            counters: Arc::new(AtomicCounters::new()),
        });

        // Extract values before moving config into Arc
        let api_port = config.api_port;
        let backend_url = config.backend_url.clone();

        let config = Arc::new(config);
        let counters = Arc::new(AtomicCounters::new());

        // Create LLM worker with backend URL from config
        let llm_worker = Arc::new(LLMWorker::new_with_backend(backend_url));

        Ok(Self {
            conversations,
            last_active_session: Arc::new(RwLock::new(None)),
            database_pool: database,
            config,
            counters,
            context_orchestrator: Arc::new(tokio::sync::RwLock::new(None)),
            llm_worker,
            extraction_coordinator: Arc::new(crate::utils::ExtractionCoordinator::new()),
            model_manager: None,
            runtime_manager: Arc::new(std::sync::RwLock::new(None)),
            engine_manager: None,
            initialization_complete: Arc::new(AtomicBool::new(false)),
            http_port: Arc::new(RwLock::new(api_port)), // Default to configured port
        })
    }

    /// Mark initialization as complete - call this after all components are initialized
    pub fn mark_initialization_complete(&self) {
        self.initialization_complete.store(true, Ordering::SeqCst);
        info!("✅ Backend initialization marked as complete");
    }

    /// Check if initialization is complete
    pub fn is_initialization_complete(&self) -> bool {
        self.initialization_complete.load(Ordering::SeqCst)
    }

    /// Set runtime manager (allows setting after initialization)
    pub fn set_runtime_manager(&self, runtime_manager: Arc<RuntimeManager>) -> anyhow::Result<()> {
        // Update the runtime manager in shared state
        let mut guard = self.runtime_manager
            .write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire runtime manager write lock: {}", e))?;
        *guard = Some(runtime_manager);
        Ok(())
    }

    /// Single source of truth for building RuntimeConfig.
    /// All callers (startup auto-load, model switch) use this instead of
    /// constructing RuntimeConfig inline.
    ///
    /// When GPU_LAYERS=auto, the offload for GGUF models is computed HERE, per
    /// model, from the file's own GGUF header (layer count, GQA dims) against
    /// measured VRAM — see model_runtime::gpu_offload. The static bucket value
    /// in Config is only the fallback when that calculation is impossible.
    pub fn build_runtime_config(
        &self,
        model_path: std::path::PathBuf,
        model_format: crate::model_runtime::ModelFormat,
        engine_binary: Option<std::path::PathBuf>,
    ) -> crate::model_runtime::RuntimeConfig {
        let gpu_layers = if self.config.gpu_layers_auto
            && model_format == crate::model_runtime::ModelFormat::GGUF
            && !model_path.as_os_str().is_empty()
        {
            match crate::model_runtime::gpu_offload::compute_gpu_layers_for_model(
                &model_path,
                self.config.ctx_size,
            ) {
                Ok(plan) => plan.gpu_layers,
                Err(e) => {
                    warn!(
                        "Dynamic GPU offload unavailable for {:?} ({}). Using the \
                         VRAM-bucket fallback: {} layers.",
                        model_path, e, self.config.gpu_layers
                    );
                    self.config.gpu_layers
                }
            }
        } else {
            self.config.gpu_layers
        };

        // batch_size 0 = omit the flag so llama-server uses its tuned defaults
        let batch_size = if self.config.batch_size_auto { 0 } else { self.config.batch_size };

        // Context clamp: CTX_SIZE (whether auto-detected or an explicit user
        // number in .env) is a REQUEST, never a command. A value larger than
        // the model's own trained context is unusable (the model was never
        // taught to attend that far) and, worse, drives llama-server to
        // allocate a KV cache sized for it - trivially exhausting RAM/VRAM on
        // any real machine for a large number. Clamp to the GGUF header's
        // own {arch}.context_length when the model declares one. Loud, never
        // silent: every clamp is logged with the exact reason.
        let context_size = if model_format == crate::model_runtime::ModelFormat::GGUF
            && !model_path.as_os_str().is_empty()
        {
            match crate::model_runtime::gguf_metadata::read_model_info(&model_path) {
                Ok(info) if info.trained_context_length > 0 => {
                    let trained = info.trained_context_length as u32;

                    if self.config.ctx_size_auto {
                        // CTX_SIZE=auto: the GGUF header is the AUTHORITY, not a
                        // ceiling on a guess.
                        //
                        // `auto_detect_ctx_size` infers from the FILENAME, which
                        // knows nothing about most models — `gemma-3-1b-it`
                        // matches none of its patterns and silently lands on the
                        // 8192 default, while the model itself declares 32768.
                        // Running at a quarter of the real window makes context
                        // truncation engage far earlier than it needs to, which
                        // is how a normal document conversation ran out of room
                        // after three or four turns.
                        //
                        // Still bounded: never above what RAM can hold, and never
                        // above AUTO_CTX_CEILING, because a model advertising
                        // 128k would otherwise have llama-server allocate a KV
                        // cache sized for it the moment it loads.
                        const AUTO_CTX_CEILING: u32 = 32_768;
                        let ram_bound = crate::config::Config::ram_safe_ctx_size(trained);
                        let chosen = trained.min(AUTO_CTX_CEILING).min(ram_bound);

                        if chosen != self.config.ctx_size {
                            info!(
                                "CTX_SIZE=auto: using {} tokens for {} (model declares {}, \
                                 ceiling {}, RAM allows {}); the filename-based estimate was {}",
                                chosen,
                                info.architecture,
                                trained,
                                AUTO_CTX_CEILING,
                                ram_bound,
                                self.config.ctx_size
                            );
                        }
                        chosen
                    } else if self.config.ctx_size > trained {
                        warn!(
                            "CTX_SIZE={} requested but model's trained context is {} \
                             tokens ({}) - clamping to {}. A context larger than the \
                             model was trained on cannot be used coherently and would \
                             force an oversized KV-cache allocation.",
                            self.config.ctx_size, trained, info.architecture, trained
                        );
                        trained
                    } else {
                        self.config.ctx_size
                    }
                }
                Ok(_) => {
                    debug!("Model does not declare a trained context_length; using requested CTX_SIZE={} unclamped", self.config.ctx_size);
                    self.config.ctx_size
                }
                Err(e) => {
                    warn!("Could not read GGUF header for context clamp ({}); using requested CTX_SIZE={} unclamped", e, self.config.ctx_size);
                    self.config.ctx_size
                }
            }
        } else {
            self.config.ctx_size
        };

        crate::model_runtime::RuntimeConfig {
            model_path,
            format: model_format,
            host: self.config.llama_host.clone(),
            port: self.config.llama_port,
            context_size,
            batch_size,
            threads: self.config.threads,
            threads_batch: self.config.threads_batch,
            gpu_layers,
            cache_reuse: self.config.cache_reuse,
            slot_save_path: Some(crate::model_runtime::slot_cache::slot_cache_dir()),
            // Text-only by default. Callers that activate a VISION model set
            // this afterwards, from the model registry (production) or the
            // MMPROJ_PATH dev override (thread_server manual load) — the
            // mmproj belongs to a specific model, so it can never be derived
            // from static config here.
            mmproj_path: None,
            runtime_binary: engine_binary,
            extra_config: serde_json::json!({}),
        }
    }

    /// Get or create session data with proper locking
    pub async fn get_or_create_session(&self, session_id: &str) -> Arc<RwLock<SessionData>> {
        // Fast path: try to get existing session
        if let Some(session) = self.conversations.sessions.get(session_id) {
            return session.clone();
        }

        // Slow path: create new session
        let new_session = Arc::new(RwLock::new(SessionData {
            session_id: session_id.to_string(),
            messages: Vec::new(),
            last_accessed: std::time::Instant::now(),
            pinned: false,
        }));

        self.conversations.sessions.insert(session_id.to_string(), new_session.clone());
        self.counters.active_sessions.fetch_add(1, Ordering::Relaxed);

        new_session
    }

    /// Queue message for asynchronous processing
    pub fn queue_message(&self, session_id: &str, message: crate::memory::Message) -> bool {
        let queue = self.conversations.message_queues
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(crossbeam_queue::ArrayQueue::new(1000)));

        queue.push(PendingMessage {
            message,
            timestamp: std::time::Instant::now(),
        }).is_ok()
    }

    /// Whether the currently running runtime is VISION-capable and ready:
    /// started with a multimodal projector (--mmproj) and passing health.
    /// The single source of truth the extraction layer and /models/active
    /// both read — deterministic, never inferred from model names.
    pub async fn vision_active(&self) -> bool {
        let rt = self.runtime_manager.read().ok().and_then(|g| g.clone());
        match rt {
            Some(rt) => {
                if !rt.is_ready().await {
                    return false;
                }
                rt.get_current_config()
                    .await
                    .map(|c| c.mmproj_path.is_some())
                    .unwrap_or(false)
            }
            None => false,
        }
    }

    /// The real, currently-usable context budget in tokens, for whatever
    /// model is actually loaded right now. Ground truth precedence:
    ///   1. Live /props readback from the running engine (the engine's own
    ///      report of what it allocated - survives model switches for free).
    ///   2. The RuntimeConfig we last requested (already clamped to the
    ///      model's trained context in build_runtime_config).
    ///   3. The static config value (no runtime active yet).
    /// A generation reserve is subtracted so retrieval/injection budgets
    /// never crowd out room for the model's own answer.
    pub async fn current_context_budget(&self) -> usize {
        const GENERATION_RESERVE: usize = 2048;
        const FLOOR: usize = 1024;

        let rt = self.runtime_manager.read().ok().and_then(|g| g.clone());
        let raw = if let Some(rt) = rt {
            if let Some(live) = rt.get_effective_context().await {
                live as usize
            } else if let Some(cfg) = rt.get_current_config().await {
                cfg.context_size as usize
            } else {
                self.config.ctx_size as usize
            }
        } else {
            self.config.ctx_size as usize
        };

        raw.saturating_sub(GENERATION_RESERVE).max(FLOOR)
    }

    /// Process queued messages for a session
    pub async fn process_queued_messages(&self, session_id: &str) -> Vec<PendingMessage> {
        let mut messages = Vec::new();

        if let Some(queue) = self.conversations.message_queues.get(session_id) {
            while let Some(msg) = queue.pop() {
                messages.push(msg);
            }
        }

        messages
    }
}

impl ConversationHierarchy {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            message_queues: DashMap::new(),
            counters: Arc::new(AtomicCounters::new()),
        }
    }
}

/// Unified application state for all API handlers.
/// This is the single state type used by the Axum router, providing access
/// to all subsystems through shared memory (Arc) rather than network hops.
#[derive(Clone)]
pub struct UnifiedAppState {
    pub shared_state: Arc<SharedSystemState>,
    pub context_orchestrator: Arc<tokio::sync::RwLock<Option<ContextOrchestrator>>>,
    pub llm_worker: Arc<LLMWorker>,
    pub auth_state: Option<Arc<crate::api::auth_api::AuthState>>,
}

impl UnifiedAppState {
    pub fn new(shared_state: Arc<SharedSystemState>) -> Self {
        let context_orchestrator = shared_state.context_orchestrator.clone();
        let llm_worker = shared_state.llm_worker.clone();
        Self {
            shared_state,
            context_orchestrator,
            llm_worker,
            auth_state: None,
        }
    }
}

// Re-exports for convenience
pub use self::SharedSystemState as SharedState;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_db::MemoryDatabase;

    fn state() -> SharedState {
        let cfg = Config::from_env().expect("Config::from_env should succeed with defaults");
        let db = Arc::new(MemoryDatabase::new_in_memory().unwrap());
        SharedState::new(cfg, db).expect("SharedState::new")
    }

    /// The warm-start feature is entirely dependent on llama-server being
    /// launched with `--slot-save-path`: without it, `/slots/{id}?action=save`
    /// and `?action=restore` are not available and every save silently does
    /// nothing. Nothing else in the suite covers the path from
    /// `build_runtime_config` to that flag, and the flag itself is only
    /// observable by spawning a real process - so this pins the one link that
    /// can be checked in isolation.
    #[test]
    fn the_runtime_config_always_requests_a_slot_save_path() {
        let cfg = state().build_runtime_config(
            std::path::PathBuf::from("model.gguf"),
            crate::model_runtime::ModelFormat::GGUF,
            None,
        );

        let path = cfg
            .slot_save_path
            .expect("slot_save_path must be set, or warm start cannot work at all");
        assert!(
            path.ends_with(crate::utils::storage_governor::KV_CACHE_DIR),
            "slot blobs must land in the directory disk accounting and eviction \
             watch ({}), got {}",
            crate::utils::storage_governor::KV_CACHE_DIR,
            path.display()
        );
    }

    /// Warm start saves whichever conversation last ran inference. If nothing
    /// ever set that, shutdown must save nothing rather than guess.
    #[test]
    fn no_conversation_has_generated_yet_means_no_active_session() {
        let s = state();
        assert!(s.last_active_session.read().unwrap().is_none());
    }
}
