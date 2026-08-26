//! Model Storage Management
//!
//! Handles local storage of models in platform-appropriate locations:
//! - Windows: %LOCALAPPDATA%\Offline Counsel AI\models
//! - macOS:   ~/Library/Application Support/Offline Counsel AI/models
//! - Linux:   ~/.local/share/Offline Counsel AI/models

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, info};

/// Sanitize a model ID for use as a filesystem directory/file name.
/// Replaces characters invalid on Windows (: / \ < > " | ? *) with underscores.
/// Also trims trailing periods and spaces, handles reserved names, and limits length.
///
/// Also the canonical registry key: the install path normalizes to this form
/// so `scan_storage` on the next startup updates the same registry entry
/// (folder names are already sanitized) instead of creating a second one.
pub fn sanitize_model_id(model_id: &str) -> String {
    let mut sanitized = model_id
        .replace(':', "_")
        .replace('/', "_")
        .replace('\\', "_")
        .replace('<', "_")
        .replace('>', "_")
        .replace('"', "_")
        .replace('|', "_")
        .replace('?', "_")
        .replace('*', "_");

    // Trim trailing periods and spaces which are invalid on Windows
    sanitized = sanitized.trim_end_matches('.').trim_end().to_string();

    // Handle Windows reserved names by adding a suffix if needed
    let upper_sanitized = sanitized.to_uppercase();
    if is_windows_reserved_name(&upper_sanitized) {
        sanitized.push_str("_model");
    }

    // Limit length to avoid Windows path length issues (keep under 200 chars to be safe)
    if sanitized.len() > 200 {
        // Hash the long name and append a portion of the original for readability
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        sanitized.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());
        
        // Take first 150 chars of sanitized + hash to ensure uniqueness while keeping readable
        let prefix_len = std::cmp::min(150, sanitized.len());
        sanitized = format!("{}_{}", &sanitized[..prefix_len], hash);
    }

    sanitized
}

/// Check if a name is a Windows reserved name
fn is_windows_reserved_name(name: &str) -> bool {
    matches!(name, 
        "CON" | "PRN" | "AUX" | "NUL" |
        "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9" |
        "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

/// Platform-specific storage location
#[derive(Debug, Clone)]
pub struct StorageLocation {
    /// Base directory for all Offline_Intelligence data
    pub app_data_dir: PathBuf,
    /// Directory for storing downloaded models
    pub models_dir: PathBuf,
    /// Directory for model metadata and registry
    pub registry_dir: PathBuf,
}

/// Model storage manager
pub struct ModelStorage {
    pub location: StorageLocation,
}

impl ModelStorage {
    /// Create new storage manager with platform-appropriate paths
    pub fn new() -> Result<Self> {
        let location = Self::get_platform_storage_location()?;

        // Ensure directories exist
        Self::ensure_directories(&location)?;

        info!(
            "Model storage initialized at: {}",
            location.models_dir.display()
        );

        Ok(Self { location })
    }

    /// Get platform-appropriate storage location
    fn get_platform_storage_location() -> Result<StorageLocation> {
        let app_data_dir = crate::config::get_app_data_dir();

        let models_dir = app_data_dir.join("models");
        let registry_dir = app_data_dir.join("registry");

        Ok(StorageLocation {
            app_data_dir,
            models_dir,
            registry_dir,
        })
    }

    /// Ensure all required directories exist
    fn ensure_directories(location: &StorageLocation) -> Result<()> {
        std::fs::create_dir_all(&location.app_data_dir)
            .context("Failed to create app data directory")?;
        std::fs::create_dir_all(&location.models_dir)
            .context("Failed to create models directory")?;
        std::fs::create_dir_all(&location.registry_dir)
            .context("Failed to create registry directory")?;

        debug!("Created storage directories successfully");
        Ok(())
    }

    /// Get the full path for a model file
    pub fn model_path(&self, model_id: &str, filename: &str) -> PathBuf {
        self.location
            .models_dir
            .join(sanitize_model_id(model_id))
            .join(filename)
    }

    /// Get the path for model metadata
    pub fn metadata_path(&self, model_id: &str) -> PathBuf {
        self.location
            .registry_dir
            .join(format!("{}.json", sanitize_model_id(model_id)))
    }

    /// List all available models in storage
    pub fn list_models(&self) -> Result<Vec<String>> {
        let mut models = Vec::new();

        if self.location.models_dir.exists() {
            for entry in std::fs::read_dir(&self.location.models_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    if let Some(model_name) = entry.file_name().to_str() {
                        models.push(model_name.to_string());
                    }
                }
            }
        }

        Ok(models)
    }

    /// Check if a model exists locally
    pub fn model_exists(&self, model_id: &str) -> bool {
        self.location
            .models_dir
            .join(sanitize_model_id(model_id))
            .exists()
    }

    /// Get total storage usage
    pub fn get_storage_usage(&self) -> Result<u64> {
        if !self.location.models_dir.exists() {
            return Ok(0);
        }

        let mut total_size = 0u64;

        for entry in walkdir::WalkDir::new(&self.location.models_dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                total_size += entry.metadata()?.len();
            }
        }

        Ok(total_size)
    }

    /// Remove a model from storage
    pub fn remove_model(&self, model_id: &str) -> Result<()> {
        let model_path = self.location.models_dir.join(sanitize_model_id(model_id));
        let metadata_path = self.metadata_path(model_id);

        if model_path.exists() {
            std::fs::remove_dir_all(model_path)?;
        }

        if metadata_path.exists() {
            std::fs::remove_file(metadata_path)?;
        }

        info!("Removed model: {}", model_id);
        Ok(())
    }

    /// Create model directory structure
    pub fn create_model_directory(&self, model_id: &str) -> Result<PathBuf> {
        let model_dir = self.location.models_dir.join(sanitize_model_id(model_id));
        std::fs::create_dir_all(&model_dir)?;
        Ok(model_dir)
    }

    /// Where a model's files live, without creating anything.
    ///
    /// `create_model_directory` answers the same question but has a side
    /// effect, which makes it wrong for callers that only want to measure or
    /// delete - disk accounting must not conjure empty directories for models
    /// that were never installed. Uses the same `sanitize_model_id` mapping as
    /// `create_model_directory` and `remove_model`, so all three agree on the
    /// path for a given id.
    pub fn model_directory(&self, model_id: &str) -> PathBuf {
        self.location.models_dir.join(sanitize_model_id(model_id))
    }

    /// Get available disk space (cross-platform)
    pub fn get_available_space(&self) -> Result<u64> {
        let target_path = if self.location.models_dir.exists() {
            self.location.models_dir.clone()
        } else {
            self.location.app_data_dir.clone()
        };
        let space = fs2::available_space(&target_path)?;
        Ok(space)
    }
}

