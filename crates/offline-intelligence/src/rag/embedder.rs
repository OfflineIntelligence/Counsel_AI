//! Embedding backend: a dedicated `llama-server --embedding` process running
//! the bundled model on the already-installed llama.cpp engine. It never
//! touches the chat model's server or slots, starts on demand and stops after
//! a period of inactivity.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lazy_static::lazy_static;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::model;

/// Idle time before the embedding server is stopped (EMBED_IDLE_SECONDS, default 600).
fn idle_shutdown() -> Duration {
    Duration::from_secs(crate::tuning::parse_or("EMBED_IDLE_SECONDS", 600u64))
}
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest input sent in one request item, in bytes.
pub const MAX_INPUT_BYTES: usize = 6000;
/// Tokens kept free in a slot for the model's special tokens.
const SLOT_TOKEN_MARGIN: usize = 16;

/// Largest input, in bytes, that always fits one slot of a server started
/// with `ctx_size` and `parallel` slots. A byte-level tokenizer never makes
/// more tokens than bytes, so a byte bound below the slot size cannot
/// overflow, whatever the text (tables and digits run near one token a byte).
pub fn byte_cap_for(ctx_size: u32, parallel: u32, prefix_bytes: usize) -> usize {
    byte_cap_within(ctx_size, parallel, u32::MAX, prefix_bytes)
}

/// [`byte_cap_for`], also bounded by the physical batch: an embedding input
/// must fit one `--ubatch-size` pass, or the server refuses it.
pub fn byte_cap_within(ctx_size: u32, parallel: u32, ubatch: u32, prefix_bytes: usize) -> usize {
    let slot_tokens = (ctx_size / parallel.max(1)).min(ubatch) as usize;
    slot_tokens
        .saturating_sub(SLOT_TOKEN_MARGIN + prefix_bytes)
        .min(MAX_INPUT_BYTES)
}

/// Default embedding server shape: 2 slots of 2,048 tokens. One slot holds
/// the largest input the indexer sends; half the slots of the former 4x2,048
/// halve the KV cache, which matters when the chat model shares a small GPU.
const DEFAULT_CTX: u32 = 4096;
const DEFAULT_PARALLEL: u32 = 2;
const DEFAULT_BATCH: u32 = 2048;
const DEFAULT_UBATCH: u32 = 2048;

fn embed_ctx() -> u32 {
    crate::tuning::parse_or("EMBED_CTX_SIZE", DEFAULT_CTX)
}
fn embed_parallel() -> u32 {
    crate::tuning::parse_or("EMBED_PARALLEL", DEFAULT_PARALLEL)
}
fn embed_ubatch() -> u32 {
    crate::tuning::parse_or("EMBED_UBATCH_SIZE", DEFAULT_UBATCH)
}

/// Largest input, in bytes, the running configuration accepts.
pub fn input_byte_cap() -> usize {
    byte_cap_within(embed_ctx(), embed_parallel(), embed_ubatch(), 0)
}

fn slot_byte_cap(prefix: &str) -> usize {
    byte_cap_within(embed_ctx(), embed_parallel(), embed_ubatch(), prefix.len())
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Raw (unnormalised is fine) embeddings for passages.
    async fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
    /// Embedding for a search query.
    async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    /// Whether the backend can serve requests without a download.
    fn available(&self) -> bool;
    /// Whether it is warm (serving now, no start-up delay).
    fn warm(&self) -> bool;
}

lazy_static! {
    static ref ENGINE_BINARY: StdMutex<Option<PathBuf>> = StdMutex::new(None);
}

/// Tell the embedder which llama.cpp engine binary the app uses.
pub fn set_engine_binary(path: PathBuf) {
    if let Ok(mut g) = ENGINE_BINARY.lock() {
        *g = Some(path);
    }
}

fn engine_binary() -> Option<PathBuf> {
    if let Some(p) = ENGINE_BINARY.lock().ok().and_then(|g| g.clone()) {
        if p.is_file() {
            return Some(p);
        }
    }
    let engines = crate::config::get_app_data_dir().join("engines");
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let mut found: Vec<PathBuf> = std::fs::read_dir(&engines)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(name))
        .filter(|p| p.is_file())
        .collect();
    // Prefer GPU builds, then newest name.
    found.sort_by_key(|p| {
        let s = p.to_string_lossy().to_lowercase();
        (!(s.contains("cuda") || s.contains("vulkan") || s.contains("metal")), std::cmp::Reverse(s))
    });
    found.into_iter().next()
}

