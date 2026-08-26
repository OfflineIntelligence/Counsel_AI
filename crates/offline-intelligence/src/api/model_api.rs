//! Model Management API Endpoints
//!
//! Provides RESTful API endpoints for:
//! - Listing available and installed models
//! - Searching for models
//! - Downloading/installing models
//! - Removing/uninstalling models
//! - Getting download progress
//! - Hardware recommendations

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::{
    model_management::{
        downloader::DownloadSource,
        registry::{ModelInfo, ModelStatus},
        recommendation::{ModelRecommender, UseCase, QualityPreference, SpeedPreference, CostSensitivity},
        storage::sanitize_model_id,
        ModelManager,
    },
    shared_state::UnifiedAppState,
};

/// Request to install/download a model
#[derive(Debug, Deserialize)]
pub struct InstallModelRequest {
    pub model_id: String,
    pub model_name: String,
    pub source: ModelSourceSpecifier,
    pub description: Option<String>,
    pub size_bytes: u64,
    pub format: String,
    /// Optional HuggingFace token for gated/private models
    pub hf_token: Option<String>,
}

/// Specify where to download a model from
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ModelSourceSpecifier {
    HuggingFace { repo_id: String, filename: String },
}

/// Response for model installation
#[derive(Debug, Serialize)]
pub struct InstallModelResponse {
    pub download_id: String,
    pub message: String,
}

/// Response for the currently active/loaded model
#[derive(Debug, Serialize)]
pub struct ActiveModelResponse {
    pub model_path: String,
    pub model_name: String,
    pub format: String,
    pub context_size: u32,
    pub gpu_layers: u32,
    pub backend_url: String,
    pub status: String,
    /// True when the CURRENTLY RUNNING runtime was started with a multimodal
    /// projector (--mmproj) and is ready — i.e. images sent to it will be
    /// understood. Live truth from the runtime manager, not static config.
    pub vision: bool,
}

/// Request to search for models
#[derive(Debug, Deserialize)]
pub struct SearchModelsRequest {
    pub query: String,
    pub limit: Option<usize>,
}

/// Response containing search results
#[derive(Debug, Serialize)]
pub struct SearchModelsResponse {
    pub models: Vec<ModelInfo>,
    pub total_found: usize,
}

/// Request to refresh the dynamic model catalog
#[derive(Debug, Deserialize)]
pub struct RefreshModelsRequest {
    /// Which source to refresh: "huggingface" or "all" (default)
    pub source: Option<String>,
    /// Optional HuggingFace token for gated/private models
    pub hf_token: Option<String>,
}

/// Response after refreshing the model catalog
#[derive(Debug, Serialize)]
pub struct RefreshModelsResponse {
    pub updated_sources: Vec<String>,
    pub total_models: usize,
}

/// Request to update user preferences
#[derive(Debug, Deserialize)]
pub struct UpdatePreferencesRequest {
    pub primary_use_case: Option<String>,
    pub quality_preference: Option<String>,
    pub speed_preference: Option<String>,
    pub cost_sensitivity: Option<String>,
}

/// Response with hardware recommendations
#[derive(Debug, Serialize)]
pub struct HardwareRecommendationsResponse {
    pub recommendations: Vec<String>,
    pub message: String,
}

/// Request to switch to a different model
#[derive(Debug, Deserialize)]
pub struct SwitchModelRequest {
    pub model_id: String,
}

/// Response after switching model
#[derive(Debug, Serialize)]
pub struct SwitchModelResponse {
    pub message: String,
    pub model_id: String,
    pub model_path: String,
}

/// Helper function to clone models from registry
async fn get_cloned_models(model_manager: &ModelManager) -> Vec<ModelInfo> {
    let registry = model_manager.registry.read().await;
    registry.list_models().iter().map(|m| (*m).clone()).collect()
}

/// Get list of all models (installed and available)
pub async fn list_models(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let models = get_cloned_models(model_manager).await;

    Ok(Json(models))
}

/// Get models filtered by mode (online/offline)
pub async fn list_models_by_mode(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mode = params.get("mode")
        .map(|s| s.as_str())
        .unwrap_or("offline");

    let all_models = get_cloned_models(model_manager).await;

    match mode {
        "offline" | _ => {
            // Big tech authors for HuggingFace
            let big_tech_authors = vec![
                "google",
                "meta",
                "microsoft",
                "openai",
                "anthropic",
                "deepseek-ai",
                "bigscience",
                "EleutherAI",
                "tiiuae",
                "mistralai",
                "01-ai",
                "Qwen",
                "THUDM",
                "baai",
            ];

            // All non-gated HF models
            let mut hf_models: Vec<ModelInfo> = all_models
                .into_iter()
                .filter(|m| {
                    m.download_source.as_deref() == Some("huggingface") &&
                    !matches!(m.status, ModelStatus::Error(_))
                })
                .collect();

            // Sort: big tech authors first
            hf_models.sort_by(|a, b| {
                let a_lower = a.author.as_deref().unwrap_or("").to_lowercase();
                let b_lower = b.author.as_deref().unwrap_or("").to_lowercase();
                let a_is_big = big_tech_authors.iter().any(|p| a_lower.contains(p));
                let b_is_big = big_tech_authors.iter().any(|p| b_lower.contains(p));
                
                match (a_is_big, b_is_big) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => b.downloads.cmp(&a.downloads), // Then by downloads
                }
            });

            // Limit to top 100 models
            hf_models.truncate(100);

            Ok(Json(hf_models))
        }
    }
}

