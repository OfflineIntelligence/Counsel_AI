//! Engine Downloader
//!
//! Handles downloading and installing llama.cpp engines from official sources
//! and third-party providers, with verification and progress tracking.

use anyhow::Result;
use futures_util::StreamExt;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};

use super::registry::{AccelerationType, EngineInfo, EngineStatus};
use super::download_progress::{EngineDownloadProgressTracker, EngineDownloadProgress, EngineDownloadStatus};

/// Source for engine downloads
#[derive(Debug, Clone)]
pub enum EngineSource {
    OfficialGithub,
    HuggingFace,
    Custom(String),
}

/// Handles downloading and installing engines
pub struct EngineDownloader {
    client: Client,
    download_dir: PathBuf,
    progress_tracker: Arc<EngineDownloadProgressTracker>,
}

impl EngineDownloader {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            download_dir: std::env::temp_dir().join("offline_intelligence_engines"),
            progress_tracker: Arc::new(EngineDownloadProgressTracker::new()),
        }
    }

    /// Download and install an engine — atomically.
    ///
    /// Invariant enforced here: a directory containing metadata.json is ALWAYS a
    /// fully downloaded, hash-verified, backend-verified engine. metadata.json is
    /// written as the LAST step, strictly after:
    ///   1. archive download + pinned SHA256 verification,
    ///   2. extraction,
    ///   3. (Windows CUDA) cudart runtime download + SHA256 + extraction + DLL check,
    ///   4. binary spawn check,
    ///   5. backend self-report verification (same check the boot scan performs).
    /// Any failure deletes the entire engine directory and propagates the error —
    /// no partial install can ever be discovered as a working engine later.
    pub async fn download_engine(&self, engine_info: &EngineInfo) -> Result<EngineInfo> {
        info!("Starting download of engine: {}", engine_info.name);

        let engine_id = engine_info.id.clone();

        self.progress_tracker.start_download(
            engine_id.clone(),
            engine_info.name.clone(),
            engine_info.file_size,
        ).await;

        let install_path = self.get_engine_storage_path()?.join(&engine_info.id);

        let result = self.run_install_steps(engine_info, &install_path).await;

        match result {
            Ok(()) => {
                self.progress_tracker.update_progress(
                    &engine_id,
                    engine_info.file_size,
                    EngineDownloadStatus::Completed,
                    None,
                ).await;

                let mut installed_engine = engine_info.clone();
                installed_engine.status = EngineStatus::Installed;
                installed_engine.install_path = Some(install_path);

                info!("Successfully installed engine: {}", engine_info.name);
                Ok(installed_engine)
            }
            Err(e) => {
                // Roll back: nothing of a failed install may remain on disk.
                if install_path.exists() {
                    match tokio::fs::remove_dir_all(&install_path).await {
                        Ok(()) => error!(
                            "Engine install failed and was rolled back (removed {:?}): {}",
                            install_path, e
                        ),
                        Err(rm_err) => error!(
                            "Engine install failed ({}) AND rollback of {:?} also failed ({}). \
                             Manual cleanup required — the directory has no metadata.json and \
                             will not be treated as an installed engine.",
                            e, install_path, rm_err
                        ),
                    }
                } else {
                    error!("Engine install failed before any files were placed: {}", e);
                }

                self.progress_tracker.update_progress(
                    &engine_id,
                    0,
                    EngineDownloadStatus::Failed,
                    Some(e.to_string()),
                ).await;

                Err(e)
            }
        }
    }

    /// The ordered install pipeline. Called only from download_engine, which
    /// owns rollback. metadata.json is written in the final step.
    async fn run_install_steps(
        &self,
        engine_info: &EngineInfo,
        install_path: &PathBuf,
    ) -> Result<()> {
        let engine_id = engine_info.id.clone();

        // Create temporary download directory
        fs::create_dir_all(&self.download_dir).await?;

        // 1. Download the archive with progress tracking
        let archive_path = self.download_archive_with_progress(engine_info).await?;

        // 2. Integrity: pinned SHA256 (from the GitHub release digests).
        //    Mismatch or missing pin = hard failure before anything is extracted.
        let hash_result = self.verify_pinned_sha256(&archive_path).await;
        if hash_result.is_err() {
            let _ = fs::remove_file(&archive_path).await;
            hash_result?;
        }

        // 3. Extract (metadata is NOT written here)
        self.progress_tracker.update_status(&engine_id, EngineDownloadStatus::Extracting).await;
        let extract_result = self.extract_and_install(engine_info, &archive_path).await;
        let _ = fs::remove_file(&archive_path).await;
        extract_result?;

        // 4. CUDA engines on Windows ship WITHOUT the CUDA runtime DLLs (cublas,
        //    cudart, …) — those live in a separate cudart-*.zip release asset that
        //    must be extracted flat alongside llama-server.exe. Without it the
        //    engine passes --version verification but fails at inference time, so
        //    a cudart failure here is fatal and rolls back the whole install.
        #[cfg(target_os = "windows")]
        if engine_info.acceleration == AccelerationType::CUDA {
            self.download_and_extract_cudart(engine_info, install_path).await?;
        }

        // 5. Verify the binary spawns at all
        self.progress_tracker.update_status(&engine_id, EngineDownloadStatus::Verifying).await;
        self.verify_installation(engine_info, install_path).await?;

        // 6. Backend self-report verification — identical to the check the boot
        //    scan performs, so install-time success guarantees boot-time success.
        let backend = super::dll_manager::DllManager::verify_backend_loads(install_path, engine_info).await?;
        if !backend.backend_loaded() {
            return Err(anyhow::anyhow!(
                "Engine '{}' installed but never self-reported loading its {} backend \
                 on launch — the download is unusable on this machine.\nLaunch output:\n{}",
                engine_info.id, engine_info.acceleration, backend.raw_output
            ));
        }

        // 7. LAST step: metadata.json — presence of this file is the marker that
        //    everything above succeeded.
        self.save_engine_metadata(engine_info, install_path).await?;

        Ok(())
    }

    /// Verify a downloaded release asset against its pinned SHA256.
    /// Assets without a pin are refused (fail closed) — see ENGINE_ASSET_SHA256.
    async fn verify_pinned_sha256(&self, file_path: &PathBuf) -> Result<()> {
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let expected = super::registry::pinned_sha256(&filename).ok_or_else(|| {
            anyhow::anyhow!(
                "No pinned SHA256 exists for release asset '{}' — refusing to install an \
                 unverified download. Add the digest to ENGINE_ASSET_SHA256 (see the \
                 release checklist at ENGINE_VERSION).",
                filename
            )
        })?;

        let path = file_path.clone();
        let actual = tokio::task::spawn_blocking(move || -> Result<String> {
            use sha2::{Digest, Sha256};
            let mut file = std::fs::File::open(&path)?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut file, &mut hasher)?;
            Ok(hex::encode(hasher.finalize()))
        })
        .await
        .map_err(|e| anyhow::anyhow!("Hash task failed: {}", e))??;

        if !actual.eq_ignore_ascii_case(expected) {
            return Err(anyhow::anyhow!(
                "SHA256 mismatch for '{}': expected {}, got {}. The download is corrupted \
                 or tampered with — install aborted.",
                filename, expected, actual
            ));
        }

        info!("SHA256 verified for {}", filename);
        Ok(())
    }
    
    /// Download engine archive from URL with progress tracking
    async fn download_archive_with_progress(&self, engine_info: &EngineInfo) -> Result<PathBuf> {
        let filename = self.get_archive_filename(engine_info);
        let archive_path = self.download_dir.join(&filename);
        let engine_id = engine_info.id.clone();
        
        info!("Downloading {} from {} to {:?}", engine_info.name, engine_info.download_url, archive_path);
        
        // Update status to downloading
        self.progress_tracker.update_status(&engine_id, EngineDownloadStatus::Downloading).await;
        
        let response = self.make_download_request(engine_info).await?;
        response.error_for_status_ref()?;
        
        let total_size = response.content_length().unwrap_or(engine_info.file_size);
        let mut downloaded: u64 = 0;
        let mut file = fs::File::create(&archive_path).await?;
        
        // Track download start time for speed calculation
        let start_time = std::time::Instant::now();
        
        // Stream the response body and report progress
        let mut stream = response.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            
            // Calculate speed and update progress every 100KB or on completion
            if downloaded % (100 * 1024) < chunk.len() as u64 || chunk.len() == 0 {
                let elapsed_secs = start_time.elapsed().as_secs_f64();
                let speed_bps = if elapsed_secs > 0.0 {
                    downloaded as f64 / elapsed_secs
                } else {
                    0.0
                };
                
                // Get current progress to update speed
                if let Some(mut progress) = self.progress_tracker.get_progress(&engine_id).await {
                    progress.bytes_downloaded = downloaded;
                    progress.total_bytes = total_size;
                    progress.speed_bps = speed_bps;
                    if total_size > 0 {
                        progress.progress_percentage = (downloaded as f32 / total_size as f32) * 100.0;
                    }
                    // Update through the tracker
                    self.progress_tracker.update_progress(
                        &engine_id,
                        downloaded,
                        EngineDownloadStatus::Downloading,
                        None
                    ).await;
                }
            }
        }
        
        file.flush().await?;
        info!("Download completed: {} bytes", downloaded);
        
        Ok(archive_path)
    }

    /// Extract archive and install engine
    async fn extract_and_install(&self, engine_info: &EngineInfo, archive_path: &PathBuf) -> Result<PathBuf> {
        let engine_storage_path = self.get_engine_storage_path()?;
        let install_path = engine_storage_path.join(&engine_info.id);
        
        // Create installation directory
        fs::create_dir_all(&install_path).await?;
        
        info!("Extracting to {:?}", install_path);
        
        // Handle different archive formats
        if archive_path.extension().map_or(false, |ext| ext == "zip") {
            self.extract_zip(archive_path, &install_path).await?;
        } else if archive_path.extension().map_or(false, |ext| ext == "tar" || ext == "gz") {
            self.extract_tar_gz(archive_path, &install_path).await?;
        } else {
            return Err(anyhow::anyhow!("Unsupported archive format"));
        }
        
        // Make ALL extracted files executable on Unix (macOS + Linux).
        // llama.cpp releases ship with multiple dylibs alongside the binary
        // (libllama.dylib, libggml.dylib, libggml-metal.dylib, etc.).
        // Only chmod-ing the main binary leaves the dylibs at the archive's
        // default mode (often 0o644), which prevents dlopen from loading them.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(entries) = std::fs::read_dir(&install_path) {
                for entry in entries.flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_file() {
                        if let Ok(meta) = std::fs::metadata(&entry_path) {
                            let mut perms = meta.permissions();
                            perms.set_mode(0o755);
                            let _ = std::fs::set_permissions(&entry_path, perms);
                        }
                    }
                }
            }
        }

        // On macOS: remove the Gatekeeper quarantine extended attribute from
        // every file in the installation directory.
        //
        // Any file fetched programmatically (not opened by the user in Finder)
        // gets `com.apple.quarantine` set by the OS.  Running a quarantined
        // binary triggers a "Developer cannot be verified" dialog or a silent
        // "Operation not permitted" error.  `xattr -r -d` applies recursively
        // so all dylibs are also cleared in one call.
        #[cfg(target_os = "macos")]
        self.remove_quarantine_attribute(&install_path).await;

        // metadata.json is deliberately NOT written here — run_install_steps
        // writes it as the final step, after cudart + backend verification, so
        // that its presence always means "fully verified engine".
        Ok(install_path)
    }

    /// Extract ZIP archive
    async fn extract_zip(&self, archive_path: &PathBuf, destination: &PathBuf) -> Result<()> {
        // Convert to sync operation using blocking task
        let archive_path_owned = archive_path.clone();
        let destination_owned = destination.clone();
        
        let result = tokio::task::spawn_blocking(move || {
            std::fs::File::open(&archive_path_owned)
                .map_err(|e| anyhow::anyhow!("Failed to open archive: {}", e))
                .and_then(|file| {
                    zip::ZipArchive::new(file)
                        .map_err(|e| anyhow::anyhow!("Failed to read ZIP: {}", e))
                })
                .and_then(|mut archive| {
                    for i in 0..archive.len() {
                        let mut file = archive.by_index(i)
                            .map_err(|e| anyhow::anyhow!("Failed to read file from archive: {}", e))?;
                        let outpath = match file.enclosed_name() {
                            Some(path) => destination_owned.join(path),
                            None => continue,
                        };
                        
                        if file.name().ends_with('/') {
                            std::fs::create_dir_all(&outpath)
                                .map_err(|e| anyhow::anyhow!("Failed to create directory: {}", e))?;
                        } else {
                            if let Some(p) = outpath.parent() {
                                if !p.exists() {
                                    std::fs::create_dir_all(p)
                                        .map_err(|e| anyhow::anyhow!("Failed to create parent directory: {}", e))?;
                                }
                            }
                            let mut outfile = std::fs::File::create(&outpath)
                                .map_err(|e| anyhow::anyhow!("Failed to create file: {}", e))?;
                            std::io::copy(&mut file, &mut outfile)
                                .map_err(|e| anyhow::anyhow!("Failed to copy file: {}", e))?;
                        }
                    }
                    Ok(())
                })
        }).await;
        
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Blocking task failed: {}", e))
        }
    }

    /// Extract tar.gz archive
    ///
    /// llama.cpp tar.gz releases use a top-level version directory, e.g.:
    ///   llama-b8037/llama-server
    ///   llama-b8037/libllama.dylib
    ///   ./  (root entry)
    ///
    /// We strip that first path component so all files land flat in `destination`,
    /// matching the same layout produced by the ZIP extractor for Windows.
    async fn extract_tar_gz(&self, archive_path: &PathBuf, destination: &PathBuf) -> Result<()> {
        let archive_path_owned = archive_path.clone();
        let destination_owned = destination.clone();

        let result = tokio::task::spawn_blocking(move || {
            std::fs::File::open(&archive_path_owned)
                .map_err(|e| anyhow::anyhow!("Failed to open archive: {}", e))
                .and_then(|file| {
                    let gz_decoder = flate2::read::GzDecoder::new(file);
                    let mut archive = tar::Archive::new(gz_decoder);

                    for entry in archive.entries()? {
                        let mut entry = entry?;
                        let raw_path = entry.path()?.into_owned();

                        // Strip the leading "./" root entry — skip it entirely.
                        // Then strip the top-level version directory (e.g. "llama-b8037/")
                        // so the binary lands flat at destination/llama-server.
                        let mut components = raw_path.components();
                        let first = components.next();

                        let stripped = match first {
                            // Entry is exactly "." — skip (it's the root dir entry)
                            Some(std::path::Component::CurDir) => {
                                let rest: std::path::PathBuf = components.collect();
                                if rest.as_os_str().is_empty() {
                                    continue; // skip the bare "." entry
                                }
                                // Strip another leading component if present (e.g. "./llama-b8037/")
                                let mut inner = rest.components();
                                inner.next(); // skip version dir
                                let final_path: std::path::PathBuf = inner.collect();
                                if final_path.as_os_str().is_empty() {
                                    continue;
                                }
                                final_path
                            }
                            // Entry starts with version dir directly (e.g. "llama-b8037/llama-server")
                            Some(_) => {
                                let rest: std::path::PathBuf = components.collect();
                                if rest.as_os_str().is_empty() {
                                    continue; // skip the version dir entry itself
                                }
                                rest
                            }
                            None => continue,
                        };

                        // Prevent directory traversal
                        if stripped.components().any(|c| c == std::path::Component::ParentDir) {
                            continue;
                        }

                        let dest_path = destination_owned.join(&stripped);

                        if let Some(parent) = dest_path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }

                        if entry.header().entry_type().is_dir() {
                            std::fs::create_dir_all(&dest_path)?;
                        } else {
                            entry.unpack(&dest_path)?;
                        }
                    }

                    Ok(())
                })
        }).await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Blocking task failed: {}", e)),
        }
    }

    /// Download and extract the CUDA runtime DLL package for a CUDA engine.
    ///
    /// llama.cpp publishes the runtime as a sibling release asset whose name is
    /// derived from the engine asset:
    ///   engine: llama-{ver}-bin-win-cuda-12.4-x64.zip
    ///   cudart: cudart-llama-bin-win-cuda-12.4-x64.zip   (no version in name)
    /// The same scheme holds for the cuda-13.1 variant. Extracted flat into the
    /// engine directory so cublas64_*/cudart64_* sit next to llama-server.exe.
    #[cfg(target_os = "windows")]
    async fn download_and_extract_cudart(
        &self,
        engine_info: &EngineInfo,
        install_path: &PathBuf,
    ) -> Result<()> {
        // Derive the cudart asset name from the engine asset name
        let engine_filename = self.get_archive_filename(engine_info);
        let suffix = engine_filename
            .split_once("bin-")
            .map(|(_, s)| s)
            .ok_or_else(|| anyhow::anyhow!(
                "Cannot derive cudart package name from engine archive '{}'",
                engine_filename
            ))?;
        let cudart_filename = format!("cudart-llama-bin-{}", suffix);

        // Same release directory as the engine asset
        let base_url = engine_info.download_url
            .rsplit_once('/')
            .map(|(base, _)| base)
            .ok_or_else(|| anyhow::anyhow!(
                "Cannot derive release URL from '{}'",
                engine_info.download_url
            ))?;
        let cudart_url = format!("{}/{}", base_url, cudart_filename);

        info!("Downloading CUDA runtime DLLs: {}", cudart_url);

        let response = self.client.get(&cudart_url).send().await
            .map_err(|e| anyhow::anyhow!("CUDA runtime DLL download failed: {}", e))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("CUDA runtime DLL download failed: {}", e))?;

        let cudart_path = self.download_dir.join(&cudart_filename);
        let mut file = fs::File::create(&cudart_path).await?;
        let mut stream = response.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        // Integrity: the cudart package is hash-pinned like every other asset.
        let hash_result = self.verify_pinned_sha256(&cudart_path).await;
        if hash_result.is_err() {
            let _ = fs::remove_file(&cudart_path).await;
            hash_result?;
        }

        // Extract flat into the engine directory (same layout as the engine ZIP)
        self.extract_zip(&cudart_path, install_path).await?;
        let _ = fs::remove_file(&cudart_path).await;

        // Verify the runtime actually landed: every cudart package contains a
        // cudart64_*.dll (and cublas64_*.dll). Absence means a broken extract.
        let has_runtime_dll = std::fs::read_dir(install_path)
            .map(|entries| {
                entries.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    name.starts_with("cudart64_") || name.starts_with("cublas64_")
                })
            })
            .unwrap_or(false);

        if !has_runtime_dll {
            return Err(anyhow::anyhow!(
                "CUDA runtime package extracted but no cudart64_*/cublas64_* DLL found in {:?} — \
                 CUDA inference would fail at runtime. Aborting install.",
                install_path
            ));
        }

        info!("CUDA runtime DLLs installed alongside engine binary");
        Ok(())
    }

    /// Verify that the engine was installed correctly
    async fn verify_installation(&self, engine_info: &EngineInfo, install_path: &PathBuf) -> Result<()> {
        let binary_path = install_path.join(&engine_info.binary_name);

        if !binary_path.exists() {
            return Err(anyhow::anyhow!("Engine binary not found at {:?}", binary_path));
        }

        // Test that the binary can be spawned.  We intentionally ignore the
        // exit code: `llama-server --help` exits with code 1 on many llama.cpp
        // builds (it prints help text then returns 1), so checking
        // `status.success()` would incorrectly reject a healthy binary.
        // What matters is that the OS was able to execute it at all.
        let mut cmd = tokio::process::Command::new(&binary_path);
        cmd.arg("--help");
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash

        match cmd.output().await
        {
            Ok(_) => {
                info!("Engine binary verified: {:?}", binary_path);
                Ok(())
            }
            Err(e) => {
                let hint = if cfg!(target_os = "macos") {
                    format!(
                        "Failed to execute engine binary: {}.\n\
                         On macOS this is usually Gatekeeper quarantine — try:\n\
                           xattr -r -d com.apple.quarantine {:?}",
                        e, binary_path
                    )
                } else {
                    format!("Failed to execute engine binary: {}", e)
                };
                Err(anyhow::anyhow!("{}", hint))
            }
        }
    }

    /// Remove the macOS Gatekeeper quarantine extended attribute from every
    /// file in `dir` so that programmatically-downloaded binaries and dylibs
    /// can be executed without a "Developer cannot be verified" dialog.
    ///
    /// Uses `xattr -r -d com.apple.quarantine <dir>` which is available on
    /// all macOS versions that Tauri targets (10.13+).  Errors are logged but
    /// not propagated — a failed removal is non-fatal because the binary may
    /// still work if Gatekeeper decides not to block it.
    #[cfg(target_os = "macos")]
    async fn remove_quarantine_attribute(&self, dir: &PathBuf) {
        let dir_owned = dir.clone();
        match tokio::process::Command::new("xattr")
            .args(["-r", "-d", "com.apple.quarantine"])
            .arg(&dir_owned)
            .output()
            .await
        {
            Ok(output) => {
                if output.status.success() {
                    info!("Removed quarantine attribute from {:?}", dir_owned);
                } else {
                    // xattr exits non-zero when no quarantine attr exists — not an error
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if !stderr.contains("No such xattr") && !stderr.trim().is_empty() {
                        tracing::warn!("xattr removal warning for {:?}: {}", dir_owned, stderr.trim());
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Could not run xattr to remove quarantine from {:?}: {}", dir_owned, e);
            }
        }
    }

    /// Save engine metadata to installation directory
    async fn save_engine_metadata(&self, engine_info: &EngineInfo, install_path: &PathBuf) -> Result<()> {
        let metadata_path = install_path.join("metadata.json");
        let metadata_json = serde_json::to_string_pretty(engine_info)?;
        fs::write(&metadata_path, metadata_json).await?;
        Ok(())
    }

    /// Get appropriate archive filename based on engine info
    fn get_archive_filename(&self, engine_info: &EngineInfo) -> String {
        let url_path = std::path::Path::new(&engine_info.download_url);
        url_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("engine_archive.zip")
            .to_string()
    }

    /// Make download request with the primary URL only
    async fn make_download_request(&self, engine_info: &EngineInfo) -> Result<reqwest::Response> {
        let url = &engine_info.download_url;
        info!("Attempting to download from: {}", url);
        
        let response = self.client.get(url).send().await?;
        
        match response.error_for_status() {
            Ok(response) => {
                info!("Successfully connected to download URL: {}", url);
                Ok(response)
            }
            Err(e) => {
                error!("Download failed with status error: {} - {}", url, e);
                Err(anyhow::anyhow!("Download failed: {}", e))
            }
        }
    }

    /// Get storage path for engines.
    fn get_engine_storage_path(&self) -> Result<PathBuf> {
        Ok(crate::config::get_app_data_dir().join("engines"))
    }

    /// Check if an engine is already downloaded
    pub async fn is_engine_downloaded(&self, engine_id: &str) -> Result<bool> {
        let storage_path = self.get_engine_storage_path()?;
        let engine_path = storage_path.join(engine_id);
        Ok(engine_path.exists())
    }

    /// Get download progress for an engine
    pub async fn get_download_progress(&self, engine_id: &str) -> Result<Option<f64>> {
        if let Some(progress) = self.progress_tracker.get_progress(engine_id).await {
            return Ok(Some(progress.progress_percentage as f64));
        }
        Ok(None)
    }
    
    /// Get all download progresses
    pub async fn get_all_download_progress(&self) -> Vec<EngineDownloadProgress> {
        self.progress_tracker.get_all_downloads().await
    }
    
    /// Get reference to progress tracker
    pub fn progress_tracker(&self) -> &Arc<EngineDownloadProgressTracker> {
        &self.progress_tracker
    }

    /// Cancel an ongoing download
    pub async fn cancel_download(&self, engine_id: &str) -> Result<()> {
        if self.progress_tracker.cancel_download(engine_id).await {
            info!("Cancelled download for engine: {}", engine_id);
        } else {
            info!("No active download found to cancel for engine: {}", engine_id);
        }
        Ok(())
    }
}

