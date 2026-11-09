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

/// Session-id prefix that marks a thread as belonging to the AI Assistant
/// side panel rather than the main discussion.
///
/// This is an existing contract, not a new one: the frontend has always
/// created assistant threads with this prefix and already relies on it to keep
/// them out of the main discussion list. Reusing it here means the split needs
/// no new request field, and therefore cannot break against a frontend that
/// predates it — an older client simply keeps sending chat sessions, which
/// classify as Chat and behave exactly as before.
///
/// Must stay byte-identical to DRAFT_ASSISTANT_SESSION_PREFIX in
/// apps/desktop/src/api/chat.ts. A mismatch does not error; it silently gives
/// the assistant the full chat budget, which is why the two tests at the
/// bottom of this file pin the exact string.
pub const ASSISTANT_SESSION_PREFIX: &str = "draft-assistant-";

/// How the context window is divided between the two clients.
///
/// 75/25 rather than an even split, because the two are not symmetric:
///
/// - The main discussion is where documents are attached, where a contract's
///   extracted text has to fit, and where long multi-turn work happens. It is
///   the client that actually runs out of room.
/// - The assistant answers about ONE page or ONE open document, and its own
///   prompt builder already caps what it reads at CONTEXT_CHARS (12k) before
///   any of this applies. Giving it half the window would reserve space it
///   has no way to spend, at the direct expense of the client that can.
///
/// These are a share of the window each client may FILL. They are not slots
/// and they do not create concurrency: with `--parallel 1` the engine still
/// serves one request at a time, so two simultaneous requests queue rather
/// than run together. Concurrency is a separate change (`--parallel 2`) with
/// its own cost — it disables warm start. See gguf_runtime.rs.
pub const CHAT_CONTEXT_PCT: usize = 75;
pub const ASSISTANT_CONTEXT_PCT: usize = 25;

/// Room kept inside each client's share for the model's own ANSWER.
///
/// Per-client rather than one shared figure, and that is a correctness
/// requirement once `--parallel 2` is on, not a tuning preference.
///
/// With a unified KV buffer both sequences draw from ONE pool, so the worst
/// case is both clients at full prompt AND generating at once. If the shares
/// were taken from a window that had already had a single reserve removed,
/// that worst case would exceed the pool: each client would be sized as though
/// it alone needed room to answer. Splitting the WHOLE window and then having
/// each client subtract its own reserve makes the two peaks sum to the window
/// exactly — see `both_clients_at_full_budget_fit_the_window`.
///
/// The assistant's is smaller because its answers are: its system prompt
/// instructs it to answer concisely, and it is a side panel, not the surface
/// where long documents get drafted. Giving it a chat-sized reserve inside a
/// quarter-window share would leave almost nothing for the document it is
/// supposed to be reading.
pub const CHAT_GENERATION_RESERVE: usize = 2048;
pub const ASSISTANT_GENERATION_RESERVE: usize = 768;

/// Smallest prompt budget worth serving. Below roughly this, retrieval returns
/// nothing usable and the client silently stops being able to see the document
/// it is answering about — which looks like a broken assistant, not a small
/// window.
pub const MIN_PROMPT_BUDGET: usize = 512;

/// Which client a request came from, and therefore its share of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    /// The main discussion — the larger share.
    Chat,
    /// The side panel — the smaller share.
    Assistant,
}

impl ClientKind {
    /// Classify by session id. Anything not carrying the assistant prefix is
    /// Chat, so an unrecognised or malformed id degrades to the full budget
    /// and behaves as the app did before this split existed.
    pub fn of_session(session_id: &str) -> Self {
        if session_id.starts_with(ASSISTANT_SESSION_PREFIX) {
            Self::Assistant
        } else {
            Self::Chat
        }
    }

    pub fn context_pct(self) -> usize {
        match self {
            Self::Chat => CHAT_CONTEXT_PCT,
            Self::Assistant => ASSISTANT_CONTEXT_PCT,
        }
    }

    /// Room this client keeps for the model's answer, inside its own share.
    pub fn generation_reserve(self) -> usize {
        match self {
            Self::Chat => CHAT_GENERATION_RESERVE,
            Self::Assistant => ASSISTANT_GENERATION_RESERVE,
        }
    }
}

