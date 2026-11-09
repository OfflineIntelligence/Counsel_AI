//! Model Management System
//!
//! Provides comprehensive model lifecycle management including:
//! - Model registry and metadata storage
//! - Download from HuggingFace
//! - Local storage management in AppData
//! - Hardware-aware model recommendations
//! - Progress tracking for downloads

pub mod registry;
pub mod downloader;
pub mod storage;
pub mod recommendation;
pub mod progress;
pub mod pending;
pub mod engine_compat;

pub use registry::{ModelRegistry, ModelInfo, ModelStatus};
pub use downloader::{ModelDownloader, DownloadSource};
pub use storage::{ModelStorage, StorageLocation};
pub use recommendation::ModelRecommender;
pub use progress::{DownloadProgress, ProgressTracker};

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;

/// Main model management service
pub struct ModelManager {
    pub registry: Arc<RwLock<ModelRegistry>>,
    pub downloader: Arc<ModelDownloader>,
    pub storage: Arc<ModelStorage>,
    pub recommender: Arc<ModelRecommender>,
}

impl ModelManager {
    pub fn new() -> Result<Self> {
        let storage = Arc::new(ModelStorage::new()?);
        let registry = Arc::new(RwLock::new(ModelRegistry::new(storage.clone())?));
        let downloader = Arc::new(ModelDownloader::new(storage.clone()));
        let recommender = Arc::new(ModelRecommender::new());

        Ok(Self {
            registry,
            downloader,
            storage,
            recommender,
        })
    }

    /// Initialize the model manager and scan for existing models
    pub async fn initialize(&self, cfg: &Config) -> Result<()> {
        // Scan storage for existing models and populate registry
        self.registry.write().await.scan_storage().await?;

        // After scanning storage, compute compatibility scores for local models
        // based on the current hardware profile so that the UI can sort by
        // "Best Match" using the compatibility_score field.
        let hardware = ModelRecommender::detect_hardware_profile(cfg);
        {
            let mut registry = self.registry.write().await;
            let recommender = &*self.recommender;
            registry.update_compatibility_scores(recommender, &hardware);
            // Persist updated registry so compatibility scores survive restarts
            let _ = registry.save_registry().await;
        }

        // Refresh the remote catalog in the background: startup must never wait
        // on the network (the Hugging Face call once took over 2 minutes and
        // held the app on its splash). MODEL_CATALOG_REFRESH_ON_START=false
        // skips it; the Models page can still refresh on demand.
        if crate::tuning::flag_or("MODEL_CATALOG_REFRESH_ON_START", true) {
            let registry = self.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = refresh_hf_catalog_unlocked(&registry, 500).await {
                    tracing::warn!("Failed to refresh Hugging Face catalog: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Refresh model catalogs from HuggingFace
    pub async fn refresh_catalogs(&self, _cfg: &Config) -> Result<()> {
        // Refresh Hugging Face GGUF/GGML catalog
        if let Err(e) = refresh_hf_catalog_unlocked(&self.registry, 500).await {
            tracing::warn!("Failed to refresh Hugging Face catalog: {}", e);
        }

        Ok(())
    }
}
/// Refresh the Hugging Face catalog WITHOUT holding the registry lock across
/// the network: the refresh runs on a detached copy, then the lock is taken
/// only to merge and save. Holding it for the whole refresh (minutes on a slow
/// connection) left `GET /models` waiting, so the Models page sat on
/// "Loading models…".
pub async fn refresh_hf_catalog_unlocked(registry: &Arc<RwLock<ModelRegistry>>, limit: usize) -> Result<()> {
    let (mut scratch, before) = {
        let live = registry.read().await;
        (live.catalog_copy(), live.model_ids())
    };
    let result = scratch.refresh_huggingface_catalog_from_api(limit).await;
    let mut live = registry.write().await;
    live.merge_catalog(scratch, &before);
    if let Err(e) = live.save_registry().await {
        tracing::error!("Failed to save model registry after HuggingFace refresh: {}", e);
    }
    result
}
