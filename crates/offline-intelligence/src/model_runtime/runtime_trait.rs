//! Core trait and types for model runtime abstraction

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default server slots. One slot: half the KV cache of two, and the only
/// mode in which slot save/restore (warm start) works. Chat and Aurora
/// requests take turns in it; the engine queues the second.
pub const DEFAULT_PARALLEL_SLOTS: u32 = 1;

fn default_parallel_slots() -> u32 {
    DEFAULT_PARALLEL_SLOTS
}

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
    /// Requires `parallel_slots == 1`, and `build_runtime_config` enforces
    /// that by clearing this whenever more slots are requested. Slot
    /// save/restore addresses a slot by index, and with more than one slot
    /// there is no way to know which one served a given conversation:
    /// llama-server assigns slots by prompt similarity, not by client. One
    /// slot makes "slot 0" a stable target.
    #[serde(default)]
    pub slot_save_path: Option<PathBuf>,

    /// Number of server slots (`--parallel`), i.e. how many requests the
    /// engine will serve at once.
    ///
    /// 2 is the product default: the main discussion and the AI Assistant are
    /// two independent clients, and with one slot an assistant question asked
    /// while a chat answer is streaming simply waits, with nothing in the UI
    /// saying so.
    ///
    /// Above 1 this is always paired with `--kv-unified` in the spawned
    /// command, and that pairing is the whole reason 2 slots are usable here.
    /// WITHOUT it llama.cpp partitions `--ctx-size` evenly, giving each slot
    /// half the window — which would hand the assistant 50%, directly
    /// contradicting the 75/25 split the application applies, and would halve
    /// the room a chat conversation has for its documents. WITH it there is
    /// one shared KV buffer that both sequences draw from, so the split is
    /// decided by how much prompt each client is allowed to build
    /// (shared_state::context_budget_for) rather than by a fixed partition.
    ///
    /// The cost of going above 1 is warm start: see `slot_save_path`.
    #[serde(default = "default_parallel_slots")]
    pub parallel_slots: u32,
    /// Path to the multimodal projector file (`--mmproj`) for vision-capable
    /// GGUF models. `None` omits the flag entirely — the runtime is text-only,
    /// exactly as before this field existed. When set, llama-server loads the
    /// projector and accepts OpenAI-style `image_url` content parts on
    /// /v1/chat/completions. Resolved from the model registry at activation
    /// (production) or from the MMPROJ_PATH .env override (development) —
    /// never guessed.
    #[serde(default)]
    pub mmproj_path: Option<PathBuf>,
    /// Whether the engine is started WITH the projector. A vision model runs
    /// text-only by default: llama-server refuses slot save/restore while a
    /// projector is loaded ("not supported by multimodal"), which would
    /// disable the KV store. lifecycle::with_projector loads it for image
    /// reading and releases it when images stop.
    #[serde(default)]
    pub projector_loaded: bool,
    /// KV cache element type for K and V (`--cache-type-k/-v`), e.g. "q8_0".
    /// `None` or "f16" omits the flags (engine default f16).
    #[serde(default)]
    pub kv_cache_type: Option<String>,
    /// Cap on the engine's RAM prompt cache in MiB (`--cache-ram`). `None`
    /// omits the flag (engine default 8192).
    #[serde(default)]
    pub cache_ram_mb: Option<i64>,
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
            parallel_slots: default_parallel_slots(),
            mmproj_path: None,
            projector_loaded: false,
            kv_cache_type: None,
            cache_ram_mb: None,
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
