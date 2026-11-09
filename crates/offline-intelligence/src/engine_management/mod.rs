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
pub mod verification_log;

pub use registry::{EngineRegistry, EngineInfo, EngineStatus, AccelerationType, ENGINE_VERSION};
pub use analyzer::{HardwareAnalyzer, HardwareProfile};
pub use dll_manager::{DllManager, BackendLoadResult};
pub use downloader::{EngineDownloader, BackendVerificationFailure};
pub use verification_log::{VerificationLog, VerificationFailure};
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
        /// The acceleration this machine WOULD have used had it worked, when
        /// the engine in use is the result of a demotion. `None` in the normal
        /// case. Surfaced so the user is told plainly that they are on a slower
        /// engine and why — a demotion is never silent.
        #[serde(skip_serializing_if = "Option::is_none")]
        demoted_from: Option<String>,
        /// The recorded verification failure behind `demoted_from`.
        #[serde(skip_serializing_if = "Option::is_none")]
        demotion_reason: Option<String>,
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
                .or_else(|| {
                    // Not in the filtered catalog. It may be an engine we
                    // previously demoted away from — and asking for it BY ID is
                    // an explicit user decision ("I updated my driver, try it
                    // again"), which outranks a stale recorded failure. The
                    // exclusion is cleared below so the attempt is real.
                    registry
                        .catalog_including_excluded(&self.hardware_capabilities)
                        .into_iter()
                        .find(|e| e.id == engine_id)
                })
                .ok_or_else(|| anyhow::anyhow!(
                    "Engine id '{}' is not in the catalog for this hardware — refusing to \
                     substitute a different engine. Use GET /engines to list valid ids.",
                    engine_id
                ))?
        };

        // An explicit by-id install retries the acceleration from scratch. If it
        // fails again, download_engine's verification records the failure anew,
        // so nothing is lost by clearing it first.
        {
            let mut registry = self.registry.write().await;
            if registry.clear_exclusion(&engine_info.acceleration) {
                registry.save_verification_log();
                tracing::info!(
                    "Cleared the recorded '{}' failure because the user explicitly requested \
                     engine '{}' — it will be verified again from scratch.",
                    engine_info.acceleration, engine_id
                );
            }
        }

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
    /// by the registry's hardware decision table.
    ///
    /// ## Backend failure is the only thing that changes the choice
    ///
    /// If the installed engine downloads and extracts correctly but never
    /// self-reports loading its acceleration backend, that is proof this
    /// machine cannot use that acceleration — not a transient error. It is
    /// recorded in the verification log and the decision table is re-run with
    /// that acceleration excluded, yielding the next correct engine for this
    /// hardware (CUDA -> Vulkan -> CPU).
    ///
    /// This is bounded and terminal, not an open-ended fallback chain:
    ///   - at most `MAX_INSTALL_ATTEMPTS` attempts,
    ///   - each attempt is the decision table's single answer given the
    ///     evidence, never a guess,
    ///   - the CPU row cannot be excluded, so the sequence always terminates,
    ///   - every other failure (network, hash mismatch, extraction, cudart)
    ///     propagates immediately and demotes nothing — those say nothing about
    ///     this machine's capabilities.
    ///
    /// Mirrors the same loop in installer-hooks.nsi (`oci_SelectAndInstall`).
    pub async fn download_suitable_engine(&self) -> Result<EngineInfo> {
        /// CUDA -> Vulkan -> CPU is the deepest path through the table.
        const MAX_INSTALL_ATTEMPTS: usize = 3;

        let mut last_backend_failure: Option<String> = None;

        for attempt in 1..=MAX_INSTALL_ATTEMPTS {
            let engine_info = {
                let registry = self.registry.read().await;
                registry.select_correct_engine(&self.hardware_capabilities)?
            };

            tracing::info!(
                "Decision table selected engine '{}' ({}) for this hardware (attempt {}/{})",
                engine_info.id, engine_info.acceleration, attempt, MAX_INSTALL_ATTEMPTS
            );

            let install_result = self.downloader.download_engine(&engine_info).await;

            let err = match install_result {
                Ok(installed) => {
                    // Success also updates the log in the clearing direction:
                    // scan_installed_engines observes a verified engine of this
                    // acceleration and removes any stale exclusion for it.
                    {
                        let mut registry = self.registry.write().await;
                        registry.scan_installed_engines(&self.hardware_capabilities).await?;
                    }
                    self.select_best_engine().await?;
                    return Ok(installed);
                }
                Err(e) => e,
            };

            // Only a proven backend-load failure demotes. Anything else is an
            // install problem, not a hardware verdict.
            let failure = match err.downcast_ref::<BackendVerificationFailure>() {
                Some(f) => f.clone(),
                None => return Err(err),
            };

            tracing::warn!(
                "Engine '{}' failed backend verification ({} backend never loaded). Recording \
                 '{}' as unusable on this machine and re-running the decision table.",
                failure.engine_id, failure.acceleration, failure.acceleration
            );

            {
                let mut registry = self.registry.write().await;
                registry.verification_log.record_failure(
                    &failure.acceleration,
                    &failure.engine_id,
                    &failure.engine_version,
                    format!(
                        "The {} backend did not load when the engine was launched on this \
                         computer. This usually means the graphics driver does not support it, \
                         or the Microsoft Visual C++ 2015-2022 Redistributable is missing or \
                         out of date (the GPU backends require VCRUNTIME140_1.dll).",
                        failure.acceleration
                    ),
                );
                registry.save_verification_log();
                // Refresh the catalog so the excluded engine disappears from it
                // before the next selection resolves against it.
                registry.refresh_available_engines(&self.hardware_capabilities).await?;
            }

            last_backend_failure = Some(format!("{}", failure));

            // CPU is terminal: if it cannot load its own backend, nothing in the
            // catalog can (every package ships the same ggml-cpu set), so there
            // is nothing left to try and no honest reason to loop again.
            if failure.acceleration == AccelerationType::CPU {
                return Err(anyhow::anyhow!(
                    "The CPU engine failed to load its own backend, so no usable engine exists \
                     on this computer. This almost always means the Microsoft Visual C++ \
                     2015-2022 Redistributable (x64) is missing or damaged.\n\n{}",
                    last_backend_failure.unwrap_or_default()
                ));
            }
        }

        Err(anyhow::anyhow!(
            "Exhausted the engine decision table after {} attempts without finding an engine \
             that works on this computer.\n\n{}",
            MAX_INSTALL_ATTEMPTS,
            last_backend_failure.unwrap_or_else(|| "(no backend failure recorded)".to_string())
        ))
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
            let demoted = registry.demoted_from(&self.hardware_capabilities);
            let demotion_reason = demoted
                .as_ref()
                .and_then(|a| registry.exclusion_reason(a))
                .map(|s| s.to_string());
            return EngineState::Ready {
                engine_id: default.id.clone(),
                acceleration: default.acceleration.to_string(),
                version: default.version.clone(),
                demoted_from: demoted.map(|a| a.to_string()),
                demotion_reason,
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
    #[allow(clippy::missing_panics_doc)]
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

#[cfg(test)]
mod engine_state_wire_format_tests {
    use super::*;

    // These pin the exact JSON `EngineState` produces, because
    // apps/desktop/src/components/EngineSetupGate.tsx reads these key names
    // from /healthz and GET /engines. The enum carries
    // #[serde(rename_all = "snake_case")], which renames VARIANTS but not
    // struct-variant FIELDS — a distinction easy to misremember, and one that
    // would silently break the gate rather than fail a build.

    #[test]
    fn ready_without_demotion_omits_the_demotion_keys() {
        let json = serde_json::to_value(EngineState::Ready {
            engine_id: "llama-cpu-windows-x64-b8037".into(),
            acceleration: "CPU".into(),
            version: "b8037".into(),
            demoted_from: None,
            demotion_reason: None,
        })
        .unwrap();

        assert_eq!(json["state"], "ready");
        assert_eq!(json["engine_id"], "llama-cpu-windows-x64-b8037");
        assert_eq!(json["acceleration"], "CPU");
        assert_eq!(json["version"], "b8037");
        // skip_serializing_if keeps the payload clean when nothing was demoted.
        assert!(json.get("demoted_from").is_none());
        assert!(json.get("demotion_reason").is_none());
    }

    #[test]
    fn ready_with_demotion_exposes_both_keys() {
        let json = serde_json::to_value(EngineState::Ready {
            engine_id: "llama-cpu-windows-x64-b8037".into(),
            acceleration: "CPU".into(),
            version: "b8037".into(),
            demoted_from: Some("Vulkan".into()),
            demotion_reason: Some("backend did not load".into()),
        })
        .unwrap();

        assert_eq!(json["state"], "ready");
        assert_eq!(json["demoted_from"], "Vulkan");
        assert_eq!(json["demotion_reason"], "backend did not load");
    }

    #[test]
    fn not_installed_and_corrupted_keys_are_unchanged() {
        // Guards the pre-existing contract the gate already depends on.
        let json = serde_json::to_value(EngineState::NotInstalled).unwrap();
        assert_eq!(json["state"], "not_installed");

        let json = serde_json::to_value(EngineState::Corrupted {
            engine_id: "e".into(),
            reason: "r".into(),
        })
        .unwrap();
        assert_eq!(json["state"], "corrupted");
        assert_eq!(json["engine_id"], "e");
        assert_eq!(json["reason"], "r");
    }
}
