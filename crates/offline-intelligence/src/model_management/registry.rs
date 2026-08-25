//! Model Registry
//!
//! Manages model metadata, tracks installed models, and provides
//! querying capabilities for available models.

use super::storage::{ModelStorage, ModelMetadata};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::recommendation::{HardwareProfile, ModelRecommender};
use reqwest::Client;

/// Status of a model in the registry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelStatus {
    /// Model is available locally
    Installed,
    /// Model is being downloaded
    Downloading,
    /// Model is available for download
    Available,
    /// Model had an error during download/installation
    Error(String),
}

/// Information about a model in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub status: ModelStatus,
    pub size_bytes: u64,
    pub format: String,
    pub download_source: Option<String>,
    /// Specific filename to download (for HuggingFace models with non-standard naming)
    #[serde(default)]
    pub filename: Option<String>,
    pub installed_version: Option<String>,
    pub last_updated: Option<chrono::DateTime<chrono::Utc>>,
    pub tags: Vec<String>,
    pub compatibility_score: Option<f32>, // 0.0 to 1.0 based on hardware match
    /// Parameter count string (e.g., "7B", "70B", "671B")
    #[serde(default)]
    pub parameters: Option<String>,
    /// Context length in tokens
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Provider name
    #[serde(default)]
    pub provider: Option<String>,
    /// Total number of shards for sharded models (None for single-file models)
    #[serde(default)]
    pub total_shards: Option<u32>,
    /// List of all shard filenames for sharded models
    #[serde(default)]
    pub shard_filenames: Vec<String>,
    /// Download count (for HuggingFace models)
    #[serde(default)]
    pub downloads: u64,
    /// Multimodal projector (mmproj) filename for VISION-capable models.
    /// `Some` marks this as a vision model: installing it downloads this file
    /// alongside the main GGUF, and activating it passes the file to
    /// llama-server via --mmproj. `None` = text-only, behaviour unchanged.
    /// Serde-defaulted so registry JSON written before this field existed
    /// keeps loading.
    #[serde(default)]
    pub mmproj_filename: Option<String>,
    /// Size of the mmproj file in bytes (0 = unknown). Counted into download
    /// totals and disk accounting alongside size_bytes.
    #[serde(default)]
    pub mmproj_size_bytes: u64,
}

impl ModelInfo {
    /// Whether this model is vision-capable (carries a multimodal projector).
    pub fn is_vision(&self) -> bool {
        self.mmproj_filename.is_some()
    }
}

/// Model registry manager
pub struct ModelRegistry {
    storage: Arc<ModelStorage>,
    models: HashMap<String, ModelInfo>,
    known_sources: Vec<ModelSource>,
}

/// Source where models can be found
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSource {
    pub name: String,
    pub url: String,
    pub api_type: SourceType,
}

/// Type of model source API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SourceType {
    HuggingFace,
}

/// Hugging Face API model response
#[derive(Debug, Deserialize)]
struct HuggingFaceModel {
    id: String,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    author: Option<String>,
    downloads: Option<u64>,
    /// Gated status: false, "auto", or "manual". Gated models require user approval.
    #[serde(default)]
    gated: Option<serde_json::Value>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    siblings: Vec<HuggingFaceSibling>,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
}

impl ModelRegistry {
    pub fn new(storage: Arc<ModelStorage>) -> Result<Self> {
        let mut registry = Self {
            storage,
            models: HashMap::new(),
            known_sources: vec![
                ModelSource {
                    name: "Hugging Face".to_string(),
                    url: "https://huggingface.co".to_string(),
                    api_type: SourceType::HuggingFace,
                },
            ],
        };

        // Load existing registry data
        registry.load_registry()?;

        // Populate default catalog (only adds models not already present)
        registry.populate_default_catalog();

        Ok(registry)
    }

