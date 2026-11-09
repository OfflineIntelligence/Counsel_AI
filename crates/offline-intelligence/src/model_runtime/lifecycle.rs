//! Model lifecycle: load on demand, unload when idle.
//!
//! The chat model is the largest consumer of RAM and VRAM in the app, and it
//! is only needed while a chat or Aurora answer is being produced. So:
//!
//! - At launch the last used model is PARKED (chosen, not loaded).
//! - The first request that needs it wakes it (`ensure_awake`), restoring the
//!   last conversation's KV cache when one was saved (warm start).
//! - The frontend can prewarm it when the user is about to ask
//!   (`POST /models/prewarm`).
//! - After `MODEL_IDLE_SECONDS` without use (or `MODEL_IDLE_UNFOCUSED_SECONDS`
//!   while the window is unfocused) the reaper saves the KV cache and unloads
//!   it, releasing all of its memory.
//!
//! Drafting, the Vault, indexing and document search never need the chat
//! model, so they never keep it awake.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::RuntimeManager;
use crate::shared_state::SharedSystemState;

/// Serializes wake and sleep so a request can never be served by a server the
/// reaper is about to stop.
static TRANSITION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Requests currently using (or about to use) the model.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Whether the app window has focus (reported by the frontend).
static FOCUSED: AtomicBool = AtomicBool::new(true);

const REAPER_TICK: Duration = Duration::from_secs(10);

/// Held for the whole life of a request that uses the model; the reaper
/// never unloads while any exists.
pub struct ModelLease {
    rt: Option<Arc<RuntimeManager>>,
}

impl ModelLease {
    /// A lease that holds no engine: for answers served by a cloud provider,
    /// which must neither wake nor pin the local model.
    pub fn none() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        ModelLease { rt: None }
    }
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        if let Some(rt) = &self.rt {
            rt.touch();
        }
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

fn runtime_of(state: &SharedSystemState) -> Option<Arc<RuntimeManager>> {
    state.runtime_manager.read().ok().and_then(|g| g.clone())
}

/// Make sure the chosen model is loaded, waking it if it sleeps. Returns a
/// lease that keeps it loaded until dropped. A wake failure is returned as an
/// error; with no model chosen this succeeds and the caller's readiness check
/// reports it.
pub async fn ensure_awake(state: &SharedSystemState) -> anyhow::Result<ModelLease> {
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    let rt = runtime_of(state);
    let lease = ModelLease { rt: rt.clone() };
    let Some(rt) = rt else { return Ok(lease) };
    rt.touch();

    let _gate = TRANSITION.lock().await;
    if rt.is_sleeping() {
        let started = std::time::Instant::now();
        rt.wake().await?;
        info!("Model woke in {} ms", started.elapsed().as_millis());
        // A fresh engine holds nothing; the first request restores the longest
        // saved prefix of its own prompt from the KV store.
        super::kv_store::engine_reset();
        if !super::kv_store::enabled() {
            restore_warm_start(state, &rt).await;
        }
    }
    rt.touch();
    Ok(lease)
}

/// Hold this while replacing the model (POST /models/switch) so a switch
/// never interleaves with a wake or an idle unload.
/// Requests currently holding a ModelLease (answers in progress).
pub fn in_flight() -> usize {
    IN_FLIGHT.load(Ordering::SeqCst)
}

/// HOT_SWAP_GRACE_SECONDS: before a model switch unloads the running model,
/// let answers already generating finish, for up to this long.
pub async fn wait_for_in_flight(grace: std::time::Duration) {
    let started = std::time::Instant::now();
    while in_flight() > 0 && started.elapsed() < grace {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if in_flight() > 0 {
        tracing::warn!("Switching model with {} answer(s) still in progress after the {} s grace period", in_flight(), grace.as_secs());
    }
}

pub async fn transition_gate() -> tokio::sync::MutexGuard<'static, ()> {
    TRANSITION.lock().await
}

/// Wake in the background (the user is about to ask). Errors are logged only;
/// the real request reports them.
pub fn prewarm(state: Arc<SharedSystemState>) {
    if !crate::tuning::prewarm_enabled() {
        return;
    }
    tokio::spawn(async move {
        match ensure_awake(&state).await {
            Ok(_lease) => {}
            Err(e) => warn!("Prewarm could not load the model: {}", e),
        }
    });
}

