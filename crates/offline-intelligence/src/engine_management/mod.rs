//! Engine Management System
//!
//! Provides comprehensive llama.cpp engine lifecycle management including:
//! - Hardware capability detection and analysis
//! - Engine registry and metadata storage
//! - Local engine storage management
//! - Automatic engine selection based on hardware
//! - Cross-platform compatibility (Windows, macOS, Linux)
//!
//! Engine acquisition paths (both hash-verified, both metadata-last):
//!   1. Install time: the NSIS installer downloads the decision-table engine
//!      (apps/desktop/src-tauri/installer-hooks.nsi).
//!   2. Runtime, user-triggered ONLY: POST /engines/install (the frontend's
//!      explicit "Install engine" button) calls download_suitable_engine().
//! Nothing downloads automatically in the background.

pub mod registry;
pub mod analyzer;
pub mod dll_manager;
pub mod downloader;
pub mod download_progress;

pub use registry::{EngineRegistry, EngineInfo, EngineStatus, AccelerationType, ENGINE_VERSION};
pub use analyzer::{HardwareAnalyzer, HardwareProfile};
pub use dll_manager::{DllManager, BackendLoadResult};
pub use downloader::EngineDownloader;
pub use download_progress::{EngineDownloadProgressTracker, EngineDownloadProgress, EngineDownloadStatus};

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::model_runtime::platform_detector::HardwareCapabilities;

/// Explicit, user-visible engine state. Computed on demand from the registry so
/// it can never drift from reality — there is no cached copy to forget to update.
/// Serialized into /healthz and GET /engines so the frontend can name the exact
/// problem and offer the explicit recovery action (POST /engines/install).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EngineState {
    /// A verified engine is installed and selected as default.
    Ready {
        engine_id: String,
        acceleration: String,
        version: String,
    },
    /// Nothing usable on disk — the installer step was skipped, cancelled, or
    /// failed. Recovery: user-triggered POST /engines/install.
    NotInstalled,
    /// Engine(s) exist on disk but failed startup verification. The reason is
    /// the exact verification failure. Recovery: user-triggered reinstall.
    Corrupted {
        engine_id: String,
        reason: String,
    },
}

/// Main engine management service
pub struct EngineManager {
    pub registry: Arc<RwLock<EngineRegistry>>,
    pub analyzer: Arc<HardwareAnalyzer>,
    pub hardware_capabilities: HardwareCapabilities,
    pub downloader: Arc<EngineDownloader>,
}

impl EngineManager {
    pub fn new() -> Result<Self> {
        let hardware_capabilities = HardwareCapabilities::detect();
        let analyzer = Arc::new(HardwareAnalyzer::new(hardware_capabilities.clone()));
        let registry = Arc::new(RwLock::new(EngineRegistry::new()?));
        let downloader = Arc::new(EngineDownloader::new());

        Ok(Self {
            registry,
            analyzer,
            hardware_capabilities,
            downloader,
        })
    }

    /// Initialize the engine manager: scan installed engines and refresh the available-engine
    /// catalog.  This is intentionally non-blocking — no downloads happen here.
    /// Returns Ok(true)  if at least one compatible installed engine was found and set as default.
    /// Returns Ok(false) if no engine is installed yet (first run or clean install).
    /// The installer should have placed an engine; Ok(false) means the installer
    /// step was skipped or failed.
    pub async fn initialize(&self, _cfg: &Config) -> Result<bool> {
        {
            let mut registry = self.registry.write().await;
            registry.scan_installed_engines(&self.hardware_capabilities).await?;
        }

        let installed_engines_count = self.registry.read().await.installed_engines.len();

        if installed_engines_count == 0 {
            tracing::info!("No engines installed — the NSIS installer should have placed one in AppData/engines/");
            return Ok(false);
        }

        let has_suitable = self.check_suitable_engine().await?;
        if has_suitable {
            self.select_best_engine().await?;
        }

        Ok(has_suitable)
    }

    /// Check if we have an engine suitable for current hardware
    pub async fn check_suitable_engine(&self) -> Result<bool> {
        let registry = self.registry.read().await;
        let suitable_engines = registry.get_compatible_engines(&self.hardware_capabilities);
        Ok(!suitable_engines.is_empty())
    }

