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
    /// On Cloud: the provider, key and model every call goes to. `None` is
    /// On Computer. The ONE switch: chat, agents, drafting and document
    /// intelligence all generate through this worker, so all follow it.
    cloud: RwLock<Option<ActiveCloud>>,
}

/// The cloud model in use while On Cloud.
#[derive(Debug, Clone)]
pub struct ActiveCloud {
    pub provider: &'static crate::cloud_providers::CloudProvider,
    pub key: String,
    pub model: String,
}

/// STREAM_TIMEOUT_SECONDS (600): the longest one streamed answer may take.
fn stream_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(crate::tuning::parse_or("STREAM_TIMEOUT_SECONDS", 600u64).max(30))
}

/// GENERATE_TIMEOUT_SECONDS (300): the longest a non-streaming completion may take.
fn generate_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(crate::tuning::parse_or("GENERATE_TIMEOUT_SECONDS", 300u64).max(10))
}

impl LLMWorker {
    /// Create with shared state (legacy constructor)
    pub fn new(shared_state: std::sync::Arc<crate::shared_state::SharedState>) -> Self {
        let backend_url = shared_state.config.backend_url.clone();
        Self {
            backend_url,
            http_client: reqwest::Client::builder()
                .timeout(stream_timeout())
                .build()
                .unwrap_or_default(),
            runtime_manager: RwLock::new(None),
            cloud: RwLock::new(None),
        }
    }

    /// Create with explicit backend URL
    pub fn new_with_backend(backend_url: String) -> Self {
        info!("LLM worker initialized with backend: {}", backend_url);
        Self {
            backend_url,
            http_client: reqwest::Client::builder()
                .timeout(stream_timeout())
                .build()
                .unwrap_or_default(),
            runtime_manager: RwLock::new(None),
            cloud: RwLock::new(None),
        }
    }

    /// Switch between On Computer (`None`) and On Cloud.
    pub fn set_cloud(&self, cloud: Option<ActiveCloud>) {
        if let Ok(mut g) = self.cloud.write() {
            match &cloud {
                Some(c) => info!("Answers now On Cloud: {} / {}", c.provider.name, c.model),
                None => info!("Answers now On Computer"),
            }
            *g = cloud;
        }
    }

    /// The active cloud model, or `None` when On Computer.
    pub fn cloud(&self) -> Option<ActiveCloud> {
        self.cloud.read().ok().and_then(|g| g.clone())
    }

    fn cloud_headers(c: &ActiveCloud) -> Vec<(&'static str, String)> {
        let mut h = vec![("Authorization", format!("Bearer {}", c.key))];
        h.extend(crate::cloud_providers::extra_headers(c.provider, &c.key));
        h
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
        // Not a turn of a conversation: it takes the engine's single slot like
        // any request, and replaces whatever the slot held, so the KV store
        // must stop assuming it knows what that is. (Callers already inside a
        // request's slot, such as the reading-notes step, use generate_with.)
        let _slot = crate::model_runtime::kv_store::lock().await;
        crate::model_runtime::kv_store::engine_reset();
        self.generate_with(context, 2000, 0.7).await
    }

