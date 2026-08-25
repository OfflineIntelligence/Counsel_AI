//! Model Recommendation System
//!
//! Provides hardware-aware model recommendations based on:
//! - Available RAM and VRAM
//! - CPU/GPU capabilities
//! - Model size requirements
//! - User preferences and use cases

use super::registry::ModelInfo;
use crate::config::Config;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// System info only needed on macOS for unified memory detection
#[cfg(target_os = "macos")]
use sysinfo::System;

/// User preferences for model recommendations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    pub primary_use_case: UseCase,
    pub quality_preference: QualityPreference,
    pub speed_preference: SpeedPreference,
    pub cost_sensitivity: CostSensitivity,
    pub preferred_formats: Vec<String>,
}

/// Primary use case for the model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UseCase {
    ChatAssistant,
    CodeGeneration,
    CreativeWriting,
    ResearchAnalysis,
    Translation,
    GeneralPurpose,
}

/// Quality preference setting
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QualityPreference {
    HighQuality,  // Prefer larger, more capable models
    Balanced,     // Balance quality and performance
    FastResponse, // Prioritize speed over quality
}

/// Speed preference setting
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SpeedPreference {
    Fastest,        // Prioritize inference speed
    Balanced,       // Balance speed and quality
    HighestQuality, // Prioritize quality over speed
}

/// Cost sensitivity (relevant for cloud/API models)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CostSensitivity {
    Budget,   // Prefer smaller, free models
    Moderate, // Balanced approach
    Premium,  // Willing to use larger/expensive models
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            primary_use_case: UseCase::GeneralPurpose,
            quality_preference: QualityPreference::Balanced,
            speed_preference: SpeedPreference::Balanced,
            cost_sensitivity: CostSensitivity::Moderate,
            preferred_formats: vec!["gguf".to_string()], // Default to GGUF for local inference
        }
    }
}

/// Hardware profile detected from system
#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub total_ram_gb: f32,
    pub available_ram_gb: f32,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub gpu_available: bool,
    pub gpu_vram_gb: Option<f32>,
    pub gpu_compute_capability: Option<f32>,
    pub system_architecture: String,
}

/// Model recommender service
pub struct ModelRecommender {
    user_preferences: UserPreferences,
}

impl ModelRecommender {
    pub fn new() -> Self {
        Self {
            user_preferences: UserPreferences::default(),
        }
    }

    /// Update user preferences
    pub fn set_preferences(&mut self, preferences: UserPreferences) {
        self.user_preferences = preferences;
        info!("Updated user preferences: {:?}", self.user_preferences);
    }

    /// Get current user preferences
    pub fn get_preferences(&self) -> &UserPreferences {
        &self.user_preferences
    }

    /// Detect hardware profile from system configuration
    pub fn detect_hardware_profile(config: &Config) -> HardwareProfile {
        let mut system = sysinfo::System::new_all();
        system.refresh_memory();
        system.refresh_cpu();

        let total_ram_gb = system.total_memory() as f32 / (1024.0 * 1024.0 * 1024.0);
        let available_ram_gb = system.available_memory() as f32 / (1024.0 * 1024.0 * 1024.0);
        let cpu_cores = num_cpus::get() as u32;
        let cpu_threads = config.threads;

        // GPU detection
        let (gpu_available, gpu_vram_gb, gpu_compute_capability) = Self::detect_gpu();

        HardwareProfile {
            total_ram_gb,
            available_ram_gb,
            cpu_cores,
            cpu_threads,
            gpu_available,
            gpu_vram_gb,
            gpu_compute_capability,
            system_architecture: std::env::consts::ARCH.to_string(),
        }
    }

