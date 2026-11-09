//! Document workspace — the Draft feature's backend.
//!
//! # Preserve-and-patch
//!
//! Documents are edited by PRESERVE-AND-PATCH: the original bytes stay the
//! source of truth, edits arrive as typed patches addressed to individual
//! nodes, and saving mutates only those nodes. Nothing the editor does not
//! understand is ever rewritten, which is what makes "the document loses
//! nothing" a property of the design rather than a hope. See
//! `ooxml::package` for the invariant and the tests that hold it.
//!
//! # The Vault is never mutated
//!
//! Opening a Vault document COPIES its bytes into `AppData/drafts/{id}/v1.ext`.
//! Version 1 is never amended, so the pristine original is recoverable for the
//! life of the draft, and the user's library cannot be damaged by editing.

pub mod compare;
pub mod ooxml;
pub mod pdf_workspace;
pub mod text_workspace;
pub mod view_model;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

use crate::memory_db::drafts_store::{DraftRecord, NewDraft};
use crate::memory_db::MemoryDatabase;
use crate::utils::storage_governor::{DRAFTS_DIR, PAGE_CACHE_DIR};
use view_model::{DraftPatch, DraftViewModel};

/// Formats the workspace can open. Deliberately narrower than the attachment
/// policy: legacy binary Office files (.doc/.xls/.ppt) are accepted as
/// attachments (where they are refused with an explicit "save as .docx"
/// message) but must never be opened in an editor that cannot round-trip
/// them — that would be the silent-corruption failure this module exists to
/// prevent.
pub const EDITABLE_FORMATS: &[&str] = &["docx", "xlsx", "pptx", "pdf", "txt"];

/// Upper bound on a package we will open in an editor.
///
/// Every part is held decompressed in memory while editing (see
/// `ooxml::package`), so this is a real memory bound, not a policy whim. Beyond
/// it the file stays readable through the existing document viewer; it simply
/// is not editable. Chosen to sit below the 50 MB request limit so a draft can
/// always be uploaded and downloaded through the API.
pub const MAX_EDITABLE_BYTES: u64 = 40 * 1024 * 1024;

/// Normalise and validate a filename's extension against `EDITABLE_FORMATS`.
pub fn editable_format_of(filename: &str) -> Result<String> {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();

    if EDITABLE_FORMATS.contains(&ext.as_str()) {
        return Ok(ext);
    }
    // Name the legacy case specifically: "unsupported" is useless advice when
    // the fix is one Save As away.
    if matches!(ext.as_str(), "doc" | "xls" | "ppt") {
        return Err(anyhow!(
            "'{}' is a legacy binary Office file, which cannot be edited without losing \
             its contents. Open it in Word/Excel/PowerPoint and save it as .{}x, then \
             add it again.",
            filename,
            ext
        ));
    }
    Err(anyhow!(
        "'{}' cannot be edited in the workspace. Editable types are: {}.",
        filename,
        EDITABLE_FORMATS.join(", ").to_uppercase()
    ))
}

/// How many edits Undo remembers per draft.
pub const MAX_UNDO_STEPS: usize = 100;

/// Which of a draft's two snapshot stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoStack {
    Undo,
    Redo,
}

/// Orchestrates draft files on disk alongside the metadata in `DraftsStore`.
pub struct DraftManager {
    app_data_dir: PathBuf,
}

impl DraftManager {
    pub fn new(app_data_dir: PathBuf) -> Self {
        Self { app_data_dir }
    }

    fn draft_dir(&self, draft_id: i64) -> PathBuf {
        self.app_data_dir.join(DRAFTS_DIR).join(draft_id.to_string())
    }

    /// Relative path stored in the database (`drafts/7/v3.docx`), kept
    /// app-data-relative so the whole directory can be moved.
    fn relative_version_path(draft_id: i64, version_no: i64, format: &str) -> String {
        format!("{}/{}/v{}.{}", DRAFTS_DIR, draft_id, version_no, format)
    }

    pub fn absolute(&self, relative: &str) -> PathBuf {
        self.app_data_dir.join(relative)
    }

