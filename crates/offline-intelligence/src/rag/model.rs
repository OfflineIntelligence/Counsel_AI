//! Embedding models: the catalog, which one is active, whether each is
//! installed and verified, and an in-app resumable download (pause, resume,
//! cancel) that mirrors the installer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use futures_util::StreamExt;
use lazy_static::lazy_static;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

#[derive(Debug, Serialize)]
pub struct EmbeddingModelSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size_bytes: u64,
    /// Prepended to search queries.
    pub query_prefix: &'static str,
    /// Prepended to passages at index time.
    pub document_prefix: &'static str,
    /// `--pooling` value; None uses the pooling declared in the GGUF.
    pub pooling: Option<&'static str>,
    pub license: &'static str,
}

pub const QWEN3_EMBEDDING: EmbeddingModelSpec = EmbeddingModelSpec {
    id: "qwen3-embedding-0.6b-q8_0",
    display_name: "Qwen3 Embedding 0.6B (multilingual)",
    description: "Best retrieval quality. 100+ languages, long passages. Recommended.",
    file_name: "Qwen3-Embedding-0.6B-Q8_0.gguf",
    url: "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf",
    sha256: "06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439",
    size_bytes: 639_150_592,
    query_prefix: "Instruct: Given a legal question, retrieve the passages of legal documents that answer it\nQuery:",
    document_prefix: "",
    pooling: Some("last"),
    license: "Apache-2.0",
};

pub const EMBEDDING_GEMMA: EmbeddingModelSpec = EmbeddingModelSpec {
    id: "embeddinggemma-300m-qat-q8_0",
    display_name: "EmbeddingGemma 300M (multilingual, light)",
    description: "About half the size and indexing time. 100+ languages. Slightly lower accuracy.",
    file_name: "embeddinggemma-300m-qat-Q8_0.gguf",
    url: "https://huggingface.co/ggml-org/embeddinggemma-300m-qat-q8_0-GGUF/resolve/main/embeddinggemma-300m-qat-Q8_0.gguf",
    sha256: "6fa0c02a9c302be6f977521d399b4de3a46310a4f2621ee0063747881b673f67",
    size_bytes: 328_577_056,
    query_prefix: "task: search result | query: ",
    document_prefix: "title: none | text: ",
    pooling: None,
    license: "Gemma Terms of Use",
};

/// Installer default comes first.
pub const CATALOG: &[&EmbeddingModelSpec] = &[&QWEN3_EMBEDDING, &EMBEDDING_GEMMA];

/// Kept for callers that refer to the installer's model.
pub const MODEL: &EmbeddingModelSpec = &QWEN3_EMBEDDING;

pub const EMBEDDINGS_DIR: &str = "embeddings";

pub fn find(id: &str) -> Option<&'static EmbeddingModelSpec> {
    CATALOG.iter().copied().find(|m| m.id == id)
}

pub fn embeddings_root() -> PathBuf {
    crate::config::get_app_data_dir().join(EMBEDDINGS_DIR)
}

pub fn model_dir_of(spec: &EmbeddingModelSpec) -> PathBuf {
    embeddings_root().join(spec.id)
}

pub fn model_path_of(spec: &EmbeddingModelSpec) -> PathBuf {
    model_dir_of(spec).join(spec.file_name)
}

fn marker_path_of(spec: &EmbeddingModelSpec) -> PathBuf {
    model_dir_of(spec).join("verified.json")
}

fn partial_path_of(spec: &EmbeddingModelSpec) -> PathBuf {
    model_dir_of(spec).join(format!("{}.part", spec.file_name))
}

fn active_file() -> PathBuf {
    embeddings_root().join("active.json")
}

/// The embedding model in use. Defaults to the installer's model.
pub fn active() -> &'static EmbeddingModelSpec {
    std::fs::read_to_string(active_file())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).and_then(find))
        .unwrap_or(MODEL)
}

/// Make `id` the active model. Stored vectors of another model are
/// discarded by the indexer, which then re-embeds everything.
pub fn set_active(id: &str) -> anyhow::Result<&'static EmbeddingModelSpec> {
    let spec = find(id).ok_or_else(|| anyhow::anyhow!("unknown embedding model '{}'", id))?;
    std::fs::create_dir_all(embeddings_root())?;
    std::fs::write(active_file(), serde_json::json!({ "id": spec.id }).to_string())?;
    info!("Active embedding model set to {}", spec.id);
    super::signal_index_work();
    Ok(spec)
}

pub fn model_path() -> PathBuf {
    model_path_of(active())
}

/// Identity of stored vectors: model plus stored dimensionality.
pub fn model_key() -> String {
    format!("{}@{}", active().id, super::vector::DIMS)
}