    /// Detect GPU capabilities (platform-specific)
    fn detect_gpu() -> (bool, Option<f32>, Option<f32>) {
        // Windows and Linux: Use NVML for NVIDIA GPU detection (only when nvidia feature enabled).
        // When NVML finds no NVIDIA GPU, fall through to the WMI-based detection so
        // AMD/Intel/Arc GPUs are reported (hardware info + compatibility scoring)
        // instead of showing up as "no GPU".
        #[cfg(all(feature = "nvidia", any(target_os = "windows", target_os = "linux")))]
        {
            match nvml_wrapper::Nvml::init() {
                Ok(nvml) => {
                    match nvml.device_count() {
                        Ok(count) if count > 0 => {
                            match nvml.device_by_index(0) {
                                Ok(device) => {
                                    let vram_bytes =
                                        device.memory_info().map(|mem| mem.total).unwrap_or(0);
                                    let vram_gb = if vram_bytes > 0 {
                                        Some(vram_bytes as f32 / (1024.0 * 1024.0 * 1024.0))
                                    } else {
                                        None
                                    };

                                    // Simplified compute capability detection
                                    let compute_capability = Some(7.5); // Default assumption

                                    (true, vram_gb, compute_capability)
                                }
                                Err(_) => Self::detect_gpu_via_system_tools(),
                            }
                        }
                        _ => Self::detect_gpu_via_system_tools(),
                    }
                }
                Err(_) => Self::detect_gpu_via_system_tools(),
            }
        }

        // Windows and Linux without NVML: use the same OS-tool detection
        #[cfg(all(not(feature = "nvidia"), any(target_os = "windows", target_os = "linux")))]
        {
            Self::detect_gpu_via_system_tools()
        }

        // macOS Apple Silicon: Metal GPU with unified memory
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            use sysinfo::System;
            let mut sys = System::new_all();
            sys.refresh_memory();
            // Apple Silicon uses unified memory - report total as "VRAM" since it's shared
            let unified_mem_gb = sys.total_memory() as f32 / (1024.0 * 1024.0 * 1024.0);
            // Metal compute capability is not directly comparable to CUDA, use a placeholder
            info!(
                "Apple Silicon detected with Metal GPU, unified memory: {:.1} GB",
                unified_mem_gb
            );
            (true, Some(unified_mem_gb), None)
        }

        // macOS Intel: No efficient GPU for LLM inference
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            info!("Intel Mac detected, no GPU acceleration available");
            (false, None, None)
        }
    }

    /// Detect any DirectX-visible GPU (AMD, Intel, NVIDIA) via WMI on Windows.
    /// Used when NVML is unavailable or reports no NVIDIA device — covers the
    /// entire AMD Radeon and Intel Arc/Iris/UHD/HD Graphics families.
    /// Returns (gpu_available, vram_gb, compute_capability=None).
    /// pub(crate): also used by the /metrics/system fallback in model_api.rs.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub(crate) fn detect_gpu_via_system_tools() -> (bool, Option<f32>, Option<f32>) {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            use std::process::{Command, Stdio};

            let ps_script = r#"