    /// Create a draft from raw bytes. Version 1 is written verbatim.
    pub fn create(
        &self,
        db: &MemoryDatabase,
        title: &str,
        filename: &str,
        bytes: &[u8],
        origin_kind: &str,
        source_document_id: Option<i64>,
        source_local_file_id: Option<i64>,
    ) -> Result<DraftRecord> {
        let format = editable_format_of(filename)?;

        if bytes.len() as u64 > MAX_EDITABLE_BYTES {
            return Err(anyhow!(
                "'{}' is {:.1} MB, beyond the {} MB the workspace can edit. It remains \
                 readable in the document viewer.",
                filename,
                bytes.len() as f64 / 1_048_576.0,
                MAX_EDITABLE_BYTES / 1_048_576
            ));
        }
        if bytes.is_empty() {
            return Err(anyhow!("'{}' is empty; there is nothing to edit.", filename));
        }

        // Validate BEFORE creating any row, so a file we cannot open never
        // leaves a half-made draft behind. This is also where a
        // password-protected package is rejected by name.
        Self::validate_openable(&format, bytes)
            .with_context(|| format!("'{}' cannot be opened for editing", filename))?;

        // The row must exist before the path is known (the path contains the id),
        // so create with a placeholder path and correct it immediately.
        let placeholder = Self::relative_version_path(0, 1, &format);
        let draft = db.drafts.create_draft(
            NewDraft {
                title,
                format: &format,
                origin_kind,
                source_document_id,
                source_local_file_id,
            },
            &placeholder,
            bytes.len() as i64,
        )?;

        let relative = Self::relative_version_path(draft.id, 1, &format);
        let absolute = self.absolute(&relative);
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        std::fs::write(&absolute, bytes)
            .with_context(|| format!("could not write {}", absolute.display()))?;

        db.drafts.fix_version_path(draft.id, 1, &relative)?;

        info!(
            "Draft {} created from '{}' ({} bytes, {})",
            draft.id,
            filename,
            bytes.len(),
            origin_kind
        );
        // Indexed on creation, so an imported document is findable before
        // anyone has opened it — which is exactly when search matters most.
        self.reindex(db, draft.id);
        Ok(draft)
    }

    /// Cheap structural check that the bytes really are the format claimed.
    fn validate_openable(format: &str, bytes: &[u8]) -> Result<()> {
        match format {
            "docx" | "xlsx" | "pptx" => {
                ooxml::package::OoxmlPackage::open(bytes)?;
                Ok(())
            }
            "pdf" => {
                if bytes.starts_with(b"%PDF") {
                    Ok(())
                } else {
                    Err(anyhow!("this file does not start with a PDF header"))
                }
            }
            "txt" => Ok(()),
            other => Err(anyhow!("unsupported workspace format '{}'", other)),
        }
    }

    /// Re-index a draft's text for search.
    ///
    /// Deliberately infallible from the caller's point of view: a failure to
    /// index must never fail the save that triggered it. Losing a search
    /// result is an inconvenience the backfill repairs on next startup;
    /// losing an edit is not recoverable at all.
    ///
    /// # Cost
    ///
    /// This parses the document a second time. `apply_patches` hands back
    /// bytes rather than a model, so the model it built internally is already
    /// gone by the time we get here. Avoiding the reparse would mean widening
    /// the `apply` signature through all three OOXML modules to return both —
    /// a large change to save a few milliseconds on a debounced save path.
    pub fn reindex(&self, db: &MemoryDatabase, draft_id: i64) {
        let outcome = (|| -> Result<()> {
            let draft = db
                .drafts
                .get_draft(draft_id)?
                .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
            let model = self.view_model(db, draft_id)?;
            let body = view_model::plain_text(&model);
            db.search
                .index_draft(draft_id, &draft.title, &body, draft.current_version)
        })();

        if let Err(e) = outcome {
            warn!("Draft {} could not be indexed for search: {}", draft_id, e);
        }
    }