/// Model metadata structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub size_bytes: u64,
    pub format: String,
    pub download_source: String,
    pub download_date: chrono::DateTime<chrono::Utc>,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    pub tags: Vec<String>,
    pub hardware_requirements: HardwareRequirements,
    pub compatibility_notes: Option<String>,
    /// Platform-specific runtime binary paths
    #[serde(default)]
    pub runtime_binaries: std::collections::HashMap<String, std::path::PathBuf>,
    /// Main model filename inside the model directory. Written at install
    /// time so restarts (which rebuild the registry from this metadata) keep
    /// an exact file reference instead of falling back to a directory scan.
    /// Serde-defaulted: metadata written before this field existed loads fine.
    #[serde(default)]
    pub filename: Option<String>,
    /// Multimodal projector filename (vision models only). `Some` is the
    /// durable, restart-surviving record that this installed model is
    /// vision-capable and which file to pass to llama-server via --mmproj.
    #[serde(default)]
    pub mmproj_filename: Option<String>,
    /// Size of the mmproj file in bytes (0 = unknown/not vision).
    #[serde(default)]
    pub mmproj_size_bytes: u64,
}

/// Hardware requirements for a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareRequirements {
    pub min_ram_gb: f32,
    pub min_vram_gb: Option<f32>,
    pub recommended_ram_gb: f32,
    pub recommended_vram_gb: Option<f32>,
    pub cpu_cores: Option<u32>,
    pub gpu_required: bool,
}

impl Default for HardwareRequirements {
    fn default() -> Self {
        Self {
            min_ram_gb: 4.0,
            min_vram_gb: None,
            recommended_ram_gb: 8.0,
            recommended_vram_gb: None,
            cpu_cores: None,
            gpu_required: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_storage_creation() -> Result<()> {
        // Create a temporary directory for testing
        let temp_dir = TempDir::new()?;
        let test_base_dir = temp_dir.path().join("test_offline_intelligence");

        // Manually create the directory structure (simulating what ModelStorage::new() does)
        let app_data_dir = test_base_dir.join("app_data");
        let models_dir = app_data_dir.join("models");
        let registry_dir = app_data_dir.join("registry");

        std::fs::create_dir_all(&app_data_dir)?;
        std::fs::create_dir_all(&models_dir)?;
        std::fs::create_dir_all(&registry_dir)?;

        // Create the storage struct with the pre-created directories
        let storage = ModelStorage {
            location: StorageLocation {
                app_data_dir,
                models_dir,
                registry_dir,
            },
        };

        // Test directory creation
        assert!(storage.location.app_data_dir.exists());
        assert!(storage.location.models_dir.exists());
        assert!(storage.location.registry_dir.exists());

        Ok(())
    }

    #[test]
    fn test_model_path_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let storage = ModelStorage {
            location: StorageLocation {
                app_data_dir: temp_dir.path().to_path_buf(),
                models_dir: temp_dir.path().join("models"),
                registry_dir: temp_dir.path().join("registry"),
            },
        };

        let path = storage.model_path("test-model", "model.gguf");
        assert_eq!(
            path,
            temp_dir
                .path()
                .join("models")
                .join("test-model")
                .join("model.gguf")
        );

        Ok(())
    }
}
