//! GGUF Runtime Adapter
//!
//! Wraps the existing llama-server.exe (llama.cpp) for GGUF models.
//! This adapter spawns the llama-server process and proxies requests via HTTP.

use async_trait::async_trait;
use super::runtime_trait::*;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn, error};
use tokio::time::sleep;

pub struct GGUFRuntime {
    config: Option<RuntimeConfig>,
    server_process: Arc<Mutex<Option<Child>>>,
    http_client: reqwest::Client,
    base_url: String,
    /// Real allocated context, read back from GET /props after startup.
    /// 0 = not yet read / read failed (never a valid ctx size).
    effective_context: Arc<AtomicU32>,
    /// Guards against concurrent restart attempts
    restarting: Arc<AtomicBool>,
}

impl GGUFRuntime {
    pub fn new() -> Self {
        Self {
            config: None,
            server_process: Arc::new(Mutex::new(None)),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            base_url: String::new(),
            effective_context: Arc::new(AtomicU32::new(0)),
            restarting: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Read GET /props and extract default_generation_settings.n_ctx - the
    /// context size llama-server actually allocated for the loaded model.
    /// Verified live against b8037: this field exists and matches reality
    /// (confirmed by requesting 4096 and reading back 4096). Failure is
    /// logged, never fatal - callers fall back to the requested value.
    async fn read_effective_context(&self) {
        let url = format!("{}/props", self.base_url);
        let resp = match self.http_client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!("GET /props returned {} - effective context unknown", r.status());
                return;
            }
            Err(e) => {
                warn!("GET /props failed ({}) - effective context unknown", e);
                return;
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("GET /props response unparsable ({}) - effective context unknown", e);
                return;
            }
        };
        match body
            .get("default_generation_settings")
            .and_then(|s| s.get("n_ctx"))
            .and_then(|n| n.as_u64())
        {
            Some(n_ctx) if n_ctx > 0 => {
                self.effective_context
                    .store(n_ctx as u32, std::sync::atomic::Ordering::SeqCst);
                info!("Effective context (from /props): {} tokens", n_ctx);
            }
            _ => warn!("GET /props response had no default_generation_settings.n_ctx"),
        }
    }

