//! The contract between the Rust patcher and the React editors.
//!
//! # The address is the contract
//!
//! Every editable thing carries an `addr` — `body/p[12]/r[0]`, `sheet[0]/B7`,
//! `slide[2]/sp[1]`. The browser never sends a document; it sends "replace the
//! text of THIS node with THIS string". Rust resolves the address to a byte
//! span in the original part and splices it.
//!
//! Two consequences fall straight out of that, and they are the whole reason
//! the design is shaped this way:
//!
//! 1. A construct the editor cannot render is never *addressed*, so it can
//!    never be damaged. Unrendered is not the same as lost.
//! 2. Two edits in different parts of a document cannot clobber each other,
//!    because each names exactly one node.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::ooxml::{docx, pptx, xlsx};
use super::{pdf_workspace, text_workspace};

/// What the editor renders. Tagged by format so the React side can switch on
/// one field rather than guessing from which keys are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase")]
pub enum DraftViewModel {
    Docx {
        version: i64,
        blocks: Vec<docx::Block>,
        styles: Vec<docx::StyleInfo>,
    },
    Xlsx {
        version: i64,
        sheets: Vec<xlsx::Sheet>,
    },
    Pptx {
        version: i64,
        slides: Vec<pptx::Slide>,
    },
    /// PDF pages are rendered as images by pdfium and fetched separately from
    /// `/drafts/:id/page/:n`; the model carries only geometry and annotations.
    Pdf {
        version: i64,
        pages: Vec<PdfPage>,
    },
    Txt {
        version: i64,
        content: String,
        /// Reported so the UI can say "Windows-1252, CRLF" rather than
        /// pretending every text file is UTF-8 with Unix endings.
        encoding: String,
        line_ending: String,
        bom: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfPage {
    pub index: usize,
    pub width: f32,
    pub height: f32,
    pub annotations: Vec<PdfAnnotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfAnnotation {
    /// Index of the annotation on its page — pdfium's own addressing.
    pub index: usize,
    pub kind: String,
    pub rect: [f32; 4],
    pub contents: Option<String>,
}

/// One edit, addressed to one node.
///
/// Deliberately a closed set. A patch the backend does not understand is
/// rejected by name rather than ignored: silently dropping an edit the user
/// made and watched appear on screen is the worst failure this system could
/// have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum DraftPatch {
    // ---- DOCX ----
    SetRunText {
        addr: String,
        text: String,
    },
    SetRunFormat {
        addr: String,
        #[serde(default)]
        bold: Option<bool>,
        #[serde(default)]
        italic: Option<bool>,
        #[serde(default)]
        underline: Option<bool>,
    },
    SetParagraphStyle {
        addr: String,
        /// `None` clears the style back to the document default.
        #[serde(default)]
        style: Option<String>,
    },
    InsertParagraphAfter {
        addr: String,
        text: String,
    },
    DeleteParagraph {
        addr: String,
    },

    // ---- XLSX ----
    SetCellValue {
        addr: String,
        /// The literal the user typed. A leading `=` makes it a formula.
        value: String,
    },
    ClearCell {
        addr: String,
    },

    // ---- PPTX ----
    SetShapeText {
        addr: String,
        text: String,
    },
    SetShapeBox {
        addr: String,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
    },
    DeleteShape {
        addr: String,
    },

    // ---- PDF ----
    AddAnnotation {
        page: usize,
        kind: String,
        rect: [f32; 4],
        #[serde(default)]
        contents: Option<String>,
        /// RGB 0-255. Defaults to the highlighter yellow when absent.
        #[serde(default)]
        color: Option<[u8; 3]>,
    },
    DeleteAnnotation {
        page: usize,
        index: usize,
    },
    DeletePage {
        page: usize,
    },
    /// Clockwise, in degrees, relative to the page's current rotation.
    /// Only quarter turns; anything else is refused rather than rounded.
    RotatePage {
        page: usize,
        degrees: i32,
    },
    // NOTE: there is deliberately no page-reorder operation.
    //
    // pdfium exposes `FPDF_MovePages`, but pdfium-render keeps the document
    // handle it needs `pub(crate)`, so it cannot be reached from here. The
    // usual workaround — copy the pages into a fresh document in the new order
    // — produces a document that has lost the original's form fields,
    // bookmarks, attachments and metadata. Reordering two pages must not
    // quietly discard the rest of the file, so this stays unimplemented until
    // the safe binding exists.

    // ---- TXT ----
    SetText {
        content: String,
    },
}

impl DraftPatch {
    /// Human name used in the "that edit does not apply to this format" error.
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::SetRunText { .. } => "SetRunText",
            Self::SetRunFormat { .. } => "SetRunFormat",
            Self::SetParagraphStyle { .. } => "SetParagraphStyle",
            Self::InsertParagraphAfter { .. } => "InsertParagraphAfter",
            Self::DeleteParagraph { .. } => "DeleteParagraph",
            Self::SetCellValue { .. } => "SetCellValue",
            Self::ClearCell { .. } => "ClearCell",
            Self::SetShapeText { .. } => "SetShapeText",
            Self::SetShapeBox { .. } => "SetShapeBox",
            Self::DeleteShape { .. } => "DeleteShape",
            Self::AddAnnotation { .. } => "AddAnnotation",
            Self::DeleteAnnotation { .. } => "DeleteAnnotation",
            Self::DeletePage { .. } => "DeletePage",
            Self::RotatePage { .. } => "RotatePage",
            Self::SetText { .. } => "SetText",
        }
    }
}

