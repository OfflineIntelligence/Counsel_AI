//! The knowledge collection: legal reference documents shipped with the app.
//!
//! # Where things live
//!
//! - **In the repository and the installer:** a `knowledge/` folder holding the
//!   documents and a `knowledge.json` catalogue. Tauri bundles the folder as a
//!   resource, so after installation it sits next to the executable.
//! - **On the user's computer:** on every launch the documents are copied into
//!   `AppData/knowledge/`, alongside `models/` and `engines/`. Only files that
//!   are missing, changed, or have a newer catalogue version are copied.
//! - **In the database:** each document is a `library_items` row carrying its
//!   catalogue key (`bundled_key`), version and `storage_path` relative to
//!   AppData, so the Library lists it like any other item. Its text is indexed
//!   into the documents store (`document_id`) so chat can cite it.
//!
//! A document the user deletes is recorded in `knowledge_removed` and is never
//! copied back, even after an update.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::shared_state::UnifiedAppState;

/// Folder name, both in the bundle and under AppData.
pub const KNOWLEDGE_DIR: &str = "knowledge";
const CATALOGUE: &str = "knowledge.json";

/// `AppData/knowledge`: where the copied documents live on this computer.
pub fn knowledge_dir() -> PathBuf {
    crate::config::get_app_data_dir().join(KNOWLEDGE_DIR)
}

#[derive(Debug, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub documents: Vec<CatalogueEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueEntry {
    /// Stable identity. Never reuse a key for a different document.
    pub key: String,
    pub title: String,
    /// File name inside the bundled `knowledge/` folder.
    pub file: String,
    /// Bump when the document's content changes, so installs refresh it.
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub jurisdiction: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_version() -> String {
    "1".to_string()
}

/// Find the bundled `knowledge/` folder that holds a catalogue.
///
/// Checked in order: an explicit override, next to the executable (installed
/// builds and `tauri dev`, which copies resources beside the binary), the
/// executable's `resources/` folder, and the repository root (plain
/// `cargo run` during development).
pub fn bundled_source_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("OFFLINE_COUNSEL_KNOWLEDGE_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(KNOWLEDGE_DIR));
            candidates.push(dir.join("resources").join(KNOWLEDGE_DIR));
        }
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(KNOWLEDGE_DIR));
    candidates.into_iter().find(|dir| dir.join(CATALOGUE).is_file())
}

pub fn read_catalogue(source: &Path) -> anyhow::Result<Catalogue> {
    let raw = std::fs::read(source.join(CATALOGUE))?;
    Ok(serde_json::from_slice(&raw)?)
}

/// A file name that is a plain name, not a path that could climb out of the folder.
fn safe_file_name(name: &str) -> Option<&str> {
    let p = Path::new(name);
    let only = p.file_name()?.to_str()?;
    (only == name && !name.is_empty()).then_some(name)
}

/// Copy, record and index every bundled document. Runs in the background at
/// startup; failures are logged per document and never stop the app.
pub fn spawn_startup_sync(state: UnifiedAppState) {
    tokio::spawn(async move {
        let Some(source) = bundled_source_dir() else {
            debug!("No bundled knowledge folder found; nothing to install");
            return;
        };
        let catalogue = match read_catalogue(&source) {
            Ok(c) => c,
            Err(e) => {
                warn!("Knowledge catalogue {} could not be read: {}", source.display(), e);
                return;
            }
        };
        let target = knowledge_dir();
        if let Err(e) = std::fs::create_dir_all(&target) {
            warn!("Knowledge folder {} could not be created: {}", target.display(), e);
            return;
        }
        let mut installed = 0usize;
        for entry in catalogue.documents {
            match sync_one(&state, &source, &target, &entry).await {
                Ok(true) => installed += 1,
                Ok(false) => {}
                Err(e) => warn!("Knowledge document '{}' was not installed: {}", entry.key, e),
            }
        }
        info!("Knowledge collection ready ({} document(s) updated) in {}", installed, target.display());
    });
}