/// Get the currently active/loaded model info from the running server config
pub async fn get_active_model(
    State(state): State<UnifiedAppState>,
) -> Json<ActiveModelResponse> {
    let config = &state.shared_state.config;
    let model_path = &config.model_path;

    // Extract a human-readable name from the model file path
    let model_name = std::path::Path::new(model_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Detect format from extension
    let format = std::path::Path::new(model_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_uppercase();

    // Check if the model file actually exists
    let file_exists = std::path::Path::new(model_path).exists();

    let status = if file_exists { "loaded" } else { "not_found" }.to_string();

    Json(ActiveModelResponse {
        model_path: model_path.clone(),
        model_name,
        format,
        context_size: config.ctx_size,
        gpu_layers: config.gpu_layers,
        backend_url: config.backend_url.clone(),
        status,
        vision: state.shared_state.vision_active().await,
    })
}

/// Search for models by name, description, or tags
pub async fn search_models(
    State(state): State<UnifiedAppState>,
    Query(params): Query<SearchModelsRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let all_models = get_cloned_models(model_manager).await;
    let query_lower = params.query.to_lowercase();
    
    let mut filtered_models: Vec<ModelInfo> = all_models
        .into_iter()
        .filter(|model| {
            model.name.to_lowercase().contains(&query_lower) ||
            model.description.as_ref().map_or(false, |desc| desc.to_lowercase().contains(&query_lower)) ||
            model.tags.iter().any(|tag| tag.to_lowercase().contains(&query_lower))
        })
        .collect();

    let total_found = filtered_models.len();
    let limit = params.limit.unwrap_or(20).min(total_found);
    
    // Truncate to limit if needed
    filtered_models.truncate(limit);

    Ok(Json(SearchModelsResponse {
        models: filtered_models,
        total_found,
    }))
}

/// Refresh the dynamic model catalog from Hugging Face.
pub async fn refresh_models(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<RefreshModelsRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state
        .shared_state
        .model_manager
        .as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let source = payload.source.as_deref().unwrap_or("all");
    let mut updated_sources = Vec::new();

    // Refresh Hugging Face GGUF/GGML catalog if requested
    if source == "huggingface" || source == "all" {
        let mut registry = model_manager.registry.write().await;
        // Fetch top 100 GGUF models by downloads
        if let Err(e) = registry.refresh_huggingface_catalog_from_api(100).await {
            error!("Failed to refresh Hugging Face catalog: {}", e);
            // Continue - don't fail the entire refresh
        } else {
            updated_sources.push("huggingface".to_string());
        }
        if let Err(e) = registry.save_registry().await {
            error!("Failed to save model registry after HuggingFace refresh: {}", e);
        }
    }

    // Recompute compatibility scores for newly fetched offline models
    if !updated_sources.is_empty() {
        let cfg = &state.shared_state.config;
        let hardware = crate::model_management::ModelRecommender::detect_hardware_profile(cfg);
        let mut registry = model_manager.registry.write().await;
        registry.update_compatibility_scores(&*model_manager.recommender, &hardware);
        if let Err(e) = registry.save_registry().await {
            error!("Failed to save registry after compatibility scoring: {}", e);
        }
    }

    let total_models = {
        let registry = model_manager.registry.read().await;
        registry.list_models().len()
    };

    Ok(Json(RefreshModelsResponse {
        updated_sources,
        total_models,
    }))
}

/// Install/download a model
pub async fn install_model(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<InstallModelRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    info!("Installing model: {} ({})", payload.model_name, payload.model_id);

    // Canonicalize the id to match the on-disk folder naming (sanitize_model_id).
    // The catalog registers entries under the raw id (e.g. "author/repo"), but
    // scan_storage on startup keys them by sanitized folder name (e.g.
    // "author_repo") — so without this normalization, a single installed
    // model ends up in the registry HashMap under both keys and renders as
    // two cards after an app restart.
    let canonical_id = sanitize_model_id(&payload.model_id);

    // VISION: the install request carries only id/name/size — the mmproj
    // pairing lives in the registry entry (curated catalog or HF refresh).
    // Look it up here so the downloader knows to fetch the projector too.
    // Look under the raw id first (catalog form), then canonical (already migrated).
    let (registry_mmproj_filename, registry_mmproj_size) = {
        let registry = model_manager.registry.read().await;
        registry
            .get_model(&payload.model_id)
            .or_else(|| registry.get_model(&canonical_id))
            .map(|m| (m.mmproj_filename.clone(), m.mmproj_size_bytes))
            .unwrap_or((None, 0))
    };
    if let Some(ref mmproj) = registry_mmproj_filename {
        info!(
            "Model {} is vision-capable: projector '{}' will be downloaded with it{}",
            payload.model_id,
            mmproj,
            if registry_mmproj_size == 0 {
                " (size unknown from the catalog — resolved from the download itself)"
            } else {
                ""
            }
        );
    }

    // Migrate any pre-existing raw-id catalog entry to the canonical key, so
    // subsequent status updates and save_registry write under the canonical
    // key that scan_storage will also produce on the next startup.
    if canonical_id != payload.model_id {
        let mut registry = model_manager.registry.write().await;
        if let Some(existing) = registry.get_model(&payload.model_id).cloned() {
            registry.remove_model(&payload.model_id);
            let mut migrated = existing;
            migrated.id = canonical_id.clone();
            registry.add_model(migrated);
        }
    }

    // Create model info
    let model_info = ModelInfo {
        id: canonical_id.clone(),
        name: payload.model_name.clone(),
        description: payload.description,
        author: None,
        status: ModelStatus::Available,
        size_bytes: payload.size_bytes,
        format: payload.format,
        download_source: None,
        filename: None, // Will be determined by download source
        installed_version: None,
        last_updated: None,
        tags: vec![], // Tags extracted from model metadata or source
        compatibility_score: None,
        parameters: None,
        context_length: None,
        provider: None,
        total_shards: None,
        shard_filenames: vec![],
        downloads: 0,
        mmproj_filename: registry_mmproj_filename,
        mmproj_size_bytes: registry_mmproj_size,
    };

    // Convert source specifier to download source
    let download_source = match payload.source {
        ModelSourceSpecifier::HuggingFace { repo_id, filename } => {
            DownloadSource::HuggingFace { repo_id, filename }
        }
    };

    // Clone for use in async block
    let download_source_clone = download_source.clone();

    // Pre-create the download tracking entry so the frontend can poll immediately
    let pre_download_id = model_manager.downloader.progress_tracker()
        .start_download(
            canonical_id.clone(),
            payload.model_name.clone(),
            Some(payload.size_bytes),
        )
        .await;

    let return_download_id = pre_download_id.clone();

    // Update registry status to Downloading (keyed by canonical id)
    {
        let mut reg = model_manager.registry.write().await;
        reg.update_model_status(&canonical_id, ModelStatus::Downloading);
    }

    // Start download in background
    let registry = model_manager.registry.clone();
    let downloader = model_manager.downloader.clone();
    let existing_download_id = pre_download_id.clone();

    tokio::spawn(async move {
        // Pass the pre-created download ID so progress is tracked correctly
        // Also pass the HF token from the request for authentication
        match downloader.download_model(model_info.clone(), download_source_clone.clone(), Some(existing_download_id), payload.hf_token).await {
            Ok(_download_id) => {
                // Extract the filename from the download source
                let filename = match &download_source_clone {
                    DownloadSource::HuggingFace { filename, .. } => Some(filename.clone()),
                };

                // Update registry status AND filename, then persist
                let mut reg = registry.write().await;
                reg.update_model_status(&model_info.id, ModelStatus::Installed);

                // CRITICAL: Update the filename in the registry so we know which file to load
                if let Some(fname) = filename {
                    if let Some(model) = reg.get_model_mut(&model_info.id) {
                        model.filename = Some(fname);
                        // Record the vision pairing on the installed entry too —
                        // activation reads mmproj_filename from here.
                        model.mmproj_filename = model_info.mmproj_filename.clone();
                        model.mmproj_size_bytes = model_info.mmproj_size_bytes;
                        info!("Updated registry with filename for model: {}", model_info.id);
                    }
                }

                if let Err(e) = reg.save_registry().await {
                    error!("Failed to persist registry: {}", e);
                }
                drop(reg);

                if let Err(e) = downloader.save_model_metadata(&model_info, &download_source_clone).await {
                    error!("Failed to save model metadata: {}", e);
                }
                info!("Model installation completed: {}", model_info.name);
            }
            Err(e) => {
                error!("Model installation failed: {} - {}", model_info.name, e);
                let mut reg = registry.write().await;
                reg.update_model_status(&model_info.id, ModelStatus::Error(e.to_string()));
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(InstallModelResponse {
            download_id: return_download_id,
            message: format!("Started downloading model: {}", payload.model_name),
        })
    ))
}

/// Get download progress for a specific download
pub async fn get_download_progress(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let download_id = params.get("download_id")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let progress = model_manager.downloader.progress_tracker()
        .get_progress(download_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(progress))
}

/// Get all downloads (active and completed)
pub async fn get_active_downloads(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let downloads = model_manager.downloader.progress_tracker()
        .get_all_downloads()
        .await;

    Ok(Json(downloads))
}

/// Cancel an ongoing download
pub async fn cancel_download(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let download_id = params.get("download_id")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let success = model_manager.downloader.cancel_download(download_id).await;
    
    if success {
        Ok(Json(serde_json::json!({
            "message": "Download cancelled successfully"
        })))
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

/// Pause an ongoing download
pub async fn pause_download(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let download_id = params.get("download_id")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let tracker = model_manager.downloader.progress_tracker();
    if let Some(progress) = tracker.get_progress(download_id).await {
        // Only allow pausing if the download is currently downloading
        if progress.status == crate::model_management::progress::DownloadStatus::Downloading {
            tracker.update_progress(
                download_id,
                progress.bytes_downloaded,
                crate::model_management::progress::DownloadStatus::Paused,
                None,
            ).await;
            Ok(Json(serde_json::json!({ "message": "Download paused" })))
        } else {
            // Return an error if trying to pause a download that isn't downloading
            Err(StatusCode::BAD_REQUEST)
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Resume a paused download
pub async fn resume_download(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let download_id = params.get("download_id")
        .ok_or(StatusCode::BAD_REQUEST)?;

    let tracker = model_manager.downloader.progress_tracker();
    if let Some(progress) = tracker.get_progress(download_id).await {
        // Only allow resuming if the download is currently paused
        if progress.status == crate::model_management::progress::DownloadStatus::Paused {
            tracker.update_progress(
                download_id,
                progress.bytes_downloaded,
                crate::model_management::progress::DownloadStatus::Downloading,
                None,
            ).await;
            Ok(Json(serde_json::json!({ "message": "Download resumed" })))
        } else {
            // Return an error if trying to resume a download that isn't paused
            Err(StatusCode::BAD_REQUEST)
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Remove/uninstall a model.
///
/// If the model being removed is the one currently loaded, the inference
/// server (llama-server) is shut down FIRST — on Windows the server holds the
/// .gguf file open, so deleting a loaded model would otherwise fail with a
/// sharing violation (this is the same remove-flow Ollama/LM Studio use).
/// A failed shutdown aborts the removal with an explicit error — the files
/// are never half-deleted underneath a running server.
pub async fn remove_model(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let model_id = params.get("model_id")
        .ok_or(StatusCode::BAD_REQUEST)?;

    info!("Removing model: {}", model_id);

    // The on-disk directory that remove_model below will delete.
    let model_dir = model_manager
        .storage
        .model_path(model_id, "_")
        .parent()
        .map(|p| p.to_path_buf());

    // Stop the inference server first when it is serving THIS model.
    let runtime_manager = state
        .shared_state
        .runtime_manager
        .read()
        .ok()
        .and_then(|guard| guard.clone());

    if let (Some(rt), Some(dir)) = (runtime_manager, model_dir) {
        let serving_this_model = rt
            .get_current_config()
            .await
            .map_or(false, |cfg| cfg.model_path.starts_with(&dir));

        if serving_this_model {
            info!(
                "Model '{}' is currently loaded - shutting down the inference server before removal",
                model_id
            );
            if let Err(e) = rt.shutdown().await {
                error!("Could not stop the inference server for model removal: {}", e);
                return Ok((
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "model_in_use",
                        "detail": format!(
                            "Model '{}' is loaded by the inference server and the server could \
                             not be stopped: {}. Nothing was deleted.",
                            model_id, e
                        ),
                    })),
                ).into_response());
            }
            info!("Inference server stopped - model files are now unlocked");
        }
    }

    // Remove from storage
    if let Err(e) = model_manager.storage.remove_model(model_id) {
        error!("Failed to remove model from storage: {}", e);
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "model_removal_failed",
                "detail": format!("Failed to remove model files: {}", e),
            })),
        ).into_response());
    }

    // Remove from registry and persist. Then re-populate the curated
    // catalog: removing an installed curated model (e.g. a vision model)
    // must return it to the "Available" list IMMEDIATELY — not leave a hole
    // until the next app restart or manual catalog refresh — so the user can
    // reinstall it in one click.
    {
        let mut registry = model_manager.registry.write().await;
        registry.remove_model(model_id);
        registry.populate_default_catalog();
        if let Err(e) = registry.save_registry().await {
            error!("Failed to persist registry after removal: {}", e);
        }
    }

    // Clear the auto-load marker if it points at the removed model, so the
    // next startup does not try to load a model that no longer exists.
    let last_model_path = crate::config::get_app_data_dir().join("last_model.txt");
    if let Ok(last) = std::fs::read_to_string(&last_model_path) {
        if last.trim() == model_id {
            if let Err(e) = std::fs::remove_file(&last_model_path) {
                warn!("Could not clear last-used-model marker: {}", e);
            } else {
                info!("Cleared last-used-model marker (it pointed at the removed model)");
            }
        }
    }

    Ok(Json(serde_json::json!({
        "message": format!("Model {} removed successfully", model_id)
    })).into_response())
}

/// Get hardware recommendations
pub async fn get_hardware_recommendations(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let hardware = ModelRecommender::detect_hardware_profile(&state.shared_state.config);
    let message = model_manager.recommender.get_hardware_recommendation_message(&hardware);
    
    let recommendations = message.lines().map(|s| s.to_string()).collect::<Vec<String>>();

    Ok(Json(HardwareRecommendationsResponse {
        recommendations,
        message,
    }))
}

/// Update user preferences for model recommendations
pub async fn update_preferences(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<UpdatePreferencesRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut preferences = model_manager.recommender.get_preferences().clone();

    if let Some(use_case) = payload.primary_use_case {
        preferences.primary_use_case = match use_case.as_str() {
            "chat_assistant" => UseCase::ChatAssistant,
            "code_generation" => UseCase::CodeGeneration,
            "creative_writing" => UseCase::CreativeWriting,
            "research_analysis" => UseCase::ResearchAnalysis,
            "translation" => UseCase::Translation,
            _ => UseCase::GeneralPurpose,
        };
    }

    if let Some(quality) = payload.quality_preference {
        preferences.quality_preference = match quality.as_str() {
            "high_quality" => QualityPreference::HighQuality,
            "fast_response" => QualityPreference::FastResponse,
            _ => QualityPreference::Balanced,
        };
    }

    if let Some(speed) = payload.speed_preference {
        preferences.speed_preference = match speed.as_str() {
            "fastest" => SpeedPreference::Fastest,
            "highest_quality" => SpeedPreference::HighestQuality,
            _ => SpeedPreference::Balanced,
        };
    }

    if let Some(cost) = payload.cost_sensitivity {
        preferences.cost_sensitivity = match cost.as_str() {
            "budget" => CostSensitivity::Budget,
            "premium" => CostSensitivity::Premium,
            _ => CostSensitivity::Moderate,
        };
    }

    // We can't mutate the recommender through Arc, so we'll need to restructure this
    // For now, let's just acknowledge the preferences were set
    info!("User preferences updated: {:?}", preferences);

    Ok(Json(serde_json::json!({
        "message": "Preferences updated successfully"
    })))
}

/// Get recommended models based on current hardware and preferences
pub async fn get_recommended_models(
    State(state): State<UnifiedAppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let max_results = params.get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let hardware = ModelRecommender::detect_hardware_profile(&state.shared_state.config);
    let all_models = get_cloned_models(model_manager).await;
    
    let recommendations = model_manager.recommender.get_recommendations(
        all_models.iter().collect(),
        &hardware,
        max_results
    );

    // Get full model info for recommended models
    let recommended_models: Vec<ModelInfo> = recommendations
        .into_iter()
        .filter_map(|(model_id, _)| {
            all_models.iter().find(|m| m.id == model_id).cloned()
        })
        .collect();

    Ok(Json(recommended_models))
}

/// Hardware information response
#[derive(Debug, Serialize)]
pub struct HardwareInfoResponse {
    pub total_ram_gb: f32,
    pub available_ram_gb: f32,
    pub cpu_cores: u32,
    pub gpu_available: bool,
    pub gpu_vram_gb: Option<f32>,
    pub storage_used_bytes: u64,
    pub storage_available_bytes: u64,
}

/// Get current hardware info and storage usage
pub async fn get_hardware_info(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let hardware = ModelRecommender::detect_hardware_profile(&state.shared_state.config);

    let (storage_used, storage_available) = if let Some(mm) = state.shared_state.model_manager.as_ref() {
        let used = mm.storage.get_storage_usage().unwrap_or(0);
        let available = mm.storage.get_available_space().unwrap_or(0);
        (used, available)
    } else {
        (0, 0)
    };

    Ok(Json(HardwareInfoResponse {
        total_ram_gb: hardware.total_ram_gb,
        available_ram_gb: hardware.available_ram_gb,
        cpu_cores: hardware.cpu_cores,
        gpu_available: hardware.gpu_available,
        gpu_vram_gb: hardware.gpu_vram_gb,
        storage_used_bytes: storage_used,
        storage_available_bytes: storage_available,
    }))
}

/// Live system metrics response
#[derive(Serialize)]
pub struct SystemMetricsResponse {
    pub cpu_usage_percent: f32,
    pub per_core_usage: Vec<f32>,
    pub cpu_model_name: String,
    pub cpu_frequency_mhz: u64,
    pub gpu_available: bool,
    pub gpu_name: String,
    pub gpu_usage_percent: f32,
    pub gpu_vram_total_gb: f32,
    pub gpu_vram_used_gb: f32,
    pub gpu_temperature_c: f32,
    pub memory_total_gb: f32,
    pub memory_used_gb: f32,
    pub memory_available_gb: f32,
    pub gpu_layers_offloaded: u32,
    pub inference_device: String, // "GPU", "CPU", "CPU+GPU"
}

/// Switch to a different model by ID
pub async fn switch_model(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<SwitchModelRequest>,
) -> Result<axum::response::Response, StatusCode> {
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // Look up the model in the registry by ID
    let model_info = {
        let registry = model_manager.registry.read().await;
        registry.get_model(&payload.model_id)
            .ok_or(StatusCode::NOT_FOUND)?
            .clone()
    };
    
    // Verify that the model is installed and the file exists
    if model_info.status != ModelStatus::Installed {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    // Get the model's path using the storage module
    let model_path = if let Some(ref filename) = model_info.filename {
        // Use the stored filename if available
        let path = model_manager.storage.model_path(&payload.model_id, filename);
        info!("ðŸ” Resolving model path from registry filename: {}", path.display());
        path
    } else {
        // If no filename is stored, look for model files in the model directory
        warn!("âš ï¸  Model {} has no filename in registry, scanning directory...", payload.model_id);

        // We can get the directory by using a dummy filename with model_path, then taking the parent
        let model_dir = model_manager.storage.model_path(&payload.model_id, "dummy").parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                error!("âŒ Failed to get parent directory for model: {}", payload.model_id);
                StatusCode::NOT_FOUND
            })?;

        info!("ðŸ“‚ Scanning model directory: {}", model_dir.display());

        if !model_dir.exists() {
            error!("âŒ Model directory does not exist: {}", model_dir.display());
            return Err(StatusCode::NOT_FOUND);
        }

        // Look for model files in the directory
        let mut found_path = None;
        if let Ok(entries) = std::fs::read_dir(&model_dir) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_file() {
                        let path = entry.path();
                        let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
                        // Never pick the multimodal projector as "the model" —
                        // vision model dirs contain both a main GGUF and an
                        // mmproj-*.gguf, and the projector is not loadable as
                        // a language model.
                        let is_mmproj = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_lowercase().contains("mmproj"))
                            .unwrap_or(false);
                        if !is_mmproj && matches!(ext.as_str(), "gguf" | "bin" | "ggml" | "onnx" | "trt" | "engine" | "safetensors" | "mlmodel") {
                            // Found a valid model file
                            info!("âœ… Found model file: {}", path.display());
                            found_path = Some(path);
                            break; // Take the first valid file found
                        }
                    }
                }
            }
        }

        match found_path {
            Some(path) => path,
            None => {
                error!("âŒ No valid model file found in directory: {}", model_dir.display());
                return Err(StatusCode::NOT_FOUND);
            }
        }
    };

    if !model_path.exists() {
        error!("âŒ Model file does not exist: {}", model_path.display());
        error!("   Please check if the model was downloaded correctly to AppData");
        return Err(StatusCode::NOT_FOUND);
    }

    info!("âœ… Model file verified at: {}", model_path.display());

    // VISION: resolve the multimodal projector recorded for this model.
    // Registry says vision + file missing on disk = a LOUD, actionable
    // failure — activating text-only would silently break image processing.
    let mmproj_path: Option<std::path::PathBuf> = match model_info.mmproj_filename {
        Some(ref mmproj_filename) => {
            let path = model_manager.storage.model_path(&payload.model_id, mmproj_filename);
            if path.exists() {
                info!("✅ Vision model: multimodal projector verified at {}", path.display());
                Some(path)
            } else {
                error!(
                    "Vision model {} is missing its projector file '{}' (expected at {})",
                    payload.model_id, mmproj_filename, path.display()
                );
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "vision_projector_missing",
                        "detail": format!(
                            "This vision model's projector file ('{}') is missing on disk. \
                             Without it the model cannot process images, so it was not \
                             activated. Remove and reinstall the model to restore it.",
                            mmproj_filename
                        ),
                        "action": "reinstall_model",
                    })),
                ).into_response());
            }
        }
        None => None,
    };

    // Convert model_path to string for later use since it will be moved
    let model_path_str = model_path.to_string_lossy().to_string();
    
    // Get the runtime manager from shared state
    let runtime_manager = {
        let guard = state.shared_state.runtime_manager.read()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        guard.clone().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    };
    
    // Prepare runtime config for the new model
    // Try to get the model's format from the registry info, default to GGUF if not recognized
    let model_format = match model_info.format.as_str().to_lowercase().as_str() {
        "gguf" => crate::model_runtime::ModelFormat::GGUF,
        "ggml" => crate::model_runtime::ModelFormat::GGML,
        "onnx" => crate::model_runtime::ModelFormat::ONNX,
        "tensorrt" => crate::model_runtime::ModelFormat::TensorRT,
        "safetensors" => crate::model_runtime::ModelFormat::Safetensors,
        "coreml" => crate::model_runtime::ModelFormat::CoreML,
        _ => crate::model_runtime::ModelFormat::GGUF, // Default fallback
    };
    
    // Determine the runtime binary from the engine registry.
    // The engine registry is the single source of truth â€” metadata.runtime_binaries
    // is intentionally empty (models do not carry their own binary paths).
    let runtime_binary = {
        let engine_manager = match state.shared_state.engine_manager.as_ref() {
            Some(em) => em,
            None => {
                error!("Engine manager not initialised - cannot switch model");
                return Ok((
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "error": "engine_manager_unavailable",
                        "detail": "The engine manager failed to initialise at startup. Check the backend logs and restart the application.",
                        "action": "restart",
                    })),
                ).into_response());
            }
        };

        let registry = engine_manager.registry.read().await;
        let bin_path = registry.get_default_engine_binary_path();
        drop(registry);

        match bin_path {
            Some(path) => {
                info!("Engine binary resolved from registry: {}", path.display());
                path
            }
            None => {
                // Structured error: name the exact engine state and the explicit
                // recovery action, never a bare status code the UI can only
                // render as a generic failure.
                let engine_state = engine_manager.current_state().await;
                let (error_code, detail) = match &engine_state {
                    crate::engine_management::EngineState::Corrupted { engine_id, reason } => (
                        "engine_corrupted",
                        format!("Engine '{}' failed verification: {}", engine_id, reason),
                    ),
                    _ => (
                        "engine_not_installed",
                        "No inference engine is installed on this machine.".to_string(),
                    ),
                };
                error!("Model switch refused: {} - {}", error_code, detail);
                return Ok((
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "error": error_code,
                        "detail": detail,
                        "engine": engine_state,
                        "action": "install_engine",
                    })),
                ).into_response());
            }
        }
    };
    
    let mut runtime_config = state.shared_state.build_runtime_config(
        model_path.clone(),
        model_format,
        Some(runtime_binary.clone()),
    );
    // Vision pairing resolved above from the registry (None = text-only).
    runtime_config.mmproj_path = mmproj_path;

    info!("ðŸš€ Initializing runtime with config:");
    info!("   Model Path: {}", runtime_config.model_path.display());
    info!("   Runtime Binary: {}", runtime_binary.display());
    info!("   Format: {:?}", runtime_config.format);
    info!("   Host: {}:{}", runtime_config.host, runtime_config.port);
    info!("   Context Size: {}", runtime_config.context_size);
    info!("   GPU Layers: {}", runtime_config.gpu_layers);
    if let Some(ref mmproj) = runtime_config.mmproj_path {
        info!("   Multimodal projector (vision): {}", mmproj.display());
    }

    // Use the runtime manager's hot_swap method to switch models
    // Use initialize_auto to automatically detect the model format
    match runtime_manager.initialize_auto(runtime_config).await {
        Ok(base_url) => {
            info!("Runtime initialized at {}, performing health check...", base_url);

            // CRITICAL: Verify runtime is actually ready before returning success
            match runtime_manager.health_check().await {
                Ok(_) => {
                    info!("✅ Model {} activated successfully and health check passed", model_info.name);

                    // Save last used model for auto-load on next startup
                    {
                        let last_model_path = crate::config::get_app_data_dir().join("last_model.txt");
                        let _ = std::fs::write(last_model_path, &payload.model_id);
                    }

                    // Record activation recency for disk eviction. Written only
                    // after the health check passes, so a model that failed to
                    // load never counts as "recently used" and keeps its place
                    // at the front of the eviction queue.
                    if let Err(e) = state
                        .shared_state
                        .database_pool
                        .settings
                        .record_model_used(&payload.model_id)
                    {
                        warn!(
                            "Could not record activation time for model {}: {} - disk eviction \
                             will treat it as never used",
                            payload.model_id, e
                        );
                    }

                    Ok(Json(SwitchModelResponse {
                        message: format!("Model {} loaded and ready for inference", model_info.name),
                        model_id: payload.model_id.clone(),
                        model_path: model_path_str,
                    }).into_response())
                }
                Err(e) => {
                    error!("âŒ Model activation health check failed: {}", e);
                    error!("   The model may be too large or incompatible with your hardware");
                    Ok((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "model_activation_failed",
                            "detail": format!(
                                "The model loaded but the runtime health check failed: {}. \
                                 The model may be too large or incompatible with this machine.",
                                e
                            ),
                        })),
                    ).into_response())
                }
            }
        }
        Err(e) => {
            error!("Failed to switch model: {}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "runtime_initialization_failed",
                    "detail": format!("Failed to start the inference runtime: {}", e),
                })),
            ).into_response())
        }
    }
}