/// Build the view model for a draft's current bytes.
pub fn build(format: &str, bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    match format {
        "docx" => docx::build(bytes, version),
        "xlsx" => xlsx::build(bytes, version),
        "pptx" => pptx::build(bytes, version),
        "pdf" => pdf_workspace::build(bytes, version),
        "txt" => text_workspace::build(bytes, version),
        other => Err(anyhow!("'{}' is not an editable workspace format", other)),
    }
}

/// Apply patches, returning the new file bytes.
pub fn apply(format: &str, bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    if patches.is_empty() {
        return Ok(bytes.to_vec());
    }
    match format {
        "docx" => docx::apply(bytes, patches),
        "xlsx" => xlsx::apply(bytes, patches),
        "pptx" => pptx::apply(bytes, patches),
        "pdf" => pdf_workspace::apply(bytes, patches),
        "txt" => text_workspace::apply(bytes, patches),
        other => Err(anyhow!("'{}' is not an editable workspace format", other)),
    }
}

/// The error raised when a patch reaches the wrong format's handler.
///
/// This is a caller bug, not user error, so it names the operation and the
/// format rather than saying "invalid request".
pub fn wrong_format(patch: &DraftPatch, format: &str) -> anyhow::Error {
    anyhow!(
        "The edit '{}' does not apply to a {} document. This is a bug in the editor, \
         not something you did — the document has not been changed.",
        patch.op_name(),
        format.to_uppercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_round_trip_through_json_with_their_op_tag() {
        let p = DraftPatch::SetRunText {
            addr: "body/p[12]/r[0]".into(),
            text: "Amended".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"op\":\"SetRunText\""), "{}", json);

        let back: DraftPatch = serde_json::from_str(&json).unwrap();
        match back {
            DraftPatch::SetRunText { addr, text } => {
                assert_eq!(addr, "body/p[12]/r[0]");
                assert_eq!(text, "Amended");
            }
            other => panic!("round-tripped into {:?}", other),
        }
    }

    #[test]
    fn an_unknown_format_is_refused_by_name() {
        let err = build("rtf", b"", 1).unwrap_err().to_string();
        assert!(err.contains("rtf"), "{}", err);
    }

    #[test]
    fn a_misdirected_patch_says_so_and_promises_the_document_is_intact() {
        let p = DraftPatch::SetCellValue { addr: "sheet[0]/A1".into(), value: "1".into() };
        let msg = wrong_format(&p, "docx").to_string();
        assert!(msg.contains("SetCellValue"), "{}", msg);
        assert!(msg.contains("DOCX"), "{}", msg);
        assert!(msg.contains("has not been changed"), "{}", msg);
    }
}