/// When the projector was last used, in ms since START.
static PROJECTOR_USED_MS: AtomicU64 = AtomicU64::new(0);
/// Whether a task is waiting to release the projector.
static PROJECTOR_RELEASER: AtomicBool = AtomicBool::new(false);
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// VISION_RELEASE_SECONDS (30): how long after the last image the projector
/// stays loaded before the engine returns to text-only.
fn projector_release_after() -> Duration {
    Duration::from_secs(crate::tuning::parse_or("VISION_RELEASE_SECONDS", 30u64).max(5))
}

/// Make sure the engine runs with its vision projector, for reading images.
///
/// A vision model normally runs text-only so the KV store works (llama-server
/// refuses slot save while a projector is loaded). Loading it restarts the
/// engine: the conversation's KV state is saved first, so the next answer
/// restores it instead of recomputing. After VISION_RELEASE_SECONDS without
/// an image, and with nothing in flight, the engine returns to text-only.
/// Returns a lease that keeps the engine loaded; with no vision model chosen
/// it is an ordinary ensure_awake.
pub async fn with_projector(state: &Arc<SharedSystemState>) -> anyhow::Result<ModelLease> {
    let lease = ensure_awake(state).await?;
    let Some(rt) = runtime_of(state) else { return Ok(lease) };
    PROJECTOR_USED_MS.store(now_ms(), Ordering::SeqCst);
    {
        let _gate = TRANSITION.lock().await;
        let Some(mut config) = rt.get_current_config().await else { return Ok(lease) };
        if config.mmproj_path.is_none() || config.projector_loaded {
            return Ok(lease);
        }
        let started = std::time::Instant::now();
        if super::kv_store::enabled() {
            super::kv_store::save_active(state).await;
        }
        config.projector_loaded = true;
        rt.hot_swap(config).await?;
        super::kv_store::engine_reset();
        info!("Vision projector loaded for image reading in {} ms", started.elapsed().as_millis());
    }
    if !PROJECTOR_RELEASER.swap(true, Ordering::SeqCst) {
        let state = state.clone();
        tokio::spawn(async move {
            release_projector_when_idle(&state).await;
            PROJECTOR_RELEASER.store(false, Ordering::SeqCst);
        });
    }
    Ok(lease)
}

async fn release_projector_when_idle(state: &SharedSystemState) {
    let after = projector_release_after();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let idle = Duration::from_millis(now_ms().saturating_sub(PROJECTOR_USED_MS.load(Ordering::SeqCst)));
        if idle < after || in_flight() > 0 {
            continue;
        }
        let _gate = TRANSITION.lock().await;
        if in_flight() > 0 {
            continue;
        }
        let Some(rt) = runtime_of(state) else { return };
        let Some(mut config) = rt.get_current_config().await else { return };
        if !config.projector_loaded {
            return;
        }
        config.projector_loaded = false;
        // Nothing to save: the engine cannot save a state while the
        // projector is loaded. Answers given meanwhile are recomputed once.
        let result = if rt.is_sleeping() { rt.park(config).await.map(|_| ()) } else { rt.hot_swap(config).await.map(|_| ()) };
        super::kv_store::engine_reset();
        match result {
            Ok(()) => info!("Vision projector released; engine back to text-only (KV store available)"),
            Err(e) => warn!("Could not return the engine to text-only: {}", e),
        }
        return;
    }
}

/// True when the chosen model has a vision projector but is asleep, so an
/// image should wake it rather than fall back to OCR.
pub async fn sleeping_with_vision(state: &SharedSystemState) -> bool {
    match runtime_of(state) {
        Some(rt) if rt.is_sleeping() => rt
            .get_current_config()
            .await
            .map(|c| c.mmproj_path.is_some())
            .unwrap_or(false),
        _ => false,
    }
}

pub fn set_focused(focused: bool) {
    let was = FOCUSED.swap(focused, Ordering::SeqCst);
    if was != focused {
        debug!("App window {}", if focused { "focused" } else { "unfocused" });
    }
}