    /// Refresh the Hugging Face GGUF/GGML model catalog from the HF Hub API.
    /// Fetches top models by downloads and extracts available quantized files.
    pub async fn refresh_huggingface_catalog_from_api(
        &mut self,
        limit: usize,
    ) -> Result<()> {
        let client = Client::new();

        // Fetch GGUF models sorted by downloads
        let url = format!(
            "https://huggingface.co/api/models?filter=gguf&sort=downloads&direction=-1&limit={}&full=true",
            limit
        );

        let resp = client
            .get(&url)
            .header("User-Agent", "Offline_Intelligence-Desktop/1.0")
            .send()
            .await
            .context("Failed to call Hugging Face models API")?;

        let resp = resp
            .error_for_status()
            .context("Hugging Face API returned error status")?;

        let models: Vec<HuggingFaceModel> = resp
            .json()
            .await
            .context("Failed to parse Hugging Face models response")?;

        let mut hf_ids: HashSet<String> = HashSet::new();

        for m in models.into_iter() {
            let repo_id = m.model_id.as_ref().unwrap_or(&m.id).clone();

            // Skip gated models that require user approval on HuggingFace
            let is_gated = match &m.gated {
                Some(serde_json::Value::Bool(false)) | None => false,
                _ => true, // "auto", "manual", true all count as gated
            };
            if is_gated {
                debug!("Skipping gated model: {}", repo_id);
                continue;
            }

            // Find GGUF files in siblings, excluding multimodal projector files
            // (mmproj files are vision adapters, not loadable language models)
            let gguf_files: Vec<&HuggingFaceSibling> = m
                .siblings
                .iter()
                .filter(|s| {
                    (s.rfilename.ends_with(".gguf") || s.rfilename.ends_with(".ggml"))
                        && !s.rfilename.to_lowercase().contains("mmproj")
                })
                .collect();

            // Multimodal projector siblings: their presence marks this repo as
            // a VISION model. The projector is recorded on the entry so the
            // installer downloads it alongside the main GGUF and activation
            // passes it via --mmproj.
            let mmproj_files: Vec<&HuggingFaceSibling> = m
                .siblings
                .iter()
                .filter(|s| {
                    s.rfilename.ends_with(".gguf")
                        && s.rfilename.to_lowercase().contains("mmproj")
                })
                .collect();

            if gguf_files.is_empty() {
                continue;
            }

            // Check if this is a sharded model by looking for shard patterns
            let mut sharded_model_info = None;
            for file in &gguf_files {
                if let Some(total_shards) = self.detect_shard_pattern_internal(&file.rfilename) {
                    // This is a sharded model, collect all shards
                    let all_shards = self.collect_shards_internal(&gguf_files, total_shards);
                    
                    // Calculate total size
                    let total_size = all_shards.iter()
                        .map(|s| s.size.unwrap_or(0))
                        .sum();
                    
                    let registry_id = repo_id.clone();
                    hf_ids.insert(registry_id.clone());

                    // Determine format from first shard
                    let format = if file.rfilename.ends_with(".gguf") {
                        "gguf"
                    } else {
                        "ggml"
                    }
                    .to_string();

                    // Build tags from HF tags + our own
                    let mut tags: Vec<String> = m
                        .tags
                        .iter()
                        .filter(|t| !t.is_empty() && *t != "gguf" && *t != "ggml")
                        .take(5)
                        .cloned()
                        .collect();
                    tags.push("offline".to_string());
                    tags.push(format.clone());
                    tags.push("sharded".to_string()); // Add sharded tag

                    // Derive a friendly name from repo_id
                    let name = repo_id
                        .split('/')
                        .last()
                        .unwrap_or(&repo_id)
                        .replace("-GGUF", "")
                        .replace("-gguf", "");

                    // Extract parameter count from name
                    let parameters = {
                        let name_str = &name;
                        let re = regex::Regex::new(r"(\d+(?:\.\d+)?(?:x\d+)?[BMK])").ok();
                        re.and_then(|r| r.find(name_str).map(|m| m.as_str().to_string()))
                    };

                    sharded_model_info = Some(ModelInfo {
                        id: registry_id.clone(),
                        name,
                        description: Some(format!("Sharded GGUF model from {} ({} parts)", repo_id, total_shards)),
                        author: m.author.clone(),
                        status: ModelStatus::Available,
                        size_bytes: total_size,
                        format,
                        download_source: Some("huggingface".to_string()),
                        filename: Some(file.rfilename.clone()), // Store the first shard as the primary filename
                        installed_version: None,
                        last_updated: None,
                        tags,
                        compatibility_score: None,
                        parameters,
                        context_length: None,
                        provider: None,
                        total_shards: Some(total_shards),
                        shard_filenames: all_shards.iter().map(|s| s.rfilename.clone()).collect(),
                        downloads: m.downloads.unwrap_or(0),
                        // Sharded vision GGUFs are rare and the shard download
                        // path has no projector handling — not detected here.
                        mmproj_filename: None,
                        mmproj_size_bytes: 0,
                    });
                    break; // Found sharded model, no need to check other files
                }
            }

            // If we found a sharded model, use that info; otherwise use the preferred single file
            let model_info = if let Some(sharded_info) = sharded_model_info {
                sharded_info
            } else {
                // Prefer Q4_K_M, Q5_K_M, Q6_K, Q8_0, IQ_X_X quantizations (good balance of size/quality)
                let preferred_file = gguf_files
                    .iter()
                    .find(|f| f.rfilename.contains("Q4_K_M"))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q5_K_M")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q6_K")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q8_0")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("IQ3_XXS")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("IQ3_S")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("IQ4_NL")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("IQ4_XS")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q3_K_S")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q3_K_M")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q3_K_L")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q5_K_S")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q5_K_L")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q2_K")))
                    .or_else(|| gguf_files.iter().find(|f| f.rfilename.contains("Q2_K_S")))
                    .or_else(|| gguf_files.first())
                    .copied();

                let Some(file) = preferred_file else {
                    continue;
                };

                let registry_id = repo_id.clone();
                hf_ids.insert(registry_id.clone());

                // Determine format from filename
                let format = if file.rfilename.ends_with(".gguf") {
                    "gguf"
                } else {
                    "ggml"
                }
                .to_string();

                // Build tags from HF tags + our own
                let mut tags: Vec<String> = m
                    .tags
                    .iter()
                    .filter(|t| !t.is_empty() && *t != "gguf" && *t != "ggml")
                    .take(5)
                    .cloned()
                    .collect();
                tags.push("offline".to_string());
                tags.push(format.clone());

                // Pick the projector variant: prefer full-precision (f32/f16)
                // — projectors are small, and quantizing them costs OCR/vision
                // quality for negligible disk savings — else take the first.
                let mmproj = Self::pick_mmproj_file(&mmproj_files);
                if mmproj.is_some() && !tags.iter().any(|t| t == "vision") {
                    tags.push("vision".to_string());
                }

                // Derive a friendly name from repo_id
                let name = repo_id
                    .split('/')
                    .last()
                    .unwrap_or(&repo_id)
                    .replace("-GGUF", "")
                    .replace("-gguf", "");

                // Extract parameter count from name
                let parameters = {
                    let name_str = &name;
                    let re = regex::Regex::new(r"(\d+(?:\.\d+)?(?:x\d+)?[BMK])").ok();
                    re.and_then(|r| r.find(name_str).map(|m| m.as_str().to_string()))
                };

                ModelInfo {
                    id: registry_id.clone(),
                    name,
                    description: Some(format!("GGUF model from {}", repo_id)),
                    author: m.author.clone(),
                    status: ModelStatus::Available,
                    size_bytes: file.size.unwrap_or(0),
                    format,
                    download_source: Some("huggingface".to_string()),
                    filename: Some(file.rfilename.clone()),
                    installed_version: None,
                    last_updated: None,
                    tags,
                    compatibility_score: None,
                    parameters,
                    context_length: None,
                    provider: None,
                    total_shards: None,
                    shard_filenames: vec![],
                    downloads: m.downloads.unwrap_or(0),
                    mmproj_filename: mmproj.map(|f| f.rfilename.clone()),
                    mmproj_size_bytes: mmproj.and_then(|f| f.size).unwrap_or(0),
                }
            };

            // Insert or update
            self.models
                .entry(model_info.id.clone())
                .and_modify(|existing| {
                    // Don't overwrite installed models
                    if existing.status != ModelStatus::Installed {
                        existing.name = model_info.name.clone();
                        existing.description = model_info.description.clone();
                        existing.author = model_info.author.clone();
                        existing.size_bytes = model_info.size_bytes;
                        existing.format = model_info.format.clone();
                        existing.download_source = model_info.download_source.clone();
                        existing.filename = model_info.filename.clone();
                        existing.tags = model_info.tags.clone();
                        existing.total_shards = model_info.total_shards;
                        existing.shard_filenames = model_info.shard_filenames.clone();
                        existing.mmproj_filename = model_info.mmproj_filename.clone();
                        existing.mmproj_size_bytes = model_info.mmproj_size_bytes;
                    }
                })
                .or_insert(model_info);
        }

        info!(
            "Refreshed Hugging Face catalog, now tracking {} GGUF/GGML models",
            hf_ids.len()
        );

        Ok(())
    }

    /// Choose which multimodal projector file to pair with a vision model.
    /// Full-precision projectors (f32/f16/bf16) are preferred — they are small
    /// relative to the main model, and projector quantization measurably hurts
    /// image understanding. Falls back to the first listed.
    fn pick_mmproj_file<'a>(
        mmproj_files: &[&'a HuggingFaceSibling],
    ) -> Option<&'a HuggingFaceSibling> {
        mmproj_files
            .iter()
            .find(|f| {
                let lower = f.rfilename.to_lowercase();
                lower.contains("f32") || lower.contains("f16") || lower.contains("bf16")
            })
            .or_else(|| mmproj_files.first())
            .copied()
    }

    /// Recompute compatibility scores for all known local models based on
    /// the current hardware profile and user preferences. This is used by
    /// the model manager to support "Best Match" sorting in the UI.
    pub fn update_compatibility_scores(
        &mut self,
        recommender: &ModelRecommender,
        hardware: &HardwareProfile,
    ) {
        for model in self.models.values_mut() {
            // Only score local models (GGUF/GGML) — these are constrained by local hardware.
            let is_local_format = model.format.eq_ignore_ascii_case("gguf")
                || model.format.eq_ignore_ascii_case("ggml");

            if is_local_format {
                let score = recommender.score_model_compatibility(model, hardware);
                model.compatibility_score = Some(score);
            }
        }
    }

    /// Load registry data from persistent storage
    fn load_registry(&mut self) -> Result<()> {
        let registry_path = self.storage.location.registry_dir.join("registry.json");
        if registry_path.exists() {
            match std::fs::read_to_string(&registry_path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<HashMap<String, ModelInfo>>(&content) {
                        Ok(saved_models) => {
                            self.models = saved_models;
                            info!("Loaded {} models from registry", self.models.len());
                        }
                        Err(e) => {
                            warn!("Registry file corrupted, starting fresh: {}", e);
                        }
                    }
                }
                Ok(_) => {
                    debug!("Registry file is empty, starting fresh");
                }
                Err(e) => {
                    warn!("Failed to read registry file: {}", e);
                }
            }
        }
        Ok(())
    }

    /// Scan local storage for existing models and populate registry
    pub async fn scan_storage(&mut self) -> Result<()> {
        let model_ids = self.storage.list_models()?;
        
        for model_id in model_ids {
            if let Some(metadata) = self.load_model_metadata(&model_id).await? {
                let model_info = ModelInfo {
                    id: model_id.clone(),
                    name: metadata.name,
                    description: metadata.description,
                    author: metadata.author,
                    status: ModelStatus::Installed,
                    size_bytes: metadata.size_bytes,
                    format: metadata.format,
                    download_source: Some(metadata.download_source),
                    // Exact file references from install-time metadata. The
                    // mmproj is what keeps a vision model vision-capable
                    // across restarts — without it activation would silently
                    // start text-only.
                    filename: metadata.filename,
                    installed_version: None, // Version extracted from model metadata
                    last_updated: Some(metadata.download_date),
                    tags: metadata.tags,
                    compatibility_score: None, // Will be calculated on demand
                    parameters: None,
                    context_length: None,
                    provider: None,
                    total_shards: None,
                    shard_filenames: vec![],
                    downloads: 0,
                    mmproj_filename: metadata.mmproj_filename,
                    mmproj_size_bytes: metadata.mmproj_size_bytes,
                };
                
                self.models.insert(model_id, model_info);
            }
        }
        
        info!("Scanned storage and found {} models", self.models.len());
        Ok(())
    }

    /// Load metadata for a specific model
    async fn load_model_metadata(&self, model_id: &str) -> Result<Option<ModelMetadata>> {
        let metadata_path = self.storage.metadata_path(model_id);
        
        if metadata_path.exists() {
            let content = tokio::fs::read_to_string(&metadata_path).await?;
            let metadata: ModelMetadata = serde_json::from_str(&content)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    }

    /// Update model status based on file existence
    pub async fn update_model_status_from_storage(&mut self, model_id: &str) -> Result<()> {
        if let Some(model_info) = self.models.get_mut(model_id) {
            let model_exists = self.storage.model_exists(model_id);
            
            if model_exists {
                model_info.status = ModelStatus::Installed;
            } else {
                // If it was installed but no longer exists, mark as available (downloadable)
                if matches!(model_info.status, ModelStatus::Installed) {
                    model_info.status = ModelStatus::Available;
                }
            }
        }
        
        Ok(())
    }

    /// Update all model statuses based on file existence in storage
    pub async fn update_all_model_statuses_from_storage(&mut self) -> Result<()> {
        let model_ids: Vec<String> = self.models.keys().cloned().collect();
        
        for model_id in model_ids {
            self.update_model_status_from_storage(&model_id).await?;
        }
        
        Ok(())
    }

    /// Get the path of an installed model by ID
    pub fn get_installed_model_path(&self, model_id: &str) -> Option<std::path::PathBuf> {
        let model_info = self.models.get(model_id)?;
        if model_info.status != ModelStatus::Installed {
            return None;
        }
        
        // Try to get the filename from model_info, otherwise look for any model file in the directory
        if let Some(filename) = &model_info.filename {
            return Some(self.storage.model_path(model_id, filename));
        }
        
        // Look for any model file in the directory
        let temp_path = self.storage.model_path(model_id, "dummy");
        let model_dir = match temp_path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => return None,
        };
        if !model_dir.exists() {
            return None;
        }
        
        if let Ok(entries) = std::fs::read_dir(&model_dir) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_file() {
                        let path = entry.path();
                        let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
                        if matches!(ext.as_str(), "gguf" | "bin" | "ggml" | "onnx" | "trt" | "engine" | "safetensors" | "mlmodel") {
                            return Some(path);
                        }
                    }
                }
            }
        }
        
        None
    }
    
    /// Get the complete model metadata including runtime binaries information
    pub async fn get_model_metadata(&self, model_id: &str) -> Option<ModelMetadata> {
        match self.load_model_metadata(model_id).await {
            Ok(Some(metadata)) => Some(metadata),
            _ => None,
        }
    }

    /// Add a model to the registry
    pub fn add_model(&mut self, model_info: ModelInfo) {
        self.models.insert(model_info.id.clone(), model_info);
    }

    /// Get model information by ID
    pub fn get_model(&self, model_id: &str) -> Option<&ModelInfo> {
        self.models.get(model_id)
    }

    /// Get mutable reference to model information
    pub fn get_model_mut(&mut self, model_id: &str) -> Option<&mut ModelInfo> {
        self.models.get_mut(model_id)
    }

    /// List all models in registry
    pub fn list_models(&self) -> Vec<&ModelInfo> {
        self.models.values().collect()
    }

    /// List models by status
    pub fn list_models_by_status(&self, status: ModelStatus) -> Vec<&ModelInfo> {
        self.models.values()
            .filter(|model| model.status == status)
            .collect()
    }

    /// Search models by name or tags
    pub fn search_models(&self, query: &str) -> Vec<&ModelInfo> {
        let query_lower = query.to_lowercase();
        self.models.values()
            .filter(|model| {
                model.name.to_lowercase().contains(&query_lower) ||
                model.description.as_ref().map_or(false, |desc| desc.to_lowercase().contains(&query_lower)) ||
                model.tags.iter().any(|tag| tag.to_lowercase().contains(&query_lower))
            })
            .collect()
    }

    /// Get models sorted by compatibility score for current hardware
    pub fn get_recommended_models(&self, max_results: usize) -> Vec<&ModelInfo> {
        let mut models: Vec<_> = self.models.values().collect();
        models.sort_by(|a, b| {
            b.compatibility_score.unwrap_or(0.0)
                .partial_cmp(&a.compatibility_score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        models.truncate(max_results);
        models
    }

    /// Update model status
    pub fn update_model_status(&mut self, model_id: &str, status: ModelStatus) {
        if let Some(model) = self.models.get_mut(model_id) {
            model.status = status;
        }
    }

    /// Remove a model from registry
    pub fn remove_model(&mut self, model_id: &str) -> bool {
        self.models.remove(model_id).is_some()
    }

    /// Get registry statistics
    pub fn get_statistics(&self) -> RegistryStats {
        let mut stats = RegistryStats::default();
        
        for model in self.models.values() {
            match model.status {
                ModelStatus::Installed => stats.installed_count += 1,
                ModelStatus::Downloading => stats.downloading_count += 1,
                ModelStatus::Available => stats.available_count += 1,
                ModelStatus::Error(_) => stats.error_count += 1,
            }
            stats.total_size_bytes += model.size_bytes;
        }
        
        stats
    }

    /// Get models by category/tags
    pub fn get_models_by_category(&self, category: &str) -> Vec<&ModelInfo> {
        self.models.values()
            .filter(|model| {
                model.tags.iter().any(|tag| 
                    tag.to_lowercase().contains(&category.to_lowercase())
                )
            })
            .collect()
    }

    /// Get trending models (recently added or popular tags)
    pub fn get_trending_models(&self, limit: usize) -> Vec<&ModelInfo> {
        let mut models: Vec<&ModelInfo> = self.models.values()
            .filter(|model| {
                // Filter for popular models (based on certain tags)
                model.tags.iter().any(|tag| 
                    tag == "popular" || tag == "trending" || tag == "featured"
                )
            })
            .collect();
        
        // Sort by some criteria (e.g., size as proxy for popularity, or by name)
        models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        models.truncate(limit);
        models
    }

    /// Get models by task type (e.g., "chat", "coding", "text-generation")
    pub fn get_models_by_task(&self, task: &str) -> Vec<&ModelInfo> {
        self.models.values()
            .filter(|model| {
                // Look in name, description and tags for the task
                model.name.to_lowercase().contains(&task.to_lowercase()) ||
                model.description.as_ref().map_or(false, |desc| 
                    desc.to_lowercase().contains(&task.to_lowercase())) ||
                model.tags.iter().any(|tag| 
                    tag.to_lowercase().contains(&task.to_lowercase()))
            })
            .collect()
    }

    /// Save registry to persistent storage
    pub async fn save_registry(&self) -> Result<()> {
        let registry_path = self.storage.location.registry_dir.join("registry.json");
        let content = serde_json::to_string_pretty(&self.models)
            .context("Failed to serialize registry")?;
        tokio::fs::write(&registry_path, content).await
            .context("Failed to write registry file")?;
        debug!("Saved {} models to registry", self.models.len());
        Ok(())
    }

    /// Populate the registry with well-known models from all sources.
    /// Only adds models that are not already in the registry.
    /// Also removes stale models that are no longer available.
    pub fn populate_default_catalog(&mut self) {
        let catalog = Self::get_default_catalog();
        // Remove stale models: Ollama and OpenRouter models (functionality removed)
        let stale_ids: Vec<String> = self.models.iter()
            .filter(|(_id, m)| {
                m.download_source.as_deref() == Some("ollama")
                || m.download_source.as_deref() == Some("openrouter")
            })
            .map(|(_id, _)| _id.clone())
            .collect();
        for id in &stale_ids {
            self.models.remove(id);
        }
        if !stale_ids.is_empty() {
            info!("Removed {} stale/obsolete models from registry", stale_ids.len());
        }

        let mut added = 0;
        for model in catalog {
            match self.models.get_mut(&model.id) {
                None => {
                    self.models.insert(model.id.clone(), model);
                    added += 1;
                }
                Some(existing) => {
                    // A registry entry for this repo may pre-date vision
                    // support (written before mmproj fields existed). Repair
                    // the pairing on non-installed entries so the curated
                    // vision pairing is never silently lost. Installed
                    // entries are left alone — their on-disk metadata is the
                    // source of truth.
                    if existing.status != ModelStatus::Installed
                        && existing.mmproj_filename.is_none()
                        && model.mmproj_filename.is_some()
                    {
                        existing.filename = model.filename.clone();
                        existing.mmproj_filename = model.mmproj_filename.clone();
                        existing.mmproj_size_bytes = model.mmproj_size_bytes;
                        if !existing.tags.iter().any(|t| t == "vision") {
                            existing.tags.push("vision".to_string());
                        }
                    }
                }
            }
        }
        if added > 0 {
            info!("Populated catalog with {} new available models", added);
        }
    }

    /// Returns the built-in catalog of well-known models.
    ///
    /// Text models load dynamically from the HF API; the entries here are the
    /// CURATED VISION models — proven mmproj pairings this product ships with
    /// so a working vision option always exists even before/without a catalog
    /// refresh. Filenames and sizes were verified against the HF API
    /// (api/models/<repo>/tree/main) on 2026-08-08 — never guessed.
    /// f16 projectors chosen deliberately: projector quantization measurably
    /// hurts image understanding for negligible disk savings.
    fn get_default_catalog() -> Vec<ModelInfo> {
        let vision_entry = |repo: &str,
                            name: &str,
                            params: &str,
                            main_file: &str,
                            main_size: u64,
                            mmproj_file: &str,
                            mmproj_size: u64,
                            description: &str| ModelInfo {
            id: repo.to_string(),
            name: name.to_string(),
            description: Some(description.to_string()),
            author: Some("ggml-org".to_string()),
            status: ModelStatus::Available,
            size_bytes: main_size,
            format: "gguf".to_string(),
            download_source: Some("huggingface".to_string()),
            filename: Some(main_file.to_string()),
            installed_version: None,
            last_updated: None,
            tags: vec![
                "vision".to_string(),
                "offline".to_string(),
                "gguf".to_string(),
                "curated".to_string(),
            ],
            compatibility_score: None,
            parameters: Some(params.to_string()),
            context_length: Some(128000),
            provider: None,
            total_shards: None,
            shard_filenames: vec![],
            downloads: 0,
            mmproj_filename: Some(mmproj_file.to_string()),
            mmproj_size_bytes: mmproj_size,
        };

        vec![
            vision_entry(
                "ggml-org/Qwen2.5-VL-3B-Instruct-GGUF",
                "Qwen2.5-VL-3B-Instruct",
                "3B",
                "Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf",
                1_929_901_056,
                "mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf",
                1_338_428_128,
                "Vision-capable model: reads text (including handwriting) from \
                 images and scanned documents. Lighter option for machines with \
                 limited VRAM/RAM.",
            ),
            vision_entry(
                "ggml-org/Qwen2.5-VL-7B-Instruct-GGUF",
                "Qwen2.5-VL-7B-Instruct",
                "7B",
                "Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf",
                4_683_072_032,
                "mmproj-Qwen2.5-VL-7B-Instruct-f16.gguf",
                1_354_162_912,
                "Vision-capable model: reads text (including handwriting) from \
                 images and scanned documents. Stronger recognition quality; \
                 needs more VRAM/RAM than the 3B.",
            ),
        ]
    }

    /// Detect if the filename follows a shard pattern (e.g., model-00001-of-00003.gguf)
    fn detect_shard_pattern_internal(&self, filename: &str) -> Option<u32> {
        // Pattern: some-name-00001-of-00003.ext
        let re = regex::Regex::new(r".*-(\d{5})-of-(\d{5})\.[^.]+$").ok()?;
        if let Some(caps) = re.captures(filename) {
            if let Some(total_str) = caps.get(2) {
                if let Ok(total) = total_str.as_str().parse::<u32>() {
                    return Some(total);
                }
            }
        }
        None
    }

    /// Collect all shards for a given total_shards number
    fn collect_shards_internal<'a>(&self, gguf_files: &[&'a HuggingFaceSibling], total_shards: u32) -> Vec<&'a HuggingFaceSibling> {
        let mut shards = Vec::new();
        
        // Find the pattern from one of the shard files
        if let Some(first_file) = gguf_files.iter().find(|f| self.detect_shard_pattern_internal(&f.rfilename).is_some()) {
            // Extract the pattern from the first file to find other shards
            if let Some(caps) = regex::Regex::new(r"(.*-)(\d{5})(-of-\d{5}\.[^.]+)$")
                .ok()
                .and_then(|re| re.captures(&first_file.rfilename)) {
                
                let prefix = caps[1].to_string();  // Owned string to avoid lifetime issues
                let suffix = caps[3].to_string();  // Owned string to avoid lifetime issues
                
                // Collect all expected shard files
                for i in 1..=total_shards {
                    let expected_filename = format!("{}{:05}{}", prefix, i, suffix);
                    if let Some(file) = gguf_files.iter().find(|f| f.rfilename == expected_filename) {
                        shards.push(*file);
                    }
                }
            }
        }
        
        shards
    }

}