    /// Current version's bytes.
    pub fn read_current(&self, db: &MemoryDatabase, draft_id: i64) -> Result<Vec<u8>> {
        let version = db
            .drafts
            .current_version(draft_id)?
            .ok_or_else(|| anyhow!("draft {} has no current version", draft_id))?;
        let path = self.absolute(&version.storage_path);
        std::fs::read(&path).with_context(|| {
            format!(
                "the bytes for draft {} version {} are missing from {}",
                draft_id,
                version.version_no,
                path.display()
            )
        })
    }

    pub fn read_version(&self, db: &MemoryDatabase, draft_id: i64, version_no: i64) -> Result<Vec<u8>> {
        let version = db
            .drafts
            .get_version(draft_id, version_no)?
            .ok_or_else(|| anyhow!("draft {} has no version {}", draft_id, version_no))?;
        std::fs::read(self.absolute(&version.storage_path)).with_context(|| {
            format!("could not read draft {} version {}", draft_id, version_no)
        })
    }

    /// Build the editable view model for the current version.
    pub fn view_model(&self, db: &MemoryDatabase, draft_id: i64) -> Result<DraftViewModel> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
        let bytes = self.read_current(db, draft_id)?;
        view_model::build(&draft.format, &bytes, draft.current_version)
    }

    /// Apply patches to the current bytes and persist the result.
    ///
    /// `cut_new_version` distinguishes autosave (amend the current version)
    /// from an explicit save (cut a new one) — the rule that stops debounced
    /// typing from producing hundreds of versions. Version 1 is never amended:
    /// the first edit after opening always cuts version 2.
    /// View model for a SPECIFIC version, for comparison against history.
    ///
    /// `view_model` reads whatever is current; this reads a named version, so
    /// "what did this clause say three saves ago" is answerable.
    pub fn view_model_at(
        &self,
        db: &MemoryDatabase,
        draft_id: i64,
        version_no: i64,
    ) -> Result<DraftViewModel> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
        let bytes = self.read_version(db, draft_id, version_no)?;
        view_model::build(&draft.format, &bytes, version_no)
    }

    pub fn apply_patches(
        &self,
        db: &MemoryDatabase,
        draft_id: i64,
        patches: &[DraftPatch],
        cut_new_version: bool,
        label: Option<&str>,
    ) -> Result<i64> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;

        let current_bytes = self.read_current(db, draft_id)?;
        let new_bytes = view_model::apply(&draft.format, &current_bytes, patches)?;
        let patch_json = serde_json::to_string(patches).ok();

        // Every change that alters the bytes is one undo step; a new edit
        // makes whatever was undone before it unreachable, as in any editor.
        if new_bytes != current_bytes {
            self.push_snapshot(draft_id, UndoStack::Undo, &draft.format, &current_bytes)?;
            self.clear_stack(draft_id, UndoStack::Redo);
        }

        self.write_current(db, draft_id, &new_bytes, cut_new_version, patch_json.as_deref(), label)
    }

    /// Persist `new_bytes` as the draft's current state: amend the current
    /// version, or cut a new one.
    fn write_current(
        &self,
        db: &MemoryDatabase,
        draft_id: i64,
        new_bytes: &[u8],
        cut_new_version: bool,
        patch_json: Option<&str>,
        label: Option<&str>,
    ) -> Result<i64> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
        let patches_len = patch_json.map(|p| p.matches("\"op\"").count()).unwrap_or(0);

        // Amend only when explicitly allowed AND we are not sitting on the
        // pristine original.
        let amend = !cut_new_version && draft.current_version > 1;

        if amend {
            let version = db
                .drafts
                .current_version(draft_id)?
                .ok_or_else(|| anyhow!("draft {} has no current version", draft_id))?;
            std::fs::write(self.absolute(&version.storage_path), new_bytes)?;
            db.drafts.amend_current_version(draft_id, new_bytes.len() as i64, patch_json)?;
            // The version number did not change, so the cache key did not
            // either — this is the case that would otherwise serve a stale
            // page image for edits made inside the amend window.
            self.prune_page_cache(draft_id);
            self.reindex(db, draft_id);
            Ok(version.version_no)
        } else {
            let next = draft.current_version + 1;
            let relative = Self::relative_version_path(draft_id, next, &draft.format);
            let absolute = self.absolute(&relative);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&absolute, new_bytes)?;
            let version_no = db.drafts.add_version(draft_id, &relative, new_bytes.len() as i64, patch_json, label)?;
            self.prune_page_cache(draft_id);
            self.reindex(db, draft_id);
            info!(
                "Draft {} saved as version {} ({} patches, {} bytes)",
                draft_id,
                version_no,
                patches_len,
                new_bytes.len()
            );
            Ok(version_no)
        }
    }

    // ------------------------------------------------------------ undo / redo

    fn stack_dir(&self, draft_id: i64, stack: UndoStack) -> PathBuf {
        self.draft_dir(draft_id).join(match stack {
            UndoStack::Undo => "undo",
            UndoStack::Redo => "redo",
        })
    }

    /// Snapshot files in a stack, oldest first. Named by a zero-padded
    /// sequence number so lexical order is push order.
    fn stack_entries(&self, draft_id: i64, stack: UndoStack) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(self.stack_dir(draft_id, stack))
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect())
            .unwrap_or_default();
        entries.sort();
        entries
    }

    fn push_snapshot(&self, draft_id: i64, stack: UndoStack, format: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.stack_dir(draft_id, stack);
        std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let entries = self.stack_entries(draft_id, stack);
        let next = entries
            .last()
            .and_then(|p| p.file_stem()?.to_str()?.parse::<u64>().ok())
            .map(|n| n + 1)
            .unwrap_or(1);
        std::fs::write(dir.join(format!("{:010}.{}", next, format)), bytes)?;
        // Bounded: the oldest steps fall off, exactly as in Word.
        let entries = self.stack_entries(draft_id, stack);
        if entries.len() > MAX_UNDO_STEPS {
            for old in &entries[..entries.len() - MAX_UNDO_STEPS] {
                let _ = std::fs::remove_file(old);
            }
        }
        Ok(())
    }

    fn clear_stack(&self, draft_id: i64, stack: UndoStack) {
        let _ = std::fs::remove_dir_all(self.stack_dir(draft_id, stack));
    }

    /// How many steps can be undone and redone.
    pub fn undo_depth(&self, draft_id: i64) -> (usize, usize) {
        (self.stack_entries(draft_id, UndoStack::Undo).len(), self.stack_entries(draft_id, UndoStack::Redo).len())
    }

    /// Step back (`Undo`) or forward (`Redo`) one edit.
    ///
    /// The restored bytes are written the way an autosave is — amending the
    /// current version — so undoing does not litter the version history. The
    /// state being left goes onto the opposite stack, so every undo can be
    /// redone. Returns `None` when there is nothing to step to.
    pub fn step(&self, db: &MemoryDatabase, draft_id: i64, direction: UndoStack) -> Result<Option<i64>> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
        let Some(snapshot) = self.stack_entries(draft_id, direction).pop() else {
            return Ok(None);
        };
        let target = std::fs::read(&snapshot).with_context(|| format!("could not read {}", snapshot.display()))?;
        let current = self.read_current(db, draft_id)?;

        let opposite = match direction {
            UndoStack::Undo => UndoStack::Redo,
            UndoStack::Redo => UndoStack::Undo,
        };
        self.push_snapshot(draft_id, opposite, &draft.format, &current)?;
        let version = self.write_current(db, draft_id, &target, false, None, None)?;
        std::fs::remove_file(&snapshot).ok();
        Ok(Some(version))
    }

    /// Restore an old version by writing its bytes as a NEW version, so
    /// restoring never destroys the state being restored from.
    pub fn restore_version(&self, db: &MemoryDatabase, draft_id: i64, version_no: i64) -> Result<i64> {
        let draft = db
            .drafts
            .get_draft(draft_id)?
            .ok_or_else(|| anyhow!("draft {} not found", draft_id))?;
        let bytes = self.read_version(db, draft_id, version_no)?;
        // A restore is an edit like any other, so Undo can take it back.
        if let Ok(current) = self.read_current(db, draft_id) {
            if current != bytes {
                self.push_snapshot(draft_id, UndoStack::Undo, &draft.format, &current)?;
                self.clear_stack(draft_id, UndoStack::Redo);
            }
        }

        let next = draft.current_version + 1;
        let relative = Self::relative_version_path(draft_id, next, &draft.format);
        let absolute = self.absolute(&relative);
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&absolute, &bytes)?;
        let created = db.drafts.add_version(
            draft_id,
            &relative,
            bytes.len() as i64,
            None,
            Some(&format!("Restored from v{}", version_no)),
        )?;
        self.prune_page_cache(draft_id);
        self.reindex(db, draft_id);
        info!("Draft {} restored v{} as v{}", draft_id, version_no, created);
        Ok(created)
    }

    /// Delete a draft's metadata and every byte it owns.
    pub fn delete(&self, db: &MemoryDatabase, draft_id: i64) -> Result<()> {
        // Before the row goes: draft_text cascades with it, but draft_fts is a
        // virtual table with no foreign keys. Left behind, it would keep a
        // deleted document's text searchable and return a hit that cannot be
        // opened.
        if let Err(e) = db.search.remove_draft(draft_id) {
            warn!("Draft {} left an entry in the search index: {}", draft_id, e);
        }
        db.drafts.delete_draft(draft_id)?;
        let dir = self.draft_dir(draft_id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                // The row is gone, so the draft is invisible either way; leaving
                // orphaned bytes silently would misreport disk usage, so say so.
                warn!(
                    "Deleted draft {} but could not remove {}: {}",
                    draft_id,
                    dir.display(),
                    e
                );
            }
        }
        Ok(())
    }

    /// Directory holding this draft's rendered page images.
    fn page_cache_dir(&self, draft_id: i64) -> PathBuf {
        self.draft_dir(draft_id).join(PAGE_CACHE_DIR)
    }

    /// Where one rendered page image is cached.
    ///
    /// The version is in the key, but that is NOT on its own enough to keep the
    /// cache honest: an autosave inside the amend window rewrites the bytes of
    /// the CURRENT version without changing its number, so a highlight added
    /// during that window would keep serving the pre-highlight image. Anything
    /// that changes a draft's bytes therefore calls `prune_page_cache`.
    pub fn page_cache_path(&self, draft_id: i64, version_no: i64, page: usize, scale_x10: u32) -> PathBuf {
        self.page_cache_dir(draft_id)
            .join(format!("v{}-p{}-s{}.png", version_no, page, scale_x10))
    }

    /// Drop every rendered page image for a draft.
    ///
    /// Called on every byte change. Blunt on purpose: working out which pages a
    /// patch could have altered is guesswork (a deleted page renumbers every
    /// page after it), and re-rendering is cheap next to showing a lawyer a
    /// page that does not match the file.
    pub fn prune_page_cache(&self, draft_id: i64) {
        let dir = self.page_cache_dir(draft_id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                // Not fatal, but it does mean stale images may be served, so it
                // is said out loud rather than swallowed.
                warn!(
                    "Could not clear the rendered-page cache for draft {} ({}); \
                     pages may render from a stale image until the next save",
                    draft_id, e
                );
            }
        }
    }

    pub fn app_data_dir(&self) -> &Path {
        &self.app_data_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_binary_office_files_are_refused_with_actionable_advice() {
        for name in ["contract.doc", "fees.xls", "deck.ppt"] {
            let err = editable_format_of(name).unwrap_err().to_string();
            assert!(err.contains("legacy binary"), "{}", err);
            assert!(err.contains("save it as"), "{}", err);
        }
    }

    #[test]
    fn editable_formats_are_accepted_case_insensitively() {
        assert_eq!(editable_format_of("Agreement.DOCX").unwrap(), "docx");
        assert_eq!(editable_format_of("fees.XlsX").unwrap(), "xlsx");
        assert_eq!(editable_format_of("notes.TXT").unwrap(), "txt");
        assert_eq!(editable_format_of("scan.pdf").unwrap(), "pdf");
        assert_eq!(editable_format_of("deck.pptx").unwrap(), "pptx");
    }

    #[test]
    fn unsupported_types_name_what_is_editable() {
        let err = editable_format_of("photo.png").unwrap_err().to_string();
        assert!(err.contains("cannot be edited"), "{}", err);
        assert!(err.contains("DOCX"), "{}", err);
    }

    #[test]
    fn a_password_protected_or_corrupt_package_is_refused_before_any_row_exists() {
        let err = DraftManager::validate_openable("docx", b"not a zip at all")
            .unwrap_err()
            .to_string();
        assert!(err.contains("password-protected"), "{}", err);
    }

    #[test]
    fn a_pdf_without_its_header_is_refused() {
        assert!(DraftManager::validate_openable("pdf", b"%PDF-1.7\n...").is_ok());
        assert!(DraftManager::validate_openable("pdf", b"just text").is_err());
    }

    /// Cross-language invariant: the editable-format policy exists in Rust and
    /// in TypeScript, and the two must agree.
    ///
    /// Parsed from the real `.ts` file rather than restated here — a restated
    /// copy would be a third list free to drift. The same technique, and the
    /// same reason, as `supported_formats_match_the_frontend_list`.
    ///
    /// What a one-sided edit would cost: the workspace offering a Vault file it
    /// then refuses to open, or hiding one it would happily have edited. Both
    /// look like bugs in the editor rather than in a list.
    #[test]
    fn editable_formats_match_the_frontend_list() {
        let ts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src/editableFormats.ts");
        let source = std::fs::read_to_string(&ts_path).unwrap_or_else(|e| {
            panic!(
                "cannot read the frontend editable-format list at {}: {} - if the file \
                 moved, update this test rather than deleting it; it is the only thing \
                 keeping the two lists in agreement",
                ts_path.display(),
                e
            )
        });

        let body = source
            .split_once("export const EDITABLE_EXTENSIONS = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(inside, _)| inside)
            .expect("EDITABLE_EXTENSIONS array literal not found in editableFormats.ts");

        let ts_extensions: Vec<String> = body
            .split(',')
            .map(|entry| entry.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|entry| !entry.is_empty())
            .collect();

        let rust_extensions: Vec<String> =
            EDITABLE_FORMATS.iter().map(|e| e.to_string()).collect();

        assert_eq!(
            ts_extensions, rust_extensions,
            "editableFormats.ts and EDITABLE_FORMATS have diverged - both must list the \
             same extensions in the same order"
        );
    }

    /// The client's size bound must be the server's, for the same reason: an
    /// oversized upload has to be refused with a sentence, not with the bare
    /// 413 the body limit produces before any handler runs.
    #[test]
    fn the_editable_size_bound_matches_the_frontend() {
        let ts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src/editableFormats.ts");
        let source = std::fs::read_to_string(&ts_path).expect("editableFormats.ts");

        let literal = source
            .split_once("export const MAX_EDITABLE_BYTES =")
            .and_then(|(_, rest)| rest.split_once('\n'))
            .map(|(value, _)| value.trim().trim_end_matches(';').to_string())
            .expect("MAX_EDITABLE_BYTES not found in editableFormats.ts");

        // "40 * 1024 * 1024"
        let product: u64 = literal
            .split('*')
            .map(|part| part.trim().parse::<u64>().expect("numeric factor"))
            .product();

        assert_eq!(
            product, MAX_EDITABLE_BYTES,
            "editableFormats.ts declares {} bytes but Rust enforces {}",
            product, MAX_EDITABLE_BYTES
        );
    }

    /// The editable set must be a strict subset of what can be attached at all.
    ///
    /// If it were not, the workspace could open a file the rest of the app
    /// refuses to ingest — and `publish` would then fail on a draft the user
    /// had already spent an afternoon editing.
    #[test]
    fn every_editable_format_is_also_an_accepted_attachment() {
        for format in EDITABLE_FORMATS {
            assert!(
                crate::utils::SUPPORTED_ATTACHMENT_EXTENSIONS.contains(format),
                "'{}' can be edited but not attached; publishing such a draft would fail",
                format
            );
        }
    }

    #[test]
    fn version_paths_are_app_data_relative_and_carry_the_format() {
        assert_eq!(
            DraftManager::relative_version_path(7, 3, "docx"),
            "drafts/7/v3.docx"
        );
    }
}
