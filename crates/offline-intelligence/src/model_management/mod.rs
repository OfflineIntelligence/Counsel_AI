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

        // Refresh model catalogs from remote sources on startup
        self.refresh_catalogs(cfg).await?;

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

        Ok(())
    }

    /// Refresh model catalogs from HuggingFace
    pub async fn refresh_catalogs(&self, _cfg: &Config) -> Result<()> {
        // Refresh Hugging Face GGUF/GGML catalog
        let mut registry = self.registry.write().await;
        if let Err(e) = registry.refresh_huggingface_catalog_from_api(500).await {
            tracing::warn!("Failed to refresh Hugging Face catalog: {}", e);
        }
        if let Err(e) = registry.save_registry().await {
            tracing::error!("Failed to save model registry after HuggingFace refresh: {}", e);
        }

        Ok(())
    }
}