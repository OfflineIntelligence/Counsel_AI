//! Settings the user can change at runtime.
//!
//! The disk budget is the first of these. It resolves user setting -> `.env`
//! -> compiled default (see `utils::storage_governor::effective_disk_limit_mb`),
//! and every response says which layer supplied the answer so the UI can show
//! "set by you" rather than leaving the user guessing why a number is what it
//! is.
//!
//! A `PUT` takes effect immediately: the eviction pass runs inside the request
//! and its outcome is returned. Nothing here is captured at startup, so
//! lowering the limit prunes straight away instead of waiting for a restart.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::memory_db::settings_store::DISK_LIMIT_MB_KEY;
use crate::shared_state::UnifiedAppState;
use crate::utils::storage_governor::{
    self, DiskUsage, EvictionContext, EvictionOutcome,
};

#[derive(Debug, Serialize)]
pub struct StorageSettingsResponse {
    /// Budget in force, in MB. 0 means unlimited.
    pub limit_mb: u64,
    /// "user_setting" or "environment".
    pub source: storage_governor::LimitSource,
    /// The shipped default, so the UI can offer "reset to default".
    pub default_limit_mb: u64,
    pub usage: DiskUsage,
    pub usage_human: String,
    pub limit_human: String,
    /// Usage as a percentage of the limit. `None` when unlimited.
    pub used_percent: Option<f32>,
    /// True when usage exceeds the limit and eviction could not fully fix it.
    pub over_budget: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateStorageSettingsRequest {
    /// New budget in MB. 0 means unlimited. `null` clears the user override
    /// and restores the `.env` default.
    pub limit_mb: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct UpdateStorageSettingsResponse {
    #[serde(flatten)]
    pub settings: StorageSettingsResponse,
    /// What the eviction pass triggered by this change actually did.
    pub eviction: EvictionOutcome,
}

/// Smallest budget accepted.
///
/// Below this the limit could not hold the engine alone, so every pass would
/// evict everything evictable and still report a shortfall - a setting that
/// can only ever be violated is a misconfiguration, not a preference. Refused
/// with an explanation rather than silently clamped: a silently-changed
/// setting is exactly the kind of quiet disagreement this codebase avoids.
const MIN_LIMIT_MB: u64 = 1024;

fn build_response(state: &UnifiedAppState) -> StorageSettingsResponse {
    let db = &state.shared_state.database_pool;
    let cfg = &state.shared_state.config;
    let limit = storage_governor::effective_disk_limit_mb(db, cfg);
    let usage = storage_governor::measure(&crate::config::get_app_data_dir());

    let used_percent = if limit.is_unlimited() {
        None
    } else {
        Some((usage.total_bytes as f64 / limit.limit_bytes() as f64 * 100.0) as f32)
    };

    StorageSettingsResponse {
        limit_mb: limit.limit_mb,
        source: limit.source,
        default_limit_mb: cfg.app_disk_limit_mb,
        usage_human: storage_governor::format_bytes(usage.total_bytes),
        limit_human: if limit.is_unlimited() {
            "Unlimited".to_string()
        } else {
            storage_governor::format_bytes(limit.limit_bytes())
        },
        over_budget: !limit.is_unlimited() && usage.total_bytes > limit.limit_bytes(),
        used_percent,
        usage,
    }
}

/// GET /settings/storage
pub async fn get_storage_settings(State(state): State<UnifiedAppState>) -> impl IntoResponse {
    Json(build_response(&state))
}

/// PUT /settings/storage
pub async fn update_storage_settings(
    State(state): State<UnifiedAppState>,
    Json(req): Json<UpdateStorageSettingsRequest>,
) -> axum::response::Response {
    let db = &state.shared_state.database_pool;

    match req.limit_mb {
        Some(mb) if mb != 0 && mb < MIN_LIMIT_MB => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "limit_too_small",
                    "detail": format!(
                        "A limit of {} MB is below the {} MB minimum. The inference engine \
                         alone is larger than that, so this budget could never be met. \
                         Choose a larger value, or 0 for unlimited.",
                        mb, MIN_LIMIT_MB
                    ),
                    "minimum_limit_mb": MIN_LIMIT_MB,
                })),
            )
                .into_response();
        }
        Some(mb) => {
            if let Err(e) = db.settings.set(DISK_LIMIT_MB_KEY, &mb.to_string()) {
                warn!("Failed to persist disk limit: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "settings_write_failed",
                        "detail": format!("Could not save the storage limit: {}", e),
                    })),
                )
                    .into_response();
            }
            info!("Storage limit set to {} MB by the user", mb);
        }
        None => {
            if let Err(e) = db.settings.clear(DISK_LIMIT_MB_KEY) {
                warn!("Failed to clear disk limit: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "settings_write_failed",
                        "detail": format!("Could not reset the storage limit: {}", e),
                    })),
                )
                    .into_response();
            }
            info!("Storage limit reset to the shipped default");
        }
    }

    let eviction = run_eviction(&state).await;

    Json(UpdateStorageSettingsResponse {
        settings: build_response(&state),
        eviction,
    })
    .into_response()
}