/// Returns whether the document's file or record changed.
async fn sync_one(
    state: &UnifiedAppState,
    source: &Path,
    target: &Path,
    entry: &CatalogueEntry,
) -> anyhow::Result<bool> {
    let db = state.shared_state.database_pool.clone();
    if db.library.is_knowledge_removed(&entry.key)? {
        return Ok(false);
    }
    let file = safe_file_name(&entry.file)
        .ok_or_else(|| anyhow::anyhow!("'{}' is not a plain file name", entry.file))?;
    let format = crate::document_workspace::editable_format_of(file)?;

    let from = source.join(file);
    let to = target.join(file);
    let source_len = std::fs::metadata(&from)?.len();
    let existing = db.library.get_bundled(&entry.key)?;
    let version_changed = existing
        .as_ref()
        .map(|i| i.bundled_version.as_deref() != Some(entry.version.as_str()))
        .unwrap_or(true);
    let file_stale = std::fs::metadata(&to).map(|m| m.len() != source_len).unwrap_or(true);

    if file_stale || version_changed {
        std::fs::copy(&from, &to)?;
    }

    let relative = format!("{}/{}", KNOWLEDGE_DIR, file);
    let item = if existing.is_none() || version_changed {
        db.library.upsert_bundled(
            &entry.key,
            &entry.version,
            &entry.title,
            "knowledge",
            &format,
            &relative,
            source_len as i64,
            entry.jurisdiction.as_deref(),
            entry.category.as_deref(),
            entry.language.as_deref(),
            &entry.tags,
        )?
    } else {
        existing.clone().expect("checked above")
    };

    // Index for chat when never indexed, or when the content was refreshed.
    if item.document_id.is_none() || version_changed || file_stale {
        let bytes = std::fs::read(&to)?;
        let (extracted_text, extraction_status, extraction_error) = crate::utils::extraction_outcome(
            crate::utils::extract_content_from_bytes(&bytes, file).await,
        );
        let doc = db.documents.upsert_document(crate::memory_db::NewDocument {
            original_bytes: &bytes,
            original_filename: file,
            source_path: Some(to.to_string_lossy().to_string()),
            source_kind: "knowledge",
            mime_type: None,
            size_bytes: bytes.len() as i64,
            extracted_text,
            extraction_status,
            extraction_error,
            local_file_id: None,
        })?;
        if let Some(old) = item.document_id.filter(|old| *old != doc.id) {
            db.documents.delete_document(old)?;
        }
        db.library.set_document_id(item.id, Some(doc.id))?;
    }

    Ok(file_stale || version_changed || existing.is_none())
}

/// Delete a bundled document for good: its record, its copy on disk and its
/// chat index, and remember the choice so updates do not restore it.
pub fn remove_bundled(state: &UnifiedAppState, key: &str) -> anyhow::Result<()> {
    let db = &state.shared_state.database_pool;
    if let Some(item) = db.library.get_bundled(key)? {
        if let Some(doc) = item.document_id {
            db.documents.delete_document(doc)?;
        }
        db.library.delete(item.id)?;
        let path = crate::config::get_app_data_dir().join(&item.storage_path);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Knowledge file {} could not be removed: {}", path.display(), e);
            }
        }
    }
    db.library.mark_knowledge_removed(key)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_cannot_climb_out_of_the_folder() {
        assert_eq!(safe_file_name("constitution.pdf"), Some("constitution.pdf"));
        assert_eq!(safe_file_name("../secrets.txt"), None);
        assert_eq!(safe_file_name("sub/doc.pdf"), None);
        assert_eq!(safe_file_name(""), None);
    }

    #[test]
    fn the_repository_catalogue_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(KNOWLEDGE_DIR);
        let catalogue = read_catalogue(&dir).expect("knowledge/knowledge.json must be valid");
        for entry in &catalogue.documents {
            assert!(safe_file_name(&entry.file).is_some(), "{} is not a plain file name", entry.file);
            assert!(dir.join(&entry.file).is_file(), "{} is listed but missing", entry.file);
        }
    }
}
