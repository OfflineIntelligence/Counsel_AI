//! Model Downloader
//!
//! Downloads models from Hugging Face Hub.

use super::{
    progress::{DownloadStatus, ProgressTracker},
    registry::ModelInfo,
    storage::{ModelStorage, ModelMetadata, HardwareRequirements},
};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{fs::File, io::AsyncWriteExt};
use tracing::{error, info, warn};

/// Source from which to download a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DownloadSource {
    HuggingFace { repo_id: String, filename: String },
}

/// Model downloader service
pub struct ModelDownloader {
    storage: Arc<ModelStorage>,
    progress_tracker: Arc<ProgressTracker>,
    http_client: Client,
}

impl ModelDownloader {
    pub fn new(storage: Arc<ModelStorage>) -> Self {
        let client = Client::builder()
            .user_agent("Offline_Intelligence/0.1.1")
            .timeout(std::time::Duration::from_secs(3300))  // 55 minutes for large model downloads
            .build()
            .expect("Failed to create HTTP client");

        Self {
            storage,
            progress_tracker: Arc::new(ProgressTracker::new()),
            http_client: client,
        }
    }

    /// Download a model from the specified source
    /// If `existing_download_id` is provided, uses that for progress tracking instead of creating a new one
    /// If `hf_token` is provided, it will be used for HuggingFace authentication
    pub async fn download_model(
        &self,
        model_info: ModelInfo,
        source: DownloadSource,
        existing_download_id: Option<String>,
        hf_token: Option<String>,
        // Continue from partial files left by an interrupted install (the app
        // was closed mid-download) instead of truncating them.
        resume_partial: bool,
    ) -> Result<String> {
        info!("Starting download of model: {} from {:?}", model_info.name, source);

        // Use existing download ID if provided, otherwise create a new one
        let download_id = if let Some(id) = existing_download_id {
            info!("Using existing download ID: {}", id);
            id
        } else {
            // Start progress tracking with a new ID
            self.progress_tracker
                .start_download(
                    model_info.id.clone(),
                    model_info.name.clone(),
                    Some(model_info.size_bytes),
                )
                .await
        };

        // Update status to starting
        self.progress_tracker
            .update_progress(&download_id, 0, DownloadStatus::Starting, None)
            .await;

        // Create model directory
        let model_dir = self.storage.create_model_directory(&model_info.id)
            .context("Failed to create model directory")?;

        let result: Result<u64> = match &source {
            DownloadSource::HuggingFace { repo_id, filename } => {
                if let Some(total_shards) = self.detect_shard_pattern(filename) {
                    info!("Detected sharded model with {} parts, downloading all shards", total_shards);
                    self.download_sharded_model(&download_id, repo_id, filename, &model_dir, total_shards, hf_token, resume_partial)
                        .await
                        .map(|_| model_info.size_bytes)
                } else {
                    self.download_single_or_vision(
                        &download_id, repo_id, filename, &model_dir, &model_info, hf_token, resume_partial,
                    )
                    .await
                }
            }
        };

        match result {
            Ok(total_bytes) => {
                info!("Successfully downloaded model: {}", model_info.name);
                let completed_bytes = if total_bytes > 0 { total_bytes } else { model_info.size_bytes };
                self.progress_tracker
                    .update_progress(&download_id, completed_bytes, DownloadStatus::Completed, None)
                    .await;
                Ok(download_id)
            }
            Err(e) => {
                error!("Failed to download model {}: {}", model_info.name, e);
                // Failed or cancelled: the file in flight has already been
                // deleted, but files that finished earlier (earlier shards, a
                // main file before its projector) would otherwise be left as
                // gigabytes nothing refers to. Remove the folder — unless the
                // model was installed before this attempt (its metadata is
                // written only on success), so a failed re-download can never
                // take a working model with it.
                if !self.storage.metadata_path(&model_info.id).exists() {
                    match std::fs::remove_dir_all(&model_dir) {
                        Ok(()) => info!("Removed partial files of {} after the failed download", model_info.name),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => warn!("Could not remove partial files in {}: {}", model_dir.display(), err),
                    }
                }
                self.progress_tracker
                    .update_progress(&download_id, 0, DownloadStatus::Failed, Some(e.to_string()))
                    .await;
                Err(e)
            }
        }
    }