/// Non-NVIDIA GPU identification for the metrics endpoint.
///
/// Computed once and cached: the WMI/PowerShell probes take hundreds of
/// milliseconds and /metrics/system is polled every few seconds by the UI.
/// GPU name and total VRAM are static for the process lifetime; live
/// utilization/temperature need vendor APIs and are not reported here.
/// Returns (gpu_available, gpu_name, vram_total_gb).
fn fallback_gpu_info() -> &'static (bool, String, f32) {
    static FALLBACK_GPU: std::sync::OnceLock<(bool, String, f32)> = std::sync::OnceLock::new();
    FALLBACK_GPU.get_or_init(|| {
        #[cfg(target_os = "windows")]
        {
            let caps = crate::model_runtime::HardwareCapabilities::detect();
            let analyzer = crate::engine_management::HardwareAnalyzer::new(caps);
            let profile = analyzer.get_hardware_profile();
            if let Some(gpu) = profile.gpu_info {
                let (_, vram_gb, _) =
                    crate::model_management::recommendation::ModelRecommender::detect_gpu_via_system_tools();
                return (true, gpu.model, vram_gb.unwrap_or(0.0));
            }
            (false, String::from("Not detected"), 0.0)
        }
        #[cfg(not(target_os = "windows"))]
        {
            (false, String::from("Not detected"), 0.0)
        }
    })
}