$gpu = Get-WmiObject Win32_VideoController | Sort-Object AdapterRAM -Descending | Select-Object -First 1
if ($gpu) {
    $vramBytes = $gpu.AdapterRAM
    if ($vramBytes -gt 0) { [math]::Round($vramBytes / 1MB) } else { 0 }
} else { 0 }
"#;
            let result = Command::new("powershell")
                .args(["-NonInteractive", "-NoProfile", "-Command", ps_script.trim()])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .output();

            if let Ok(output) = result {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(vram_mb) = stdout.trim().parse::<u64>() {
                    if vram_mb > 0 {
                        // AdapterRAM is a 32-bit DWORD and overflows for GPUs with
                        // more than 4 GB VRAM (reported as ~4294 MB). Treat anything
                        // >= 4000 MB as "at least 4 GB" — same clamp as config.rs.
                        let vram_gb = if vram_mb >= 4000 {
                            ((vram_mb / 1024).max(4)) as f32
                        } else {
                            vram_mb as f32 / 1024.0
                        };
                        info!(
                            "GPU detected via WMI: {:.1} GB VRAM (AMD/Intel/other — no NVML)",
                            vram_gb
                        );
                        return (true, Some(vram_gb), None);
                    }
                }
            }

            debug!("No DirectX-visible GPU found via WMI");
            (false, None, None)
        }

        // Linux without NVML: no AMD/Intel VRAM probe implemented at this level
        #[cfg(not(target_os = "windows"))]
        {
            (false, None, None)
        }
    }

    /// Score a model's compatibility with current hardware
    pub fn score_model_compatibility(&self, model: &ModelInfo, hardware: &HardwareProfile) -> f32 {
        let mut score = 1.0f32;

        // RAM requirements check
        let model_ram_gb = (model.size_bytes as f32) / (1024.0 * 1024.0 * 1024.0) * 1.5; // Estimate with buffer

        if model_ram_gb > hardware.available_ram_gb {
            score *= 0.3; // Heavy penalty for insufficient RAM
        } else if model_ram_gb > hardware.total_ram_gb * 0.8 {
            score *= 0.7; // Moderate penalty for tight RAM
        }

        // GPU requirements check
        let requires_gpu = model.tags.contains(&"gpu".to_string())
            || model.format.eq_ignore_ascii_case("tensorrt");

        if requires_gpu && !hardware.gpu_available {
            score *= 0.2; // Heavy penalty for GPU requirement without GPU
        } else if requires_gpu && hardware.gpu_available {
            if let Some(vram_gb) = hardware.gpu_vram_gb {
                let required_vram = match model.size_bytes {
                    s if s < 5 * 1024 * 1024 * 1024 => 6.0, // 5GB model needs ~6GB VRAM
                    s if s < 10 * 1024 * 1024 * 1024 => 12.0,
                    _ => 24.0,
                };

                if vram_gb < required_vram * 0.8 {
                    score *= 0.5; // Penalty for tight VRAM
                }
            }
        }

        // Format preference bonus
        if self
            .user_preferences
            .preferred_formats
            .iter()
            .any(|f| model.format.eq_ignore_ascii_case(f))
        {
            score *= 1.2; // Bonus for preferred format
        }

        // Use case alignment
        score *= self.score_use_case_alignment(model);

        // Quality/speed preference adjustment
        score *= self.score_quality_speed_preference(model);

        score.clamp(0.0, 1.0)
    }

    /// Score how well a model aligns with the user's use case
    fn score_use_case_alignment(&self, model: &ModelInfo) -> f32 {
        let use_case_tags: Vec<&str> = model.tags.iter().map(|s| s.as_str()).collect();

        match self.user_preferences.primary_use_case {
            UseCase::ChatAssistant => {
                if use_case_tags.contains(&"chat") || use_case_tags.contains(&"instruction") {
                    1.3
                } else if use_case_tags.contains(&"general") {
                    1.1
                } else {
                    1.0
                }
            }
            UseCase::CodeGeneration => {
                if use_case_tags.contains(&"code") || use_case_tags.contains(&"programming") {
                    1.4
                } else {
                    1.0
                }
            }
            UseCase::CreativeWriting => {
                if use_case_tags.contains(&"creative") || use_case_tags.contains(&"story") {
                    1.3
                } else if use_case_tags.contains(&"text-generation") {
                    1.1
                } else {
                    1.0
                }
            }
            UseCase::ResearchAnalysis => {
                if use_case_tags.contains(&"research") || use_case_tags.contains(&"analysis") {
                    1.3
                } else {
                    1.0
                }
            }
            UseCase::Translation => {
                if use_case_tags.contains(&"translation") || use_case_tags.contains(&"multilingual")
                {
                    1.4
                } else {
                    1.0
                }
            }
            UseCase::GeneralPurpose => 1.0,
        }
    }

    /// Adjust score based on quality/speed preferences
    fn score_quality_speed_preference(&self, model: &ModelInfo) -> f32 {
        let model_size_category = if model.size_bytes < 3 * 1024 * 1024 * 1024 {
            "small" // < 3GB
        } else if model.size_bytes < 10 * 1024 * 1024 * 1024 {
            "medium" // 3-10GB
        } else {
            "large" // > 10GB
        };

        match (
            &self.user_preferences.quality_preference,
            &self.user_preferences.speed_preference,
        ) {
            (QualityPreference::HighQuality, SpeedPreference::HighestQuality) => {
                match model_size_category {
                    "large" => 1.3,
                    "medium" => 1.1,
                    "small" => 0.8,
                    _ => 1.0,
                }
            }
            (QualityPreference::FastResponse, SpeedPreference::Fastest) => {
                match model_size_category {
                    "small" => 1.3,
                    "medium" => 1.0,
                    "large" => 0.6,
                    _ => 1.0,
                }
            }
            _ => 1.0, // Balanced preferences
        }
    }

    /// Get top recommended models for current hardware and preferences
    pub fn get_recommendations(
        &self,
        models: Vec<&ModelInfo>,
        hardware: &HardwareProfile,
        max_results: usize,
    ) -> Vec<(String, f32)> {
        let mut scored_models: Vec<(String, f32)> = models
            .iter()
            .map(|model| {
                let score = self.score_model_compatibility(model, hardware);
                (model.id.clone(), score)
            })
            .collect();

        // Sort by compatibility score (descending)
        scored_models.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take top results
        scored_models.truncate(max_results);

        debug!("Generated {} model recommendations", scored_models.len());
        scored_models
    }

    /// Get hardware recommendations message
    pub fn get_hardware_recommendation_message(&self, hardware: &HardwareProfile) -> String {
        let mut recommendations = Vec::new();

        if hardware.total_ram_gb < 8.0 {
            recommendations
                .push("Consider upgrading RAM to 8GB+ for better model performance".to_string());
        }

        if !hardware.gpu_available && hardware.total_ram_gb >= 16.0 {
            recommendations.push(
                "A GPU would significantly accelerate inference for larger models".to_string(),
            );
        }

        if hardware.gpu_available {
            if let Some(vram) = hardware.gpu_vram_gb {
                if vram < 8.0 {
                    recommendations.push(format!(
                        "With {:.1}GB VRAM, you can run medium-sized models efficiently",
                        vram
                    ));
                } else if vram >= 16.0 {
                    recommendations.push(format!(
                        "With {:.1}GB VRAM, you can run large models with full GPU acceleration",
                        vram
                    ));
                }
            }
        }

        if recommendations.is_empty() {
            "Your system configuration is well-suited for running various model sizes".to_string()
        } else {
            recommendations.join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_hardware_detection() {
        let config = Config {
            model_path: "".to_string(),
            mmproj_path: "".to_string(),
            llama_bin: "".to_string(),
            llama_host: "127.0.0.1".to_string(),
            llama_port: 9639,
            ctx_size: 8192,
            batch_size: 128,
            threads: 6,
            threads_batch: 6,
            gpu_layers: 20,
            gpu_layers_auto: false,
            ctx_size_auto: false,
            batch_size_auto: false,
            cache_reuse: 256,
            app_disk_limit_mb: crate::config::DEFAULT_APP_DISK_LIMIT_MB,
            health_timeout_seconds: 600,
            hot_swap_grace_seconds: 25,
            max_concurrent_streams: 2,
            prometheus_port: 9000,
            api_host: "127.0.0.1".to_string(),
            api_port: 8888,
            requests_per_second: 24,
            generate_timeout_seconds: 300,
            stream_timeout_seconds: 600,
            health_check_timeout_seconds: 900,
            queue_size: 1000,
            queue_timeout_seconds: 300,
            backend_url: "http://127.0.0.1:9639".to_string(),
        };

        let hardware = ModelRecommender::detect_hardware_profile(&config);
        assert!(hardware.total_ram_gb > 0.0);
        assert!(hardware.cpu_cores > 0);
    }
}