    /// Start llama-server process
    /// Detect a crashed llama-server child process and restart it in the background.
    async fn try_restart_crashed_server(&self) {
        // Only one restart attempt at a time
        if self.restarting.swap(true, Ordering::SeqCst) {
            return;
        }

        let has_exited = {
            let mut guard = self.server_process.lock().await;
            if let Some(ref mut child) = *guard {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        error!("llama-server process exited unexpectedly ({})", status);
                        guard.take();
                        true
                    }
                    Ok(None) => false,  // still running — might just be slow
                    Err(e) => {
                        warn!("Could not check llama-server status: {}", e);
                        false
                    }
                }
            } else {
                false
            }
        };

        if has_exited {
            if let Some(ref config) = self.config {
                info!("Auto-restarting llama-server after crash...");
                let config = config.clone();
                let process = self.server_process.clone();
                let base_url = self.base_url.clone();
                let http_client = self.http_client.clone();
                let effective_context = self.effective_context.clone();
                let restarting = self.restarting.clone();

                tokio::spawn(async move {
                    match Self::spawn_server_process(&config) {
                        Ok(child) => {
                            *process.lock().await = Some(child);
                            // Wait for health
                            let restart_wait = crate::tuning::parse_or("HEALTH_CHECK_TIMEOUT_SECONDS", 120u64).max(10);
                            for attempt in 1..=(restart_wait / 2).max(1) {
                                sleep(Duration::from_secs(2)).await;
                                let health_url = format!("{}/health", base_url);
                                if let Ok(resp) = http_client.get(&health_url).send().await {
                                    if resp.status().is_success() {
                                        info!("llama-server restarted successfully after {} seconds", attempt * 2);
                                        // Re-read effective context
                                        let props_url = format!("{}/props", base_url);
                                        if let Ok(resp) = http_client.get(&props_url).send().await {
                                            if let Ok(json) = resp.json::<serde_json::Value>().await {
                                                if let Some(n_ctx) = json.get("default_generation_settings")
                                                    .and_then(|s| s.get("n_ctx"))
                                                    .and_then(|v| v.as_u64())
                                                {
                                                    effective_context.store(n_ctx as u32, Ordering::SeqCst);
                                                }
                                            }
                                        }
                                        restarting.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                            error!("llama-server restart timed out after {} seconds (HEALTH_CHECK_TIMEOUT_SECONDS)", restart_wait);
                            restarting.store(false, Ordering::SeqCst);
                        }
                        Err(e) => {
                            error!("Failed to restart llama-server: {}", e);
                            restarting.store(false, Ordering::SeqCst);
                        }
                    }
                });
            } else {
                self.restarting.store(false, Ordering::SeqCst);
            }
        } else {
            self.restarting.store(false, Ordering::SeqCst);
        }
    }

    /// Spawn the llama-server process with the given config. Factored out so
    /// both `start_server` and the auto-restart path can use the same logic.
    /// With the projector loaded and placed on the GPU: the chat layers it
    /// leaves room for. None: no projector, or it runs on the CPU.
    fn projector_layers(config: &RuntimeConfig) -> Option<u32> {
        let mmproj = config.mmproj_path.as_ref().filter(|_| config.projector_loaded)?;
        let bytes = std::fs::metadata(mmproj).map(|m| m.len()).unwrap_or(0);
        super::machine::projector_gpu_layers(bytes, super::machine::current().as_ref())
    }

    fn gpu_layers_for(config: &RuntimeConfig) -> u32 {
        match Self::projector_layers(config) {
            Some(layers) if layers < config.gpu_layers => {
                info!(
                    "Vision projector on the GPU: {} of {} chat layers stay there while images are read",
                    layers, config.gpu_layers
                );
                layers
            }
            _ => config.gpu_layers,
        }
    }

    fn spawn_server_process(config: &RuntimeConfig) -> anyhow::Result<Child> {
        let binary_path = config.runtime_binary.as_ref()
            .ok_or_else(|| anyhow::anyhow!("GGUF runtime requires runtime_binary path"))?;

        let mut cmd = Command::new(binary_path);
        cmd.arg("--model").arg(&config.model_path)
            .arg("--host").arg(&config.host)
            .arg("--port").arg(config.port.to_string())
            .arg("--ctx-size").arg(config.context_size.to_string())
            .arg("--threads").arg(config.threads.to_string())
            .arg("--n-gpu-layers").arg(Self::gpu_layers_for(config).to_string());

        if config.batch_size > 0 {
            cmd.arg("--batch-size").arg(config.batch_size.to_string());
        }
        if config.threads_batch > 0 {
            cmd.arg("--threads-batch").arg(config.threads_batch.to_string());
        }
        if config.cache_reuse > 0 {
            cmd.arg("--cache-reuse").arg(config.cache_reuse.to_string());
        }
        if let Some(mmproj) = config.mmproj_path.as_ref().filter(|_| config.projector_loaded) {
            // Vision model: load the multimodal projector. The path was
            // verified to exist by whoever built this config (model switch /
            // auto-load / dev override) — a missing file here is a hard spawn
            // failure by design, never a silent text-only start.
            cmd.arg("--mmproj").arg(mmproj);
            if Self::projector_layers(config).is_none() {
                info!("Vision projector on the CPU");
                cmd.arg("--no-mmproj-offload");
            }
        }
        if let Some(kv) = config.kv_cache_type.as_deref().filter(|t| *t != "f16") {
            // A quantized V cache requires flash attention; "auto" lets the
            // engine use it where the backend supports it (LLAMA_FLASH_ATTN).
            cmd.arg("--cache-type-k").arg(kv).arg("--cache-type-v").arg(kv);
        }
        if let Some(mb) = config.cache_ram_mb {
            cmd.arg("--cache-ram").arg(mb.to_string());
        }
        if let Some(ref slot_path) = config.slot_save_path {
            // Created here rather than at save time: llama-server resolves
            // filenames against this directory and will not create it itself,
            // so a missing directory turns every save into a runtime failure
            // long after the misconfiguration happened. A failure to create it
            // is not fatal - the flags are simply omitted and warm start is
            // unavailable, which is degraded but correct.
            if let Err(e) = std::fs::create_dir_all(slot_path) {
                warn!(
                    "Could not create the slot KV cache directory {} ({}). Warm start \
                     will be unavailable this run.",
                    slot_path.display(),
                    e
                );
            }
        }
        // ── Slots, and how --ctx-size is divided between them ──────────────
        //
        // These branches are mutually exclusive by construction:
        // build_runtime_config clears slot_save_path whenever more than one
        // slot is requested, because slot save/restore needs a stable slot
        // index and llama-server assigns slots by prompt similarity rather
        // than by client.
        if config.parallel_slots > 1 {
            cmd.arg("--parallel").arg(config.parallel_slots.to_string());

            // --kv-unified is NOT optional alongside --parallel > 1 here, and
            // is the single flag that makes a 75/25 split possible at all.
            //
            // Without it llama.cpp partitions the context evenly, so two slots
            // would mean each gets --ctx-size / 2. The assistant would receive
            // 50% of a window it is not allowed to fill and - far worse - the
            // main discussion would lose half the room its documents need,
            // silently, as a side effect of a concurrency change.
            //
            // With it there is ONE KV buffer shared by both sequences. Neither
            // client holds a reserved partition; each builds a prompt within
            // the share shared_state::context_budget_for allows, and those
            // shares are sized so two simultaneous requests at full budget sum
            // to the window rather than overcommitting it.
            cmd.arg("--kv-unified");
        } else if let Some(ref slot_path) = config.slot_save_path.as_ref().filter(|p| p.exists()) {
            // Single slot: "slot 0" is a stable target, so warm start works,
            // and --ctx-size unambiguously belongs to that one slot.
            cmd.arg("--parallel").arg("1");
            cmd.arg("--slot-save-path").arg(slot_path);
        }
        // Performance flags from .env (LLAMA_*), see tuning.rs.
        cmd.args(crate::tuning::chat_engine_args());
        if let Ok(extra) = std::env::var("LLAMA_EXTRA_ARGS") {
            let extra = extra.trim();
            if !extra.is_empty() {
                for token in extra.split_whitespace() {
                    cmd.arg(token);
                }
            }
        }

        super::process_util::configure_server_command(&mut cmd);
        let mut child = cmd.spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn llama-server: {}", e))?;
        super::process_util::attach_managed_child(&mut child, "llama-server");
        Ok(child)
    }

    async fn start_server(&mut self, config: &RuntimeConfig) -> anyhow::Result<()> {
        let binary_path = config.runtime_binary.as_ref()
            .ok_or_else(|| anyhow::anyhow!("GGUF runtime requires runtime_binary path"))?;

        if !binary_path.exists() {
            return Err(anyhow::anyhow!(
                "llama-server binary not found at: {}",
                binary_path.display()
            ));
        }

        info!("Starting llama-server for GGUF model: {}", config.model_path.display());
        info!("  Binary: {}", binary_path.display());
        info!("  Port: {}", config.port);
        info!("  Context Size: {}", config.context_size);
        info!("  GPU Layers: {}", config.gpu_layers);

        if !config.model_path.exists() {
            return Err(anyhow::anyhow!(
                "Model file not found at: {}",
                config.model_path.display()
            ));
        }

        let mut config = config.clone();
        let child = Self::spawn_server_process(&config)?;

        *self.server_process.lock().await = Some(child);
        self.base_url = format!("http://{}:{}", config.host, config.port);

        info!("llama-server process started, waiting for health check...");

        // Poll every 250 ms (a wake from sleep should feel quick) for up to
        // 120 s. Health is probed directly rather than through is_ready(),
        // whose crash auto-restart would respawn a config that cannot start.
        let started = std::time::Instant::now();
        let health_url = format!("{}/health", self.base_url);
        let start_wait = crate::tuning::parse_or("HEALTH_TIMEOUT_SECONDS", 120u64).max(10);
        while started.elapsed() < Duration::from_secs(start_wait) {
            sleep(Duration::from_millis(250)).await;

            let exited = {
                let mut guard = self.server_process.lock().await;
                match guard.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => {
                        guard.take();
                        Some(status)
                    }
                    _ => None,
                }
            };
            if let Some(status) = exited {
                // A quantized KV cache needs flash attention support in the
                // backend; if the engine refuses it, start again on f16.
                let quantized = config.kv_cache_type.as_deref().map_or(false, |t| t != "f16");
                if quantized {
                    warn!(
                        "llama-server exited during startup ({}) with KV cache type {:?}; \
                         retrying with the default f16 cache",
                        status, config.kv_cache_type
                    );
                    config.kv_cache_type = None;
                    self.config = Some(config.clone());
                    let child = Self::spawn_server_process(&config)?;
                    *self.server_process.lock().await = Some(child);
                    continue;
                }
                return Err(anyhow::anyhow!("llama-server exited during startup ({})", status));
            }

            if let Ok(resp) = self.http_client.get(&health_url).timeout(Duration::from_secs(2)).send().await {
                if resp.status().is_success() {
                    info!("✅ GGUF runtime ready after {} ms", started.elapsed().as_millis());
                    self.read_effective_context().await;
                    return Ok(());
                }
            }
        }

        Err(anyhow::anyhow!("llama-server failed to start within {} seconds (HEALTH_TIMEOUT_SECONDS)", start_wait))
    }
}