/// Run one eviction pass against the current budget.
///
/// Gathers the runtime facts eviction must respect - which model is loaded,
/// which models are installed and where - and hands them to the governor,
/// which owns the tier policy and does the deleting.
pub async fn run_eviction(state: &UnifiedAppState) -> EvictionOutcome {
    let db = &state.shared_state.database_pool;
    let cfg = &state.shared_state.config;
    let limit = storage_governor::effective_disk_limit_mb(db, cfg);

    // The model currently loaded by llama-server. Read from the ACTIVE runtime
    // config rather than last_model.txt: the marker file records intent, the
    // runtime records what is genuinely open and therefore unsafe to delete.
    let active_model_id = active_model_id(state).await;

    let installed_models = match state.shared_state.model_manager {
        Some(ref mm) => {
            let registry = mm.registry.read().await;
            registry
                .list_models()
                .into_iter()
                .filter(|m| {
                    matches!(
                        m.status,
                        crate::model_management::registry::ModelStatus::Installed
                    )
                })
                .map(|m| {
                    let dir = mm.storage.model_directory(&m.id);
                    (m.id.clone(), dir)
                })
                .collect()
        }
        None => Vec::new(),
    };

    let ctx = EvictionContext {
        app_data_dir: crate::config::get_app_data_dir(),
        active_model_id,
        installed_models,
        database: db,
    };

    let outcome = storage_governor::enforce_limit(&ctx, limit);
    info!("Storage eviction: {}", outcome.reason);
    outcome
}

/// Id of the model the running inference server has open, if any.
async fn active_model_id(state: &UnifiedAppState) -> Option<String> {
    let runtime = state
        .shared_state
        .runtime_manager
        .read()
        .ok()
        .and_then(|g| g.clone())?;
    let config = runtime.get_current_config().await?;
    // The runtime knows a path, not an id. The model directory is named after
    // the (sanitised) model id, so the parent directory name is the link back.
    config
        .model_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
}

