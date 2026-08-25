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
                            for attempt in 1..=60 {
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
                            error!("llama-server restart timed out after 120 seconds");
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
    fn spawn_server_process(config: &RuntimeConfig) -> anyhow::Result<Child> {
        let binary_path = config.runtime_binary.as_ref()
            .ok_or_else(|| anyhow::anyhow!("GGUF runtime requires runtime_binary path"))?;

        let mut cmd = Command::new(binary_path);
        cmd.arg("--model").arg(&config.model_path)
            .arg("--host").arg(&config.host)
            .arg("--port").arg(config.port.to_string())
            .arg("--ctx-size").arg(config.context_size.to_string())
            .arg("--threads").arg(config.threads.to_string())
            .arg("--n-gpu-layers").arg(config.gpu_layers.to_string());

        if config.batch_size > 0 {
            cmd.arg("--batch-size").arg(config.batch_size.to_string());
        }
        if config.threads_batch > 0 {
            cmd.arg("--threads-batch").arg(config.threads_batch.to_string());
        }
        if config.cache_reuse > 0 {
            cmd.arg("--cache-reuse").arg(config.cache_reuse.to_string());
        }
        if let Some(ref mmproj) = config.mmproj_path {
            // Vision model: load the multimodal projector. The path was
            // verified to exist by whoever built this config (model switch /
            // auto-load / dev override) — a missing file here is a hard spawn
            // failure by design, never a silent text-only start.
            cmd.arg("--mmproj").arg(mmproj);
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
        if let Some(ref slot_path) = config.slot_save_path.as_ref().filter(|p| p.exists()) {
            // --parallel 1 is not optional alongside this: the save/restore
            // endpoints address a slot by index, and llama-server assigns
            // slots by prompt similarity rather than by client, so with
            // several slots "slot 0" is not a stable target.
            //
            // It also removes an ambiguity about context. How --ctx-size
            // relates to per-slot context depends on the KV-cache mode
            // (unified treats it as a shared budget across sequences;
            // --no-kv-unified partitions it), and this build defaults
            // --parallel to -1 = auto, so the slot count was not previously
            // known. With one slot the question does not arise: --ctx-size is
            // that slot's context. This is a single-user desktop app, so
            // nothing is given up by pinning it.
            cmd.arg("--parallel").arg("1");
            cmd.arg("--slot-save-path").arg(slot_path);
        }
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

        let child = Self::spawn_server_process(config)?;

        *self.server_process.lock().await = Some(child);
        self.base_url = format!("http://{}:{}", config.host, config.port);

        info!("llama-server process started, waiting for health check...");

        // Wait for server to be ready (up to 120 seconds)
        for attempt in 1..=60 {
            sleep(Duration::from_secs(2)).await;
            
            if self.is_ready().await {
                info!("✅ GGUF runtime ready after {} seconds", attempt * 2);
                self.read_effective_context().await;
                return Ok(());
            }
            
            if attempt % 10 == 0 {
                info!("Still waiting for llama-server... ({}/120s)", attempt * 2);
            }
        }

        Err(anyhow::anyhow!("llama-server failed to start within 120 seconds"))
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