impl Default for GGUFRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelRuntime for GGUFRuntime {
    fn supported_format(&self) -> ModelFormat {
        ModelFormat::GGUF
    }

    async fn initialize(&mut self, config: RuntimeConfig) -> anyhow::Result<()> {
        info!("Initializing GGUF runtime");
        
        // Validate config
        if config.format != ModelFormat::GGUF {
            return Err(anyhow::anyhow!(
                "GGUF runtime received wrong format: {:?}",
                config.format
            ));
        }

        if !config.model_path.exists() {
            return Err(anyhow::anyhow!(
                "Model file not found: {}",
                config.model_path.display()
            ));
        }

        self.config = Some(config.clone());
        self.start_server(&config).await?;
        
        Ok(())
    }

    async fn is_ready(&self) -> bool {
        if self.base_url.is_empty() {
            return false;
        }

        let health_url = format!("{}/health", self.base_url);
        match self.http_client.get(&health_url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    return true;
                }
                self.try_restart_crashed_server().await;
                false
            }
            Err(_) => {
                self.try_restart_crashed_server().await;
                false
            }
        }
    }

    async fn health_check(&self) -> anyhow::Result<String> {
        if self.base_url.is_empty() {
            return Err(anyhow::anyhow!("Runtime not initialized"));
        }

        let health_url = format!("{}/health", self.base_url);
        let resp = self.http_client.get(&health_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Health check failed: {}", e))?;

        if resp.status().is_success() {
            Ok("healthy".to_string())
        } else {
            Err(anyhow::anyhow!("Health check returned: {}", resp.status()))
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn generate(
        &self,
        request: InferenceRequest,
    ) -> anyhow::Result<InferenceResponse> {
        let url = self.completions_url();
        
        let payload = serde_json::json!({
            "model": "local-llm",
            "messages": request.messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": false,
        });

        let resp = self.http_client.post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Inference request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Inference failed ({}): {}", status, body));
        }

        let response: serde_json::Value = resp.json().await
            .map_err(|e| anyhow::anyhow!("Failed to parse response: {}", e))?;

        let content = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let finish_reason = response["choices"][0]["finish_reason"]
            .as_str()
            .map(|s| s.to_string());

        Ok(InferenceResponse {
            content,
            finish_reason,
        })
    }

    async fn generate_stream(
        &self,
        request: InferenceRequest,
    ) -> anyhow::Result<Box<dyn futures_util::Stream<Item = Result<String, anyhow::Error>> + Send + Unpin>> {
        use futures_util::StreamExt;
        
        let url = self.completions_url();
        
        let payload = serde_json::json!({
            "model": "local-llm",
            "messages": request.messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
        });

        let resp = self.http_client.post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Stream request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Stream failed ({}): {}", status, body));
        }

        let byte_stream = resp.bytes_stream();
        
        let sse_stream = async_stream::try_stream! {
            let mut buffer = String::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = chunk_result.map_err(|e| anyhow::anyhow!("Stream read error: {}", e))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    if data == "[DONE]" {
                        return;
                    }

                    yield format!("data: {}\n\n", data);
                }
            }
        };

        Ok(Box::new(Box::pin(sse_stream)))
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        info!("Shutting down GGUF runtime");

        let mut guard = self.server_process.lock().await;
        if let Some(mut child) = guard.take() {
            match child.kill() {
                Ok(_) => {
                    info!("llama-server process killed successfully");
                    let _ = child.wait();
                }
                Err(e) => {
                    warn!("Failed to kill llama-server process: {}", e);
                }
            }
        }

        Ok(())
    }

    fn metadata(&self) -> RuntimeMetadata {
        RuntimeMetadata {
            format: ModelFormat::GGUF,
            runtime_name: "llama.cpp (llama-server)".to_string(),
            version: "latest".to_string(),
            supports_gpu: true,
            supports_streaming: true,
        }
    }

    async fn effective_context(&self) -> Option<u32> {
        let v = self.effective_context.load(std::sync::atomic::Ordering::SeqCst);
        if v > 0 {
            Some(v)
        } else {
            None
        }
    }
}

impl Drop for GGUFRuntime {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.server_process.try_lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
