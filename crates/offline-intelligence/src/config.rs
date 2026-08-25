// Offline_Intelligence/crates/offline-intelligence/src/config.rs

use anyhow::Result;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use sysinfo::System;
use tracing::{info, warn};

// NVIDIA GPU detection only available when nvidia feature is enabled (Windows and Linux)
#[cfg(all(feature = "nvidia", any(target_os = "windows", target_os = "linux")))]
use nvml_wrapper::Nvml;

/// Fallback disk budget when neither the database nor `.env` supplies one.
///
/// 20 GB is deliberately generous: this app legitimately stores multi-gigabyte
/// models and engines, and a default that evicts them on a normal install
/// would be worse than having no limit at all. The number exists so a missing
/// or corrupt `.env` still yields a sane, finite budget.
pub const DEFAULT_APP_DISK_LIMIT_MB: u64 = 20 * 1024;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Config {
    pub model_path: String,
    /// Development-only multimodal projector override paired with MODEL_PATH
    /// (empty = none; production resolves the mmproj from the model registry).
    pub mmproj_path: String,
    pub llama_bin: String,
    pub llama_host: String,
    pub llama_port: u16,
    pub ctx_size: u32,
    /// True when CTX_SIZE was "auto"/unset.
    ///
    /// This distinction is load-bearing. An explicit CTX_SIZE is a REQUEST that
    /// gets clamped down to the model's trained context. "auto" means we have
    /// no user intent at all, so the model's own declared context should be the
    /// starting point rather than a ceiling on a filename guess — see
    /// `SharedSystemState::build_runtime_config`.
    pub ctx_size_auto: bool,
    pub batch_size: u32,
    /// True when BATCH_SIZE was "auto"/unset: the batch flag is then omitted
    /// entirely so llama-server uses its upstream-tuned defaults (2048/512),
    /// which are far better for prompt-processing throughput than any value
    /// this codebase used to guess.
    pub batch_size_auto: bool,
    pub threads: u32,
    /// Threads for prompt processing (--threads-batch). Prefill is compute-
    /// bound and benefits from every logical core, unlike token generation.
    pub threads_batch: u32,
    pub gpu_layers: u32,
    /// True when GPU_LAYERS was "auto"/unset: the per-model dynamic offload
    /// calculation (GGUF layer count vs measured VRAM) runs at model load and
    /// overrides the static `gpu_layers` bucket value.
    pub gpu_layers_auto: bool,
    /// Minimum KV chunk size llama-server may reuse via KV shifting
    /// (--cache-reuse). Lets a rewritten-but-overlapping prompt skip
    /// re-prefilling the matching chunks - the main time-to-first-token
    /// lever for multi-turn chat. 0 disables the flag. Env: CACHE_REUSE.
    pub cache_reuse: u32,
    /// Default ceiling, in megabytes, for everything this app stores under its
    /// app-data directory: models, engines, the database, the Local Storage
    /// vault, downloads and any KV cache blobs.
    ///
    /// This is the SHIPPED default only. The user's own choice lives in the
    /// `app_settings` table and takes precedence - see
    /// `utils::storage_governor::effective_disk_limit_mb` for the resolution
    /// order. 0 means "no limit", which disables eviction entirely.
    /// Env: APP_DISK_LIMIT_MB.
    pub app_disk_limit_mb: u64,
    pub health_timeout_seconds: u64,
    pub hot_swap_grace_seconds: u64,
    pub max_concurrent_streams: u32,
    pub prometheus_port: u16,
    pub api_host: String,
    pub api_port: u16,
    pub requests_per_second: u32,
    pub generate_timeout_seconds: u64,
    pub stream_timeout_seconds: u64,
    pub health_check_timeout_seconds: u64,
    pub queue_size: usize,
    pub queue_timeout_seconds: u64,
    pub backend_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Try to load .env from multiple locations in order:
        // 1. First try executable directory (for production builds with bundled .env)
        // 2. Then try project root (where .env actually is during development)
        // 3. Finally try current directory as fallback

        let mut env_loaded = false;

        // 1. Try executable directory (and Tauri resource subdirectory)
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                // Check next to exe first, then in resources/ (Tauri v2 bundle location)
                let candidates = [
                    exe_dir.join(".env"),
                    exe_dir.join("resources").join(".env"),
                    exe_dir.join("Resources").join(".env"),
                ];
                for env_path in &candidates {
                    if env_path.exists() {
                        match dotenvy::from_path(env_path) {
                            Ok(_) => {
                                info!("Loaded .env from: {:?}", env_path);
                                env_loaded = true;
                                break;
                            }
                            Err(e) => {
                                warn!("Failed to load .env from {:?}: {}", env_path, e);
                            }
                        }
                    }
                }

                // 2. If not found in exe dir, try project root (../../ from target/release/)
                if !env_loaded {
                    let project_root = if exe_dir.ends_with("target/release")
                        || exe_dir.ends_with("target\\release")
                    {
                        exe_dir.parent().and_then(|p| p.parent())
                    } else {
                        None
                    };

                    if let Some(root) = project_root {
                        let root_env = root.join(".env");
                        if root_env.exists() {
                            match dotenvy::from_path(&root_env) {
                                Ok(_) => {
                                    info!("Loaded .env from project root: {:?}", root_env);
                                    env_loaded = true;
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to load .env from project root {:?}: {}",
                                        root_env, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. If still not loaded, try current directory (development fallback)
        if !env_loaded {
            if let Err(e) = dotenvy::dotenv() {
                warn!("Failed to load .env from current directory: {}. Using system environment variables.", e);
            } else {
                info!("Loaded environment variables from .env file in current directory");
            }
        }

        // Auto-detect llama binary based on OS, with optional LLAMA_BIN override
        let llama_bin = Self::get_llama_binary_path()?;
        info!("Using llama binary: {}", llama_bin);

        // Use MODEL_PATH from env, or try to find embedded model
        let model_path = Self::get_model_path_with_fallback()?;

        // MMPROJ_PATH: DEVELOPMENT-ONLY override pairing a multimodal
        // projector with the MODEL_PATH model, mirroring the MODEL_PATH /
        // LLAMA_BIN pattern. In production this stays empty and the mmproj is
        // resolved from the model registry at activation time — the projector
        // belongs to a specific model, not to static config. Unset, empty, or
        // the literal "none" all mean "no override". A set-but-missing path is
        // a loud warning and is IGNORED (never a broken spawn later): the
        // failure is surfaced at the moment of misconfiguration.
        let mmproj_path = match env::var("MMPROJ_PATH") {
            Ok(raw) => {
                let raw = raw.trim().to_string();
                if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
                    String::new()
                } else if std::path::Path::new(&raw).exists() {
                    info!("Using multimodal projector from MMPROJ_PATH: {}", raw);
                    raw
                } else {
                    warn!(
                        "MMPROJ_PATH is set but the file does not exist: {} — ignoring it. \
                         The model will run TEXT-ONLY. Fix the path to enable vision.",
                        raw
                    );
                    String::new()
                }
            }
            Err(_) => String::new(),
        };

        // Auto‑detect threads if set to "auto"
        let threads = if env::var("THREADS").unwrap_or_else(|_| "auto".into()) == "auto" {
            Self::auto_detect_threads()
        } else {
            env::var("THREADS")
                .unwrap_or_else(|_| "6".into())
                .parse()
                .unwrap_or(6)
        };

        // Auto‑detect GPU layers if set to "auto".
        // The value computed here is the model-agnostic fallback; when
        // gpu_layers_auto is true, the real per-model offload is computed at
        // model-load time from the GGUF header (see gpu_offload.rs).
        let gpu_layers_auto = env::var("GPU_LAYERS").unwrap_or_else(|_| "auto".into()) == "auto";
        let gpu_layers = if gpu_layers_auto {
            Self::auto_detect_gpu_layers()
        } else {
            env::var("GPU_LAYERS")
                .unwrap_or_else(|_| "20".into())
                .parse()
                .unwrap_or(20)
        };

        // Prompt-processing thread count: all logical cores unless overridden.
        let threads_batch = match env::var("THREADS_BATCH").unwrap_or_else(|_| "auto".into()).as_str() {
            "auto" => num_cpus::get() as u32,
            s => s.parse().unwrap_or(num_cpus::get() as u32),
        };

        // Auto-detect context size.
        //
        // The value computed here is only a STARTING POINT when auto: the real
        // context is settled at runtime against the model's GGUF header, which
        // is the only trustworthy source. See `build_runtime_config`.
        let ctx_size_auto = env::var("CTX_SIZE").unwrap_or_else(|_| "auto".into()) == "auto";
        let ctx_size = if ctx_size_auto {
            Self::auto_detect_ctx_size(&model_path)
        } else {
            env::var("CTX_SIZE")
                .unwrap_or_else(|_| "8192".into())
                .parse()
                .unwrap_or(8192)
        };

        // Batch size: "auto" now means "omit the flag — use llama-server's
        // upstream defaults (n_batch 2048 / n_ubatch 512)". The old heuristic
        // capped batch as low as 64, throttling prompt processing badly.
        // An explicit numeric BATCH_SIZE is still honored.
        let batch_size_auto = env::var("BATCH_SIZE").unwrap_or_else(|_| "auto".into()) == "auto";
        let batch_size = if batch_size_auto {
            Self::auto_detect_batch_size(gpu_layers, ctx_size)
        } else {
            env::var("BATCH_SIZE")
                .unwrap_or_else(|_| "256".into())
                .parse()
                .unwrap_or(256)
        };

        // KV-cache chunk reuse (--cache-reuse): minimum chunk size that
        // llama-server may reuse via KV shifting when a later prompt overlaps
        // a cached one. Cuts time-to-first-token on follow-up turns. An
        // unset/invalid value gets the tuned default; 0 disables the flag.
        let cache_reuse: u32 = env::var("CACHE_REUSE")
            .unwrap_or_else(|_| "256".into())
            .parse()
            .unwrap_or(256);

        // Total app-data disk budget. Shipped default; the user can override
        // it in Settings, which is stored in the database rather than here.
        // An unset/invalid value gets DEFAULT_APP_DISK_LIMIT_MB; an explicit 0
        // means unlimited.
        let app_disk_limit_mb: u64 = env::var("APP_DISK_LIMIT_MB")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_APP_DISK_LIMIT_MB);

        // Get backend URL components
        let llama_host = env::var("LLAMA_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let llama_port = env::var("LLAMA_PORT")
            .unwrap_or_else(|_| "9639".into())
            .parse()?;
        let backend_url = format!("http://{}:{}", llama_host, llama_port);

        info!(
            "Resource Configuration: {} GPU layers, {} threads, batch size: {}, context: {}",
            gpu_layers, threads, batch_size, ctx_size
        );

        Ok(Self {
            model_path,
            mmproj_path,
            llama_bin,
            llama_host: llama_host.clone(),
            llama_port,
            ctx_size,
            batch_size,
            ctx_size_auto,
            batch_size_auto,
            threads,
            threads_batch,
            gpu_layers,
            gpu_layers_auto,
            cache_reuse,
            app_disk_limit_mb,
            health_timeout_seconds: env::var("HEALTH_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".into())
                .parse()?,
            hot_swap_grace_seconds: env::var("HOT_SWAP_GRACE_SECONDS")
                .unwrap_or_else(|_| "25".into())
                .parse()?,
            max_concurrent_streams: env::var("MAX_CONCURRENT_STREAMS")
                .unwrap_or_else(|_| "4".into())
                .parse()?,
            prometheus_port: env::var("PROMETHEUS_PORT")
                .unwrap_or_else(|_| "9000".into())
                .parse()?,
            api_host: env::var("API_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            api_port: env::var("API_PORT")
                .unwrap_or_else(|_| "8888".into())
                .parse()?,
            requests_per_second: env::var("REQUESTS_PER_SECOND")
                .unwrap_or_else(|_| "24".into())
                .parse()?,
            generate_timeout_seconds: env::var("GENERATE_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "300".into())
                .parse()?,
            stream_timeout_seconds: env::var("STREAM_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "600".into())
                .parse()?,
            health_check_timeout_seconds: env::var("HEALTH_CHECK_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "90".into())
                .parse()?,
            queue_size: env::var("QUEUE_SIZE")
                .unwrap_or_else(|_| "100".into())
                .parse()?,
            queue_timeout_seconds: env::var("QUEUE_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "30".into())
                .parse()?,
            backend_url,
        })
    }

    fn get_model_path_with_fallback() -> Result<String> {
        // First try environment variable
        if let Ok(model_path) = env::var("MODEL_PATH") {
            // Check if the path exists
            if std::path::Path::new(&model_path).exists() {
                info!("Using model from MODEL_PATH: {}", model_path);
                return Ok(model_path);
            } else {
                warn!("MODEL_PATH set but file doesn't exist: {}", model_path);
            }
        }

        // Try to find embedded model
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        // Check multiple possible embedded model locations (MULTI-FORMAT SUPPORT)
        let possible_model_locations = vec![
            // GGUF formats
            exe_dir.join("resources/models/default.gguf"),
            exe_dir.join("resources/models/model.gguf"),
            exe_dir.join("models/default.gguf"),
            exe_dir.join("models/model.gguf"),
            exe_dir.join("default.gguf"),
            // ONNX formats
            exe_dir.join("resources/models/default.onnx"),
            exe_dir.join("resources/models/model.onnx"),
            // TensorRT formats
            exe_dir.join("resources/models/default.trt"),
            exe_dir.join("resources/models/model.engine"),
            // Safetensors formats
            exe_dir.join("resources/models/default.safetensors"),
            exe_dir.join("resources/models/model.safetensors"),
            // GGML formats
            exe_dir.join("resources/models/default.ggml"),
            exe_dir.join("resources/models/model.bin"),
        ];

        for model_path in possible_model_locations {
            if model_path.exists() {
                info!("Using embedded model: {}", model_path.display());
                return Ok(model_path.to_string_lossy().to_string());
            }
        }

        // Check for any supported model file in models directory
        if let Ok(entries) = std::fs::read_dir(exe_dir.join("resources/models")) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    let ext_str = ext.to_str().unwrap_or("").to_lowercase();
                    // Check if extension matches any supported format
                    if matches!(
                        ext_str.as_str(),
                        "gguf"
                            | "ggml"
                            | "onnx"
                            | "trt"
                            | "engine"
                            | "plan"
                            | "safetensors"
                            | "mlmodel"
                    ) {
                        info!("Using found model: {}", entry.path().display());
                        return Ok(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }

        // Return a default path when no model is found, allowing the system to start
        // Models can be downloaded later via the model registry
        Ok("".to_string())
    }

    /// Auto-detect the llama-server binary path based on the current OS.
    ///
    /// Search order:
    /// 1. LLAMA_BIN environment variable (if set and exists)
    /// 2. Resources/bin/{OS}/ relative to executable
    /// 3. Resources/bin/{OS}/ relative to current working directory
    /// 4. Resources/bin/{OS}/ relative to crate root (for development)
    fn get_llama_binary_path() -> Result<String> {
        // 1. Check LLAMA_BIN environment variable first (allows override)
        if let Ok(llama_bin) = env::var("LLAMA_BIN") {
            if std::path::Path::new(&llama_bin).exists() {
                info!("Using llama binary from LLAMA_BIN env: {}", llama_bin);
                return Ok(llama_bin);
            } else {
                warn!(
                    "LLAMA_BIN set but file doesn't exist: {}, falling back to auto-detection",
                    llama_bin
                );
            }
        }

        // Determine OS-specific binary name and folder
        let (os_folder, binary_name) = Self::get_platform_binary_info();
        info!(
            "Auto-detecting llama binary for OS: {} (binary: {})",
            os_folder, binary_name
        );

        // Get potential base directories
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));

        let cwd = std::env::current_dir().ok();

        // Build list of directories to search
        let mut search_dirs: Vec<PathBuf> = Vec::new();

        if let Some(ref exe) = exe_dir {
            search_dirs.push(exe.clone());
            // Also check parent directories (for bundled apps)
            if let Some(parent) = exe.parent() {
                search_dirs.push(parent.to_path_buf());
                if let Some(grandparent) = parent.parent() {
                    search_dirs.push(grandparent.to_path_buf());
                }
            }
        }

        if let Some(ref cwd_path) = cwd {
            search_dirs.push(cwd_path.clone());
        }

        // In development builds, also check relative to the crate source directory
        #[cfg(debug_assertions)]
        {
            let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            search_dirs.push(crate_dir);
        }

        // Search for binary in each potential location
        // Check both "Resources" (uppercase) and "resources" (lowercase, Tauri v2 bundle)
        let resource_folder_names = ["Resources", "resources"];
        for base_dir in &search_dirs {
            for resource_folder in &resource_folder_names {
            let bin_dir = base_dir.join(resource_folder).join("bin").join(os_folder);

            if bin_dir.exists() {
                // Search for the binary in subdirectories (e.g., llama-b6970-bin-win-cuda-12.4-x64/)
                if let Ok(entries) = std::fs::read_dir(&bin_dir) {
                    for entry in entries.flatten() {
                        let entry_path = entry.path();
                        if entry_path.is_dir() {
                            let potential_binary = entry_path.join(binary_name);
                            if potential_binary.exists() {
                                info!("Found llama binary at: {}", potential_binary.display());
                                return Ok(potential_binary.to_string_lossy().to_string());
                            }
                        }
                    }
                }

                // Also check directly in the OS folder
                let direct_binary = bin_dir.join(binary_name);
                if direct_binary.exists() {
                    info!("Found llama binary at: {}", direct_binary.display());
                    return Ok(direct_binary.to_string_lossy().to_string());
                }
            }
            } // end resource_folder_names loop
        }

        let arch = Self::get_arch_hint();
        warn!(
            "Llama binary not found. Searched in Resources/bin/{os_folder}/ for '{binary_name}'.\n\
             Please either:\n\
             1. Set LLAMA_BIN environment variable to the full path\n\
             2. Place the binary in Resources/bin/{os_folder}/<subfolder>/\n\
             \n\
             Expected binary name: {binary_name}\n\
             OS detected: {os_folder}\n\
             Architecture: {arch}\n\
             Searched directories: {:?}",
            search_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

        // Return empty string instead of crashing - allows the HTTP server to start.
        // Models and binaries can be downloaded later via the model registry.
        Ok(String::new())
    }

    /// Returns (os_folder_name, binary_name) for the current platform and architecture
    fn get_platform_binary_info() -> (&'static str, &'static str) {
        #[cfg(target_os = "windows")]
        {
            ("Windows", "llama-server.exe")
        }

        // macOS Apple Silicon (M1/M2/M3/M4)
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            ("MacOS", "llama-server")
            // Will search in: Resources/bin/MacOS/llama-*-macos-arm64/
        }

        // macOS Intel
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            ("MacOS", "llama-server")
            // Will search in: Resources/bin/MacOS/llama-*-macos-x64/
        }

        #[cfg(target_os = "linux")]
        {
            ("Linux", "llama-server")
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            compile_error!(
                "Unsupported operating system. Only Windows, macOS, and Linux are supported."
            );
        }
    }

    /// Returns the current system architecture string for logging and binary matching
    fn get_arch_hint() -> &'static str {
        #[cfg(target_arch = "x86_64")]
        {
            "x64"
        }
        #[cfg(target_arch = "aarch64")]
        {
            "arm64"
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            "unknown"
        }
    }

    fn auto_detect_threads() -> u32 {
        let num_cpus = num_cpus::get() as u32;
        info!("Auto‑detected CPU cores: {}", num_cpus);

        match num_cpus {
            1..=2 => 1,
            3..=4 => (num_cpus * 2) / 3,
            5..=8 => (num_cpus * 3) / 5,
            9..=16 => num_cpus / 2,
            17..=32 => (num_cpus * 2) / 5,
            _ => 16,
        }
    }

    fn auto_detect_gpu_layers() -> u32 {
        // NVIDIA GPU detection via NVML (only when nvidia feature is enabled).
        // When NVML finds no NVIDIA GPU, fall through to the OS-tool detection
        // below so AMD/Intel/Arc GPUs still get layers offloaded — previously
        // this path returned 0 and non-NVIDIA GPUs ran with no offload at all.
        #[cfg(all(feature = "nvidia", any(target_os = "windows", target_os = "linux")))]
        {
            if let Ok(nvml) = Nvml::init() {
                if let Ok(device_count) = nvml.device_count() {
                    if device_count > 0 {
                        if let Ok(first_gpu) = nvml.device_by_index(0) {
                            if let Ok(memory) = first_gpu.memory_info() {
                                let vram_gb = memory.total / 1024 / 1024 / 1024;
                                let layers = match vram_gb {
                                    0..=4 => 12,
                                    5..=8 => 20,
                                    9..=12 => 32,
                                    13..=16 => 40,
                                    _ => 50,
                                };
                                info!(
                                    "Auto‑detected NVIDIA GPU layers: {} ({} GB VRAM)",
                                    layers, vram_gb
                                );
                                return layers;
                            }
                        }
                    }
                }
            }
            info!("No NVIDIA GPU via NVML — falling back to OS-tool GPU detection (AMD/Intel)");
            return detect_gpu_layers_via_system_tools();
        }

        // Without the nvidia feature: go straight to the OS-tool detection.
        #[cfg(not(all(feature = "nvidia", any(target_os = "windows", target_os = "linux"))))]
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            return detect_gpu_layers_via_system_tools();
        }

        // Shared OS-tool GPU detection — nvidia-smi (NVIDIA) first, then
        // DXGI/WMI VRAM (AMD/Intel on Windows). Nested fn so both cfg branches
        // above can call it; block items are visible regardless of declaration
        // order. Body unchanged from the previous non-NVML fallback path.
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        fn detect_gpu_layers_via_system_tools() -> u32 {
            use std::process::{Command, Stdio};

            // ── 1. Try nvidia-smi (NVIDIA GPU) ────────────────────────────────
            #[cfg(target_os = "windows")]
            let child = {
                use std::os::windows::process::CommandExt;
                Command::new("nvidia-smi")
                    .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .creation_flags(0x08000000) // CREATE_NO_WINDOW — no console flash
                    .spawn()
            };

            #[cfg(not(target_os = "windows"))]
            let child = Command::new("nvidia-smi")
                .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            match child {
                Ok(mut process) => {
                    let start = std::time::Instant::now();
                    loop {
                        match process.try_wait() {
                            Ok(Some(status)) => {
                                if status.success() {
                                    if let Ok(output) = process.wait_with_output() {
                                        let stdout = String::from_utf8_lossy(&output.stdout);
                                        if let Some(vram_mb_str) = stdout.lines().next() {
                                            if let Ok(vram_mb) = vram_mb_str.trim().parse::<u64>() {
                                                let vram_gb = vram_mb / 1024;
                                                let layers = match vram_gb {
                                                    0..=4 => 12,
                                                    5..=8 => 20,
                                                    9..=12 => 32,
                                                    13..=16 => 40,
                                                    _ => 50,
                                                };
                                                info!(
                                                    "Auto‑detected NVIDIA GPU layers via nvidia-smi: {} ({} GB VRAM)",
                                                    layers, vram_gb
                                                );
                                                return layers;
                                            }
                                        }
                                    }
                                }
                                // nvidia-smi ran but gave no useful output — not NVIDIA
                                break;
                            }
                            Ok(None) => {
                                if start.elapsed() > std::time::Duration::from_secs(5) {
                                    let _ = process.kill();
                                    let _ = process.wait();
                                    info!("nvidia-smi timed out — not an NVIDIA system");
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => {
                    info!("nvidia-smi not available — checking for AMD/Intel GPU");
                }
            }

            // ── 2. AMD / Intel GPU — query VRAM (registry first, WMI second) ──
            // Primary source: the display adapter's HardwareInformation.qwMemorySize
            // registry value — a 64-bit QWORD that reports dedicated VRAM correctly
            // for modern discrete AMD/Intel/Arc cards (16 GB reads as 16 GB).
            // Secondary source: Win32_VideoController.AdapterRAM — a 32-bit DWORD
            // that saturates at 4 GB and misreports iGPU shared memory, so values
            // from it carry a conservative >=4 GB clamp.
            // The script tags its output with the source (REG:/WMI:) so the parse
            // below knows whether the value is exact or saturated.
            #[cfg(target_os = "windows")]
            {
                let ps_script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$max = [int64]0
$cls = Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}'
foreach ($c in @($cls)) {
    if (-not $c) { continue }
    $v = Get-ItemProperty -Path $c.PSPath -Name 'HardwareInformation.qwMemorySize'
    if ($v) {
        $q = [int64]$v.'HardwareInformation.qwMemorySize'
        if ($q -gt $max) { $max = $q }
    }
}
if ($max -gt 0) {
    Write-Output ('REG:' + [math]::Round($max / 1MB))
} else {
    $gpu = Get-WmiObject Win32_VideoController | Sort-Object AdapterRAM -Descending | Select-Object -First 1
    if ($gpu -and $gpu.AdapterRAM -gt 0) {
        Write-Output ('WMI:' + [math]::Round([int64]$gpu.AdapterRAM / 1MB))
    } else {
        Write-Output 'NONE:0'
    }
}
"#;
                let result = {
                    use std::os::windows::process::CommandExt;
                    Command::new("powershell")
                        .args([
                            "-NonInteractive",
                            "-NoProfile",
                            "-Command",
                            ps_script.trim(),
                        ])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .creation_flags(0x08000000) // CREATE_NO_WINDOW — no console flash
                        .output()
                };

                if let Ok(output) = result {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let line = stdout.trim();
                    let (source, mb_str) = line.split_once(':').unwrap_or(("", line));
                    if let Ok(vram_mb) = mb_str.trim().parse::<u64>() {
                        if vram_mb > 0 {
                            let effective_vram_gb = if source == "REG" {
                                // 64-bit registry value: exact, no clamp needed
                                vram_mb / 1024
                            } else if vram_mb >= 4000 {
                                // AdapterRAM DWORD saturates at ~4294 MB for any GPU
                                // over 4 GB — the true size is unknowable from this
                                // source, so treat as "at least 4 GB".
                                (vram_mb / 1024).max(4)
                            } else {
                                vram_mb / 1024
                            };

                            let layers = match effective_vram_gb {
                                0..=2 => 8,   // Intel HD / iGPU with shared memory
                                3..=4 => 16,  // AMD RX 580 / Intel Arc A380
                                5..=8 => 24,  // AMD RX 6600 / Intel Arc A770
                                9..=12 => 32, // AMD RX 6800 / RX 7800
                                13..=16 => 40, // AMD RX 6900 XT / RX 7900
                                _ => 48,      // AMD RX 7900 XTX / high-end cards
                            };
                            info!(
                                "Auto-detected AMD/Intel GPU layers: {} ({} MB VRAM via {}, {} GB effective)",
                                layers, vram_mb, source, effective_vram_gb
                            );
                            return layers;
                        }
                    }
                }

                // Loud, not silent: a GPU-accelerated engine may still be selected
                // by the installer/registry (Vulkan ICD present) while VRAM could
                // not be measured. thread_server logs error! and /healthz carries a
                // warning when a GPU engine ends up with gpu_layers == 0.
                warn!(
                    "GPU VRAM could not be determined from the registry or WMI — \
                     gpu_layers=0 (no GPU offload). If a GPU engine is active, \
                     inference will run at CPU speed until this is resolved."
                );
                0
            }

            // Linux without NVIDIA: no AMD/Intel detection implemented yet
            #[cfg(not(target_os = "windows"))]
            {
                info!("No NVIDIA GPU detected (nvidia-smi not available), using CPU defaults (0 GPU layers)");
                0
            }
        }

        // macOS Apple Silicon (M1/M2/M3/M4): Use Metal with unified memory
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            // Apple Silicon has unified memory architecture with Metal GPU support
            // Use moderate GPU layers that work well with llama.cpp's Metal backend
            // M1: 8GB-16GB unified, M2: 8GB-24GB, M3/M4: 8GB-128GB
            let total_mem_gb = {
                let mut sys = System::new_all();
                sys.refresh_memory();
                sys.total_memory() / 1024 / 1024 / 1024
            };

            // Scale GPU layers based on unified memory (shared between CPU and GPU)
            let layers = match total_mem_gb {
                0..=8 => 24,   // Base M1/M2 (8GB)
                9..=16 => 32,  // M1/M2 Pro or 16GB models
                17..=32 => 40, // M1/M2/M3 Max
                33..=64 => 48, // M2/M3 Ultra
                _ => 56,       // M3 Ultra 128GB+
            };
            info!(
                "Apple Silicon detected ({} GB unified memory), using Metal GPU layers: {}",
                total_mem_gb, layers
            );
            layers
        }

        // macOS Intel: No Metal GPU acceleration, use CPU-only mode
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            // Intel Macs don't have efficient Metal GPU support for LLM inference
            // Use CPU-only mode (0 GPU layers) for best compatibility
            info!("Intel Mac detected, using CPU-only mode (0 GPU layers)");
            0
        }
    }

    fn auto_detect_ctx_size(model_path: &str) -> u32 {
        let inferred = Self::read_ctx_size_from_model_path(model_path).unwrap_or_else(|| {
            info!("Falling back to default context size (8192)");
            8192
        });
        let adjusted = Self::adjust_ctx_size_for_system(inferred);
        info!("Final context size: {} (inferred: {})", adjusted, inferred);
        adjusted
    }

    fn read_ctx_size_from_model_path(model_path: &str) -> Option<u32> {
        // Simple heuristic based on model filename patterns
        let path_lower = model_path.to_lowercase();

        if path_lower.contains("32k") {
            Some(32768)
        } else if path_lower.contains("16k") {
            Some(16384)
        } else if path_lower.contains("8k") {
            Some(8192)
        } else if path_lower.contains("4k") {
            Some(4096)
        } else if path_lower.contains("2k") {
            Some(2048)
        } else if path_lower.contains("7b")
            || path_lower.contains("8b")
            || path_lower.contains("13b")
        {
            Some(4096)
        } else if path_lower.contains("34b") || path_lower.contains("70b") {
            Some(8192)
        } else {
            // Default fallback
            Some(8192)
        }
    }

    /// Largest context this machine's free RAM can support, using the same
    /// rule the auto-detector applies.
    ///
    /// Public so the runtime can bound a GGUF-declared context the same way,
    /// rather than restating the arithmetic and letting the two drift.
    pub fn ram_safe_ctx_size(desired_ctx: u32) -> u32 {
        Self::adjust_ctx_size_for_system(desired_ctx)
    }

    fn adjust_ctx_size_for_system(inferred_ctx: u32) -> u32 {
        let mut system = System::new_all();
        system.refresh_memory();

        let available_ram_gb = system.available_memory() / 1024 / 1024 / 1024;
        let _total_ram_gb = system.total_memory() / 1024 / 1024 / 1024;

        let required_ram_gb = (inferred_ctx as f32 / 4096.0) * 1.5;
        if available_ram_gb < required_ram_gb as u64 {
            let adjusted = (available_ram_gb as f32 * 4096.0 / 1.5) as u32;
            let safe_ctx = adjusted.min(inferred_ctx).max(2048);
            warn!(
                "Reducing context size from {} → {} due to limited RAM ({}GB available)",
                inferred_ctx, safe_ctx, available_ram_gb
            );
            safe_ctx
        } else {
            inferred_ctx
        }
    }

    fn auto_detect_batch_size(gpu_layers: u32, ctx_size: u32) -> u32 {
        let mut system = System::new_all();
        system.refresh_memory();

        let available_mb = system.available_memory() / 1024;
        let has_gpu = gpu_layers > 0;
        let memory_per_batch = Self::estimate_memory_per_batch(ctx_size, has_gpu);
        let safe_available_mb = (available_mb as f32 * 0.6) as u32;
        let max_batch = (safe_available_mb as f32 / memory_per_batch).max(1.0) as u32;

        let optimal = Self::apply_batch_limits(max_batch, ctx_size, has_gpu);
        info!(
            "Auto batch size: {} (ctx: {}, GPU: {}, est mem: {:.1}MB/batch)",
            optimal, ctx_size, has_gpu, memory_per_batch
        );
        optimal
    }

    fn estimate_memory_per_batch(ctx_size: u32, has_gpu: bool) -> f32 {
        if has_gpu {
            (ctx_size as f32 / 1024.0) * 0.5
        } else {
            (ctx_size as f32 / 1024.0) * 1.2
        }
    }

    fn apply_batch_limits(batch_size: u32, ctx_size: u32, _has_gpu: bool) -> u32 {
        let limited = batch_size.clamp(16, 1024);
        match ctx_size {
            0..=2048 => limited.min(512),
            2049..=4096 => limited.min(384),
            4097..=8192 => limited.min(256),
            8193..=16384 => limited.min(128),
            16385..=32768 => limited.min(64),
            _ => limited.min(32),
        }
    }

    pub fn print_config(&self) {
        info!("Current Configuration:");
        info!("- Model Path: {}", self.model_path);
        info!("- Llama Binary: {}", self.llama_bin);
        info!("- Context Size: {}", self.ctx_size);
        if self.batch_size_auto {
            info!("- Batch Size: engine default (n_batch 2048 / n_ubatch 512)");
        } else {
            info!("- Batch Size: {}", self.batch_size);
        }
        info!("- Threads: {} (generation) / {} (prompt processing)", self.threads, self.threads_batch);
        if self.gpu_layers_auto {
            info!("- GPU Layers: dynamic per-model (fallback: {})", self.gpu_layers);
        } else {
            info!("- GPU Layers: {}", self.gpu_layers);
        }
        if self.cache_reuse > 0 {
            info!("- KV Cache Reuse: min chunk {} tokens", self.cache_reuse);
        } else {
            info!("- KV Cache Reuse: disabled");
        }
        info!("- Max Streams: {}", self.max_concurrent_streams);
        info!("- API: {}:{}", self.api_host, self.api_port);
        info!("- Backend: {}:{}", self.llama_host, self.llama_port);
        info!("- Queue Size: {}", self.queue_size);
        info!("- Queue Timeout: {}s", self.queue_timeout_seconds);
        info!("- Backend URL: {}", self.backend_url);
    }

    pub fn api_addr(&self) -> SocketAddr {
        format!("{}:{}", self.api_host, self.api_port)
            .parse()
            .unwrap()
    }
}

/// Measure GPU VRAM once per process, for the dynamic offload calculation.
///
/// Returns (megabytes, source) where source is one of:
///   "nvml-free"       — free VRAM via NVML (most accurate; display use excluded)
///   "nvidia-smi-free" — free VRAM via nvidia-smi
///   "registry-total"  — total adapter VRAM via HardwareInformation.qwMemorySize
///   "wmi-total"       — total via Win32_VideoController.AdapterRAM (32-bit, ≤4GB)
/// Sources ending in "total" require the caller to reserve the OS/display share
/// (see gpu_offload::VramKind). Cached in a OnceLock because the registry/WMI
/// probes spawn PowerShell (hundreds of ms).
pub fn detect_available_vram_mb() -> Option<(u64, &'static str)> {
    use std::sync::OnceLock;
    static VRAM: OnceLock<Option<(u64, &'static str)>> = OnceLock::new();

    *VRAM.get_or_init(|| {
        // 1. NVML free VRAM (NVIDIA)
        #[cfg(all(feature = "nvidia", any(target_os = "windows", target_os = "linux")))]
        {
            if let Ok(nvml) = Nvml::init() {
                if let Ok(device) = nvml.device_by_index(0) {
                    if let Ok(mem) = device.memory_info() {
                        let free_mb = mem.free / 1024 / 1024;
                        if free_mb > 0 {
                            info!("VRAM probe: {} MB free via NVML", free_mb);
                            return Some((free_mb, "nvml-free"));
                        }
                    }
                }
            }
        }

        // 2. nvidia-smi free VRAM (NVIDIA without NVML)
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            use std::process::{Command, Stdio};
            let mut cmd = Command::new("nvidia-smi");
            cmd.args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            if let Ok(output) = cmd.output() {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Some(mb) = stdout.lines().next().and_then(|l| l.trim().parse::<u64>().ok()) {
                        if mb > 0 {
                            info!("VRAM probe: {} MB free via nvidia-smi", mb);
                            return Some((mb, "nvidia-smi-free"));
                        }
                    }
                }
            }
        }

        // 3. Display-adapter registry / WMI (AMD, Intel, Arc — total VRAM only)
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            use std::process::{Command, Stdio};

            let ps_script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$max = [int64]0
$cls = Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}'
foreach ($c in @($cls)) {
    if (-not $c) { continue }
    $v = Get-ItemProperty -Path $c.PSPath -Name 'HardwareInformation.qwMemorySize'
    if ($v) {
        $q = [int64]$v.'HardwareInformation.qwMemorySize'
        if ($q -gt $max) { $max = $q }
    }
}
if ($max -gt 0) {
    Write-Output ('REG:' + [math]::Round($max / 1MB))
} else {
    $gpu = Get-WmiObject Win32_VideoController | Sort-Object AdapterRAM -Descending | Select-Object -First 1
    if ($gpu -and $gpu.AdapterRAM -gt 0) {
        Write-Output ('WMI:' + [math]::Round([int64]$gpu.AdapterRAM / 1MB))
    } else {
        Write-Output 'NONE:0'
    }
}
"#;
            let result = Command::new("powershell")
                .args(["-NonInteractive", "-NoProfile", "-Command", ps_script.trim()])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .output();

            if let Ok(output) = result {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let line = stdout.trim();
                if let Some((source, mb_str)) = line.split_once(':') {
                    if let Ok(mb) = mb_str.trim().parse::<u64>() {
                        if mb > 0 {
                            let tag = if source == "REG" { "registry-total" } else { "wmi-total" };
                            info!("VRAM probe: {} MB total via {}", mb, tag);
                            return Some((mb, tag));
                        }
                    }
                }
            }
        }

        warn!("VRAM probe: no GPU memory measurable on this machine");
        None
    })
}

/// Canonical app data directory used by every crate in this workspace.
///
/// Windows : `%LOCALAPPDATA%\Offline Counsel AI`   (AppData\Local)
/// macOS   : `~/Library/Application Support/Offline Counsel AI`
/// Linux   : `~/.local/share/Offline Counsel AI`
///
/// Uses `dirs::data_local_dir()` so that on Windows we always land in
/// AppData\Local (not Roaming), matching where the Tauri shell stores
/// its own data.
pub fn get_app_data_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .join("Offline Counsel AI")
}

#[cfg(test)]
mod context_sizing_tests {
    use super::*;

    /// The filename heuristic is blind to most real model names.
    ///
    /// This is not a bug to fix in the heuristic — you cannot infer a context
    /// window from a filename in general — it is the reason the GGUF header
    /// must win whenever CTX_SIZE is "auto". Pinned here so nobody later
    /// mistakes the 8192 default for a considered answer.
    #[test]
    fn the_filename_heuristic_cannot_see_gemma_and_falls_back_to_the_default() {
        // Gemma 3 declares 32768 in its GGUF header; the filename says nothing.
        assert_eq!(
            Config::read_ctx_size_from_model_path("models/gemma-3-1b-it-Q4_K_M.gguf"),
            Some(8192),
            "a name with no size hint must land on the default, not a guess"
        );
        // Same for other common names carrying no hint.
        for name in ["qwen2.5-coder-1.5b.gguf", "phi-4-mini.gguf", "smollm2-360m.gguf"] {
            assert_eq!(Config::read_ctx_size_from_model_path(name), Some(8192), "{}", name);
        }
        // The hints it CAN read still work.
        assert_eq!(Config::read_ctx_size_from_model_path("mistral-7b-32k.gguf"), Some(32768));
        assert_eq!(Config::read_ctx_size_from_model_path("llama-3-8b.gguf"), Some(4096));
    }

    /// The RAM bound is shared with the runtime rather than restated there.
    #[test]
    fn the_ram_bound_never_exceeds_what_was_asked_for() {
        for desired in [2048u32, 8192, 32_768, 131_072] {
            let bound = Config::ram_safe_ctx_size(desired);
            assert!(
                bound <= desired,
                "the RAM bound must never raise a request: asked {}, got {}",
                desired,
                bound
            );
            assert!(bound >= 2048, "a usable floor must survive: got {}", bound);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function to create a test Config with default values
    fn create_test_config() -> Config {
        Config {
            model_path: "/test/model.gguf".to_string(),
            mmproj_path: String::new(),
            llama_bin: "/test/llama-server".to_string(),
            llama_host: "127.0.0.1".to_string(),
            llama_port: 9639,
            ctx_size: 8192,
            batch_size: 128,
            ctx_size_auto: false,
            batch_size_auto: false,
            threads: 6,
            threads_batch: 6,
            gpu_layers: 20,
            gpu_layers_auto: false,
            cache_reuse: 256,
            app_disk_limit_mb: DEFAULT_APP_DISK_LIMIT_MB,
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
        }
    }

    // ===== Configuration Structure Tests =====

    #[test]
    fn test_config_creation_with_default_values() {
        let config = create_test_config();

        assert_eq!(config.model_path, "/test/model.gguf");
        assert_eq!(config.llama_bin, "/test/llama-server");
        assert_eq!(config.api_port, 8888);
        assert_eq!(config.llama_port, 9639);
    }

    #[test]
    fn test_config_clone() {
        let config1 = create_test_config();
        let config2 = config1.clone();

        assert_eq!(config1.api_host, config2.api_host);
        assert_eq!(config1.threads, config2.threads);
        assert_eq!(config1.gpu_layers, config2.gpu_layers);
    }

    // ===== API Address Tests =====

    #[test]
    fn test_api_addr_parsing() {
        let config = create_test_config();
        let addr = config.api_addr();

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 8888);
    }

    #[test]
    fn test_api_addr_with_different_ports() {
        let mut config = create_test_config();
        config.api_port = 3000;

        let addr = config.api_addr();
        assert_eq!(addr.port(), 3000);
    }

    #[test]
    fn test_api_addr_with_zero_address() {
        let mut config = create_test_config();
        config.api_host = "0.0.0.0".to_string();
        config.api_port = 5000;

        let addr = config.api_addr();
        assert_eq!(addr.port(), 5000);
        // 0.0.0.0 represents all interfaces
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
    }

    // ===== Timeout Tests =====

    #[test]
    fn test_config_timeouts_are_positive() {
        let config = create_test_config();

        assert!(config.health_timeout_seconds > 0);
        assert!(config.generate_timeout_seconds > 0);
        assert!(config.stream_timeout_seconds > 0);
        assert!(config.health_check_timeout_seconds > 0);
    }

    #[test]
    fn test_health_check_timeout_greater_than_health_timeout() {
        let config = create_test_config();

        // Health check timeout should typically be longer than regular health timeout
        assert!(config.health_check_timeout_seconds >= config.health_timeout_seconds);
    }

    // ===== Resource Limits Tests =====

    #[test]
    fn test_max_concurrent_streams_is_positive() {
        let config = create_test_config();
        assert!(config.max_concurrent_streams > 0);
    }

    #[test]
    fn test_requests_per_second_is_reasonable() {
        let config = create_test_config();

        // Should be a reasonable number (not 0, not extremely high)
        assert!(config.requests_per_second > 0);
        assert!(config.requests_per_second <= 1000);
    }

    #[test]
    fn test_queue_size_is_positive() {
        let config = create_test_config();
        assert!(config.queue_size > 0);
    }

    // ===== Context and Batch Size Tests =====

    #[test]
    fn test_context_size_within_valid_range() {
        let config = create_test_config();

        // Context size should be between 512 and 32768
        assert!(config.ctx_size >= 512);
        assert!(config.ctx_size <= 32768);
    }

    #[test]
    fn test_batch_size_valid_range() {
        let config = create_test_config();

        // Batch size should be between 16 and 1024
        assert!(config.batch_size >= 16);
        assert!(config.batch_size <= 1024);
    }

    #[test]
    fn test_batch_size_reasonable_vs_context() {
        let config = create_test_config();

        // Batch size should typically be less than context size
        assert!(config.batch_size < config.ctx_size);
    }

    // ===== Thread Configuration Tests =====

    #[test]
    fn test_threads_is_positive() {
        let config = create_test_config();
        assert!(config.threads > 0);
    }

    #[test]
    fn test_threads_within_reasonable_range() {
        let config = create_test_config();

        // Should not exceed typical CPU thread count significantly
        assert!(config.threads <= 256);
    }

    // ===== GPU Configuration Tests =====

    #[test]
    fn test_gpu_layers_non_negative() {
        let config = create_test_config();
        assert!(config.gpu_layers <= config.ctx_size);
    }

    #[test]
    fn test_gpu_layers_within_range() {
        let config = create_test_config();

        // GPU layers should typically be 0-50
        assert!(config.gpu_layers <= 100);
    }

    // ===== Port Configuration Tests =====

    #[test]
    fn test_api_port_valid() {
        let config = create_test_config();
        assert!(config.api_port > 0);
        assert!(config.api_port != config.llama_port);
    }

    #[test]
    fn test_llama_port_valid() {
        let config = create_test_config();
        assert!(config.llama_port > 0);
    }

    #[test]
    fn test_prometheus_port_valid() {
        let config = create_test_config();
        assert!(config.prometheus_port > 0);
    }

    #[test]
    fn test_ports_are_different() {
        let config = create_test_config();

        // Ports should be unique to avoid conflicts
        assert_ne!(config.api_port, config.llama_port);
        assert_ne!(config.api_port, config.prometheus_port);
        assert_ne!(config.llama_port, config.prometheus_port);
    }

    // ===== Path Configuration Tests =====

    #[test]
    fn test_model_path_not_empty() {
        let config = create_test_config();
        assert!(!config.model_path.is_empty());
    }

    #[test]
    fn test_llama_bin_not_empty() {
        let config = create_test_config();
        assert!(!config.llama_bin.is_empty());
    }

    #[test]
    fn test_backend_url_not_empty() {
        let config = create_test_config();
        assert!(!config.backend_url.is_empty());
    }

    #[test]
    fn test_backend_url_format() {
        let config = create_test_config();

        // Should be a valid URL format
        assert!(
            config.backend_url.starts_with("http://") || config.backend_url.starts_with("https://")
        );
    }

    // ===== Host Configuration Tests =====

    #[test]
    fn test_api_host_not_empty() {
        let config = create_test_config();
        assert!(!config.api_host.is_empty());
    }

    #[test]
    fn test_llama_host_not_empty() {
        let config = create_test_config();
        assert!(!config.llama_host.is_empty());
    }

    // ===== Grace Period Tests =====

    #[test]
    fn test_hot_swap_grace_positive() {
        let config = create_test_config();
        assert!(config.hot_swap_grace_seconds > 0);
    }

    #[test]
    fn test_hot_swap_grace_reasonable() {
        let config = create_test_config();

        // Grace period should be less than 5 minutes
        assert!(config.hot_swap_grace_seconds < 300);
    }

    // ===== Auto-detect Helper Tests =====

    #[test]
    fn test_auto_detect_threads_returns_positive() {
        let threads = Config::auto_detect_threads();
        assert!(threads > 0);
    }

    #[test]
    fn test_auto_detect_gpu_layers_non_negative() {
        let layers = Config::auto_detect_gpu_layers();
        assert!(layers <= 512);
    }

    #[test]
    fn test_apply_batch_limits_small_context() {
        // For context < 2048, batch should be limited to 512
        let batch = Config::apply_batch_limits(1024, 1024, false);
        assert!(batch <= 512);
    }

    #[test]
    fn test_apply_batch_limits_medium_context() {
        // For context 2048-4096, batch should be limited to 384
        let batch = Config::apply_batch_limits(1024, 3000, false);
        assert!(batch <= 384);
    }

    #[test]
    fn test_apply_batch_limits_large_context() {
        // For context 16384-32768, batch should be limited to 64
        let batch = Config::apply_batch_limits(1024, 24576, false);
        assert!(batch <= 64);
    }

    #[test]
    fn test_apply_batch_limits_minimum() {
        // Batch size should always be at least 16
        let batch = Config::apply_batch_limits(1, 8192, false);
        assert!(batch >= 16);
    }

    #[test]
    fn test_estimate_memory_per_batch_cpu() {
        let memory_cpu = Config::estimate_memory_per_batch(8192, false);
        assert!(memory_cpu > 0.0);
    }

    #[test]
    fn test_estimate_memory_per_batch_gpu() {
        let memory_gpu = Config::estimate_memory_per_batch(8192, true);
        assert!(memory_gpu > 0.0);
    }

    #[test]
    fn test_estimate_memory_gpu_less_than_cpu() {
        let memory_cpu = Config::estimate_memory_per_batch(8192, false);
        let memory_gpu = Config::estimate_memory_per_batch(8192, true);

        // GPU memory estimate should be less than CPU
        assert!(memory_gpu < memory_cpu);
    }

    // ===== Queue Configuration Tests =====

    #[test]
    fn test_queue_timeout_is_positive() {
        let config = create_test_config();
        assert!(config.queue_timeout_seconds > 0);
    }

    #[test]
    fn test_queue_timeout_less_than_generate_timeout() {
        let config = create_test_config();

        // Queue timeout should be less than or equal to generate timeout
        assert!(config.queue_timeout_seconds <= config.generate_timeout_seconds);
    }

    // ===== Integration Tests =====

    #[test]
    fn test_config_values_consistency() {
        let config = create_test_config();

        // Verify all timeout values are reasonable
        assert!(config.health_timeout_seconds <= 3600); // Max 1 hour
        assert!(config.generate_timeout_seconds <= 1800); // Max 30 mins
        assert!(config.stream_timeout_seconds <= 3600); // Max 1 hour
        assert!(config.health_check_timeout_seconds <= 3600); // Max 1 hour
    }

    #[test]
    fn test_config_backend_url_consistency() {
        let config = create_test_config();

        // Backend URL should contain the llama host and port
        assert!(
            config.backend_url.contains(&config.llama_host)
                || config.backend_url.contains("127.0.0.1")
                || config.backend_url.contains("localhost")
        );
    }

    #[test]
    fn test_config_all_fields_initialized() {
        let config = create_test_config();

        // Ensure all critical fields have valid values
        assert!(!config.model_path.is_empty());
        assert!(!config.llama_bin.is_empty());
        assert!(!config.api_host.is_empty());
        assert!(!config.llama_host.is_empty());
        assert!(config.threads > 0);
        assert!(config.gpu_layers <= config.ctx_size);
        assert!(config.api_port > 0);
        assert!(config.llama_port > 0);
    }
}
