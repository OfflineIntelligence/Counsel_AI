//! Engine Download Progress Tracking
//!
//! Provides real-time progress tracking specifically for engine downloads.
//! This is separate from model download tracking to maintain proper type safety
//! and field naming consistency.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

/// Status of an engine download
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EngineDownloadStatus {
    Queued,
    Starting,
    Downloading,
    Paused,
    Extracting,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl EngineDownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, 
            EngineDownloadStatus::Queued | 
            EngineDownloadStatus::Starting | 
            EngineDownloadStatus::Downloading |
            EngineDownloadStatus::Paused |
            EngineDownloadStatus::Extracting |
            EngineDownloadStatus::Verifying
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, 
            EngineDownloadStatus::Completed | 
            EngineDownloadStatus::Failed | 
            EngineDownloadStatus::Cancelled
        )
    }
}

/// Engine download progress information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineDownloadProgress {
    pub download_id: String,
    pub engine_id: String,
    pub engine_name: String,
    pub status: EngineDownloadStatus,
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub progress_percentage: f32,
    pub speed_bps: f64,
    pub error_message: Option<String>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for EngineDownloadProgress {
    fn default() -> Self {
        Self {
            download_id: String::new(),
            engine_id: String::new(),
            engine_name: String::new(),
            status: EngineDownloadStatus::Queued,
            bytes_downloaded: 0,
            total_bytes: 0,
            progress_percentage: 0.0,
            speed_bps: 0.0,
            error_message: None,
            started_at: None,
            completed_at: None,
        }
    }
}

/// Progress tracker specifically for engine downloads
pub struct EngineDownloadProgressTracker {
    downloads: Arc<RwLock<HashMap<String, EngineDownloadProgress>>>,
}

impl EngineDownloadProgressTracker {
    pub fn new() -> Self {
        Self {
            downloads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start tracking a new engine download
    pub async fn start_download(
        &self,
        engine_id: String,
        engine_name: String,
        total_bytes: u64,
    ) -> String {
        let download_id = Uuid::new_v4().to_string();
        let progress = EngineDownloadProgress {
            download_id: download_id.clone(),
            engine_id: engine_id.clone(),
            engine_name,
            status: EngineDownloadStatus::Starting,
            bytes_downloaded: 0,
            total_bytes,
            progress_percentage: 0.0,
            speed_bps: 0.0,
            error_message: None,
            started_at: Some(chrono::Utc::now()),
            completed_at: None,
        };

        {
            let mut downloads = self.downloads.write().await;
            downloads.insert(engine_id.clone(), progress);
        }

        info!("Started tracking engine download: {} for engine: {}", download_id, engine_id);
        download_id
    }

    /// Update engine download progress
    pub async fn update_progress(
        &self,
        engine_id: &str,
        bytes_downloaded: u64,
        status: EngineDownloadStatus,
        error_message: Option<String>,
    ) {
        let mut downloads = self.downloads.write().await;
        
        if let Some(progress) = downloads.get_mut(engine_id) {
            progress.bytes_downloaded = bytes_downloaded;
            progress.status = status;
            progress.error_message = error_message;

            // Calculate percentage
            if progress.total_bytes > 0 {
                progress.progress_percentage = (bytes_downloaded as f32 / progress.total_bytes as f32) * 100.0;
            }

            // Update completed timestamp if finished
            if progress.status.is_finished() {
                progress.completed_at = Some(chrono::Utc::now());
            }

            debug!("Updated engine download progress for {}: {:.1}%", engine_id, progress.progress_percentage);
        }
    }

    /// Update download status without changing bytes
    pub async fn update_status(
        &self,
        engine_id: &str,
        status: EngineDownloadStatus,
    ) {
        let mut downloads = self.downloads.write().await;
        
        if let Some(progress) = downloads.get_mut(engine_id) {
            progress.status = status;
            
            if progress.status.is_finished() {
                progress.completed_at = Some(chrono::Utc::now());
            }
        }
    }

    /// Get current progress for an engine download
    pub async fn get_progress(&self, engine_id: &str) -> Option<EngineDownloadProgress> {
        let downloads = self.downloads.read().await;
        downloads.get(engine_id).cloned()
    }

    /// Get all active engine downloads
    pub async fn get_active_downloads(&self) -> Vec<EngineDownloadProgress> {
        let downloads = self.downloads.read().await;
        downloads.values()
            .filter(|p| p.status.is_active())
            .cloned()
            .collect()
    }

    /// Get all engine downloads (active and completed)
    pub async fn get_all_downloads(&self) -> Vec<EngineDownloadProgress> {
        let downloads = self.downloads.read().await;
        downloads.values().cloned().collect()
    }

    /// Remove a completed/failed download from tracking
    pub async fn remove_download(&self, engine_id: &str) {
        let mut downloads = self.downloads.write().await;
        if let Some(progress) = downloads.get(engine_id) {
            if progress.status.is_finished() {
                downloads.remove(engine_id);
                info!("Removed completed engine download tracking: {}", engine_id);
            }
        }
    }

    /// Cancel an active download
    pub async fn cancel_download(&self, engine_id: &str) -> bool {
        let mut downloads = self.downloads.write().await;
        if let Some(progress) = downloads.get_mut(engine_id) {
            if progress.status.is_active() {
                progress.status = EngineDownloadStatus::Cancelled;
                progress.completed_at = Some(chrono::Utc::now());
                info!("Cancelled engine download: {}", engine_id);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Clean up old completed downloads (call periodically)
    pub async fn cleanup_old_downloads(&self, max_age_hours: i64) {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(max_age_hours);
        let mut downloads = self.downloads.write().await;
        
        let to_remove: Vec<String> = downloads.iter()
            .filter(|(_, progress)| {
                progress.status.is_finished() && 
                progress.completed_at.map_or(false, |t| t < cutoff)
            })
            .map(|(id, _)| id.clone())
            .collect();
        
        for id in to_remove {
            downloads.remove(&id);
        }
    }
}

impl Default for EngineDownloadProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_download_progress_tracking() {
        let tracker = EngineDownloadProgressTracker::new();
        
        let download_id = tracker.start_download(
            "test-engine".to_string(),
            "Test Engine".to_string(),
            1000
        ).await;

        assert!(!download_id.is_empty());

        // Update progress
        tracker.update_progress("test-engine", 500, EngineDownloadStatus::Downloading, None).await;
        
        let progress = tracker.get_progress("test-engine").await.unwrap();
        assert_eq!(progress.bytes_downloaded, 500);
        assert_eq!(progress.progress_percentage, 50.0);
        assert_eq!(progress.status, EngineDownloadStatus::Downloading);
        assert_eq!(progress.engine_id, "test-engine");
    }

    #[tokio::test]
    async fn test_active_vs_finished() {
        let tracker = EngineDownloadProgressTracker::new();
        
        tracker.start_download(
            "active-engine".to_string(),
            "Active Engine".to_string(),
            1000
        ).await;

        tracker.update_progress("active-engine", 500, EngineDownloadStatus::Downloading, None).await;
        
        let active = tracker.get_active_downloads().await;
        assert_eq!(active.len(), 1);

        // Complete the download
        tracker.update_progress("active-engine", 1000, EngineDownloadStatus::Completed, None).await;
        
        let active = tracker.get_active_downloads().await;
        assert_eq!(active.len(), 0);
        
        let all = tracker.get_all_downloads().await;
        assert_eq!(all.len(), 1);
    }
}