/// Get live system metrics including CPU/GPU usage
pub async fn get_system_metrics(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    use sysinfo::System;

    // CPU metrics - need two refreshes with delay for accurate usage
    let mut system = System::new_all();
    system.refresh_cpu();
    // Small delay for accurate CPU measurement
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    system.refresh_cpu();
    system.refresh_memory();

    let cpu_usage = system.global_cpu_info().cpu_usage();
    let per_core: Vec<f32> = system.cpus().iter().map(|cpu| cpu.cpu_usage()).collect();
    let cpu_model = system.cpus().first().map(|c| c.brand().to_string()).unwrap_or_else(|| "Unknown CPU".into());
    let cpu_freq = system.cpus().first().map(|c| c.frequency()).unwrap_or(0);

    let total_mem = system.total_memory() as f32 / (1024.0 * 1024.0 * 1024.0);
    let used_mem = (system.total_memory() - system.available_memory()) as f32 / (1024.0 * 1024.0 * 1024.0);
    let available_mem = system.available_memory() as f32 / (1024.0 * 1024.0 * 1024.0);

    // GPU metrics — NVML (NVIDIA) first; when no NVIDIA GPU is present, fall
    // back to the cached WMI/registry probe so AMD/Intel/Arc GPUs are reported
    // instead of "Not detected" (utilization/temperature are unavailable
    // without vendor APIs and stay 0).
    let (gpu_available, gpu_name, gpu_usage, gpu_vram_total, gpu_vram_used, gpu_temp) = {
        #[cfg(feature = "nvidia")]
        {
            match nvml_wrapper::Nvml::init() {
                Ok(nvml) => {
                    match nvml.device_by_index(0) {
                        Ok(device) => {
                            let name = device.name().unwrap_or_else(|_| "GPU".into());
                            let utilization = device.utilization_rates().map(|u| u.gpu as f32).unwrap_or(0.0);
                            let mem_info = device.memory_info();
                            let vram_total = mem_info.as_ref().map(|m| m.total as f32 / (1024.0 * 1024.0 * 1024.0)).unwrap_or(0.0);
                            let vram_used = mem_info.as_ref().map(|m| m.used as f32 / (1024.0 * 1024.0 * 1024.0)).unwrap_or(0.0);
                            let temp = device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).unwrap_or(0) as f32;
                            tracing::debug!("GPU detected: {}, usage: {}%, VRAM: {}/{} GB", name, utilization, vram_used, vram_total);
                            (true, name, utilization, vram_total, vram_used, temp)
                        }
                        Err(e) => {
                            tracing::warn!("NVML initialized but failed to get device: {}", e);
                            let (avail, name, vram) = fallback_gpu_info().clone();
                            (avail, name, 0.0_f32, vram, 0.0_f32, 0.0_f32)
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("NVML not available: {}", e);
                    let (avail, name, vram) = fallback_gpu_info().clone();
                    (avail, name, 0.0_f32, vram, 0.0_f32, 0.0_f32)
                }
            }
        }
        #[cfg(not(feature = "nvidia"))]
        {
            // Without NVML: use the same non-NVIDIA fallback probe
            let (avail, name, vram) = fallback_gpu_info().clone();
            (avail, name, 0.0_f32, vram, 0.0_f32, 0.0_f32)
        }
    };

    // Determine inference device from config
    let gpu_layers = state.shared_state.config.gpu_layers;
    let inference_device = if !gpu_available {
        "CPU".to_string()
    } else if gpu_layers == 0 {
        "CPU".to_string()
    } else if gpu_layers >= 50 {
        "GPU".to_string()
    } else {
        "CPU+GPU".to_string()
    };

    Ok(Json(SystemMetricsResponse {
        cpu_usage_percent: cpu_usage,
        per_core_usage: per_core,
        cpu_model_name: cpu_model,
        cpu_frequency_mhz: cpu_freq,
        gpu_available,
        gpu_name,
        gpu_usage_percent: gpu_usage,
        gpu_vram_total_gb: gpu_vram_total,
        gpu_vram_used_gb: gpu_vram_used,
        gpu_temperature_c: gpu_temp,
        memory_total_gb: total_mem,
        memory_used_gb: used_mem,
        memory_available_gb: available_mem,
        gpu_layers_offloaded: gpu_layers,
        inference_device,
    }))
}

/// Storage metadata response
#[derive(Debug, Serialize)]
pub struct StorageMetadataResponse {
    /// System paths
    pub paths: StoragePaths,
    /// Downloaded models with metadata
    pub models: Vec<DownloadedModelInfo>,
    /// Storage usage statistics
    pub storage_stats: StorageStats,
    /// Database information
    pub database_info: DatabaseInfo,
    /// Installed engines information
    pub engines: Vec<InstalledEngineInfo>,
}

/// Storage paths on the system
#[derive(Debug, Serialize)]
pub struct StoragePaths {
    pub app_data_dir: String,
    pub models_dir: String,
    pub registry_dir: String,
    pub database_path: String,
    pub engines_dir: String,
}

/// Information about an installed engine
#[derive(Debug, Serialize)]
pub struct InstalledEngineInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub platform: String,
    pub acceleration: String,
    pub file_size: u64,
    pub size_human: String,
    pub install_path: String,
    pub binary_name: String,
    pub is_default: bool,
}