fn idle_limit(state: &SharedSystemState) -> Option<Duration> {
    let secs = if FOCUSED.load(Ordering::SeqCst) {
        state.config.model_idle_seconds
    } else {
        state.config.model_idle_unfocused_seconds
    };
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Algorithm 2's condition, used both before and after taking TRANSITION:
/// idle unloading enabled, idle at least the limit, no request in flight and
/// nothing generating.
fn may_reap(limit: Option<Duration>, idle: Duration, in_flight: usize, generating: bool) -> bool {
    match limit {
        Some(l) => idle >= l && in_flight == 0 && !generating,
        None => false,
    }
}

/// Save the KV cache and unload the model. Caller holds TRANSITION.
async fn put_to_sleep(state: &SharedSystemState, rt: &Arc<RuntimeManager>, idle: Duration) {
    if super::kv_store::enabled() {
        super::kv_store::save_active(state).await;
    } else {
        save_warm_start(state, rt).await;
    }
    let slept = rt.sleep().await;
    super::kv_store::engine_reset();
    match slept {
        Ok(()) => info!("Model unloaded after {} s idle; it reloads on the next question", idle.as_secs()),
        Err(e) => warn!("Could not unload the idle model: {}", e),
    }
}

/// Background task: unload the model once it has been idle long enough.
pub fn spawn_idle_reaper(state: Arc<SharedSystemState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAPER_TICK).await;
            let Some(rt) = runtime_of(&state) else { continue };
            if rt.get_state() != super::RuntimeState::Ready {
                continue;
            }
            let limit = idle_limit(&state);
            if !may_reap(limit, rt.idle_for(), IN_FLIGHT.load(Ordering::SeqCst), crate::rag::runtime().generation_active()) {
                continue;
            }
            let Ok(_gate) = TRANSITION.try_lock() else { continue };
            // Re-check under the gate: a request may have started meanwhile.
            if !may_reap(limit, rt.idle_for(), IN_FLIGHT.load(Ordering::SeqCst), crate::rag::runtime().generation_active()) {
                continue;
            }
            put_to_sleep(&state, &rt, rt.idle_for()).await;
        }
    });
}

pub(crate) fn model_id_of(config: &super::RuntimeConfig) -> String {
    config
        .model_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub(crate) async fn engine_id_of(state: &SharedSystemState) -> String {
    match state.engine_manager {
        Some(ref em) => {
            let registry = em.registry.read().await;
            registry.get_default_engine().map(|e| e.id.clone()).unwrap_or_default()
        }
        None => String::new(),
    }
}

/// Save the active slot's KV cache for the last conversation, so the next
/// wake (or launch) resumes it without re-reading the prompt.
pub async fn save_warm_start(state: &SharedSystemState, rt: &Arc<RuntimeManager>) {
    let session_id = match state.last_active_session.read().ok().and_then(|g| g.clone()) {
        Some(id) => id,
        None => {
            debug!("No warm start saved: no conversation generated yet");
            return;
        }
    };
    let Some(base_url) = rt.get_base_url().await else { return };
    let Some(config) = rt.get_current_config().await else { return };
    if config.slot_save_path.is_none() {
        debug!("No warm start saved: the engine runs without --slot-save-path");
        return;
    }
    let ctx_size = rt.get_effective_context().await.unwrap_or(config.context_size);
    super::slot_cache::save_active_slot(
        &state.database_pool,
        &base_url,
        &session_id,
        &model_id_of(&config),
        &engine_id_of(state).await,
        ctx_size,
    )
    .await;
}

/// Restore the saved KV cache into the freshly loaded engine, if it matches.
pub async fn restore_warm_start(state: &SharedSystemState, rt: &Arc<RuntimeManager>) {
    let Some(base_url) = rt.get_base_url().await else { return };
    let Some(config) = rt.get_current_config().await else { return };
    if config.slot_save_path.is_none() {
        return;
    }
    let ctx_size = rt.get_effective_context().await.unwrap_or(config.context_size);
    use super::slot_cache::RestoreOutcome;
    match super::slot_cache::restore_if_valid(
        &state.database_pool,
        &base_url,
        &model_id_of(&config),
        &engine_id_of(state).await,
        ctx_size,
    )
    .await
    {
        RestoreOutcome::Restored { session_id, n_tokens } => {
            info!("Warm start: {} tokens restored for session {}", n_tokens, session_id)
        }
        RestoreOutcome::NoRecord => debug!("No warm-start cache to restore"),
        RestoreOutcome::Rejected(reason) => info!("Warm-start cache not used: {}", reason),
        RestoreOutcome::Failed(reason) => warn!("Warm-start restore failed: {}", reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn an_idle_model_with_no_request_may_be_unloaded() {
        assert!(may_reap(Some(S(300)), S(300), 0, false));
        assert!(may_reap(Some(S(300)), S(900), 0, false));
    }

    #[test]
    fn a_request_in_flight_or_a_reply_in_progress_always_blocks_unloading() {
        assert!(!may_reap(Some(S(300)), S(9_999), 1, false), "a lease is held");
        assert!(!may_reap(Some(S(300)), S(9_999), 0, true), "a reply is streaming");
    }

    #[test]
    fn the_model_stays_until_the_idle_limit_and_a_zero_limit_disables_unloading() {
        assert!(!may_reap(Some(S(300)), S(299), 0, false));
        assert!(!may_reap(None, S(9_999), 0, false));
    }
}
