//! LLM worker thread implementation
//!
//! Handles LLM inference by proxying requests to the local llama-server process.
//! This is the 1-hop architecture: shared memory state → HTTP to localhost llama-server.

use std::sync::{Arc, RwLock};
use futures_util::StreamExt;
use tracing::{info, debug, warn};
use serde::{Deserialize, Serialize};

use crate::{
    memory::Message,
    model_runtime::{RuntimeManager, RuntimeState},
};

/// Chat completion request sent to llama-server (OpenAI-compatible format)
#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Non-streaming response from llama-server
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: Option<ChatMessage>,
}

/// Streaming delta chunk from llama-server
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<ChatDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ChatDelta {
    content: Option<String>,
}

pub struct LLMWorker {
    backend_url: String,
    http_client: reqwest::Client,
    runtime_manager: RwLock<Option<Arc<RuntimeManager>>>,
}

impl LLMWorker {
    /// Create with shared state (legacy constructor)
    pub fn new(shared_state: std::sync::Arc<crate::shared_state::SharedState>) -> Self {
        let backend_url = shared_state.config.backend_url.clone();
        Self {
            backend_url,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            runtime_manager: RwLock::new(None),
        }
    }

    /// Create with explicit backend URL
    pub fn new_with_backend(backend_url: String) -> Self {
        info!("LLM worker initialized with backend: {}", backend_url);
        Self {
            backend_url,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
            runtime_manager: RwLock::new(None),
        }
    }

    /// Set the runtime manager
    pub fn set_runtime_manager(&self, runtime_manager: Arc<RuntimeManager>) {
        if let Ok(mut guard) = self.runtime_manager.write() {
            *guard = Some(runtime_manager);
            info!("✅ Runtime manager linked to LLM worker");
        } else {
            warn!("⚠️  Failed to acquire lock to set runtime manager on LLM worker");
        }
    }
    
    /// Get the runtime manager if available
    fn get_runtime_manager(&self) -> Option<Arc<RuntimeManager>> {
        self.runtime_manager.read().ok().and_then(|guard| (*guard).clone())
    }

    /// Check if runtime is ready for inference
    pub async fn is_runtime_ready(&self) -> bool {
        if let Some(ref rm) = self.get_runtime_manager() {
            rm.is_ready().await
        } else {
            false
        }
    }

    /// Check if a model switch is currently in progress
    pub fn is_switching(&self) -> bool {
        if let Some(ref rm) = self.get_runtime_manager() {
            matches!(rm.get_state(), RuntimeState::Switching { .. })
        } else {
            false
        }
    }

    /// Get the current runtime lifecycle state
    pub fn get_runtime_state(&self) -> RuntimeState {
        if let Some(ref rm) = self.get_runtime_manager() {
            rm.get_state()
        } else {
            RuntimeState::Idle
        }
    }

    // Override the original methods to use runtime manager when available