/// Registry statistics
#[derive(Debug, Default)]
pub struct RegistryStats {
    pub installed_count: usize,
    pub downloading_count: usize,
    pub available_count: usize,
    pub error_count: usize,
    pub total_size_bytes: u64,
}

impl RegistryStats {
    pub fn total_models(&self) -> usize {
        self.installed_count + self.downloading_count + self.available_count + self.error_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_registry_creation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let storage = Arc::new(ModelStorage {
            location: super::super::storage::StorageLocation {
                app_data_dir: temp_dir.path().to_path_buf(),
                models_dir: temp_dir.path().join("models"),
                registry_dir: temp_dir.path().join("registry"),
            },
        });
        
        let registry = ModelRegistry::new(storage)?;
        // A fresh registry is not empty: the curated vision catalog ships
        // with the product so a working vision option always exists.
        let curated = ModelRegistry::get_default_catalog();
        assert_eq!(registry.models.len(), curated.len());
        for model in &curated {
            let entry = registry
                .get_model(&model.id)
                .expect("curated entry must be present in a fresh registry");
            assert!(
                entry.mmproj_filename.is_some(),
                "curated vision entry {} must carry its mmproj pairing",
                model.id
            );
            assert!(entry.is_vision());
            assert!(entry.tags.iter().any(|t| t == "vision"));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_model_addition_and_lookup() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let storage = Arc::new(ModelStorage {
            location: super::super::storage::StorageLocation {
                app_data_dir: temp_dir.path().to_path_buf(),
                models_dir: temp_dir.path().join("models"),
                registry_dir: temp_dir.path().join("registry"),
            },
        });
        
        let mut registry = ModelRegistry::new(storage)?;
        
        let model_info = ModelInfo {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            description: Some("A test model".to_string()),
            author: Some("Test Author".to_string()),
            status: ModelStatus::Available,
            size_bytes: 1024,
            format: "gguf".to_string(),
            download_source: Some("huggingface".to_string()),
            filename: None,
            installed_version: None,
            last_updated: None,
            tags: vec!["test".to_string()],
            compatibility_score: Some(0.8),
            parameters: None,
            context_length: None,
            provider: None,
            total_shards: None,
            shard_filenames: vec![],
            downloads: 0,
            mmproj_filename: None,
            mmproj_size_bytes: 0,
        };

        registry.add_model(model_info);
        // Curated vision entries + the one just added.
        assert_eq!(
            registry.models.len(),
            ModelRegistry::get_default_catalog().len() + 1
        );

        let retrieved = registry.get_model("test-model");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "Test Model");
        
        Ok(())
    }
}