//! Context engine module - Orchestrates context memory system

pub mod retrieval_planner;
pub mod tier_manager;
pub mod orchestrator;
pub mod document_memory;
pub mod reference_detector;
pub mod past_retrieval;

pub use retrieval_planner::{RetrievalPlanner, RetrievalPlan};
pub use tier_manager::{TierManager, TierManagerConfig, TierStats};
pub use orchestrator::{ContextOrchestrator, OrchestratorConfig, SessionStats, CleanupStats};
pub use document_memory::{build_document_context, fold_into_system_message, DocumentContextBlocks};
pub use reference_detector::{detect_reference, ReferenceIntent, ReferenceSignal};
pub use past_retrieval::{build_past_context, PastContext};
pub use document_memory::fold_into_latest_message;

/// Default Context Orchestrator.
/// `max_context_tokens` MUST reflect the real model context window (minus a
/// generation reserve) - a budget smaller than the true window forces
/// needless context rewriting, which busts llama-server's prompt cache and
/// re-prefills the whole conversation every turn.
pub async fn create_default_orchestrator(
    database: std::sync::Arc<crate::memory_db::MemoryDatabase>,
    max_context_tokens: usize,
) -> anyhow::Result<ContextOrchestrator> {
    let mut config = OrchestratorConfig::default();
    config.max_context_tokens = max_context_tokens;
    ContextOrchestrator::new(database, config).await
}