    /// Convert internal Message format to OpenAI-compatible ChatMessage
    fn to_chat_messages(messages: &[Message]) -> Vec<ChatMessage> {
        messages.iter().map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        }).collect()
    }

    /// Generate a complete (non-streaming) response from the LLM.
    pub async fn generate_response(
        &self,
        _session_id: String,
        context: Vec<Message>,
    ) -> anyhow::Result<String> {
        debug!("LLM worker generating response (non-streaming)");

        let request = ChatCompletionRequest {
            model: "local-llm".to_string(),
            messages: Self::to_chat_messages(&context),
            max_tokens: 2000,
            temperature: 0.7,
            stream: false,
        };

        // Determine the URL to use based on whether runtime manager is available AND ready
        let url = if let Some(ref rm) = self.get_runtime_manager() {
            // Check if runtime is actually initialized and ready
            if rm.is_ready().await {
                if let Some(base_url) = rm.get_base_url().await {
                    format!("{}/v1/chat/completions", base_url)
                } else {
                    // Runtime manager exists but no base URL - engine not ready
                    return Err(anyhow::anyhow!(
                        "Model engine is initializing. Please wait a moment and try again, or load a model from the Models panel."
                    ));
                }
            } else {
                // Runtime manager exists but not ready
                return Err(anyhow::anyhow!(
                    "Model engine is not ready yet. Please load a model from the Models panel first."
                ));
            }
        } else {
            // No runtime manager set yet - still initializing or no engine installed
            return Err(anyhow::anyhow!(
                "No model loaded. Please download an engine and load a model from the Models panel."
            ));
        };
        
        let response = self.http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    anyhow::anyhow!(
                        "Cannot connect to local LLM server. Please download and load a model from the Models panel."
                    )
                } else {
                    anyhow::anyhow!("LLM backend request failed: {}", e)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("LLM backend returned {}: {}", status, body));
        }

        let completion: ChatCompletionResponse = response.json().await
            .map_err(|e| anyhow::anyhow!("Failed to parse LLM response: {}", e))?;

        let content = completion.choices
            .first()
            .and_then(|c| c.message.as_ref())
            .map(|m| m.content.clone())
            .unwrap_or_default();

        Ok(content)
    }

    /// Stream response tokens from the LLM as Server-Sent Events.
    /// Returns a stream of SSE-formatted strings ready to send to the client.
    pub async fn stream_response(
        &self,
        messages: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<impl futures_util::Stream<Item = Result<String, anyhow::Error>>> {
        debug!("LLM worker starting streaming response");

        let request = ChatCompletionRequest {
            model: "local-llm".to_string(),
            messages: Self::to_chat_messages(&messages),
            max_tokens,
            temperature,
            stream: true,
        };

        // Determine the URL to use based on whether runtime manager is available AND ready
        let url = if let Some(ref rm) = self.get_runtime_manager() {
            // Check if runtime is actually initialized and ready
            if rm.is_ready().await {
                if let Some(base_url) = rm.get_base_url().await {
                    format!("{}/v1/chat/completions", base_url)
                } else {
                    // Runtime manager exists but no base URL - engine not ready
                    return Err(anyhow::anyhow!(
                        "Model engine is initializing. Please wait a moment and try again, or load a model from the Models panel."
                    ));
                }
            } else {
                // Runtime manager exists but not ready
                return Err(anyhow::anyhow!(
                    "Model engine is not ready yet. Please load a model from the Models panel first."
                ));
            }
        } else {
            // No runtime manager set yet - still initializing or no engine installed
            return Err(anyhow::anyhow!(
                "No model loaded. Please download an engine and load a model from the Models panel."
            ));
        };
        
        let response = self.http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    anyhow::anyhow!(
                        "Cannot connect to local LLM server. Please download and load a model from the Models panel."
                    )
                } else {
                    anyhow::anyhow!("LLM backend request failed: {}", e)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("LLM backend returned {}: {}", status, body));
        }

        let byte_stream = response.bytes_stream();

        let sse_stream = async_stream::try_stream! {
            let mut buffer = String::new();

            futures_util::pin_mut!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = chunk_result
                    .map_err(|e| anyhow::anyhow!("Stream read error: {}", e))?;

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }

                    if line.starts_with("data: ") {
                        let data = &line[6..];

                        if data == "[DONE]" {
                            yield "data: [DONE]\n\n".to_string();
                            return;
                        }

                        match serde_json::from_str::<StreamChunk>(data) {
                            Ok(chunk) => {
                                let finished = chunk.choices.iter()
                                    .any(|c| c.finish_reason.is_some());

                                yield format!("data: {}\n\n", data);

                                if finished {
                                    yield "data: [DONE]\n\n".to_string();
                                    return;
                                }
                            }
                            Err(_) => {
                                yield format!("data: {}\n\n", data);
                            }
                        }
                    }
                }
            }

            yield "data: [DONE]\n\n".to_string();
        };

        Ok(sse_stream)
    }

    /// Transcribe all text in an image via the ACTIVE VISION runtime.
    ///
    /// Sends an OpenAI content-parts request (text prompt + base64 `data:`
    /// image URL) to /v1/chat/completions — the format llama-server accepts
    /// when started with --mmproj. Isolated from every text path: the typed
    /// `ChatCompletionRequest` structs are untouched; this builds raw JSON.
    ///
    /// The CALLER decides whether vision is active
    /// (SharedSystemState::vision_active) — calling this against a text-only
    /// runtime returns llama-server's own error, propagated loudly.
    pub async fn transcribe_image(
        &self,
        image_bytes: &[u8],
        filename: &str,
    ) -> anyhow::Result<String> {
        let rm = self
            .get_runtime_manager()
            .ok_or_else(|| anyhow::anyhow!("No model runtime available for image transcription"))?;
        if !rm.is_ready().await {
            return Err(anyhow::anyhow!("Model runtime is not ready for image transcription"));
        }
        let base_url = rm
            .get_base_url()
            .await
            .ok_or_else(|| anyhow::anyhow!("Model runtime has no base URL"))?;

        // MIME from extension; image/png is a safe default — llama-server's
        // decoder sniffs the actual container from the decoded bytes.
        let ext = filename.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
        let mime = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "bmp" => "image/bmp",
            "gif" => "image/gif",
            "tiff" | "tif" => "image/tiff",
            _ => "image/png",
        };
        use base64::Engine as _;
        let data_url = format!(
            "data:{};base64,{}",
            mime,
            base64::engine::general_purpose::STANDARD.encode(image_bytes)
        );

        const TRANSCRIBE_PROMPT: &str =
            "Transcribe ALL text visible in this image exactly as written, including \
             handwritten text. Preserve the reading order and line breaks of the \
             original. Do not correct spelling or grammar. Where a word is genuinely \
             unreadable, write [illegible]. Output ONLY the transcribed text with no \
             commentary. If the image contains no readable text at all, reply with \
             exactly: NO_TEXT_FOUND followed by a one-sentence description of the image.";

        let request = serde_json::json!({
            "model": "local-llm",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": TRANSCRIBE_PROMPT },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
            "max_tokens": 4096,
            "temperature": 0.0,
            "stream": false
        });

        debug!("Vision transcription request for '{}' ({} bytes)", filename, image_bytes.len());
        let response = self
            .http_client
            .post(format!("{}/v1/chat/completions", base_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Vision transcription request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Vision model returned {} for image '{}': {}",
                status, filename, body
            ));
        }

        let completion: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse vision model response: {}", e))?;
        let content = completion
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .map(|m| m.content.clone())
            .unwrap_or_default();

        info!(
            "Vision model transcribed '{}': {} chars recovered",
            filename,
            content.len()
        );
        Ok(content)
    }

    /// Batch process multiple prompts (non-streaming)
    pub async fn batch_process(
        &self,
        prompts: Vec<(String, Vec<Message>)>,
    ) -> anyhow::Result<Vec<String>> {
        debug!("LLM worker batch processing {} prompts", prompts.len());

        let mut responses = Vec::new();
        for (session_id, messages) in prompts {
            match self.generate_response(session_id.clone(), messages).await {
                Ok(response) => responses.push(response),
                Err(e) => {
                    warn!("Batch item {} failed: {}", session_id, e);
                    responses.push(format!("Error: {}", e));
                }
            }
        }

        info!("Batch processed {} prompts", responses.len());
        Ok(responses)
    }

    /// Initialize LLM model (no-op for HTTP proxy mode)
    pub async fn initialize_model(&self, model_path: &str) -> anyhow::Result<()> {
        debug!("LLM worker model init (HTTP proxy mode): {}", model_path);
        Ok(())
    }

}
