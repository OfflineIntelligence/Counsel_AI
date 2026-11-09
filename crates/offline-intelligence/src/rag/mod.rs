//! Retrieval-augmented generation over documents: chunking, keyword and
//! vector indexes, matter scoping, query routing and evidence assembly.
//! Design: docs/rag/RAG_R0-R6_PLAN.md.

pub mod chunker;
pub mod context;
pub mod embedder;
pub mod indexer;
pub mod model;
pub mod router;
pub mod scope;
pub mod search;
pub mod store;
pub mod vector;
pub mod versions;
pub mod citations;
pub mod echo_guard;

pub mod eval;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use lazy_static::lazy_static;
use tokio::sync::Notify;

pub use store::RagStore;

lazy_static! {
    static ref INDEX_WORK: Notify = Notify::new();
    static ref RUNTIME: RwLock<Arc<RagRuntime>> = RwLock::new(Arc::new(RagRuntime::new(Arc::new(embedder::LlamaEmbedder::new()))));
}

/// Wake the background indexer.
pub fn signal_index_work() {
    INDEX_WORK.notify_one();
}

pub(crate) async fn index_work_signalled() {
    INDEX_WORK.notified().await;
}

pub struct RagRuntime {
    pub embedder: Arc<dyn embedder::Embedder>,
    pub llama: Option<Arc<embedder::LlamaEmbedder>>,
    pub index: RwLock<vector::VectorIndex>,
    /// Chat messages, grouped by session row id.
    pub message_index: RwLock<vector::VectorIndex>,
    active_generations: AtomicUsize,
}

impl RagRuntime {
    fn new(llama: Arc<embedder::LlamaEmbedder>) -> Self {
        Self {
            embedder: llama.clone(),
            llama: Some(llama),
            index: RwLock::new(vector::VectorIndex::default()),
            message_index: RwLock::new(vector::VectorIndex::default()),
            active_generations: AtomicUsize::new(0),
        }
    }

    pub fn with_embedder(embedder: Arc<dyn embedder::Embedder>) -> Self {
        Self {
            embedder,
            llama: None,
            index: RwLock::new(vector::VectorIndex::default()),
            message_index: RwLock::new(vector::VectorIndex::default()),
            active_generations: AtomicUsize::new(0),
        }
    }

    pub fn generation_active(&self) -> bool {
        self.active_generations.load(Ordering::SeqCst) > 0
    }
}

pub fn runtime() -> Arc<RagRuntime> {
    RUNTIME.read().map(|g| g.clone()).unwrap_or_else(|p| p.into_inner().clone())
}

/// Replace the runtime (tests use a deterministic embedder).
pub fn set_runtime(rt: Arc<RagRuntime>) {
    if let Ok(mut g) = RUNTIME.write() {
        *g = rt;
    }
}

/// Held while a chat response streams; the indexer yields compute meanwhile.
pub struct GenerationGuard(Arc<RagRuntime>);

impl GenerationGuard {
    pub fn new() -> Self {
        let rt = runtime();
        rt.active_generations.fetch_add(1, Ordering::SeqCst);
        Self(rt)
    }
}

impl Default for GenerationGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for GenerationGuard {
    fn drop(&mut self) {
        self.0.active_generations.fetch_sub(1, Ordering::SeqCst);
    }
}