    /// Download a single-file model — and, for vision models, its multimodal
    /// projector — as ONE continuous download.
    ///
    /// Why one: the previous implementation ran the projector as a second,
    /// independent progress cycle, so the UI bar snapped back to 0% with a new
    /// total the moment the main file finished — users reasonably read that as
    /// "the download restarted". The combined total is set ONCE up front (via
    /// HEAD requests) and the byte counter runs continuously across both files.
    ///
    /// The projector remains NOT optional: its failure rolls back the entire
    /// model directory so a vision model can never look installed while being
    /// unable to see.
    async fn download_single_or_vision(
        &self,
        download_id: &str,
        repo_id: &str,
        filename: &str,
        model_dir: &std::path::Path,
        model_info: &ModelInfo,
        hf_token: Option<String>,
        resume_partial: bool,
    ) -> Result<u64> {
        let token = hf_token.or_else(|| self.get_hf_token());
        let main_url = format!("https://huggingface.co/{}/resolve/main/{}", repo_id, filename);
        let mmproj: Option<(String, String)> = model_info.mmproj_filename.as_ref().map(|f| {
            (
                format!("https://huggingface.co/{}/resolve/main/{}", repo_id, f),
                f.clone(),
            )
        });

        // One combined total, set before the first byte moves. HEAD is the
        // truth (the catalog's size fields are 0 for many HF repos); the
        // registry size is only a fallback when HEAD fails.
        let main_size = match self.head_content_length(&main_url, &token).await {
            Some(n) => n,
            None => model_info.size_bytes,
        };
        let mmproj_size = match &mmproj {
            Some((url, _)) => match self.head_content_length(url, &token).await {
                Some(n) => n,
                None => model_info.mmproj_size_bytes,
            },
            None => 0,
        };
        let combined_total = main_size + mmproj_size;
        if combined_total > 0 {
            self.progress_tracker
                .update_total_bytes(download_id, combined_total)
                .await;
        }

        info!(
            "Downloading '{}' ({} bytes{}) from {}",
            filename,
            main_size,
            match &mmproj {
                Some((_, f)) => format!(" + vision projector '{}' ({} bytes)", f, mmproj_size),
                None => String::new(),
            },
            repo_id
        );

        // `remaining_after` lets each file correct the job total from its OWN
        // response Content-Length — the only fully authoritative source. HEAD
        // can fail, and catalog sizes are frequently 0 (the HuggingFace list
        // API omits sibling sizes), which previously left the total at
        // main-file-only and drove the bar past 100% once the projector's
        // bytes landed on top of it.
        let mut done: u64 = self
            .download_file_resumable(
                download_id, &main_url, model_dir, filename, &token, 0, mmproj_size, resume_partial,
            )
            .await?;

        if let Some((mmproj_url, mmproj_filename)) = mmproj {
            info!(
                "Vision model {}: downloading multimodal projector '{}'",
                model_info.name, mmproj_filename
            );
            match self
                .download_file_resumable(
                    download_id, &mmproj_url, model_dir, &mmproj_filename, &token, done, 0, resume_partial,
                )
                .await
            {
                Ok(n) => {
                    done += n;
                    info!(
                        "Multimodal projector '{}' downloaded for {}",
                        mmproj_filename, model_info.name
                    );
                }
                Err(e) => {
                    error!(
                        "Projector download failed for vision model {} — rolling back \
                         the whole install (model dir {}): {}",
                        model_info.name,
                        model_dir.display(),
                        e
                    );
                    let _ = std::fs::remove_dir_all(model_dir);
                    return Err(anyhow::anyhow!(
                        "The vision model's multimodal projector ('{}') could not be \
                         downloaded: {}. The install was rolled back — without the \
                         projector the model cannot process images.",
                        mmproj_filename, e
                    ));
                }
            }
        }

        Ok(done)
    }