// NOTE: a `current_limit(state)` helper used to sit here, documented as being
// "exposed for the storage-metadata endpoint". It never was — the storage page
// reads the budget from GET /settings/storage directly, which is the better
// path anyway because it returns the usage breakdown alongside the limit rather
// than the limit alone. Removed rather than left as a plausible-looking
// accessor nobody calls.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_APP_DISK_LIMIT_MB};
    use crate::memory_db::MemoryDatabase;
    use crate::utils::storage_governor::{effective_disk_limit_mb, LimitSource};

    fn config() -> Config {
        Config::from_env().expect("Config::from_env should succeed with defaults")
    }

    #[test]
    fn an_unset_limit_resolves_to_the_env_default_and_says_so() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let cfg = config();
        let limit = effective_disk_limit_mb(&db, &cfg);
        assert_eq!(limit.limit_mb, cfg.app_disk_limit_mb);
        assert_eq!(limit.source, LimitSource::Environment);
    }

    #[test]
    fn a_user_setting_overrides_the_env_default_and_says_so() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.settings.set(DISK_LIMIT_MB_KEY, "51200").unwrap();
        let limit = effective_disk_limit_mb(&db, &config());
        assert_eq!(limit.limit_mb, 51200);
        assert_eq!(limit.source, LimitSource::UserSetting);
    }

    #[test]
    fn clearing_the_user_setting_falls_back_to_the_env_default() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        let cfg = config();
        db.settings.set(DISK_LIMIT_MB_KEY, "51200").unwrap();
        db.settings.clear(DISK_LIMIT_MB_KEY).unwrap();
        let limit = effective_disk_limit_mb(&db, &cfg);
        assert_eq!(limit.limit_mb, cfg.app_disk_limit_mb);
        assert_eq!(limit.source, LimitSource::Environment);
    }

    /// The compiled fallback must be a real, finite budget. A 0 here would
    /// silently mean "unlimited" for every install whose .env failed to load,
    /// which is the opposite of what a fallback is for.
    #[test]
    fn the_compiled_default_is_finite_and_above_the_accepted_minimum() {
        assert!(DEFAULT_APP_DISK_LIMIT_MB >= MIN_LIMIT_MB);
    }

    /// Cross-language invariant: every budget the UI offers must be one the
    /// backend will actually accept.
    ///
    /// Enforced by PARSING the real .ts file rather than restating its values,
    /// for the same reason as `frontend_upload_limit_mirrors_the_server_limit`
    /// in thread_server.rs - a restated copy is a third number free to drift.
    /// Without this, adding a "500 MB" option to the dropdown would compile,
    /// ship, and then reject every selection at runtime with limit_too_small.
    #[test]
    fn every_limit_the_ui_offers_is_one_the_backend_accepts() {
        let ts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src/api/storageSettings.ts");
        let source = std::fs::read_to_string(&ts_path).unwrap_or_else(|e| {
            panic!(
                "cannot read the frontend storage-limit choices at {}: {} - if the file \
                 moved, update this test rather than deleting it; it is the only thing \
                 keeping the dropdown's options in agreement with MIN_LIMIT_MB",
                ts_path.display(),
                e
            )
        });

        // Anchored on "= [" rather than the first '[': the declaration's TYPE
        // annotation (`readonly {...}[]`) contains brackets of its own, and
        // splitting on those yields an empty array and a test that passes
        // vacuously.
        let block = source
            .split_once("LIMIT_CHOICES_MB")
            .and_then(|(_, rest)| rest.split_once("= ["))
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(inner, _)| inner.to_string())
            .expect("LIMIT_CHOICES_MB array not found in storageSettings.ts");

        let mut checked = 0;
        for entry in block.split("value:").skip(1) {
            let expr: String = entry
                .chars()
                .take_while(|c| *c != ',' && *c != '}')
                .collect();
            // Values are written as products like `5 * 1024`.
            let value: u64 = expr
                .split('*')
                .map(|f| {
                    f.trim().parse::<u64>().unwrap_or_else(|_| {
                        panic!(
                            "LIMIT_CHOICES_MB values must be products of plain integers so \
                             this test can evaluate them, got {:?}",
                            expr
                        )
                    })
                })
                .product();
            checked += 1;

            assert!(
                value == 0 || value >= MIN_LIMIT_MB,
                "the UI offers a {} MB limit, but update_storage_settings rejects anything \
                 below {} MB - selecting it would fail with limit_too_small",
                value,
                MIN_LIMIT_MB
            );
        }
        assert!(checked > 0, "parsed no choices - the parser has drifted from the .ts format");
    }
}
