// Server/src/backend_target.rs
// Lock-free backend target using atomic pointer swapping

use std::sync::Arc;
use arc_swap::ArcSwap;
use tracing::{info, warn};

#[derive(Clone)]
pub struct BackendTarget {
    inner: Arc<ArcSwap<String>>,
}

impl BackendTarget {
    pub fn new(initial: String) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(initial))),
        }
    }

    pub async fn set(&self, new_target: String) {
        let current = self.inner.load();
        
        // If current value is empty, always set it (no warning)
        if current.is_empty() {
            info!("🔄 Setting initial backend target to: {}", new_target);
            self.inner.store(Arc::new(new_target));
        } 
        // Only warn if we're changing from one non-empty value to another
        else if **current != new_target {
            info!("🔄 Switching backend target from {} → {}", **current, new_target);
            self.inner.store(Arc::new(new_target));
        } else {
            warn!("backend target set() called, but no change (still {})", new_target);
        }
    }

    pub async fn get(&self) -> String {
        (**self.inner.load()).clone()
    }

    /// Check if backend target is properly initialized
    pub async fn is_initialized(&self) -> bool {
        !self.inner.load().is_empty()
    }

    /// Main generation endpoint used by your Python code
    /// Your Python Core_engine.rs calls the root endpoint "/"
    pub async fn generate_url(&self) -> String {
        let base = self.get().await;
        if base.is_empty() {
            warn!("Backend target not initialized yet, returning empty URL");
        }
        format!("{}/", base)
    }

    /// Health check endpoint for connection testing
    pub async fn health_url(&self) -> String {
        let base = self.get().await;
        format!("{}/health", base)
    }

    /// Chat completions (OpenAI-compatible endpoint used by proxy)
    pub async fn chat_completions_url(&self) -> String {
        let base = self.get().await;
        format!("{}/v1/chat/completions", base)
    }

    /// NEW: Direct endpoint for your Python code's current structure
    /// This matches what your Core_engine.rs _post() method expects
    pub async fn direct_generation_url(&self) -> String {
        let base = self.get().await;
        if base.is_empty() {
            warn!("Backend target not initialized yet, returning empty URL");
        }
        format!("{}/", base)
    }
}