/// The context window used while answers come from a cloud model.
pub const CLOUD_CONTEXT_TOKENS: usize = 32_768;

impl SharedSystemState {
    pub fn new(config: Config, database: Arc<MemoryDatabase>) -> anyhow::Result<Self> {
        info!("Initializing shared system state");

        let conversations = Arc::new(ConversationHierarchy {
            sessions: DashMap::new(),
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
        // The embedding server runs on whichever engine the chat model uses.
        if let Some(ref binary) = engine_binary {
            crate::rag::embedder::set_engine_binary(binary.clone());
        }
        // One plan for this model on this machine (model_runtime::machine):
        // GPU layers, window size and RAM prefix cache from measured GPU
        // memory, RAM and storage and the model's own shape. Falls back to the
        // per-part logic below when the model's header cannot be read.
        let planned = if model_format == crate::model_runtime::ModelFormat::GGUF
            && !model_path.as_os_str().is_empty()
            && (self.config.gpu_layers_auto || self.config.ctx_size_auto)
        {
            match crate::model_runtime::gguf_metadata::read_model_info(&model_path) {
                Ok(info) => {
                    use crate::model_runtime::machine;
                    let shape = machine::ModelShape::from_gguf(&info, &self.config.kv_cache_type);
                    let trained = if shape.trained_ctx > 0 { shape.trained_ctx } else { self.config.ctx_size };
                    let ceiling = crate::tuning::parse_or("CTX_CEILING", 32_768u32);
                    let (want_ctx, ctx_fixed) = if self.config.ctx_size_auto {
                        (trained.min(ceiling), false)
                    } else {
                        (self.config.ctx_size.min(trained), true)
                    };
                    let limits = machine::PlanLimits {
                        want_ctx,
                        ctx_fixed,
                        min_ctx: 1024.min(want_ctx),
                        cache_ram_cap_mb: self.config.cache_ram_mb.max(0) as u64,
                        kv_store_cap_mb: crate::tuning::parse_or("KV_STORE_MAX_MB", 20_480u64),
                        embed_gpu_mb: machine::embedder_gpu_need_mb(),
                        embed_mode: machine::EmbedMode::from_env(),
                    };
                    let profile = machine::profile(&crate::model_runtime::slot_cache::slot_cache_dir());
                    let plan = machine::plan(&profile, &shape, &limits);
                    if !plan.fits {
                        warn!("{} does not fit this machine's memory: {}", info.architecture, plan.summary);
                    }
                    machine::set_current(plan.clone());
                    Some(plan)
                }
                Err(e) => {
                    warn!("Could not read the model header for resource planning ({}); using per-part defaults", e);
                    None
                }
            }
        } else {
            None
        };

        let gpu_layers = if let (true, Some(p)) = (self.config.gpu_layers_auto, planned.as_ref()) {
            p.gpu_layers
        } else if self.config.gpu_layers_auto
            && model_format == crate::model_runtime::ModelFormat::GGUF
            && !model_path.as_os_str().is_empty()
        {
            match crate::model_runtime::gpu_offload::compute_gpu_layers_for_model(
                &model_path,
                self.config.ctx_size,
                &self.config.kv_cache_type,
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
        let context_size = if let (true, Some(p)) = (self.config.ctx_size_auto, planned.as_ref()) {
            p.ctx
        } else if model_format == crate::model_runtime::ModelFormat::GGUF
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

        // How many requests the engine serves at once: one (see
        // DEFAULT_PARALLEL_SLOTS), which halves the KV cache and enables warm
        // start.
        let parallel_slots = crate::tuning::chat_parallel_slots();

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
            // Warm start and multiple slots are mutually exclusive, and the
            // choice is made HERE, once, rather than in the two places that
            // consume it. Slot save/restore addresses a slot by index, and
            // llama-server assigns slots by prompt similarity rather than by
            // client, so with two slots "slot 0" is whichever conversation
            // happened to look similar — restoring it would hand one thread's
            // KV state to another.
            //
            // This is the real cost of --parallel 2: the first message after
            // launch re-prefills instead of resuming a saved cache. It is paid
            // on one message per run, against a collision between the chat and
            // the assistant that otherwise blocks silently every time it
            // happens.
            slot_save_path: if parallel_slots > 1 {
                None
            } else {
                Some(crate::model_runtime::slot_cache::slot_cache_dir())
            },
            parallel_slots,
            // Text-only by default. Callers that activate a VISION model set
            // this afterwards, from the model registry (production) or the
            // MMPROJ_PATH dev override (thread_server manual load) — the
            // mmproj belongs to a specific model, so it can never be derived
            // from static config here.
            mmproj_path: None,
            projector_loaded: false,
            kv_cache_type: Some(self.config.kv_cache_type.clone()).filter(|t| t != "f16"),
            // The planned RAM prefix cache never exceeds the configured cap and
            // shrinks on machines with little free RAM; 0 and -1 keep their
            // engine meanings (off, unlimited).
            cache_ram_mb: Some(match planned.as_ref() {
                Some(p) if self.config.cache_ram_mb > 0 => (p.cache_ram_mb as i64).min(self.config.cache_ram_mb),
                _ => self.config.cache_ram_mb,
            }),
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

    /// Whether the currently running runtime is VISION-capable and ready:
    /// started with a multimodal projector (--mmproj) and passing health.
    /// The single source of truth the extraction layer and /models/active
    /// both read — deterministic, never inferred from model names.
    /// The chosen model is a vision model, whether it is loaded or asleep
    /// (for display; an image wakes a sleeping one, see lifecycle.rs).
    pub async fn vision_chosen(&self) -> bool {
        let rt = self.runtime_manager.read().ok().and_then(|g| g.clone());
        match rt {
            Some(rt) => rt.get_current_config().await.map(|c| c.mmproj_path.is_some()).unwrap_or(false),
            None => false,
        }
    }

    pub async fn vision_active(&self) -> bool {
        let rt = self.runtime_manager.read().ok().and_then(|g| g.clone());
        match rt {
            Some(rt) => {
                if !rt.is_ready().await {
                    return false;
                }
                rt.get_current_config()
                    .await
                    .map(|c| c.mmproj_path.is_some() && c.projector_loaded)
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
    ///
    /// This returns the WHOLE usable window. Callers serving a specific client
    /// want `context_budget_for` instead, which applies that client's share.
    pub async fn current_context_budget(&self) -> usize {
        const FLOOR: usize = 1024;
        let raw = self.raw_context_window().await;
        raw.saturating_sub(CHAT_GENERATION_RESERVE).max(FLOOR)
    }

    /// The full context window the engine is actually running with, before any
    /// generation reserve is taken out. This is the number the 75/25 split is
    /// applied to, because the shares have to cover each client's answer room
    /// as well as its prompt.
    pub async fn raw_context_window(&self) -> usize {
        // On Cloud the local engine's window does not apply. Cloud models
        // take 128k tokens or more; 32k keeps room for retrieval and history
        // while holding cost and latency to a sensible size per answer.
        if self.llm_worker.cloud().is_some() {
            return CLOUD_CONTEXT_TOKENS;
        }
        let rt = self.runtime_manager.read().ok().and_then(|g| g.clone());
        if let Some(rt) = rt {
            if let Some(live) = rt.get_effective_context().await {
                live as usize
            } else if let Some(cfg) = rt.get_current_config().await {
                cfg.context_size as usize
            } else {
                self.config.ctx_size as usize
            }
        } else {
            self.config.ctx_size as usize
        }
    }

    /// The context budget one client may fill, in tokens.
    ///
    /// The share is applied to the window AFTER the generation reserve has
    /// already been taken out by `current_context_budget`, not before. That
    /// ordering matters: the reserve is room for the model's ANSWER and is not
    /// divisible between clients — both need a full answer's worth of room,
    /// and taking 25% of a budget that still contained it would quietly bill
    /// the assistant for a reserve it cannot use.
    ///
    /// The floor is the same one `current_context_budget` applies. Without it,
    /// 25% of an already-floored 1024-token window would be 256 tokens, which
    /// is below the point where retrieval produces anything usable — the
    /// assistant would silently stop being able to see the page it is
    /// answering about, on exactly the low-spec machines this app targets.
    pub async fn context_budget_for(&self, kind: ClientKind) -> usize {
        let window = self.raw_context_window().await;
        // The 75/25 shares are kept with one slot as well: they cap how much
        // prompt each answer processes, which is where the compute goes.
        Self::prompt_budget(window, kind)
    }

    /// The share arithmetic, split out so it can be tested without a live
    /// runtime — and so the invariant that matters (two clients at full budget
    /// fit one window) is asserted against the real function rather than a
    /// copy of it in a test.
    ///
    /// # Why this is not simply `window * pct / 100 - reserve`
    ///
    /// That was the first version, and `both_clients_at_full_budget_fit_the_window`
    /// caught it overcommitting by 256 tokens on a 4096 window. The cause is
    /// the minimum prompt budget: on a small window the assistant's 25% share
    /// cannot hold MIN_PROMPT_BUDGET plus its reserve, so raising it to the
    /// floor pushed its peak past its own share — and with a shared KV buffer,
    /// past the window.
    ///
    /// The floor is worth keeping (a 256-token prompt cannot retrieve anything
    /// usable, so the assistant would appear broken rather than small), so the
    /// resolution is to fund it from the client that has slack rather than
    /// from nowhere: the assistant is sized first, and CHAT TAKES WHAT IS
    /// LEFT. On any window roomy enough for the plain percentages this is
    /// exactly 75/25 — at 8192 it still yields 4096 and 1280 — and it only
    /// diverges where the arithmetic would otherwise have been unsafe.
    pub fn prompt_budget(window: usize, kind: ClientKind) -> usize {
        // The assistant is sized from its share, then floored.
        let assistant_prompt = (window * ASSISTANT_CONTEXT_PCT / 100)
            .saturating_sub(ASSISTANT_GENERATION_RESERVE)
            .max(MIN_PROMPT_BUDGET);

        match kind {
            ClientKind::Assistant => assistant_prompt.min(window),
            ClientKind::Chat => {
                // Everything the assistant's worst case does not need. On a
                // roomy window this equals chat's own 75% share; on a tight
                // one it is what is genuinely left, which is the only figure
                // that keeps the shared buffer safe.
                let assistant_peak = assistant_prompt + ASSISTANT_GENERATION_RESERVE;
                window
                    .saturating_sub(assistant_peak)
                    .saturating_sub(CHAT_GENERATION_RESERVE)
                    .max(MIN_PROMPT_BUDGET)
                    .min(window)
            }
        }
    }
}

impl ConversationHierarchy {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
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

/// Lets handlers that only need the shared system state take
/// `State<Arc<SharedSystemState>>` (memory_api, admin_api).
impl axum::extract::FromRef<UnifiedAppState> for Arc<SharedSystemState> {
    fn from_ref(state: &UnifiedAppState) -> Self {
        Arc::clone(&state.shared_state)
    }
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
    fn warm_start_and_multiple_slots_are_mutually_exclusive() {
        // This test previously asserted slot_save_path was ALWAYS set. That
        // stopped being correct when the default slot count became 2: warm
        // start restores a slot BY INDEX, and llama-server assigns slots by
        // prompt similarity rather than by client, so "slot 0" with two slots
        // is whichever conversation happened to look similar - restoring it
        // would hand one thread's KV state to another. The rule the config
        // must uphold now is the exclusivity, not the presence.
        let cfg = state().build_runtime_config(
            std::path::PathBuf::from("model.gguf"),
            crate::model_runtime::ModelFormat::GGUF,
            None,
        );

        if cfg.parallel_slots > 1 {
            assert!(
                cfg.slot_save_path.is_none(),
                "warm start must be off with {} slots",
                cfg.parallel_slots
            );
        } else {
            let path = cfg
                .slot_save_path
                .expect("with a single slot, warm start must be enabled");
            assert!(
                path.ends_with(crate::utils::storage_governor::KV_CACHE_DIR),
                "slot blobs must land where disk accounting watches, got {}",
                path.display()
            );
        }
    }

    /// Warm start saves whichever conversation last ran inference. If nothing
    /// ever set that, shutdown must save nothing rather than guess.
    #[test]
    fn no_conversation_has_generated_yet_means_no_active_session() {
        let s = state();
        assert!(s.last_active_session.read().unwrap().is_none());
    }

    // ── The 75/25 context split ──────────────────────────────────────────

    /// The prefix is a contract with the FRONTEND, and breaking it fails
    /// silently rather than loudly: a mismatch does not error, it just
    /// classifies every assistant thread as Chat and hands it the full
    /// budget. Nothing would look wrong until someone measured it. Pinning
    /// the literal here means a rename has to come past a red test.
    #[test]
    fn assistant_prefix_matches_the_frontend_contract() {
        assert_eq!(
            ASSISTANT_SESSION_PREFIX, "draft-assistant-",
            "must stay byte-identical to DRAFT_ASSISTANT_SESSION_PREFIX in              apps/desktop/src/api/chat.ts"
        );
    }

    #[test]
    fn sessions_are_classified_by_prefix() {
        // The global panel thread and a per-document thread both carry it.
        assert_eq!(
            ClientKind::of_session("draft-assistant-global"),
            ClientKind::Assistant
        );
        assert_eq!(
            ClientKind::of_session("draft-assistant-42"),
            ClientKind::Assistant
        );
        // A normal discussion id is a timestamp.
        assert_eq!(ClientKind::of_session("1737000000000"), ClientKind::Chat);
        // Anything unrecognised degrades to Chat — the pre-split behaviour.
        assert_eq!(ClientKind::of_session(""), ClientKind::Chat);
    }

    #[test]
    fn the_two_shares_are_75_25_and_account_for_the_whole_window() {
        assert_eq!(ClientKind::Chat.context_pct(), 75);
        assert_eq!(ClientKind::Assistant.context_pct(), 25);
        assert_eq!(
            ClientKind::Chat.context_pct() + ClientKind::Assistant.context_pct(),
            100,
            "the shares must not overcommit the window"
        );
    }

    /// THE invariant that makes --parallel 2 safe.
    ///
    /// With --kv-unified both slots draw from one shared KV buffer, so the
    /// worst case is both clients holding a full prompt AND generating at the
    /// same time. If that sums past the window the engine must evict or fail
    /// mid-answer - a failure that appears only under concurrency, i.e.
    /// rarely, and in front of a user.
    ///
    /// This is precisely why the shares are taken from the WHOLE window and
    /// each client subtracts its own reserve, rather than one shared reserve
    /// being removed first and the remainder split.
    #[test]
    fn both_clients_at_full_budget_fit_the_window() {
        for window in [4096usize, 8192, 16_384, 32_768, 131_072] {
            let chat = SharedSystemState::prompt_budget(window, ClientKind::Chat)
                + ClientKind::Chat.generation_reserve();
            let assistant = SharedSystemState::prompt_budget(window, ClientKind::Assistant)
                + ClientKind::Assistant.generation_reserve();
            assert!(
                chat + assistant <= window,
                "window {}: chat peak {} + assistant peak {} = {} overcommits the shared KV buffer",
                window, chat, assistant, chat + assistant
            );
        }
    }

    #[test]
    fn a_roomy_window_divides_as_asked() {
        // 8192: chat 75% = 6144, less its 2048 reserve = 4096.
        //       assistant 25% = 2048, less its 768 reserve = 1280.
        assert_eq!(SharedSystemState::prompt_budget(8192, ClientKind::Chat), 4096);
        assert_eq!(SharedSystemState::prompt_budget(8192, ClientKind::Assistant), 1280);
    }

    #[test]
    fn chat_always_gets_the_larger_prompt_budget() {
        // The whole point of 75/25. Checked across sizes because the two
        // reserves differ, so a careless change to either could invert this on
        // a small window without touching the percentages at all.
        for window in [2048usize, 4096, 8192, 32_768] {
            assert!(
                SharedSystemState::prompt_budget(window, ClientKind::Chat)
                    >= SharedSystemState::prompt_budget(window, ClientKind::Assistant),
                "window {}: chat must never get less prompt room than the assistant",
                window
            );
        }
    }

    #[test]
    fn no_budget_can_exceed_the_window_that_must_hold_it() {
        for window in [256usize, 512, 1024] {
            for kind in [ClientKind::Chat, ClientKind::Assistant] {
                assert!(
                    SharedSystemState::prompt_budget(window, kind) <= window,
                    "window {} / {:?} produced a budget larger than the window",
                    window, kind
                );
            }
        }
    }
}