    /// Size of a remote file, or None on any failure — callers fall back to
    /// catalog sizes; a missing total only degrades the percentage display,
    /// never the download.
    ///
    /// Two layers, both needed (verified against real HF URLs, 2026-08-08):
    /// 1. HEAD, reading the Content-Length HEADER directly. reqwest's
    ///    `content_length()` is NOT that header — it reports the expected
    ///    BODY size, which for a HEAD response is 0. Trusting it made a
    ///    vision install show "112%": the combined total silently fell back
    ///    to the main file's catalog size while the counter kept running
    ///    through the projector.
    /// 2. If the header is absent/0: a 1-byte ranged GET. The 206 response's
    ///    Content-Range ("bytes 0-0/TOTAL") always carries the full size.
    async fn head_content_length(&self, url: &str, token: &Option<String>) -> Option<u64> {
        let mut request = self.http_client.head(url);
        if let Some(t) = token {
            request = request.header("Authorization", format!("Bearer {}", t));
        }
        if let Ok(resp) = request.send().await {
            if resp.status().is_success() {
                let header_len = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|n| *n > 0);
                if header_len.is_some() {
                    return header_len;
                }
            }
        }

        let mut request = self
            .http_client
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-0");
        if let Some(t) = token {
            request = request.header("Authorization", format!("Bearer {}", t));
        }
        let resp = request.send().await.ok()?;
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)?
            .to_str()
            .ok()?
            .rsplit('/')
            .next()?
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
    }

    /// Download one file with CONTINUOUS progress, automatic mid-stream retry,
    /// and partial-file cleanup.
    ///
    /// `offset_base` is the byte count already completed by earlier files of
    /// the same install (main model before projector), so the tracker's
    /// counter never moves backwards across files.
    ///
    /// Resilience (added after a real 3.5GB download died 4 minutes in with
    /// "Error reading download stream" and left an 858MB orphan on disk):
    /// a dropped connection mid-stream is RETRIED up to MAX_STREAM_RETRIES
    /// times, resuming from the bytes already on disk via an HTTP Range
    /// request (HuggingFace's CDN supports ranges; a server that ignores the
    /// Range and replies 200 restarts that file from zero, honestly). Only
    /// when every retry is exhausted does it fail — and then the partial file
    /// is DELETED, never left behind looking like a model.
    async fn download_file_resumable(
        &self,
        download_id: &str,
        url: &str,
        model_dir: &std::path::Path,
        filename: &str,
        token: &Option<String>,
        offset_base: u64,
        remaining_after: u64,
        resume_partial: bool,
    ) -> Result<u64> {
        use futures_util::StreamExt;
        use tokio::io::{AsyncSeekExt, SeekFrom};

        const MAX_STREAM_RETRIES: u32 = 4;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

        let path = model_dir.join(filename);
        // A FAILED install deletes its partial file (see `cleanup`), so a file
        // that is still here was left by an INTERRUPTED one — the app closed
        // or was killed mid-download. Bytes are written strictly in order, so
        // what is on disk is an exact prefix of the file and safe to continue
        // from. Without `resume_partial` the file starts clean, as before.
        let (mut file, mut written) = match resume_partial {
            true => match tokio::fs::OpenOptions::new().write(true).open(&path).await {
                Ok(mut f) => {
                    let len = f.metadata().await?.len();
                    f.seek(SeekFrom::End(0)).await?;
                    if len > 0 {
                        info!("Resuming '{}' from {} bytes already on disk", filename, len);
                    }
                    (f, len)
                }
                Err(_) => (File::create(&path).await?, 0),
            },
            false => (File::create(&path).await?, 0),
        };
        if written > 0 {
            self.progress_tracker
                .update_progress(download_id, offset_base + written, DownloadStatus::Downloading, None)
                .await;
        }
        let mut attempt: u32 = 0;
        let start_time = std::time::Instant::now();

        let cleanup = |reason: &str| {
            let p = path.clone();
            let r = reason.to_string();
            async move {
                if let Err(e) = tokio::fs::remove_file(&p).await {
                    warn!("Could not delete partial file {} after {}: {}", p.display(), r, e);
                } else {
                    info!("Deleted partial file {} ({})", p.display(), r);
                }
            }
        };

        loop {
            let mut request = self.http_client.get(url);
            if let Some(t) = token {
                request = request.header("Authorization", format!("Bearer {}", t));
            }
            if written > 0 {
                request = request.header("Range", format!("bytes={}-", written));
            }

            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    attempt += 1;
                    if attempt > MAX_STREAM_RETRIES {
                        cleanup("download failed").await;
                        return Err(anyhow::anyhow!(
                            "Download of '{}' failed after {} attempts: {}",
                            filename, attempt, e
                        ));
                    }
                    warn!(
                        "Request for '{}' failed (attempt {}/{}): {} — retrying in {}s",
                        filename, attempt, MAX_STREAM_RETRIES, e, RETRY_DELAY.as_secs()
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
            };

            let status = response.status();
            if written > 0 && status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                // Asked for bytes past the end: the file on disk is already
                // complete (a resumed install whose main file finished before
                // the interruption).
                info!("'{}' was already fully downloaded ({} bytes)", filename, written);
                return Ok(written);
            }
            if written > 0 && status == reqwest::StatusCode::OK {
                // Server ignored the Range request: it is sending the whole
                // file again. Restart this file's accounting honestly.
                warn!(
                    "Server ignored the resume request for '{}' — restarting this file from 0",
                    filename
                );
                file.set_len(0).await?;
                file.seek(SeekFrom::Start(0)).await?;
                written = 0;
            } else if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                cleanup("HTTP error").await;
                return Err(anyhow::anyhow!(Self::http_error_message(status, url)));
            }

            // AUTHORITATIVE total, from the response actually being streamed.
            // `content_length()` is this transfer's remaining bytes, so on a
            // resume it must be added to what is already written. Correcting
            // the job total here is what guarantees the bar can never exceed
            // 100%, regardless of what HEAD or the catalog claimed.
            if let Some(this_transfer) = response.content_length() {
                let file_total = if status == reqwest::StatusCode::PARTIAL_CONTENT {
                    written + this_transfer
                } else {
                    this_transfer
                };
                self.progress_tracker
                    .update_total_bytes(download_id, offset_base + file_total + remaining_after)
                    .await;
            }

            let mut stream = response.bytes_stream();
            let mut stream_failed: Option<String> = None;

            while let Some(chunk_result) = stream.next().await {
                // Cancellation / pause, checked per chunk exactly as before.
                if let Some(progress) = self.progress_tracker.get_progress(download_id).await {
                    if progress.status == DownloadStatus::Cancelled {
                        cleanup("cancelled by user").await;
                        return Err(anyhow::anyhow!("Download cancelled by user"));
                    }
                    if progress.status == DownloadStatus::Paused {
                        // Everything received so far is on disk before the
                        // wait, so closing the app while paused loses nothing.
                        file.flush().await?;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            match self.progress_tracker.get_progress(download_id).await {
                                Some(p) if p.status == DownloadStatus::Paused => continue,
                                Some(p) if p.status == DownloadStatus::Cancelled => {
                                    cleanup("cancelled by user").await;
                                    return Err(anyhow::anyhow!("Download cancelled by user"));
                                }
                                _ => break,
                            }
                        }
                    }
                }

                match chunk_result {
                    Ok(chunk) => {
                        file.write_all(&chunk).await?;
                        written += chunk.len() as u64;
                        self.progress_tracker
                            .update_elapsed_time(download_id, start_time.elapsed())
                            .await;
                        self.progress_tracker
                            .record_bytes(download_id, offset_base + written)
                            .await;
                    }
                    Err(e) => {
                        stream_failed = Some(e.to_string());
                        break;
                    }
                }
            }

            match stream_failed {
                None => {
                    file.flush().await?;
                    info!(
                        "Downloaded '{}' ({} bytes{})",
                        filename,
                        written,
                        if attempt > 0 { format!(", after {} resume(s)", attempt) } else { String::new() }
                    );
                    return Ok(written);
                }
                Some(err) => {
                    attempt += 1;
                    if attempt > MAX_STREAM_RETRIES {
                        cleanup("download failed").await;
                        return Err(anyhow::anyhow!(
                            "Error reading download stream for '{}' after {} attempts: {}. \
                             Check your connection and try again — the download will start clean.",
                            filename, attempt, err
                        ));
                    }
                    // Make the retry resume from what is actually on disk.
                    file.flush().await?;
                    warn!(
                        "Stream for '{}' dropped at {} bytes (attempt {}/{}): {} — resuming in {}s",
                        filename, written, attempt, MAX_STREAM_RETRIES, err, RETRY_DELAY.as_secs()
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    /// User-facing message for an HTTP error status, with the 401 gated-model
    /// guidance preserved verbatim from the original implementation.
    fn http_error_message(status: reqwest::StatusCode, url: &str) -> String {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let repo_id = url
                .strip_prefix("https://huggingface.co/")
                .and_then(|s| s.split("/resolve/").next())
                .unwrap_or("unknown");
            format!(
                "Download failed with HTTP status: 401 Unauthorized.\n\n\
                This may be because:\n\
                1. The model requires authentication - check your HF_TOKEN\n\
                2. The model is gated and requires terms acceptance at huggingface.co\n\
                3. You've hit the unauthenticated rate limit (100 req/hour)\n\
                4. The model is private or has been removed\n\n\
                To fix:\n\
                - Get a token from https://huggingface.co/settings/tokens\n\
                - Visit https://huggingface.co/{} to request access to gated models\n\
                - Set your token in the app settings and try again\n\n\
                REPO_ID:{}",
                repo_id, repo_id
            )
        } else {
            format!("Download failed with HTTP status: {}", status)
        }
    }

    /// Detect if the filename follows a shard pattern (e.g., model-00001-of-00003.gguf)
    fn detect_shard_pattern(&self, filename: &str) -> Option<u32> {
        // Pattern: some-name-00001-of-00003.ext
        let re = regex::Regex::new(r".*-(\d{5})-of-(\d{5})\.[^.]+$").ok()?;
        if let Some(caps) = re.captures(filename) {
            if let Some(total_str) = caps.get(2) {
                if let Ok(total) = total_str.as_str().parse::<u32>() {
                    return Some(total);
                }
            }
        }
        None
    }

    /// Download all shards of a sharded model
    async fn download_sharded_model(
        &self,
        download_id: &str,
        repo_id: &str,
        first_filename: &str,
        model_dir: &std::path::Path,
        total_shards: u32,
        hf_token: Option<String>,
        resume_partial: bool,
    ) -> Result<()> {
        // Extract the pattern from the first filename to construct other shard names
        let re = regex::Regex::new(r"(.*-)(\d{5})(-of-\d{5}\.[^.]+)$").unwrap();
        let caps = re.captures(first_filename).ok_or_else(|| {
            anyhow::anyhow!("Invalid shard filename format: {}", first_filename)
        })?;

        let prefix = &caps[1];
        let suffix = &caps[3];
        let token = hf_token.or_else(|| self.get_hf_token());

        // Each shard's size, for one continuous progress bar across all of
        // them. A shard whose size cannot be read counts as 0 here; its own
        // transfer corrects the total once it starts.
        let mut shards: Vec<(String, String, u64)> = Vec::with_capacity(total_shards as usize);
        for i in 1..=total_shards {
            let shard_filename = format!("{}{:05}{}", prefix, i, suffix);
            let url = format!("https://huggingface.co/{}/resolve/main/{}", repo_id, shard_filename);
            let mut head = self.http_client.head(&url);
            if let Some(t) = &token {
                head = head.header("Authorization", format!("Bearer {}", t));
            }
            let size = match head.send().await {
                Ok(r) if r.status().is_success() => r.content_length().unwrap_or(0),
                _ => 0,
            };
            shards.push((shard_filename, url, size));
        }
        let total_expected_size: u64 = shards.iter().map(|s| s.2).sum();
        self.progress_tracker.update_total_bytes(download_id, total_expected_size).await;

        // Every shard goes through the same resumable path as a single-file
        // model: HTTP Range retries mid-stream, pause/cancel checked per
        // chunk, and — when resuming an interrupted install — each shard
        // continues from the bytes already on disk. A shard that finished
        // before the interruption is recognised as complete (the server
        // answers 416 to a range past its end) and is not downloaded again.
        let mut done = 0u64;
        for (index, (shard_filename, url, _)) in shards.iter().enumerate() {
            let remaining_after: u64 = shards[index + 1..].iter().map(|s| s.2).sum();
            info!("Downloading shard {}/{}: {}", index + 1, total_shards, shard_filename);
            done += self
                .download_file_resumable(
                    download_id,
                    url,
                    model_dir,
                    shard_filename,
                    &token,
                    done,
                    remaining_after,
                    resume_partial,
                )
                .await?;
        }

        info!("Successfully downloaded all {} shards", total_shards);
        Ok(())
    }

    /// Get HuggingFace token from environment
    fn get_hf_token(&self) -> Option<String> {
        std::env::var("HF_TOKEN").ok()
    }

    /// Save model metadata after successful download
    pub async fn save_model_metadata(
        &self,
        model_info: &ModelInfo,
        source: &DownloadSource,
    ) -> Result<()> {
        let metadata = ModelMetadata {
            id: model_info.id.clone(),
            name: model_info.name.clone(),
            description: model_info.description.clone(),
            author: model_info.author.clone(),
            size_bytes: model_info.size_bytes,
            format: model_info.format.clone(),
            download_source: match source {
                DownloadSource::HuggingFace { .. } => "huggingface".to_string(),
            },
            download_date: chrono::Utc::now(),
            last_used: None,
            tags: model_info.tags.clone(),
            hardware_requirements: HardwareRequirements::default(),
            compatibility_notes: None,
            runtime_binaries: std::collections::HashMap::new(), // Resolved at runtime from engine registry
            filename: match source {
                DownloadSource::HuggingFace { filename, .. } => Some(filename.clone()),
            },
            // Durable vision record: restarts rebuild the registry from this
            // metadata, so the mmproj reference must survive here.
            mmproj_filename: model_info.mmproj_filename.clone(),
            mmproj_size_bytes: model_info.mmproj_size_bytes,
        };

        let metadata_path = self.storage.metadata_path(&model_info.id);
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        tokio::fs::write(&metadata_path, metadata_json).await?;

        Ok(())
    }

    /// Get reference to progress tracker
    pub fn progress_tracker(&self) -> &Arc<ProgressTracker> {
        &self.progress_tracker
    }

    /// Cancel an ongoing download
    pub async fn cancel_download(&self, download_id: &str) -> bool {
        self.progress_tracker.cancel_download(download_id).await
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_downloader_creation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let storage = Arc::new(ModelStorage {
            location: super::super::storage::StorageLocation {
                app_data_dir: temp_dir.path().to_path_buf(),
                models_dir: temp_dir.path().join("models"),
                registry_dir: temp_dir.path().join("registry"),
            },
        });

        let downloader = ModelDownloader::new(storage);
        assert!(downloader.progress_tracker().get_all_downloads().await.is_empty());
        
        Ok(())
    }
}