    /// Select and set the best engine for current hardware as default
    pub async fn select_best_engine(&self) -> Result<Option<EngineInfo>> {
        let mut registry = self.registry.write().await;
        let best_engine = registry.select_best_compatible_engine(&self.hardware_capabilities);

        if let Some(engine) = &best_engine {
            registry.set_default_engine(&engine.id)?;
            tracing::info!("Selected engine: {} for hardware: {:?}",
                engine.name, self.hardware_capabilities);
        }

        Ok(best_engine)
    }

    /// Download and install a specific engine by its ID from the available engines list.
    /// After installation, re-scans and selects the best engine.
    ///
    /// The requested ID is installed or the call fails — an unknown ID is an
    /// explicit error, never substituted with a different engine.
    pub async fn install_engine_by_id(&self, engine_id: &str) -> Result<EngineInfo> {
        let engine_info = {
            let registry = self.registry.read().await;

            // Check if already installed (and verified — only verified engines
            // are ever present in installed_engines with Installed status)
            if let Some(installed) = registry.installed_engines.get(engine_id) {
                if installed.status == EngineStatus::Installed {
                    return Ok(installed.clone());
                }
            }

            registry.available_engines.iter()
                .find(|e| e.id == engine_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!(
                    "Engine id '{}' is not in the catalog for this hardware — refusing to \
                     substitute a different engine. Use GET /engines to list valid ids.",
                    engine_id
                ))?
        };

        let installed = self.downloader.download_engine(&engine_info).await?;

        // Re-scan so the registry picks up the new engine
        {
            let mut registry = self.registry.write().await;
            registry.scan_installed_engines(&self.hardware_capabilities).await?;
        }
        self.select_best_engine().await?;

        Ok(installed)
    }

    /// Download and install the ONE correct engine for this machine, as decided
    /// by the registry's hardware decision table. No alternatives are tried on
    /// failure — the error propagates to the caller for explicit user action.
    pub async fn download_suitable_engine(&self) -> Result<EngineInfo> {
        let engine_info = {
            let registry = self.registry.read().await;
            registry.select_correct_engine(&self.hardware_capabilities)?
        };

        tracing::info!(
            "Decision table selected engine '{}' ({}) for this hardware",
            engine_info.id, engine_info.acceleration
        );

        let installed = self.downloader.download_engine(&engine_info).await?;

        {
            let mut registry = self.registry.write().await;
            registry.scan_installed_engines(&self.hardware_capabilities).await?;
        }
        self.select_best_engine().await?;

        Ok(installed)
    }

    /// Get download progress for all engine downloads
    pub async fn get_download_progress(&self) -> Vec<EngineDownloadProgress> {
        self.downloader.get_all_download_progress().await
    }

    /// Cancel an ongoing engine download
    pub async fn cancel_engine_download(&self, engine_id: &str) -> Result<()> {
        self.downloader.cancel_download(engine_id).await
    }

    /// Get information about current hardware capabilities
    pub fn get_hardware_info(&self) -> &HardwareCapabilities {
        &self.hardware_capabilities
    }

    /// Compute the current explicit engine state from the registry.
    ///
    /// Precedence: a usable default engine wins; otherwise the (deterministically
    /// first) Corrupted engine is reported with its verification failure; otherwise
    /// nothing is installed. Never cached — always reflects the registry as-is.
    pub async fn current_state(&self) -> EngineState {
        let registry = self.registry.read().await;

        if let Some(default) = registry.get_default_engine() {
            return EngineState::Ready {
                engine_id: default.id.clone(),
                acceleration: default.acceleration.to_string(),
                version: default.version.clone(),
            };
        }

        let mut corrupted: Vec<&EngineInfo> = registry
            .installed_engines
            .values()
            .filter(|e| e.status == EngineStatus::Corrupted)
            .collect();
        corrupted.sort_by(|a, b| a.id.cmp(&b.id));

        if let Some(c) = corrupted.first() {
            return EngineState::Corrupted {
                engine_id: c.id.clone(),
                reason: c
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "verification failed (no detail recorded)".to_string()),
            };
        }

        EngineState::NotInstalled
    }

    /// Get detailed status information about the engine manager
    pub async fn get_status_info(&self) -> String {
        let registry = self.registry.read().await;
        let installed_count = registry.installed_engines.len();
        let default_engine = registry.default_engine.as_deref().unwrap_or("None");

        format!(
            "Engine Manager Status:\n  Installed Engines: {}\n  Default Engine: {}\n  Hardware: {:?} {:?} (CUDA: {})",
            installed_count,
            default_engine,
            self.hardware_capabilities.platform,
            self.hardware_capabilities.architecture,
            self.hardware_capabilities.has_cuda,
        )
    }
}