lazy_static! {
    static ref VERIFIED_CACHE: Mutex<std::collections::HashMap<&'static str, (u64, bool)>> =
        Mutex::new(std::collections::HashMap::new());
}

/// Installed = pinned size and verified once (marker written by the installer
/// or the in-app download; a right-sized file without a marker is hashed once).
pub fn is_installed_spec(spec: &'static EmbeddingModelSpec) -> bool {
    let path = model_path_of(spec);
    let Ok(meta) = std::fs::metadata(&path) else { return false };
    if meta.len() != spec.size_bytes {
        return false;
    }
    if marker_path_of(spec).is_file() {
        return true;
    }
    if let Ok(guard) = VERIFIED_CACHE.lock() {
        if let Some((len, ok)) = guard.get(spec.id) {
            if *len == meta.len() {
                return *ok;
            }
        }
    }
    let ok = sha256_file(&path).map(|h| h == spec.sha256).unwrap_or(false);
    if ok {
        write_marker(spec);
    } else {
        warn!("Embedding model at {} failed SHA256 verification", path.display());
    }
    if let Ok(mut guard) = VERIFIED_CACHE.lock() {
        guard.insert(spec.id, (meta.len(), ok));
    }
    ok
}

pub fn is_installed() -> bool {
    is_installed_spec(active())
}

fn write_marker(spec: &EmbeddingModelSpec) {
    let body = serde_json::json!({ "id": spec.id, "sha256": spec.sha256, "size_bytes": spec.size_bytes });
    if let Err(e) = std::fs::write(marker_path_of(spec), body.to_string()) {
        warn!("Could not write embedding model verification marker: {}", e);
    }
}

fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Remove a model that is not active.
pub fn remove(id: &str) -> anyhow::Result<()> {
    let spec = find(id).ok_or_else(|| anyhow::anyhow!("unknown embedding model '{}'", id))?;
    if active().id == spec.id {
        anyhow::bail!("the active embedding model cannot be removed; choose another model first");
    }
    if current_download().as_deref() == Some(spec.id) {
        anyhow::bail!("this model is downloading; cancel the download first");
    }
    let dir = model_dir_of(spec);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    if let Ok(mut guard) = VERIFIED_CACHE.lock() {
        guard.remove(spec.id);
    }
    info!("Removed embedding model {}", spec.id);
    Ok(())
}

// ---- download -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DownloadStatus {
    /// idle | downloading | verifying | paused | failed | installed
    pub state: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone)]
struct DlState {
    model: Option<&'static str>,
    state: String,
    error: Option<(&'static str, String)>,
}

lazy_static! {
    static ref DOWNLOADING: AtomicBool = AtomicBool::new(false);
    static ref STOP: AtomicBool = AtomicBool::new(false);
    static ref DISCARD: AtomicBool = AtomicBool::new(false);
    static ref DOWNLOADED: AtomicU64 = AtomicU64::new(0);
    static ref DL: Mutex<DlState> = Mutex::new(DlState { model: None, state: "idle".into(), error: None });
}

fn current_download() -> Option<String> {
    if DOWNLOADING.load(Ordering::SeqCst) {
        DL.lock().ok().and_then(|g| g.model.map(|m| m.to_string()))
    } else {
        None
    }
}

fn set_state(model: &'static str, state: &str, error: Option<String>) {
    if let Ok(mut g) = DL.lock() {
        g.model = Some(model);
        g.state = state.to_string();
        g.error = error.map(|e| (model, e));
    }
}

pub fn download_status_of(spec: &'static EmbeddingModelSpec) -> DownloadStatus {
    let g = DL.lock().map(|g| g.clone()).unwrap_or(DlState { model: None, state: "idle".into(), error: None });
    let partial = std::fs::metadata(partial_path_of(spec)).map(|m| m.len()).unwrap_or(0);
    if g.model == Some(spec.id) && (DOWNLOADING.load(Ordering::SeqCst) || g.state == "failed") {
        return DownloadStatus {
            state: g.state.clone(),
            downloaded_bytes: if DOWNLOADING.load(Ordering::SeqCst) { DOWNLOADED.load(Ordering::SeqCst) } else { partial },
            total_bytes: spec.size_bytes,
            error: g.error.filter(|(m, _)| *m == spec.id).map(|(_, e)| e),
        };
    }
    if is_installed_spec(spec) {
        return DownloadStatus { state: "installed".into(), downloaded_bytes: spec.size_bytes, total_bytes: spec.size_bytes, error: None };
    }
    if partial > 0 {
        return DownloadStatus { state: "paused".into(), downloaded_bytes: partial, total_bytes: spec.size_bytes, error: None };
    }
    DownloadStatus { state: "idle".into(), downloaded_bytes: 0, total_bytes: spec.size_bytes, error: None }
}

pub fn download_status() -> DownloadStatus {
    download_status_of(active())
}

/// Start (or resume) a background download. Returns false when nothing was
/// started (already installed, or another download is running).
pub fn start_download_of(spec: &'static EmbeddingModelSpec) -> bool {
    if is_installed_spec(spec) {
        return false;
    }
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return false;
    }
    STOP.store(false, Ordering::SeqCst);
    DISCARD.store(false, Ordering::SeqCst);
    set_state(spec.id, "downloading", None);
    tokio::spawn(async move {
        let result = download(spec).await;
        let stopped = STOP.load(Ordering::SeqCst);
        let discarded = DISCARD.load(Ordering::SeqCst);
        if discarded {
            let _ = tokio::fs::remove_file(partial_path_of(spec)).await;
        }
        DOWNLOADING.store(false, Ordering::SeqCst);
        match result {
            _ if discarded => {
                set_state(spec.id, "idle", None);
                info!("Embedding model download cancelled ({})", spec.id);
            }
            _ if stopped => {
                set_state(spec.id, "paused", None);
                info!("Embedding model download paused ({})", spec.id);
            }
            Ok(()) => {
                set_state(spec.id, "installed", None);
                info!("Embedding model installed at {}", model_path_of(spec).display());
                super::signal_index_work();
            }
            Err(e) => {
                warn!("Embedding model download failed ({}): {}", spec.id, e);
                set_state(spec.id, "failed", Some(e.to_string()));
            }
        }
    });
    true
}