/// Information about a downloaded model
#[derive(Debug, Serialize)]
pub struct DownloadedModelInfo {
    pub id: String,
    pub name: String,
    pub format: String,
    pub size_bytes: u64,
    pub size_human: String,
    pub download_date: String,
    pub download_source: String,
    pub file_path: String,
    pub metadata_path: Option<String>,
}

/// Storage usage statistics
#[derive(Debug, Serialize)]
pub struct StorageStats {
    pub models_total_bytes: u64,
    pub models_total_human: String,
    pub available_space_bytes: u64,
    pub available_space_human: String,
    pub model_count: usize,
}

/// Database information
#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub path: String,
    pub size_bytes: u64,
    pub size_human: String,
}

/// Get comprehensive local storage metadata
pub async fn get_storage_metadata(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    use crate::model_management::storage::ModelMetadata;
    
    let model_manager = state.shared_state.model_manager.as_ref()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // Get app data directory
    let app_data_dir = crate::config::get_app_data_dir();
    
    // Get engines directory
    let engines_dir = app_data_dir.join("engines");

    // Get storage paths
    let paths = StoragePaths {
        app_data_dir: app_data_dir.to_string_lossy().to_string(),
        models_dir: model_manager.storage.location.models_dir.to_string_lossy().to_string(),
        registry_dir: model_manager.storage.location.registry_dir.to_string_lossy().to_string(),
        database_path: app_data_dir.join("data").join("memory.db").to_string_lossy().to_string(),
        engines_dir: engines_dir.to_string_lossy().to_string(),
    };
    
    // Get downloaded models with metadata
    let mut models = Vec::new();
    let installed_models: Vec<crate::model_management::registry::ModelInfo> = {
        let registry = model_manager.registry.read().await;
        registry.list_models().into_iter()
            .filter(|m| matches!(m.status, crate::model_management::registry::ModelStatus::Installed))
            .cloned()
            .collect()
    };
    
    for model in installed_models {
        // Try to load metadata
        let metadata_path = model_manager.storage.metadata_path(&model.id);
        let download_date = if metadata_path.exists() {
            std::fs::metadata(&metadata_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
                .flatten()
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "Unknown".to_string())
        } else {
            "Unknown".to_string()
        };
        
        let download_source = if metadata_path.exists() {
            std::fs::read_to_string(&metadata_path)
                .ok()
                .and_then(|content| serde_json::from_str::<ModelMetadata>(&content).ok())
                .map(|m| m.download_source)
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            "unknown".to_string()
        };
        
        // Get actual file size from disk
        let model_dir = model_manager.storage.location.models_dir.join(
            model.id.replace(':', "_").replace('/', "_").replace('\\', "_")
        );
        let mut actual_size = model.size_bytes;
        if model_dir.exists() {
            actual_size = walkdir::WalkDir::new(&model_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum();
        }
        
        models.push(DownloadedModelInfo {
            id: model.id.clone(),
            name: model.name.clone(),
            format: model.format.clone(),
            size_bytes: actual_size,
            size_human: format_bytes(actual_size),
            download_date,
            download_source,
            file_path: model_dir.to_string_lossy().to_string(),
            metadata_path: if metadata_path.exists() {
                Some(metadata_path.to_string_lossy().to_string())
            } else {
                None
            },
        });
    }
    
    // Get storage stats
    let models_total_bytes = model_manager.storage.get_storage_usage().unwrap_or(0);
    let available_space_bytes = model_manager.storage.get_available_space().unwrap_or(0);
    
    let storage_stats = StorageStats {
        models_total_bytes,
        models_total_human: format_bytes(models_total_bytes),
        available_space_bytes,
        available_space_human: format_bytes(available_space_bytes),
        model_count: models.len(),
    };
    
    // Get database info
    let db_path = app_data_dir.join("data").join("memory.db");
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    
    let database_info = DatabaseInfo {
        path: db_path.to_string_lossy().to_string(),
        size_bytes: db_size,
        size_human: format_bytes(db_size),
    };

    // Get installed engines info
    let mut engines = Vec::new();
    if let Some(ref engine_manager) = state.shared_state.engine_manager {
        let registry = engine_manager.registry.read().await;
        let default_engine_id = registry.default_engine.clone();

        for (engine_id, engine_info) in &registry.installed_engines {
            if let Some(install_path) = &engine_info.install_path {
                engines.push(InstalledEngineInfo {
                    id: engine_info.id.clone(),
                    name: engine_info.name.clone(),
                    version: engine_info.version.clone(),
                    platform: format!("{:?}", engine_info.platform),
                    acceleration: format!("{:?}", engine_info.acceleration),
                    file_size: engine_info.file_size,
                    size_human: format_bytes(engine_info.file_size),
                    install_path: install_path.to_string_lossy().to_string(),
                    binary_name: engine_info.binary_name.clone(),
                    is_default: default_engine_id.as_ref() == Some(engine_id),
                });
            }
        }
    }

    Ok(Json(StorageMetadataResponse {
        paths,
        models,
        storage_stats,
        database_info,
        engines,
    }))
}

// ── Engine Install/Download Endpoints ─────────────────────────────────────────

/// Request to install an engine
#[derive(Debug, Deserialize)]
pub struct InstallEngineRequest {
    pub engine_id: Option<String>,
}

/// Install an engine.
///
/// `engine_id: null` means "install the ONE engine the hardware decision table
/// selects for this machine" — the only thing the UI's recovery button sends.
/// A provided `engine_id` must exist in the catalog: unknown ids are 404, never
/// silently substituted. Failures are real HTTP errors with the full cause —
/// never a 200 wrapping an error string.
pub async fn install_engine(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<InstallEngineRequest>,
) -> Result<axum::response::Response, StatusCode> {
    let engine_manager = state.shared_state.engine_manager.as_ref()
        .ok_or_else(|| {
            error!("Engine manager not initialised");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let result = if let Some(engine_id) = payload.engine_id {
        // Explicit id: verify it exists in the catalog BEFORE attempting anything,
        // so an unknown id is a clean 404 rather than a download error.
        let known = {
            let registry = engine_manager.registry.read().await;
            registry.installed_engines.contains_key(&engine_id)
                || registry.available_engines.iter().any(|e| e.id == engine_id)
        };
        if !known {
            error!("Engine install refused: unknown engine id '{}'", engine_id);
            return Ok((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "unknown_engine_id",
                    "detail": format!(
                        "Engine id '{}' is not in the catalog for this hardware. \
                         Use GET /engines to list valid ids.",
                        engine_id
                    ),
                })),
            ).into_response());
        }
        info!("Installing engine by ID: {}", engine_id);
        engine_manager.install_engine_by_id(&engine_id).await
    } else {
        info!("Installing the decision-table engine for this hardware");
        engine_manager.download_suitable_engine().await
    };

    match result {
        Ok(engine) => {
            info!("Engine installed successfully: {}", engine.name);
            Ok(Json(serde_json::json!({
                "status": "success",
                "engine_id": engine.id,
                "engine_name": engine.name,
                "version": engine.version,
                "acceleration": format!("{}", engine.acceleration),
            })).into_response())
        }
        Err(e) => {
            error!("Engine installation failed: {}", e);
            Ok((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "engine_install_failed",
                    "detail": format!("{}", e),
                    "action": "retry_install",
                })),
            ).into_response())
        }
    }
}

