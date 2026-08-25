//! Core trait and types for model runtime abstraction

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Supported model formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelFormat {
    /// GGUF format (llama.cpp quantized)
    GGUF,
    /// GGML format (llama.cpp legacy)
    GGML,
    /// ONNX format (Open Neural Network Exchange)
    ONNX,
    /// TensorRT optimized format (NVIDIA)
    TensorRT,
    /// Safetensors format (Hugging Face)
    Safetensors,
    /// CoreML format (Apple)
    CoreML,
}

impl ModelFormat {
    /// Get file extensions for this format
    pub fn extensions(&self) -> &[&str] {
        match self {
            ModelFormat::GGUF => &["gguf"],
            ModelFormat::GGML => &["ggml", "bin"],
            ModelFormat::ONNX => &["onnx"],
            ModelFormat::TensorRT => &["trt", "engine", "plan"],
            ModelFormat::Safetensors => &["safetensors"],
            ModelFormat::CoreML => &["mlmodel", "mlpackage"],
        }
    }

    /// Get human-readable name
    pub fn name(&self) -> &str {
        match self {
            ModelFormat::GGUF => "GGUF (llama.cpp)",
            ModelFormat::GGML => "GGML (llama.cpp legacy)",
            ModelFormat::ONNX => "ONNX Runtime",
            ModelFormat::TensorRT => "TensorRT",
            ModelFormat::Safetensors => "Safetensors",
            ModelFormat::CoreML => "CoreML",
        }
    }
}

/// Runtime configuration for model initialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Path to model file
    pub model_path: PathBuf,
    /// Model format
    pub format: ModelFormat,
    /// Host for runtime server (e.g., "127.0.0.1")
    pub host: String,
    /// Port for runtime server (e.g., 9639)
    pub port: u16,
    /// Context size
    pub context_size: u32,
    /// Batch size. 0 = do not pass the flag — the engine's own tuned defaults
    /// (llama-server: n_batch 2048 / n_ubatch 512) are used.
    pub batch_size: u32,
    /// Number of CPU threads for token generation
    pub threads: u32,
    /// Threads for prompt processing (--threads-batch). 0 = do not pass the flag.
    #[serde(default)]
    pub threads_batch: u32,
    /// GPU layers to offload (0 = CPU only)
    pub gpu_layers: u32,
    /// Minimum KV chunk size the engine may reuse via KV shifting
    /// (llama-server --cache-reuse). 0 = do not pass the flag.
    #[serde(default)]
    pub cache_reuse: u32,
    /// Directory llama-server may write slot KV-cache blobs into
    /// (`--slot-save-path`). `None` omits the flag, which DISABLES the
    /// `/slots/{id}?action=save|restore` endpoints entirely - they return an
    /// error without it.
    ///
    /// Setting this also forces `--parallel 1`. Slot save/restore addresses a
    /// slot by index, and with more than one slot there is no way to know
    /// which one served a given conversation: llama-server assigns slots by
    /// prompt similarity, not by client. One slot makes "slot 0" a stable
    /// target. It also means `--ctx-size` is the context of that single slot
    /// rather than a total divided among several.
    #[serde(default)]
    pub slot_save_path: Option<PathBuf>,
    /// Path to the multimodal projector file (`--mmproj`) for vision-capable
    /// GGUF models. `None` omits the flag entirely — the runtime is text-only,
    /// exactly as before this field existed. When set, llama-server loads the
    /// projector and accepts OpenAI-style `image_url` content parts on
    /// /v1/chat/completions. Resolved from the model registry at activation
    /// (production) or from the MMPROJ_PATH .env override (development) —
    /// never guessed.
    #[serde(default)]
    pub mmproj_path: Option<PathBuf>,
    /// Path to runtime binary (e.g., llama-server.exe)
    pub runtime_binary: Option<PathBuf>,
    /// Additional runtime-specific configuration
    pub extra_config: serde_json::Value,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::new(),
            format: ModelFormat::GGUF,
            host: "127.0.0.1".to_string(),
            port: 9639,
            context_size: 8192,
            batch_size: 128,
            threads: 6,
            threads_batch: 0,
            gpu_layers: 0,
            cache_reuse: 0,
            slot_save_path: None,
            mmproj_path: None,
            runtime_binary: None,
            extra_config: serde_json::json!({}),
        }
    }
}

/// Inference request (OpenAI-compatible format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_stream")]
    pub stream: bool,
}

fn default_max_tokens() -> u32 { 2000 }
fn default_temperature() -> f32 { 0.7 }
fn default_stream() -> bool { false }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Inference response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub content: String,
    pub finish_reason: Option<String>,
}

/// Model runtime trait - all runtime adapters must implement this
#[async_trait]
pub trait ModelRuntime: Send + Sync {
    /// Get the format this runtime supports
    fn supported_format(&self) -> ModelFormat;

    /// Initialize the runtime (start server process, load model, etc.)
    async fn initialize(&mut self, config: RuntimeConfig) -> anyhow::Result<()>;

    /// Check if runtime is ready for inference
    async fn is_ready(&self) -> bool;

    /// Get health status
    async fn health_check(&self) -> anyhow::Result<String>;

    /// Get the base URL for inference API (e.g., "http://127.0.0.1:9639")
    fn base_url(&self) -> String;

    /// Get the OpenAI-compatible chat completions endpoint
    fn completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url())
    }

    /// Perform inference (non-streaming)
    async fn generate(
        &self,
        request: InferenceRequest,
    ) -> anyhow::Result<InferenceResponse>;

    /// Perform streaming inference
    async fn generate_stream(
        &self,
        request: InferenceRequest,
    ) -> anyhow::Result<Box<dyn futures_util::Stream<Item = Result<String, anyhow::Error>> + Send + Unpin>>;

    /// Shutdown the runtime (stop server, cleanup resources)
    async fn shutdown(&self) -> anyhow::Result<()>;

    /// The context size the ENGINE actually allocated for this model, read
    /// back from the running server after startup (not our request, not a
    /// guess). None when the runtime has no such concept or hasn't reported
    /// it yet. This is the ground truth used for per-request budget
    /// decisions - a requested/auto ctx_size can differ (fitter, clamps).
    async fn effective_context(&self) -> Option<u32> {
        None
    }

    /// Get runtime metadata
    fn metadata(&self) -> RuntimeMetadata;
}

/// Runtime metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeMetadata {
    pub format: ModelFormat,
    pub runtime_name: String,
    pub version: String,
    pub supports_gpu: bool,
    pub supports_streaming: bool,
}
