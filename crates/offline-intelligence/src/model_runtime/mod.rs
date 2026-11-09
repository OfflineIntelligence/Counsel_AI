//! Model Runtime Abstraction Layer
//!
//! Provides a unified interface for hosting multiple model formats (GGUF, ONNX, 
//! TensorRT, Safetensors, GGML, CoreML) through a trait-based runtime system.
//!
//! Architecture:
//! - Each model format has its own runtime adapter
//! - All runtimes expose OpenAI-compatible HTTP API
//! - Maintains 1-hop architecture: Rust → HTTP → Runtime Server
//! - Automatic format detection from file extension

pub mod runtime_trait;
pub mod gguf_runtime;
pub mod format_detector;
pub mod gguf_metadata;
pub mod gpu_offload;
pub mod platform_detector;
pub mod process_util;
pub mod runtime_manager;
pub mod slot_cache;
pub mod lifecycle;
pub mod kv_store;
pub mod machine;
pub mod intel_vram;

pub use runtime_trait::{ModelRuntime, ModelFormat, RuntimeConfig, InferenceRequest, InferenceResponse, DEFAULT_PARALLEL_SLOTS};
pub use gguf_runtime::GGUFRuntime;
pub use format_detector::FormatDetector;
pub use platform_detector::HardwareCapabilities;
pub use runtime_manager::{RuntimeManager, RuntimeState};