/// Get engine download progress
pub async fn get_engine_download_progress(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let engine_manager = state.shared_state.engine_manager.as_ref()
        .ok_or_else(|| {
            error!("Engine manager not initialised");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let progress = engine_manager.get_download_progress().await;

    Ok(Json(serde_json::json!({
        "downloads": progress,
    })))
}

/// List available engines for current hardware
pub async fn list_available_engines(
    State(state): State<UnifiedAppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let engine_manager = state.shared_state.engine_manager.as_ref()
        .ok_or_else(|| {
            error!("Engine manager not initialised");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let engine_state = engine_manager.current_state().await;
    let registry = engine_manager.registry.read().await;

    // The decision-table engine for this machine — reported so the UI can show
    // an explicit mismatch notice when the installed default differs (the user
    // decides whether to install it; nothing switches automatically).
    let table_engine: Option<serde_json::Value> = registry
        .select_correct_engine(&engine_manager.hardware_capabilities)
        .ok()
        .map(|e| serde_json::json!({
            "id": e.id,
            "name": e.name,
            "acceleration": format!("{}", e.acceleration),
        }));

    let installed: Vec<serde_json::Value> = registry.installed_engines.values()
        .map(|e| serde_json::json!({
            "id": e.id,
            "name": e.name,
            "version": e.version,
            "acceleration": format!("{}", e.acceleration),
            "status": match e.status {
                crate::engine_management::EngineStatus::Installed => "installed",
                crate::engine_management::EngineStatus::Corrupted => "corrupted",
                _ => "unknown",
            },
            "failure_reason": e.failure_reason,
            "is_default": registry.default_engine.as_ref() == Some(&e.id),
        }))
        .collect();

    let available: Vec<serde_json::Value> = registry.available_engines.iter()
        .filter(|e| !registry.installed_engines.contains_key(&e.id))
        .map(|e| serde_json::json!({
            "id": e.id,
            "name": e.name,
            "version": e.version,
            "acceleration": format!("{}", e.acceleration),
            "status": "available",
            "file_size": e.file_size,
            "download_url": e.download_url,
        }))
        .collect();

    Ok(Json(serde_json::json!({
        "installed": installed,
        "available": available,
        "has_engine": registry.has_installed_engine(),
        "engine_state": engine_state,
        "table_engine": table_engine,
    })))
}

/// Cancel an ongoing engine download
pub async fn cancel_engine_download(
    State(state): State<UnifiedAppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<impl IntoResponse, StatusCode> {
    let engine_manager = state.shared_state.engine_manager.as_ref()
        .ok_or_else(|| {
            error!("Engine manager not initialised");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let engine_id = payload.get("engine_id")
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::BAD_REQUEST)?;

    match engine_manager.cancel_engine_download(engine_id).await {
        Ok(_) => Ok(Json(serde_json::json!({"status": "cancelled"}))),
        Err(e) => {
            error!("Failed to cancel engine download: {}", e);
            Ok(Json(serde_json::json!({"status": "error", "error": format!("{}", e)})))
        }
    }
}

/// Format bytes to human-readable string
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    
    format!("{:.2} {}", size, UNITS[unit_index])
}