    /// Non-streaming completion with an explicit length and temperature. The
    /// caller must hold the KV store slot (`kv_store::lock`).
    pub async fn generate_with(
        &self,
        context: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<String> {
        debug!("LLM worker generating response (non-streaming)");

        if let Some(c) = self.cloud() {
            return self.complete_cloud(&c, context, max_tokens, temperature).await;
        }

        let request = ChatCompletionRequest {
            model: "local-llm".to_string(),
            messages: Self::to_chat_messages(&context),
            max_tokens,
            temperature,
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
            .timeout(generate_timeout())
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

    /// Non-streaming completion from the active cloud model.
    async fn complete_cloud(
        &self,
        c: &ActiveCloud,
        context: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<String> {
        let body = Self::request_body(Some(c.provider), &c.model, &context, max_tokens, temperature, false);
        let mut builder = self.http_client.post(format!("{}/chat/completions", c.provider.base_url)).json(&body);
        for (k, v) in Self::cloud_headers(c) {
            builder = builder.header(k, v);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Could not reach {}: {}", c.provider.name, e))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow::anyhow!("{} returned {}: {}", c.provider.name, status, Self::provider_error(&text)));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} response: {}", c.provider.name, e))?;
        Ok(v.pointer("/choices/0/message/content").and_then(|c| c.as_str()).unwrap_or_default().to_string())
    }

    /// Stream response tokens from the LLM as Server-Sent Events.
    /// Returns a stream of SSE-formatted strings ready to send to the client.
    /// Exact prompt size as the engine will see it: rendered with the loaded
    /// model's chat template and counted by its tokenizer. None when no model
    /// is serving or the engine does not answer quickly.
    pub async fn count_prompt_tokens(&self, messages: &[Message]) -> Option<usize> {
        // A cloud model's tokenizer is not the local one; the budget's
        // characters-per-token estimate stands instead.
        if self.cloud().is_some() {
            return None;
        }
        let rm = self.get_runtime_manager()?;
        if !rm.is_ready().await {
            return None;
        }
        let base_url = rm.get_base_url().await?;
        let timeout = std::time::Duration::from_secs(10);
        let rendered: serde_json::Value = self
            .http_client
            .post(format!("{}/apply-template", base_url))
            .timeout(timeout)
            .json(&serde_json::json!({ "messages": Self::to_chat_messages(messages) }))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let prompt = rendered.get("prompt")?.as_str()?.to_string();
        let tokens: serde_json::Value = self
            .http_client
            .post(format!("{}/tokenize", base_url))
            .timeout(timeout)
            .json(&serde_json::json!({ "content": prompt, "add_special": true }))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(tokens.get("tokens")?.as_array()?.len())
    }

    pub async fn stream_response(
        &self,
        messages: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<impl futures_util::Stream<Item = Result<String, anyhow::Error>>> {
        debug!("LLM worker starting streaming response");

        if let Some(c) = self.cloud() {
            let url = format!("{}/chat/completions", c.provider.base_url);
            return self.stream_chat_completions(&url, &c.model, &Self::cloud_headers(&c), messages, max_tokens, temperature, Some(c.provider)).await;
        }

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
        
        self.stream_chat_completions(&url, "local-llm", &[], messages, max_tokens, temperature, None).await
    }

    /// Stream from a cloud provider's OpenAI-compatible endpoint. Same SSE
    /// shape back as the local engine, so callers cannot tell them apart.
    pub async fn stream_cloud(
        &self,
        provider: &'static crate::cloud_providers::CloudProvider,
        api_key: &str,
        model: &str,
        messages: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<impl futures_util::Stream<Item = Result<String, anyhow::Error>>> {
        let url = format!("{}/chat/completions", provider.base_url);
        let mut headers = vec![("Authorization", format!("Bearer {}", api_key))];
        headers.extend(crate::cloud_providers::extra_headers(provider, api_key));
        self.stream_chat_completions(&url, model, &headers, messages, max_tokens, temperature, Some(provider)).await
    }

    async fn stream_chat_completions(
        &self,
        url: &str,
        model: &str,
        headers: &[(&'static str, String)],
        messages: Vec<Message>,
        max_tokens: u32,
        temperature: f32,
        cloud: Option<&'static crate::cloud_providers::CloudProvider>,
    ) -> anyhow::Result<impl futures_util::Stream<Item = Result<String, anyhow::Error>>> {
        let body = Self::request_body(cloud, model, &messages, max_tokens, temperature, true);
        let mut builder = self.http_client.post(url).json(&body);
        for (k, v) in headers {
            builder = builder.header(*k, v);
        }
        let who = cloud.map(|p| p.name).unwrap_or("the local engine");
        let response = builder
            .send()
            .await
            .map_err(|e| match cloud {
                None if e.is_connect() => anyhow::anyhow!(
                    "Cannot connect to local LLM server. Please download and load a model from the Models panel."
                ),
                None => anyhow::anyhow!("LLM backend request failed: {}", e),
                Some(p) => anyhow::anyhow!("Could not reach {}: {}", p.name, e),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(match cloud {
                Some(p) => anyhow::anyhow!("{} returned {}: {}", p.name, status, Self::provider_error(&body)),
                None => anyhow::anyhow!("LLM backend returned {}: {}", status, body),
            });
        }

        let byte_stream = response.bytes_stream();
        let is_cloud = cloud.is_some();
        let who = who.to_string();

        let sse_stream = async_stream::try_stream! {
            // Bytes, not text: a multi-byte character split across two network
            // chunks must be joined before it is decoded.
            let mut buffer: Vec<u8> = Vec::new();
            // Cloud reasoning arrives in its own field; it is shown as the
            // <think>…</think> block the app already renders as Thoughts.
            let mut in_think = false;

            futures_util::pin_mut!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = chunk_result
                    .map_err(|e| anyhow::anyhow!("Stream read error: {}", e))?;
                buffer.extend_from_slice(&chunk);

                while let Some(newline_pos) = buffer.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = buffer.drain(..=newline_pos).collect();
                    let line = String::from_utf8_lossy(&raw).trim().to_string();
                    if line.is_empty() || line.starts_with(':') {
                        continue; // blank, or an SSE comment (keep-alive)
                    }
                    let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };

                    if data == "[DONE]" {
                        if in_think {
                            yield Self::content_frame("</think>");
                        }
                        yield "data: [DONE]\n\n".to_string();
                        return;
                    }

                    if !is_cloud {
                        match serde_json::from_str::<StreamChunk>(data) {
                            Ok(chunk) => {
                                let finished = chunk.choices.iter().any(|c| c.finish_reason.is_some());
                                yield format!("data: {}\n\n", data);
                                if finished {
                                    yield "data: [DONE]\n\n".to_string();
                                    return;
                                }
                            }
                            Err(_) => yield format!("data: {}\n\n", data),
                        }
                        continue;
                    }

                    // Cloud: read the chunk, re-emit only what the app uses.
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
                    if let Some(err) = v.get("error") {
                        let msg = err.get("message").and_then(|m| m.as_str()).map(str::to_string)
                            .unwrap_or_else(|| err.to_string());
                        Err(anyhow::anyhow!("{}: {}", who, msg))?;
                    }
                    let choice = v.get("choices").and_then(|c| c.get(0));
                    let delta = choice.and_then(|c| c.get("delta"));
                    let reasoning = delta.and_then(|d| {
                        d.get("reasoning_content").and_then(|r| r.as_str())
                            .or_else(|| d.get("reasoning").and_then(|r| r.as_str()))
                    }).filter(|r| !r.is_empty());
                    let content = delta.and_then(|d| d.get("content")).and_then(|c| c.as_str()).filter(|c| !c.is_empty());

                    if let Some(r) = reasoning {
                        if !in_think {
                            in_think = true;
                            yield Self::content_frame(&format!("<think>{}", r));
                        } else {
                            yield Self::content_frame(r);
                        }
                    }
                    if let Some(c) = content {
                        if in_think {
                            in_think = false;
                            yield Self::content_frame(&format!("</think>{}", c));
                        } else {
                            yield Self::content_frame(c);
                        }
                    }
                    if choice.and_then(|c| c.get("finish_reason")).map(|f| !f.is_null()).unwrap_or(false) {
                        if in_think {
                            yield Self::content_frame("</think>");
                        }
                        yield "data: [DONE]\n\n".to_string();
                        return;
                    }
                }
            }

            if in_think {
                yield Self::content_frame("</think>");
            }
            yield "data: [DONE]\n\n".to_string();
        };

        Ok(sse_stream)
    }

    /// One OpenAI-shaped content delta, as the app's stream expects.
    fn content_frame(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": text } }] })
        )
    }

    /// The provider's own error message out of an error body, when it has one.
    fn provider_error(body: &str) -> String {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message").and_then(|m| m.as_str()).map(str::to_string).or_else(|| e.as_str().map(str::to_string)))
                    .or_else(|| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
            })
            .unwrap_or_else(|| body.chars().take(400).collect())
    }

    /// The request body. The local engine and most providers take
    /// `max_tokens` + `temperature`; OpenAI's current models take
    /// `max_completion_tokens`, and its reasoning models (o-series, GPT-5)
    /// accept only the default temperature.
    fn request_body(
        cloud: Option<&crate::cloud_providers::CloudProvider>,
        model: &str,
        messages: &[Message],
        max_tokens: u32,
        temperature: f32,
        stream: bool,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": model,
            "messages": Self::to_chat_messages(messages),
            "stream": stream,
        });
        let m = model.to_lowercase();
        let openai = cloud.map(|p| p.id == "openai").unwrap_or(false);
        let reasoning_family = ["o1", "o3", "o4", "gpt-5"].iter().any(|p| m.starts_with(p));
        if openai {
            body["max_completion_tokens"] = serde_json::json!(max_tokens);
            if !reasoning_family {
                body["temperature"] = serde_json::json!(temperature);
            }
        } else {
            body["max_tokens"] = serde_json::json!(max_tokens);
            body["temperature"] = serde_json::json!(temperature);
        }
        body
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
        // On Cloud: the chosen cloud model reads the image (same request
        // shape; every listed provider speaks OpenAI content parts).
        if let Some(c) = self.cloud() {
            return self.transcribe_image_cloud(&c, image_bytes, filename).await;
        }
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

    /// `transcribe_image` against the active cloud model. A model without
    /// image input answers with an error, which the caller reports.
    async fn transcribe_image_cloud(
        &self,
        c: &ActiveCloud,
        image_bytes: &[u8],
        filename: &str,
    ) -> anyhow::Result<String> {
        let ext = filename.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
        let mime = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/png",
        };
        use base64::Engine as _;
        let data_url = format!(
            "data:{};base64,{}",
            mime,
            base64::engine::general_purpose::STANDARD.encode(image_bytes)
        );
        let prompt = "Transcribe ALL text visible in this image exactly as written, including              handwritten text. Preserve the reading order and line breaks of the              original. Do not correct spelling or grammar. Where a word is genuinely              unreadable, write [illegible]. Output ONLY the transcribed text with no              commentary. If the image contains no readable text at all, reply with              exactly: NO_TEXT_FOUND followed by a one-sentence description of the image.";
        let mut body = serde_json::json!({
            "model": c.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
            "stream": false
        });
        // OpenAI's newer models take max_completion_tokens; the rest max_tokens.
        if c.provider.id == "openai" {
            body["max_completion_tokens"] = serde_json::json!(4096);
        } else {
            body["max_tokens"] = serde_json::json!(4096);
            body["temperature"] = serde_json::json!(0.0);
        }
        let mut builder = self
            .http_client
            .post(format!("{}/chat/completions", c.provider.base_url))
            .json(&body);
        for (k, v) in Self::cloud_headers(c) {
            builder = builder.header(k, v);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Could not reach {}: {}", c.provider.name, e))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "{} could not read image '{}' ({}): {}",
                c.provider.name, filename, status, Self::provider_error(&text)
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} response: {}", c.provider.name, e))?;
        let content = v.pointer("/choices/0/message/content").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        info!("{} ({}) transcribed '{}': {} chars", c.provider.name, c.model, filename, content.len());
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