struct Running {
    child: Child,
    base_url: String,
    gpu: bool,
    binary: PathBuf,
    model_id: &'static str,
}

pub struct LlamaEmbedder {
    running: Mutex<Option<Running>>,
    last_used: AtomicU64,
    started_at: Instant,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl LlamaEmbedder {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(None),
            last_used: AtomicU64::new(0),
            started_at: Instant::now(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
        }
    }

    fn touch(&self) {
        self.last_used.store(self.started_at.elapsed().as_secs(), Ordering::SeqCst);
    }

    fn spawn(binary: &PathBuf, spec: &model::EmbeddingModelSpec, port: u16, gpu: bool) -> anyhow::Result<Child> {
        let mut cmd = Command::new(binary);
        cmd.arg("--model").arg(model::model_path_of(spec)).arg("--embedding");
        if let Some(pooling) = spec.pooling {
            cmd.arg("--pooling").arg(pooling);
        }
        cmd.arg("--ctx-size").arg(embed_ctx().to_string())
            .arg("--parallel").arg(embed_parallel().to_string())
            .arg("--batch-size").arg(crate::tuning::parse_or("EMBED_BATCH_SIZE", DEFAULT_BATCH).to_string())
            .arg("--ubatch-size").arg(embed_ubatch().to_string())
            .arg("--n-gpu-layers").arg(if gpu { "99" } else { "0" })
            .args(crate::tuning::embed_engine_args())
            .arg("--host").arg("127.0.0.1")
            .arg("--port").arg(port.to_string());
        crate::model_runtime::process_util::configure_server_command(&mut cmd);
        // Output is not consumed; discard it so full pipes can never block the process.
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        crate::model_runtime::process_util::attach_managed_child(&mut child, "embedding llama-server");
        Ok(child)
    }

    async fn wait_healthy(&self, base_url: &str, child: &mut Child) -> bool {
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return false;
            }
            if let Ok(r) = self.client.get(format!("{}/health", base_url)).timeout(Duration::from_secs(2)).send().await {
                if r.status().is_success() {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        false
    }

    async fn ensure_started(&self) -> anyhow::Result<String> {
        let mut guard = self.running.lock().await;
        let spec = model::active();
        let binary = engine_binary().ok_or_else(|| anyhow::anyhow!("no llama.cpp engine is installed"))?;
        if let Some(r) = guard.as_mut() {
            let same_setup = r.model_id == spec.id && r.binary == binary;
            if same_setup && matches!(r.child.try_wait(), Ok(None)) {
                return Ok(r.base_url.clone());
            }
            if same_setup {
                warn!("Embedding server exited; restarting");
            } else {
                info!("Embedding model or engine changed; restarting the embedding server");
                let _ = r.child.kill();
                let _ = r.child.wait();
            }
            *guard = None;
        }
        if !model::is_installed_spec(spec) {
            anyhow::bail!("embedding model is not installed");
        }
        let cpu_only_engine = binary.to_string_lossy().to_lowercase().contains("-cpu-");
        // EMBED_GPU: auto (GPU, falling back to CPU), gpu, or cpu (keeps all VRAM
        // for the chat model, useful with large chat models).
        let mode = crate::tuning::var("EMBED_GPU").map(|v| v.to_ascii_lowercase());
        let no_gpu_room = mode.is_none()
            && crate::model_runtime::machine::current().map(|p| !p.embed_on_gpu).unwrap_or(false);
        let attempts: &[bool] = if cpu_only_engine || mode.as_deref() == Some("cpu") || no_gpu_room {
            &[false]
        } else if mode.as_deref() == Some("gpu") {
            &[true]
        } else {
            &[true, false]
        };
        for &gpu in attempts {
            let port = free_port()?;
            let base_url = format!("http://127.0.0.1:{}", port);
            let mut child = match Self::spawn(&binary, spec, port, gpu) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Could not start embedding server (gpu={}): {}", gpu, e);
                    continue;
                }
            };
            if self.wait_healthy(&base_url, &mut child).await {
                info!("Embedding server ready at {} (gpu offload: {})", base_url, gpu);
                *guard = Some(Running { child, base_url: base_url.clone(), gpu, binary: binary.clone(), model_id: spec.id });
                self.touch();
                return Ok(base_url);
            }
            let _ = child.kill();
            let _ = child.wait();
            warn!("Embedding server did not become healthy (gpu={}); {}", gpu, if gpu { "retrying on CPU" } else { "giving up" });
        }
        anyhow::bail!("embedding server failed to start")
    }

    async fn request(&self, inputs: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        let base_url = self.ensure_started().await?;
        self.touch();
        let body = serde_json::json!({ "model": model::active().id, "input": inputs });
        let resp = self.client.post(format!("{}/v1/embeddings", base_url)).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("embedding server returned {}: {}", status, text);
        }
        let mut parsed: EmbeddingResponse = resp.json().await?;
        parsed.data.sort_by_key(|d| d.index);
        self.touch();
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    /// Stop the server when idle for long enough.
    pub async fn reap_if_idle(&self) {
        let idle = self.started_at.elapsed().as_secs().saturating_sub(self.last_used.load(Ordering::SeqCst));
        if idle < idle_shutdown().as_secs() {
            return;
        }
        let mut guard = self.running.lock().await;
        if let Some(mut r) = guard.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
            info!("Embedding server stopped after {} s idle (gpu: {})", idle, r.gpu);
        }
    }

    pub async fn shutdown(&self) {
        let mut guard = self.running.lock().await;
        if let Some(mut r) = guard.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }
}