pub fn start_download() -> bool {
    start_download_of(active())
}

/// Pause keeps the partial file so the next start resumes it.
pub fn pause_download() -> bool {
    if !DOWNLOADING.load(Ordering::SeqCst) {
        return false;
    }
    STOP.store(true, Ordering::SeqCst);
    true
}

/// Cancel stops the download and deletes the partial file. Also clears a
/// paused partial file for `spec` when nothing is running.
pub fn cancel_download_of(spec: &'static EmbeddingModelSpec) -> bool {
    if DOWNLOADING.load(Ordering::SeqCst) && current_download().as_deref() == Some(spec.id) {
        DISCARD.store(true, Ordering::SeqCst);
        STOP.store(true, Ordering::SeqCst);
        return true;
    }
    let removed = std::fs::remove_file(partial_path_of(spec)).is_ok();
    if let Ok(mut g) = DL.lock() {
        if g.model == Some(spec.id) {
            g.state = "idle".into();
            g.error = None;
        }
    }
    removed
}

async fn download(spec: &'static EmbeddingModelSpec) -> anyhow::Result<()> {
    let dir = model_dir_of(spec);
    tokio::fs::create_dir_all(&dir).await?;
    let part = partial_path_of(spec);
    let mut have = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    if have > spec.size_bytes {
        tokio::fs::remove_file(&part).await.ok();
        have = 0;
    }
    DOWNLOADED.store(have, Ordering::SeqCst);

    if have < spec.size_bytes {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;
        let mut req = client.get(spec.url);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={}-", have));
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("download server returned {}", status);
        }
        let append = have > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if !append {
            have = 0;
            DOWNLOADED.store(0, Ordering::SeqCst);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&part)
            .await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if STOP.load(Ordering::SeqCst) {
                file.flush().await?;
                return Ok(());
            }
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            have += chunk.len() as u64;
            DOWNLOADED.store(have, Ordering::SeqCst);
        }
        file.flush().await?;
    }

    set_state(spec.id, "verifying", None);
    let part_for_hash = part.clone();
    let hash = tokio::task::spawn_blocking(move || sha256_file(&part_for_hash)).await??;
    if hash != spec.sha256 {
        tokio::fs::remove_file(&part).await.ok();
        anyhow::bail!("downloaded file failed SHA256 verification and was removed");
    }
    tokio::fs::rename(&part, model_path_of(spec)).await?;
    write_marker(spec);
    if let Ok(mut guard) = VERIFIED_CACHE.lock() {
        guard.remove(spec.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for m in CATALOG {
            assert!(ids.insert(m.id), "duplicate id {}", m.id);
            assert_eq!(m.sha256.len(), 64);
            assert!(m.url.starts_with("https://huggingface.co/") && m.url.ends_with(m.file_name));
            assert!(m.size_bytes > 100_000_000);
        }
        assert_eq!(CATALOG[0].id, MODEL.id, "installer default must be first");
    }
}
