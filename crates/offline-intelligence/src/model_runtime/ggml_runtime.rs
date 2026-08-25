//! GGML Runtime Adapter (legacy format)
//! Similar to GGUF but for older llama.cpp GGML models

use async_trait::async_trait;
use super::gguf_runtime::GGUFRuntime;
use super::runtime_trait::*;

/// GGML runtime - reuses GGUF runtime implementation since llama-server supports both
pub struct GGMLRuntime {
    inner: GGUFRuntime,
}

impl GGMLRuntime {
    pub fn new() -> Self {
        Self {
            inner: GGUFRuntime::new(),
        }
    }
}

impl Default for GGMLRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelRuntime for GGMLRuntime {
    fn supported_format(&self) -> ModelFormat {
        ModelFormat::GGML
    }

    async fn initialize(&mut self, mut config: RuntimeConfig) -> anyhow::Result<()> {
        // GGML uses the same llama-server as GGUF
        config.format = ModelFormat::GGUF; // Internal override
        self.inner.initialize(config).await
    }

    async fn is_ready(&self) -> bool {
        self.inner.is_ready().await
    }

    async fn health_check(&self) -> anyhow::Result<String> {
        self.inner.health_check().await
    }

    fn base_url(&self) -> String {
        self.inner.base_url()
    }

    async fn generate(&self, request: InferenceRequest) -> anyhow::Result<InferenceResponse> {
        self.inner.generate(request).await
    }

    async fn generate_stream(
        &self,
        request: InferenceRequest,
    ) -> anyhow::Result<Box<dyn futures_util::Stream<Item = Result<String, anyhow::Error>> + Send + Unpin>> {
        self.inner.generate_stream(request).await
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        self.inner.shutdown().await
    }

    fn metadata(&self) -> RuntimeMetadata {
        RuntimeMetadata {
            format: ModelFormat::GGML,
            runtime_name: "llama.cpp (llama-server)".to_string(),
            version: "latest".to_string(),
            supports_gpu: true,
            supports_streaming: true,
        }
    }
}