impl Default for LlamaEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for LlamaEmbedder {
    async fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefix = model::active().document_prefix;
        let cap = slot_byte_cap(prefix);
        let inputs = texts.iter().map(|t| format!("{}{}", prefix, truncate_bytes(t, cap))).collect();
        self.request(inputs).await
    }

    async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let prefix = model::active().query_prefix;
        let input = format!("{}{}", prefix, truncate_bytes(text, slot_byte_cap(prefix).min(2000)));
        self.request(vec![input])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding server returned no vector"))
    }

    fn available(&self) -> bool {
        model::is_installed() && engine_binary().is_some()
    }

    fn warm(&self) -> bool {
        self.running.try_lock().map(|g| g.is_some()).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_byte_cap_never_exceeds_one_slot() {
        // Default server: 8192 tokens over 4 slots = 2048 per slot.
        assert!(byte_cap_for(8192, 4, 0) <= 2048 - SLOT_TOKEN_MARGIN);
        // The prefix shares the slot.
        let p = "title: none | text: ".len();
        assert!(byte_cap_for(8192, 4, p) + p + SLOT_TOKEN_MARGIN <= 2048);
        // Fewer slots allow more, up to MAX_INPUT_BYTES.
        assert_eq!(byte_cap_for(8192, 2, 0), 4096 - SLOT_TOKEN_MARGIN);
        assert_eq!(byte_cap_for(65536, 1, 0), MAX_INPUT_BYTES);
        // Degenerate settings do not panic.
        assert_eq!(byte_cap_for(8192, 0, 0), MAX_INPUT_BYTES);
        assert_eq!(byte_cap_for(8, 4, 0), 0);
    }

    #[test]
    fn the_physical_batch_also_bounds_an_input() {
        assert_eq!(byte_cap_within(8192, 1, 2048, 0), 2048 - SLOT_TOKEN_MARGIN);
        assert_eq!(byte_cap_within(4096, 2, 4096, 0), 2048 - SLOT_TOKEN_MARGIN);
        // The new default shape keeps the former per-slot size, so no input is cut shorter.
        assert_eq!(byte_cap_within(DEFAULT_CTX, DEFAULT_PARALLEL, DEFAULT_UBATCH, 0), byte_cap_for(8192, 4, 0));
    }

    #[test]
    fn the_message_that_stalled_the_indexer_now_fits() {
        // The failing input: a numeric table, 2,800 bytes, which the server
        // counted as 2,175 tokens against a 2,048-token slot.
        let table = "0.674 0.842 1.036 1.282 1.645 1.960 2.326 2.576 3.090 3.291 ".repeat(60);
        let cut = truncate_bytes(&table, byte_cap_for(8192, 4, 0));
        // Token count can never exceed byte count, so this cannot overflow 2048.
        assert!(cut.len() + SLOT_TOKEN_MARGIN <= 2048);
    }
}
